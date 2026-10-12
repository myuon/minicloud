//! The admin side of the host (issue #17): the `host` module (capability
//! `admin`), the changes it makes and keeps, and routing by hostname.
//!
//! The admin app here is the `control` fixture — a plain-text window on the
//! module, one path per operation — copied in as the app `admin`, reached by
//! the hostname `admin.example`. Nothing asserts a duration: where a test
//! needs a request in flight it waits for the host's stats to say so.

mod common;

use std::path::Path;
use std::thread;

use common::*;
use minicloud::Host;

/// The admin app's hostname in these tests.
const ADMIN_HOST: &str = "admin.example";

const CONTROL_TOML: &str = "grant = [\"admin\"]\n\n[limits]\ndeadline = \"60s\"\n\n\
[route]\nhosts = [\"admin.example\"]\n";

/// The control fixture as the app `admin`.
fn control() -> AppSpec<'static> {
    AppSpec {
        name: "admin",
        from: fixtures().join("control"),
        config: Some(CONTROL_TOML),
    }
}

/// `GET path` with `Host: host`.
fn get_at(addr: std::net::SocketAddr, host: &str, path: &str) -> Answer {
    send_raw(
        addr,
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
}

/// One operation of the control fixture.
fn op(host: &Host, path: &str) -> Answer {
    get_at(host.addr, ADMIN_HOST, path)
}

/// The fixture's line for `app` in `/apps`.
fn line(host: &Host, app: &str) -> String {
    let apps = op(host, "/apps");
    assert_eq!(apps.status, 200, "{}", apps.body);
    apps.body
        .lines()
        .find(|line| line.starts_with(&format!("{app} ")))
        .unwrap_or_else(|| panic!("no `{app}` in\n{}", apps.body))
        .to_string()
}

/// A request to the admin listener.
fn listener(host: &Host, method: &str, path: &str) -> Answer {
    send_raw(
        host.admin_addr.expect("an admin listener"),
        format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Authorization: Bearer {ADMIN_TOKEN}\r\nContent-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    )
}

fn overrides(data: &Path) -> String {
    std::fs::read_to_string(data.join("_host/overrides.json")).unwrap_or_default()
}

#[test]
fn the_list_shows_every_app_its_state_and_its_capabilities() {
    let apps = apps(&[
        control(),
        sample("notes"),
        sample("hello"),
        fixture("greedy"),
    ]);
    let host = start(&apps, 2);
    let admin = line(&host, "admin");
    assert!(admin.contains(" serving "), "{admin}");
    assert!(
        admin.contains("granted=[admin] required=[admin]"),
        "{admin}"
    );
    assert!(admin.contains("admin=true"), "{admin}");
    assert!(admin.contains("hosts=[admin.example]"), "{admin}");
    let notes = line(&host, "notes");
    assert!(
        notes.contains("granted=[kv,log] required=[kv,log]"),
        "{notes}"
    );
    assert!(notes.contains("hostcalls=1000"), "{notes}");
    assert!(notes.contains("admin=false"), "{notes}");
    let greedy = line(&host, "greedy");
    assert!(greedy.contains(" refused "), "{greedy}");
    assert!(greedy.contains("requires `log`"), "{greedy}");
    // The capabilities a grant may name, `admin` among them.
    let capabilities = op(&host, "/capabilities").body;
    assert_eq!(
        capabilities.trim(),
        "admin,auth,defer,fetch,kv,log,random,sql,time,timer"
    );
    // And the grant shows where every listing shows grants.
    assert_eq!(app_stats(&host, "admin")["granted"], "admin");
    assert!(host.banner().contains("granted [admin]"));
}

#[test]
fn the_admin_app_reads_another_apps_store_and_log_and_makes_no_store() {
    let apps = apps(&[control(), sample("notes"), sample("hello")]);
    let host = start(&apps, 2);
    for (name, body) in [("alpha", "one"), ("beta", "two"), ("gamma", "three")] {
        let request = format!(
            "PUT /notes/{name} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        assert_eq!(send_raw(host.addr, request.as_bytes()).status, 201);
    }

    let all = op(&host, "/kv?app=notes&n=10");
    assert_eq!(all.status, 200, "{}", all.body);
    assert_eq!(
        all.body.lines().collect::<Vec<_>>(),
        [
            "note:alpha | one | 3 | false",
            "note:beta | two | 3 | false",
            "note:gamma | three | 5 | false"
        ]
    );
    let some = op(&host, "/kv?app=notes&prefix=note:&after=note:alpha&n=1");
    assert_eq!(some.body.trim(), "note:beta | two | 3 | false");
    let none = op(&host, "/kv?app=notes&prefix=zzz&n=10");
    assert_eq!(none.body.trim(), "");

    let one = op(&host, "/kvValue?app=notes&key=note:gamma");
    assert_eq!(one.status, 200, "{}", one.body);
    assert_eq!(one.body.trim(), "note:gamma | three | 5 | false");
    let missing = op(&host, "/kvValue?app=notes&key=todo");
    assert_eq!(missing.status, 404);
    assert!(
        missing.body.contains("no key `todo` in `notes`'s store"),
        "{}",
        missing.body
    );

    let logs = op(&host, "/logs?app=notes&n=20");
    assert_eq!(logs.status, 200, "{}", logs.body);
    assert!(
        logs.body
            .lines()
            .any(|l| l.contains(" | info | stored gamma")),
        "{}",
        logs.body
    );

    // An app with no store has none made by being looked at, and a name
    // that is no app reads nothing.
    let hello = op(&host, "/kv?app=hello&n=10");
    assert_eq!(hello.status, 200, "{}", hello.body);
    assert_eq!(hello.body.trim(), "");
    assert!(!apps.data.join("hello").join("kv.sqlite3").exists());
    assert_eq!(op(&host, "/kv?app=..&n=10").body.trim(), "");
    assert_eq!(op(&host, "/kvValue?app=hello&key=x").status, 404);
    assert!(!apps.data.join("hello").join("kv.sqlite3").exists());
}

#[test]
fn a_disabled_app_answers_503_finishes_what_it_had_and_keeps_its_data() {
    let apps = apps(&[control(), sample("notes"), sample("slow"), sample("hello")]);
    let host = start(&apps, 2);
    let addr = host.addr;
    let stored = send_raw(
        addr,
        b"PUT /notes/kept HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 4\r\n\r\nsafe",
    );
    assert_eq!(stored.status, 201, "{}", stored.body);

    // A request in flight — parked — when its app is disabled.
    let parked = thread::spawn(move || get(addr, "/slow/?ms=800"));
    wait_until("the sleep to park", || count(&host, "slow", "parked") == 1);
    let disabled = op(&host, "/enable?app=slow&on=false");
    assert_eq!(disabled.status, 200, "{}", disabled.body);
    let refused = get(addr, "/slow/?ms=1");
    assert_eq!(refused.status, 503);
    assert!(refused.body.contains("disabled"), "{}", refused.body);
    let finished = parked.join().unwrap();
    assert_eq!(finished.status, 200, "{}", finished.body);
    assert_eq!(count(&host, "slow", "rejected.disabled"), 1);
    assert_eq!(app_stats(&host, "slow")["state"], "disabled");
    assert!(line(&host, "slow").contains(" disabled "));
    // The others serve throughout.
    assert_eq!(get(addr, "/hello/").status, 200);

    // Disabled and enabled again, with its store as it was.
    assert_eq!(op(&host, "/enable?app=notes&on=false").status, 200);
    assert_eq!(get(addr, "/notes/kept").status, 503);
    assert!(overrides(&apps.data).contains("\"enabled\": false"));
    assert_eq!(op(&host, "/enable?app=notes&on=true").status, 200);
    let back = get(addr, "/notes/kept");
    assert_eq!((back.status, back.body.as_str()), (200, "safe"));
    assert_eq!(op(&host, "/enable?app=slow&on=true").status, 200);
    assert_eq!(get(addr, "/slow/?ms=1").status, 200);
    // Nothing is left of the overrides once both are enabled again.
    assert!(
        !overrides(&apps.data).contains("notes"),
        "{}",
        overrides(&apps.data)
    );
}

#[test]
fn taking_away_a_needed_capability_refuses_that_app_and_no_other() {
    let apps = apps(&[control(), sample("notes"), sample("hello")]);
    let host = start(&apps, 2);
    let addr = host.addr;
    send_raw(
        addr,
        b"PUT /notes/kept HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok",
    );
    let taken = op(&host, "/configure?app=notes&grant=log");
    assert_eq!(taken.status, 200, "{}", taken.body);
    assert!(taken.body.contains("now refused"), "{}", taken.body);
    let notes = line(&host, "notes");
    assert!(notes.contains(" refused "), "{notes}");
    assert!(notes.contains("removed=[kv]"), "{notes}");
    assert!(
        notes.contains("requires `kv`, which app.toml with the admin's changes does not grant"),
        "{notes}"
    );
    let refused = get(addr, "/notes/kept");
    assert_eq!(refused.status, 503);
    assert!(refused.body.contains("requires `kv`"), "{}", refused.body);
    assert_eq!(get(addr, "/hello/").status, 200);
    assert!(overrides(&apps.data).contains("\"grant_remove\""));

    // Granted back: it loads again, with its store.
    let given = op(&host, "/configure?app=notes&grant=kv,log");
    assert_eq!(given.status, 200, "{}", given.body);
    assert!(line(&host, "notes").contains(" serving "));
    assert_eq!(get(addr, "/notes/kept").body, "ok");
    assert!(!overrides(&apps.data).contains("notes"));
}

#[test]
fn a_change_that_is_wrong_is_refused_with_the_reason_and_changes_nothing() {
    let apps = apps(&[control(), sample("notes"), sample("hello")]);
    let host = start(&apps, 2);
    let before = line(&host, "hello");
    for (change, reason) in [
        ("inflight=0", "`maxInFlight` must be at least 1"),
        ("hostcalls=-5", "`maxHostCalls` cannot be negative"),
        ("deadline=0", "`deadlineMs` must be at least 1"),
        ("heap=9999999999", "largest heap a run can have"),
        ("allow=ftp://nope", "is not an allowlist entry"),
        (
            "grant=log,teleport",
            "`teleport` is not a capability this host has",
        ),
        ("grant=admin", "only the app `admin` may be granted"),
    ] {
        let answer = op(&host, &format!("/configure?app=hello&{change}"));
        assert_eq!(answer.status, 422, "{change}: {}", answer.body);
        assert!(answer.body.contains(reason), "{change}: {}", answer.body);
    }
    // The same version, the same config, nothing kept.
    assert_eq!(line(&host, "hello"), before);
    assert_eq!(get(host.addr, "/hello/").status, 200);
    assert!(!overrides(&apps.data).contains("hello"));
    // Every attempt is in the history, refused.
    let history = op(&host, "/history?n=100").body;
    assert_eq!(
        history
            .lines()
            .filter(|l| l.contains("| hello | configure | refused: "))
            .count(),
        7,
        "{history}"
    );

    // A change that is right is kept.
    let fine = op(
        &host,
        "/configure?app=hello&hostcalls=3000&inflight=7&allow=https://example.com",
    );
    assert_eq!(fine.status, 200, "{}", fine.body);
    let after = line(&host, "hello");
    assert!(after.contains("hostcalls=3000"), "{after}");
    assert!(after.contains("inflight=7"), "{after}");
    assert!(after.contains("allow=[https://example.com]"), "{after}");
    assert!(after.contains("version=v2-"), "{after}");
    assert_eq!(app_stats(&host, "hello")["limits"]["max_in_flight"], 7);
}

#[test]
fn only_the_admin_app_may_be_granted_admin() {
    let apps = apps(&[
        control(),
        sample_with("hello", "grant = [\"admin\"]\n"),
        sample("notes"),
    ]);
    let host = start(&apps, 2);
    let hello = line(&host, "hello");
    assert!(hello.contains(" refused "), "{hello}");
    assert!(
        hello.contains("only the app `admin` may be granted"),
        "{hello}"
    );
    assert_eq!(get(host.addr, "/hello/").status, 503);
    // `check` refuses it the same way.
    let report = minicloud::toolchain::check(
        &apps.root,
        &["hello".to_string()],
        &minicloud::HostModules::standard(),
    )
    .unwrap();
    assert!(!report.ok);
    assert!(
        report.out.contains("only the app `admin` may be granted"),
        "{}",
        report.out
    );
    // And no change can grant it.
    let answer = op(&host, "/configure?app=notes&grant=kv,log,admin");
    assert_eq!(answer.status, 422, "{}", answer.body);
}

#[test]
fn the_admin_app_cannot_disable_itself_or_drop_admin_but_the_listener_can() {
    let apps = apps(&[control(), sample("hello")]);
    let host = start(&apps, 2);
    let disable = op(&host, "/enable?app=admin&on=false");
    assert_eq!(disable.status, 422, "{}", disable.body);
    assert!(
        disable.body.contains("cannot disable itself"),
        "{}",
        disable.body
    );
    let drop = op(&host, "/configure?app=admin&grant=");
    assert_eq!(drop.status, 422, "{}", drop.body);
    assert!(drop.body.contains("keeps `admin`"), "{}", drop.body);
    assert!(line(&host, "admin").contains(" serving "));
    // Its own limits it may change.
    assert_eq!(op(&host, "/configure?app=admin&hostcalls=2000").status, 200);

    // The emergency exit: the listener disables, enables and resets it.
    let off = listener(&host, "POST", "/apps/admin/disable");
    assert_eq!(off.status, 200, "{}", off.body);
    assert_eq!(op(&host, "/apps").status, 503);
    assert_eq!(listener(&host, "POST", "/apps/admin/enable").status, 200);
    assert!(line(&host, "admin").contains("changed=[max_host_calls]"));
    let reset = listener(&host, "POST", "/apps/admin/reset");
    assert_eq!(reset.status, 200, "{}", reset.body);
    assert!(line(&host, "admin").contains("hostcalls=1000"));
    assert!(line(&host, "admin").contains("changed=[]"));
    assert_eq!(listener(&host, "POST", "/apps/nope/disable").status, 404);
    // The listener's changes are in the history too.
    let changes = listener(&host, "GET", "/changes?n=10");
    assert!(
        changes.body.contains("\"who\": \"admin listener\""),
        "{}",
        changes.body
    );
    assert!(
        changes.body.contains("\"action\": \"reset\""),
        "{}",
        changes.body
    );
}

#[test]
fn changes_survive_a_restart_and_a_release_that_replaces_the_apps() {
    let first = apps(&[control(), sample("notes"), sample("hello"), sample("slow")]);
    let host = start(&first, 2);
    assert_eq!(op(&host, "/enable?app=slow&on=false").status, 200);
    assert_eq!(op(&host, "/configure?app=hello&hostcalls=3000").status, 200);
    assert_eq!(op(&host, "/configure?app=notes&grant=log").status, 200);
    drop(host);

    // A restart over the same apps and data.
    let host = start(&first, 2);
    let check = |host: &Host| {
        assert!(line(host, "slow").contains(" disabled "));
        assert_eq!(get(host.addr, "/slow/?ms=1").status, 503);
        assert!(line(host, "hello").contains("hostcalls=3000"));
        assert!(line(host, "notes").contains(" refused "));
        assert!(line(host, "notes").contains("removed=[kv]"));
        assert_eq!(get(host.addr, "/hello/").status, 200);
    };
    check(&host);
    drop(host);

    // A release: `apps/` replaced by fresh copies, the data directory kept.
    let release = apps(&[control(), sample("notes"), sample("hello"), sample("slow")]);
    let mut options = options(&release, 2);
    options.data = Some(first.data.clone());
    let host = Host::start(options).unwrap();
    check(&host);
    // And the history goes back to before the restarts.
    let history = op(&host, "/history?n=10").body;
    assert!(history.contains("| slow | disable | applied"), "{history}");
    assert!(
        history.contains("| hello | configure | applied"),
        "{history}"
    );
}

#[test]
fn the_history_says_who_when_and_what() {
    let apps = apps(&[control(), sample("hello")]);
    let host = start(&apps, 2);
    assert_eq!(
        op(
            &host,
            "/configure?app=hello&hostcalls=4000&who=owner@example.com"
        )
        .status,
        200
    );
    assert_eq!(listener(&host, "POST", "/apps/hello/update").status, 200);
    let history = op(&host, "/history?n=5").body;
    let lines: Vec<&str> = history.lines().collect();
    assert!(
        lines[0].starts_with("admin listener | hello | update | applied"),
        "{history}"
    );
    assert!(
        lines[1].starts_with("admin app: owner@example.com | hello | configure | applied"),
        "{history}"
    );
    // On disk, one JSON line each, with the time and what was asked.
    let file = std::fs::read_to_string(apps.data.join("_host/changes.jsonl")).unwrap();
    let configure: serde_json::Value = serde_json::from_str(file.lines().next().unwrap()).unwrap();
    assert!(configure["unix_ms"].as_u64().unwrap() > 1_700_000_000_000);
    assert!(
        configure["detail"]
            .as_str()
            .unwrap()
            .contains("max_host_calls=4000"),
        "{configure}"
    );
}

#[test]
fn an_app_with_a_hostname_is_reached_by_it_and_by_nothing_else() {
    let apps = apps(&[control(), sample("hello")]);
    let mut options = options(&apps, 2);
    options.forwarding.public_origin = Some("https://tools.example".parse().unwrap());
    let host = Host::start(options).unwrap();
    let addr = host.addr;
    // By its hostname, with the whole path; its origin is that hostname's.
    let origin = get_at(addr, "admin.example:8790", "/origin");
    assert_eq!(origin.status, 200, "{}", origin.body);
    assert_eq!(
        origin.body.trim(),
        "https://admin.example prefix= path=/origin"
    );
    // Not by its prefix, on the public hostname or any other.
    for host_header in ["tools.example", "localhost", "evil.example"] {
        let answer = get_at(addr, host_header, "/admin/apps");
        assert_eq!(answer.status, 404, "{host_header}: {}", answer.body);
    }
    // The hostname is the app's whole: `/hello/` there is the admin app's.
    assert_eq!(
        get_at(addr, ADMIN_HOST, "/hello/").body.trim(),
        "no such operation"
    );
    // Other apps as before, at the public origin.
    assert_eq!(get_at(addr, "tools.example", "/hello/").status, 200);
    assert!(host.banner().contains("public origin"));
}

#[test]
fn a_hostname_reaches_one_app() {
    let apps = apps(&[
        control(),
        sample_with("hello", "[route]\nhosts = [\"admin.example\"]\n"),
    ]);
    let host = start(&apps, 2);
    let hello = line(&host, "hello");
    assert!(hello.contains(" refused "), "{hello}");
    assert!(
        hello.contains("`admin.example` already reaches the app `admin`"),
        "{hello}"
    );
    assert_eq!(get_at(host.addr, ADMIN_HOST, "/apps").status, 200);
}

// ------------------------------------------------------- the real admin app

/// `apps/admin`'s app.toml, its secret a literal.
fn admin_app_toml() -> String {
    std::fs::read_to_string(bundled().join("admin/app.toml"))
        .unwrap()
        .replace("{ env = \"ADMIN_UI_TOKEN\" }", "{ value = \"ui-secret\" }")
}

/// A host over `apps/admin` and two sample apps, told it is reached at
/// `https://covtools.ramda.io`, as deployed.
fn deployed(apps: &Apps) -> Host {
    let mut options = options(apps, 2);
    options.forwarding.public_origin = Some("https://covtools.ramda.io".parse().unwrap());
    Host::start(options).unwrap()
}

const UI_HOST: &str = "covtools-admin.ramda.io";
/// `Basic base64("owner:ui-secret")`.
const UI_LOGIN: &str = "Basic b3duZXI6dWktc2VjcmV0";

/// A request to the admin app, with `extra` header lines.
fn ui(host: &Host, method: &str, path: &str, extra: &str, body: &str) -> Answer {
    send_raw(
        host.addr,
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {UI_HOST}\r\nConnection: close\r\n{extra}\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
}

fn authed() -> String {
    format!("Authorization: {UI_LOGIN}\r\n")
}

/// What a browser on the admin app's own pages sends with a form.
fn same_site() -> String {
    format!(
        "Authorization: {UI_LOGIN}\r\nOrigin: https://{UI_HOST}\r\nSec-Fetch-Site: same-origin\r\n"
    )
}

/// The configure form for hello, every limit as given but its host calls.
fn hello_form(host_calls: &str) -> String {
    format!(
        "allow=&maxHostCalls={host_calls}&deadlineMs=2000&maxHeapWords=0&maxInFlight=64\
         &maxQueued=256&maxRequestBytes=1048576&maxResponseBytes=4194304"
    )
}

fn ui_apps() -> Apps {
    let config = admin_app_toml();
    // Leaked: the spec borrows it for the test's length.
    let config: &'static str = Box::leak(config.into_boxed_str());
    apps(&[
        sample_with("admin", config),
        sample("hello"),
        sample("notes"),
    ])
}

#[test]
fn the_admin_app_needs_its_secret() {
    let apps = ui_apps();
    let host = deployed(&apps);
    for extra in [
        String::new(),
        "Authorization: Bearer wrong\r\n".to_string(),
        "Authorization: Basic b3duZXI6d3Jvbmc=\r\n".to_string(),
    ] {
        let answer = ui(&host, "GET", "/", &extra, "");
        assert_eq!(answer.status, 401, "{extra}");
        assert!(answer
            .header("www-authenticate")
            .unwrap()
            .starts_with("Basic"));
        let change = ui(&host, "POST", "/apps/hello/disable", &extra, "");
        assert_eq!(change.status, 401, "{extra}");
    }
    assert_eq!(get(host.addr, "/hello/").status, 200);
    let page = ui(&host, "GET", "/", &authed(), "");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("<b>hello</b>"), "{}", page.body);
    let csp = page.header("content-security-policy").unwrap();
    assert!(csp.contains("default-src 'none'") && csp.contains("frame-ancestors 'none'"));
    assert!(!csp.contains("script-src"), "{csp}");
    // Bearer works as well, for curl.
    let bearer = ui(
        &host,
        "GET",
        "/history",
        "Authorization: Bearer ui-secret\r\n",
        "",
    );
    assert_eq!(bearer.status, 200);
}

#[test]
fn a_cross_site_form_changes_nothing() {
    let apps = ui_apps();
    let host = deployed(&apps);
    for extra in [
        "Origin: https://evil.example\r\n",
        // The admin app's origin is its own hostname, not the main one.
        "Origin: https://covtools.ramda.io\r\n",
        "Origin: http://covtools-admin.ramda.io\r\n",
        "Sec-Fetch-Site: cross-site\r\n",
        "Sec-Fetch-Site: same-site\r\nOrigin: https://covtools-admin.ramda.io\r\n",
    ] {
        let answer = ui(
            &host,
            "POST",
            "/apps/hello/disable",
            &format!("{}{extra}", authed()),
            "",
        );
        assert_eq!(answer.status, 403, "{extra}: {}", answer.body);
    }
    assert_eq!(get(host.addr, "/hello/").status, 200);
    assert!(!overrides(&apps.data).contains("hello"));
    // From its own pages, the same form goes through.
    let answer = ui(&host, "POST", "/apps/hello/disable", &same_site(), "");
    assert_eq!(answer.status, 303, "{}", answer.body);
    assert_eq!(answer.header("location"), Some("/apps/hello?done=disable"));
    assert_eq!(get(host.addr, "/hello/").status, 503);
}

#[test]
fn the_admin_app_is_reached_by_its_hostname_only() {
    let apps = ui_apps();
    let host = deployed(&apps);
    for host_header in ["covtools.ramda.io", "localhost", "127.0.0.1:8790"] {
        for path in ["/admin/", "/admin/apps/hello", "/admin/history"] {
            let answer = send_raw(
                host.addr,
                format!(
                    "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\
                     Authorization: {UI_LOGIN}\r\n\r\n"
                )
                .as_bytes(),
            );
            assert_eq!(answer.status, 404, "{host_header}{path}: {}", answer.body);
        }
    }
    // Locally, by `admin.localhost`.
    let local = get_at(host.addr, "admin.localhost:8790", "/");
    assert_eq!(local.status, 401);
}

#[test]
fn the_pages_enable_disable_configure_and_reset() {
    let apps = ui_apps();
    let host = deployed(&apps);
    let addr = host.addr;
    // A wrong form: the problems, by field, and nothing done.
    let wrong = ui(
        &host,
        "POST",
        "/apps/hello/configure",
        &same_site(),
        &hello_form("lots"),
    );
    assert_eq!(wrong.status, 422);
    assert!(
        wrong
            .body
            .contains("host calls per request: `lots` is not a whole number"),
        "{}",
        wrong.body
    );
    assert!(
        wrong.body.contains("value=\"lots\""),
        "the form comes back as posted"
    );
    // A form from a page served before the fuel limit was removed (Cove ADR
    // 0091) is refused by name, not applied without it.
    let stale = ui(
        &host,
        "POST",
        "/apps/hello/configure",
        &same_site(),
        &format!("fuel=3000000&{}", hello_form("1000")),
    );
    assert_eq!(stale.status, 422);
    assert!(
        stale.body.contains("removed (Cove ADR 0091)"),
        "{}",
        stale.body
    );
    // A form the host refuses: its reason, and nothing done.
    let refused = ui(
        &host,
        "POST",
        "/apps/hello/configure",
        &same_site(),
        &hello_form("100").replace("maxHeapWords=0", "maxHeapWords=9999999999"),
    );
    assert_eq!(refused.status, 422);
    assert!(
        refused.body.contains("Refused: nothing was changed"),
        "{}",
        refused.body
    );
    assert!(
        refused.body.contains("largest heap a run can have"),
        "{}",
        refused.body
    );
    assert!(!overrides(&apps.data).contains("hello"));
    // A good one.
    let applied = ui(
        &host,
        "POST",
        "/apps/hello/configure",
        &same_site(),
        &hello_form("3000"),
    );
    assert_eq!(applied.status, 303, "{}", applied.body);
    let page = ui(&host, "GET", "/apps/hello?done=configure", &authed(), "");
    assert!(page.body.contains("Applied."), "{}", page.body);
    assert!(
        page.body.contains("3000 <span class=added"),
        "{}",
        page.body
    );
    assert_eq!(app_stats(&host, "hello")["limits"]["max_host_calls"], 3000);
    // Taking `kv` from notes refuses notes, and the page says why.
    let form = "cap.log=on&".to_string()
        + &hello_form("1000").replace("deadlineMs=2000", "deadlineMs=10000");
    let taken = ui(&host, "POST", "/apps/notes/configure", &same_site(), &form);
    assert_eq!(taken.status, 303, "{}", taken.body);
    assert_eq!(get(addr, "/notes/x").status, 503);
    let overview = ui(&host, "GET", "/", &authed(), "").body;
    assert!(overview.contains("state refused"), "{overview}");
    // Reset brings it back.
    assert_eq!(
        ui(&host, "POST", "/apps/notes/reset", &same_site(), "").status,
        303
    );
    assert_ne!(get(addr, "/notes/x").status, 503);
    // Disable and enable.
    assert_eq!(
        ui(&host, "POST", "/apps/hello/disable", &same_site(), "").status,
        303
    );
    assert_eq!(get(addr, "/hello/").status, 503);
    assert_eq!(
        ui(&host, "POST", "/apps/hello/enable", &same_site(), "").status,
        303
    );
    assert_eq!(get(addr, "/hello/").status, 200);
    // Not itself.
    let itself = ui(&host, "POST", "/apps/admin/disable", &same_site(), "");
    assert_eq!(itself.status, 422);
    assert!(
        itself.body.contains("cannot disable itself"),
        "{}",
        itself.body
    );
    let own = ui(&host, "GET", "/apps/admin", &authed(), "").body;
    assert!(
        !own.contains("action=\"/apps/admin/disable\""),
        "no disable button for itself"
    );
    // The history says who: with Access off (no `[access]` team or aud
    // here), the secret, which names nobody. A verified Access user's email
    // is recorded instead (tests/access.rs).
    let history = ui(&host, "GET", "/history", &authed(), "").body;
    assert!(history.contains("admin app: token"), "{history}");
}

#[test]
fn the_pages_escape_what_they_show() {
    let apps = ui_apps();
    let host = deployed(&apps);
    let sneaky = format!(
        "Authorization: {UI_LOGIN}\r\nCf-Access-Authenticated-User-Email: <script>alert(1)</script>\r\n"
    );
    ui(&host, "POST", "/apps/hello/disable", &sneaky, "");
    // What a client says of its user is not who: only a verified identity
    // is (`auth.identity`), so the header is not recorded at all.
    let history = ui(&host, "GET", "/history", &authed(), "").body;
    assert!(!history.contains("alert(1)"), "{history}");
    assert!(history.contains("admin app: token"), "{history}");
    // A form posted back is escaped too.
    let echoed = ui(
        &host,
        "POST",
        "/apps/hello/configure",
        &same_site(),
        &hello_form("%22%3E%3Cimg+src%3Dx%3E"),
    );
    assert_eq!(echoed.status, 422);
    assert!(!echoed.body.contains("<img"), "{}", echoed.body);
    assert!(
        echoed.body.contains("&quot;&gt;&lt;img src=x&gt;"),
        "{}",
        echoed.body
    );
}

/// An `overrides.json` written by a host from before Cove's ADR 0091 can
/// set `limits.fuel`. It is the operator's data, so the host starts and the
/// app serves: the key is dropped on load, and the rest of the change kept.
#[test]
fn a_persisted_override_with_the_removed_fuel_limit_does_not_stop_the_app() {
    let apps = apps(&[control(), sample("hello")]);
    std::fs::create_dir_all(apps.data.join("_host")).unwrap();
    std::fs::write(
        apps.data.join("_host/overrides.json"),
        r#"{ "apps": { "hello": { "limits": { "fuel": 3000000, "max_host_calls": 3000 } } } }"#,
    )
    .unwrap();
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/hello/").status, 200);
    assert_eq!(app_stats(&host, "hello")["limits"]["max_host_calls"], 3000);
    let hello = line(&host, "hello");
    assert!(hello.contains(" serving "), "{hello}");
    assert!(hello.contains("changed=[max_host_calls]"), "{hello}");
    // The next change written leaves it out.
    assert_eq!(op(&host, "/configure?app=hello&inflight=7").status, 200);
    assert!(!overrides(&apps.data).contains("fuel"));
}

#[test]
fn the_pages_read_an_apps_store_and_its_log() {
    let apps = ui_apps();
    let host = deployed(&apps);
    // What `notes` holds and what it said of holding it.
    // The third key's path is encoded: its key holds a space and an `&`.
    for (key, value) in [("todo", "buy milk"), ("zeta", "last"), ("a%20b%26c", "odd")] {
        let put = send_raw(
            host.addr,
            format!(
                "PUT /notes/{key} HTTP/1.1\r\nHost: covtools.ramda.io\r\nConnection: close\r\n\
                 Content-Length: {}\r\n\r\n{value}",
                value.len()
            )
            .as_bytes(),
        );
        assert_eq!(put.status, 201, "{}", put.body);
    }
    let store = ui(&host, "GET", "/apps/notes/kv", &authed(), "");
    assert_eq!(store.status, 200, "{}", store.body);
    assert!(store.body.contains("note:todo"), "{}", store.body);
    assert!(store.body.contains("buy milk"), "{}", store.body);
    // A key of its own, whatever is in the key: the link carries it encoded.
    // `notes` keys by the path as it came, so this key holds the `%20` and
    // the `%26` themselves, and the link encodes those percent signs again.
    let odd = "key=note%3Aa%2520b%2526c";
    assert!(store.body.contains(odd), "{}", store.body);
    let followed = ui(
        &host,
        "GET",
        &format!("/apps/notes/kv?{odd}"),
        &authed(),
        "",
    );
    assert_eq!(followed.status, 200, "{}", followed.body);
    assert!(followed.body.contains(">odd<"), "{}", followed.body);
    let key = ui(
        &host,
        "GET",
        "/apps/notes/kv?key=note%3Atodo",
        &authed(),
        "",
    );
    assert_eq!(key.status, 200, "{}", key.body);
    assert!(key.body.contains("buy milk"), "{}", key.body);
    let missing = ui(&host, "GET", "/apps/notes/kv?key=nothing", &authed(), "");
    assert_eq!(missing.status, 200, "{}", missing.body);
    assert!(missing.body.contains("Not read."), "{}", missing.body);
    // The log, with the level of each line.
    let log = ui(&host, "GET", "/apps/notes/logs?n=50", &authed(), "");
    assert_eq!(log.status, 200, "{}", log.body);
    assert!(log.body.contains("lvl-info"), "{}", log.body);
    assert!(log.body.contains("stored todo"), "{}", log.body);
    // An app with no store says so, and is given none by being looked at.
    let none = ui(&host, "GET", "/apps/hello/kv", &authed(), "");
    assert_eq!(none.status, 200, "{}", none.body);
    assert!(none.body.contains("is not granted"), "{}", none.body);
    assert!(!apps.data.join("hello/kv.sqlite3").exists());
    // Neither page writes, whatever is posted to it.
    for path in ["/apps/notes/kv", "/apps/notes/logs"] {
        let posted = ui(&host, "POST", path, &same_site(), "");
        assert_eq!(posted.status, 404, "{path}: {}", posted.body);
    }
}
