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
        "admin,auth,defer,fetch,kv,log,queue,random,socket,sql,stream,time,timer"
    );
    // And the grant shows where every listing shows grants.
    assert_eq!(app_stats(&host, "admin")["granted"], "admin");
    assert!(host.banner().contains("granted [admin]"));

    // The one thing the app can ask about itself, and no app name to ask it
    // with: the version it is running, the same string its answers carry as
    // `x-cove-app-version`. The admin app stamps the URLs of its own files
    // with it, so a deploy is a URL nothing has cached.
    let version = op(&host, "/selfVersion").body.trim().to_string();
    assert!(version.starts_with("v1-"), "{version}");
    assert_eq!(
        app_stats(&host, "admin")["version"].as_str(),
        Some(version.as_str())
    );
    assert_eq!(
        get_at(host.addr, ADMIN_HOST, "/capabilities").header("x-cove-app-version"),
        Some(version.as_str())
    );
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
fn the_admin_app_reads_an_apps_history_and_a_host_with_none_answers_nothing() {
    let apps = apps(&[control(), sample("hello")]);
    let host = start(&apps, 2);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // Minutes are taken for the minute that ended, at a time of the test's
    // own; the first call has nothing to take a difference from.
    let minute = now - now % 60;
    host.sample_now(minute - 180);
    for (n, requests) in [(1, 3), (2, 0)] {
        for _ in 0..requests {
            assert_eq!(get_at(host.addr, "localhost", "/hello/").status, 200);
        }
        host.sample_now(minute - 180 + n * 60);
    }

    let rows = op(&host, "/series?app=hello&over=day");
    assert_eq!(rows.status, 200, "{}", rows.body);
    let rows: Vec<&str> = rows.body.lines().collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    // Oldest first, each the increase over its minute.
    assert!(
        rows[0].starts_with(&format!(
            "{} | served=3 ok=3 errors=0 rejected=0",
            minute - 180
        )),
        "{}",
        rows[0]
    );
    assert!(
        rows[1].starts_with(&format!("{} | served=0 ", minute - 120)),
        "{}",
        rows[1]
    );
    // An `over` nobody knows is the hour; the month reads the hour table,
    // which nothing has rolled into yet; and an app with no history is
    // nothing.
    assert_eq!(
        op(&host, "/series?app=hello&over=eon").body.lines().count(),
        2
    );
    assert_eq!(op(&host, "/series?app=hello&over=month").body.trim(), "");
    assert_eq!(op(&host, "/series?app=nobody&over=day").body.trim(), "");

    // A host that keeps no history has no rows to give, and says so.
    let mut options = options(&apps, 2);
    options.data = None;
    let host = Host::start(options).expect("the host starts");
    let none = op(&host, "/series?app=hello&over=day");
    assert_eq!(none.status, 200, "{}", none.body);
    assert_eq!(none.body.trim(), "");
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
        for page in ["/", "/logs"] {
            let answer = ui(&host, "GET", page, &extra, "");
            assert_eq!(answer.status, 401, "{page}: {extra}");
            assert!(answer
                .header("www-authenticate")
                .unwrap()
                .starts_with("Basic"));
        }
        let change = ui(&host, "POST", "/apps/hello/disable", &extra, "");
        assert_eq!(change.status, 401, "{extra}");
    }
    assert_eq!(get(host.addr, "/hello/").status, 200);
    let page = ui(&host, "GET", "/", &authed(), "");
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains(">hello</a>"), "{}", page.body);
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
    assert_eq!(
        answer.header("location"),
        Some("/apps/hello/settings?done=disable")
    );
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
    let page = ui(
        &host,
        "GET",
        "/apps/hello/settings?done=configure",
        &authed(),
        "",
    );
    assert!(page.body.contains("Applied."), "{}", page.body);
    // The field holds the new value and says the value is the admin's.
    assert!(page.body.contains("value=\"3000\""), "{}", page.body);
    assert!(
        page.body
            .contains("host calls per request <span class=added title=\"set here, not in app.toml\">(changed)</span>"),
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
    // A refused app is one the list has something to say about, so it is
    // above the quiet ones and its pill is the way to the page that fixes
    // it.
    assert!(
        overview.contains("href=\"/apps/notes/settings\">refused →</a>"),
        "{overview}"
    );
    assert!(
        overview.find("notes").unwrap() < overview.find(">hello</a>").unwrap(),
        "the app with something to say is first:\n{overview}"
    );
    // And its Overview says why before anybody asks.
    let its_own = ui(&host, "GET", "/apps/notes", &authed(), "").body;
    assert!(
        its_own.contains("Refused, so nothing reaches it"),
        "{its_own}"
    );
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
    let own = ui(&host, "GET", "/apps/admin/settings", &authed(), "").body;
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

/// An app is a workspace (issue #108): every page about it carries the same
/// row of tabs, and a tab is there when the app has the thing.
#[test]
fn an_apps_tabs_are_what_it_was_granted() {
    let apps = ui_apps();
    let host = deployed(&apps);
    let has = |page: &str, tab: &str| page.contains(&format!(">{tab}</a>"));
    // `notes` is granted `kv` and `log`: a Data tab, and no store it has no
    // grant for.
    let notes = ui(&host, "GET", "/apps/notes", &authed(), "").body;
    for tab in ["Overview", "Logs", "Data", "Code", "Settings"] {
        assert!(has(&notes, tab), "no {tab} tab:\n{notes}");
    }
    for tab in ["SQL", "Queues"] {
        assert!(!has(&notes, tab), "a {tab} tab:\n{notes}");
    }
    // `hello` is granted nothing at all, so no store has a tab — and Logs
    // is still there, because the ring it reads is the host's and holds the
    // host's own lines about the app.
    let hello = ui(&host, "GET", "/apps/hello", &authed(), "").body;
    for tab in ["Overview", "Logs", "Code", "Settings"] {
        assert!(has(&hello, tab), "no {tab} tab:\n{hello}");
    }
    for tab in ["Data", "SQL", "Queues"] {
        assert!(!has(&hello, tab), "a {tab} tab:\n{hello}");
    }
    // The tab the page is marks itself, and the row is on every page about
    // the app, not only the Overview.
    assert!(
        hello.contains("<a href=\"/apps/hello\" aria-current=page>Overview</a>"),
        "{hello}"
    );
    let log = ui(&host, "GET", "/apps/hello/logs", &authed(), "").body;
    assert!(
        log.contains("<a href=\"/apps/hello/logs\" aria-current=page>Logs</a>"),
        "{log}"
    );
    let settings = ui(&host, "GET", "/apps/hello/settings", &authed(), "").body;
    assert!(
        settings.contains("<a href=\"/apps/hello/settings\" aria-current=page>Settings</a>"),
        "{settings}"
    );
    // Settings holds every form and no way into a window: the tabs are the
    // way, and a second one is a second thing to keep worded.
    for form in ["/apps/hello/configure", "/apps/hello/reset"] {
        assert!(settings.contains(form), "{settings}");
    }
    for gone in ["what it is", "what is waiting", "a statement to read them"] {
        assert!(!settings.contains(gone), "{gone}:\n{settings}");
    }
}

/// The Overview says what is worth a look before being asked — and says so
/// when nothing is, which is the thing somebody came to find out.
#[test]
fn the_overview_says_what_is_worth_a_look_and_when_nothing_is() {
    let apps = ui_apps();
    let host = deployed(&apps);
    let quiet = ui(&host, "GET", "/apps/hello", &authed(), "").body;
    assert!(quiet.contains("<h2>Worth a look</h2>"), "{quiet}");
    assert!(quiet.contains("class=\"is-ok\""), "{quiet}");
    assert!(quiet.contains("Nothing: no request failed"), "{quiet}");
    // The green line names what was looked at, the comparison with the
    // app's directory included — which this deploy is small enough for.
    assert!(
        quiet.contains("every file of the deploy is the same as the one in the app's directory"),
        "{quiet}"
    );
    // The numbers are above it, and the way to Settings below.
    assert!(quiet.contains("<h2>Now</h2>"), "{quiet}");
    assert!(
        quiet.contains("/apps/hello/settings\">Settings</a>:"),
        "{quiet}"
    );
    // `/spin` runs past the deadline: one failed request, and the Overview
    // leads with it.
    assert_eq!(get(host.addr, "/hello/spin").status, 504);
    let loud = ui(&host, "GET", "/apps/hello", &authed(), "").body;
    assert!(loud.contains("A request failed"), "{loud}");
    assert!(loud.contains("class=\"is-bad\""), "{loud}");
    assert!(loud.contains("Nothing else: "), "{loud}");
    assert!(
        loud.find("is-bad").unwrap() < loud.find("is-ok").unwrap(),
        "the bad news is first:\n{loud}"
    );
    // And the list puts it above the quiet apps, with the pill the way
    // there.
    let list = ui(&host, "GET", "/", &authed(), "").body;
    // The failure, and the admin app itself: its `[access]` names a team
    // and an aud the environment does not set here, so Access is off for it
    // and the list says so.
    assert!(list.contains("2 want a look."), "{list}");
    assert!(list.contains("1 error."), "{list}");
    assert!(
        list.contains("href=\"/apps/hello\">1 failed →</a>"),
        "{list}"
    );
    assert!(list.contains(">Access off →</a>"), "{list}");
    assert!(
        list.find(">hello</a>").unwrap() < list.find(">notes</a>").unwrap(),
        "the app with something to say is first:\n{list}"
    );
    // A quiet app says so, and says it quietly.
    assert!(list.contains("<span class=quiet>quiet</span>"), "{list}");
    // The two capability lists are gone from here; they are what Settings
    // is for.
    assert!(!list.contains("code requires"), "{list}");
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
    // The level is a cell of its own, so that a narrow screen can keep it
    // with the time and give what was said a line under them.
    assert!(
        log.body
            .contains("<td class=lv><span class=\"lvl lvl-info\">info</span></td>"),
        "{}",
        log.body
    );
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

/// Logs across every app (issue #108): the host mixes every app's ring into
/// one sequence, and each line says which app it came from.
#[test]
fn every_apps_lines_are_one_order() {
    let config = admin_app_toml();
    // Leaked: the spec borrows it for the test's length.
    let config: &'static str = Box::leak(config.into_boxed_str());
    let apps = apps(&[
        sample_with("admin", config),
        fixture("noisy"),
        sample("hello"),
    ]);
    let host = deployed(&apps);
    // Only the admin app has said anything yet — its `[access]` names a team
    // the environment does not set here — so it is the only chip: an app the
    // host has no line of is not offered, because the page a chip leads to
    // would be empty.
    let quiet = ui(&host, "GET", "/logs", &authed(), "");
    assert_eq!(quiet.status, 200, "{}", quiet.body);
    let quiet = quiet.body;
    assert!(
        quiet.contains("<a href=\"/apps/admin/logs\">admin</a>"),
        "{quiet}"
    );
    for silent in ["/apps/noisy/logs", "/apps/hello/logs"] {
        assert!(!quiet.contains(silent), "{silent}:\n{quiet}");
    }
    // Where the page is, in the navigation and in the row of apps.
    assert!(
        quiet.contains("<a href=\"/logs\" aria-current=page>logs</a>"),
        "{quiet}"
    );
    assert!(
        quiet.contains("<a href=\"/logs\" aria-current=page>all apps</a>"),
        "{quiet}"
    );

    // Two apps talking, and the host itself: one sequence, oldest first,
    // every line saying whose it is.
    assert_eq!(get(host.addr, "/noisy/say?text=before").status, 200);
    assert_eq!(get(host.addr, "/hello/spin").status, 504);
    assert_eq!(get(host.addr, "/noisy/say?text=after").status, 200);
    let page = ui(&host, "GET", "/logs", &authed(), "").body;
    assert!(
        page.contains("<td class=app><code>noisy</code></td>"),
        "{page}"
    );
    assert!(
        page.contains("<td class=app><code>hello</code></td>"),
        "{page}"
    );
    let before = page.find("before").unwrap();
    let failed = page.find("504 deadline").unwrap();
    let after = page.find("after").unwrap();
    assert!(
        before < failed && failed < after,
        "the apps are one sequence, oldest first:\n{page}"
    );
    // And every app with a log is a way to read it alone.
    for app in ["admin", "noisy", "hello"] {
        assert!(
            page.contains(&format!("<a href=\"/apps/{app}/logs\">{app}</a>")),
            "{page}"
        );
    }

    // A line is whatever the app wrote: an app that logs markup makes a line
    // of text here, never a tag.
    assert_eq!(
        get(host.addr, "/noisy/say?text=%3Cimg%20src=y%3E%26%22%27").status,
        200
    );
    let escaped = ui(&host, "GET", "/logs", &authed(), "").body;
    assert!(
        escaped.contains("&lt;img src=y&gt;&amp;&quot;&#39;"),
        "{escaped}"
    );
    assert!(!escaped.contains("<img"), "{escaped}");

    // `?n=` is how many lines, as it is on an app's own log.
    let one = ui(&host, "GET", "/logs?n=1", &authed(), "").body;
    assert_eq!(one.matches("<td class=app>").count(), 1, "{one}");
    assert!(!one.contains("504 deadline"), "{one}");

    // One app with a lot to say fills the page, and the quiet ones fall off
    // the end: that is what "the last n lines" is. `?n=` is how to see past
    // it...
    assert_eq!(get(host.addr, "/noisy/say?text=chatter&n=60").status, 200);
    let fifty = ui(&host, "GET", "/logs?n=50", &authed(), "").body;
    assert_eq!(fifty.matches("<td class=app>").count(), 50, "{fifty}");
    assert!(!fifty.contains("<code>hello</code>"), "{fifty}");
    let more = ui(&host, "GET", "/logs?n=200", &authed(), "").body;
    assert!(more.contains("<code>hello</code>"), "{more}");
    // ...and an app's own Logs is how to read it alone, however loud its
    // neighbour is. The chip is still offered, too.
    assert!(
        fifty.contains("<a href=\"/apps/hello/logs\">hello</a>"),
        "the way to a quiet app's log does not come and go with `?n=`:\n{fifty}"
    );
    let alone = ui(&host, "GET", "/apps/hello/logs?n=50", &authed(), "").body;
    assert!(alone.contains("504 deadline"), "{alone}");
    // One app's page knows the app, so it draws no column of it.
    assert!(!alone.contains("<td class=app>"), "{alone}");

    // The page only reads, whatever is posted to it.
    assert_eq!(ui(&host, "POST", "/logs", &same_site(), "").status, 404);
}

#[test]
fn the_platform_page_shows_the_host_against_its_limits() {
    let apps = ui_apps();
    let host = deployed(&apps);
    let cpu = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let used = std::sync::Arc::clone(&cpu);
    host.set_platform_source(std::sync::Arc::new(move || {
        minicloud::platform::PlatformSample {
            memory_bytes: Some(1 << 30),
            memory_max_bytes: Some(2 << 30),
            cpu_usage_usec: Some(used.load(std::sync::atomic::Ordering::Relaxed)),
            cpu_throttled_usec: Some(0),
            cpu_quota_millicores: Some(2000),
            disk_total_bytes: Some(100 << 30),
            disk_available_bytes: Some(4 << 30),
            load1_milli: Some(1250),
            host_memory_total_bytes: Some(8 << 30),
            host_memory_available_bytes: Some(6 << 30),
            in_flight: 3,
            queued: 2,
            busy: 1,
            workers: 2,
            ..Default::default()
        }
    }));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let minute = now - now % 60;
    host.sample_now(minute - 180);
    for (n, usec) in [(1, 30_000_000), (2, 36_000_000)] {
        cpu.store(usec, std::sync::atomic::Ordering::Relaxed);
        host.sample_now(minute - 180 + n * 60);
    }
    let page = ui(&host, "GET", "/platform", &authed(), "");
    assert_eq!(page.status, 200, "{}", page.body);
    let body = page.body;
    for expected in [
        "<a href=\"/platform\" aria-current=page>platform</a>",
        // Memory: 1 GiB of 2, half.
        "<span class=big>1.0 GiB</span>",
        "of 2.0 GiB, 50%",
        // CPU: the last minute used 6 s of two CPUs' 120 s.
        "<span class=big>5%</span>",
        "of 2.00 CPUs (CPUQuota), last minute",
        // Disk: 96% used is trouble.
        "<div class=\"fig is-bad\">",
        "<span class=big>4.0 GiB</span>",
        // Workers, with requests waiting.
        "<span class=big>1 / 2</span>",
        "3 in flight, 2 queued",
        "<span class=big>1.25</span>",
        "2 rows, one for each minute",
        "<svg class=chart",
        "class=\"ln ln-limit\"",
    ] {
        assert!(body.contains(expected), "no `{expected}` in:\n{body}");
    }
    // The other windows: the month has no hours rolled up yet.
    let month = ui(&host, "GET", "/platform?over=month", &authed(), "").body;
    assert!(
        month.contains("Nothing to chart for the last 30 days"),
        "{month}"
    );
    assert!(
        month.contains("<span class=big>5%</span>"),
        "the figures are now"
    );
}

#[test]
fn the_timeline_pages_show_where_each_request_was() {
    let apps = ui_apps();
    let host = deployed(&apps);
    // A recorder of fixed times, so that the admin's own requests are not
    // what is drawn; the trackers of those in flight are kept to the end.
    let (recorder, _in_flight) = fixed_timeline();
    host.set_timeline_source(recorder);

    let live = ui(&host, "GET", "/live", &authed(), "");
    assert_eq!(live.status, 200, "{}", live.body);
    for expected in [
        "<meta http-equiv=refresh content=\"2\">",
        "<a href=\"/live\" aria-current=page>live</a>",
        "<a href=\"/timeline\">timeline</a>",
        "<a href=\"/platform\">platform</a>",
        // Oldest first: the parked one, the running one, the waiting one.
        "<a href=\"/requests/109\"><code>109</code></a>",
        "parked on timer.sleep",
        "<td class=nums data-label=elapsed>1000.0 ms</td>",
        "waiting for a worker",
        // Worker #1 runs 110; #0 is idle, its last request parked.
        "<span class=\"state busy\">busy</span>",
        "<span class=\"state idle\">idle</span>",
    ] {
        assert!(
            live.body.contains(expected),
            "no `{expected}` in:\n{}",
            live.body
        );
    }
    let order: Vec<usize> = ["/requests/109\"", "/requests/110\"", "/requests/111\""]
        .iter()
        .map(|id| live.body.find(id).unwrap())
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "oldest first");

    let picture = ui(&host, "GET", "/timeline?over=10s", &authed(), "");
    assert_eq!(picture.status, 200, "{}", picture.body);
    for expected in [
        "<a href=\"/timeline\" aria-current=page>timeline</a>",
        "<b>the last 10 seconds</b>",
        "<svg class=timeline",
        // A lane is the way to its request, with what it is on hover.
        "<a href=\"/requests/102\"><title>#102 slow: GET /slow/sleep — 200, 1080.0 ms</title>",
        // 102 resumed on another worker; 105's deadline; 107 turned away.
        "class=tl-moved",
        "class=tl-crit",
        "class=tl-rej",
        "class=tl-yield",
        "<line class=\"tl-park tl-l",
        // Each app's figures, and a chip each.
        "<a href=\"/timeline?app=slow&amp;over=10s\">slow</a>",
    ] {
        assert!(
            picture.body.contains(expected),
            "no `{expected}` in:\n{}",
            picture.body
        );
    }
    // The old requests are in the 5 minutes and not in the 10 seconds.
    assert!(!picture.body.contains("/requests/80\""));
    let wide = ui(&host, "GET", "/timeline?over=5m", &authed(), "").body;
    assert!(wide.contains("/requests/80\""), "{wide}");
    // One app's: only its lanes.
    let slow = ui(&host, "GET", "/timeline?app=slow&over=10s", &authed(), "").body;
    assert!(slow.contains("/requests/105\""), "{slow}");
    assert!(!slow.contains("/requests/101\""), "{slow}");

    let moved = ui(&host, "GET", "/requests/102", &authed(), "");
    assert_eq!(moved.status, 200, "{}", moved.body);
    for expected in [
        "<h1>Request 102</h1>",
        "parked on timer.sleep 1000.0 ms (answered)",
        "resumed on #1 (moved), ran 30.0 ms",
        "<a href=\"/timeline?app=slow&amp;over=1m\">timeline of slow</a>",
    ] {
        assert!(
            moved.body.contains(expected),
            "no `{expected}` in:\n{}",
            moved.body
        );
    }
    let late = ui(&host, "GET", "/requests/105", &authed(), "").body;
    assert!(
        late.contains("parked on fetch 2000.0 ms (the deadline)"),
        "{late}"
    );
    assert!(late.contains("504 deadline"), "{late}");
    let unknown = ui(&host, "GET", "/requests/424242", &authed(), "");
    assert_eq!(unknown.status, 404);
    assert!(
        unknown.body.contains("no request 424242 in the timeline"),
        "{}",
        unknown.body
    );
    assert_eq!(ui(&host, "GET", "/requests/abc", &authed(), "").status, 404);
    // The pages only read.
    assert_eq!(ui(&host, "POST", "/live", &same_site(), "").status, 404);
}

#[test]
fn the_load_page_starts_stops_and_shows_a_run() {
    let apps = ui_apps();
    let host = deployed(&apps);
    let idle = ui(&host, "GET", "/load", &authed(), "");
    assert_eq!(idle.status, 200, "{}", idle.body);
    assert!(idle
        .body
        .contains("No load has been run since the host started."));
    assert!(idle
        .body
        .contains("<a href=\"/load\" aria-current=page>load</a>"));
    assert!(!idle.body.contains("http-equiv"));

    // A form from another site starts nothing.
    let form = "mix=light&rate=0&connections=1&seconds=60";
    let crossed = ui(
        &host,
        "POST",
        "/load/start",
        &format!("{}Sec-Fetch-Site: cross-site\r\n", authed()),
        form,
    );
    assert_eq!(crossed.status, 403, "{}", crossed.body);
    assert!(!host.loader().status().running());

    let started = ui(&host, "POST", "/load/start", &same_site(), form);
    assert_eq!(started.status, 303, "{}", started.body);
    assert!(host.loader().status().running());
    // One at a time: the second is refused, with the host's reason.
    let again = ui(&host, "POST", "/load/start", &same_site(), form);
    assert_eq!(again.status, 422, "{}", again.body);
    assert!(
        again.body.contains("a load run is going on"),
        "{}",
        again.body
    );
    let running = ui(&host, "GET", "/load", &authed(), "");
    for expected in [
        "<meta http-equiv=refresh content=\"2\">",
        "<h2>Running</h2>",
        "action=\"/load/stop\"",
        "closed loop on 1 connections",
    ] {
        assert!(
            running.body.contains(expected),
            "no `{expected}` in:\n{}",
            running.body
        );
    }
    let stopped = ui(&host, "POST", "/load/stop", &same_site(), "");
    assert_eq!(stopped.status, 303, "{}", stopped.body);
    wait_until("the run to stop", || !host.loader().status().running());
    let last = ui(&host, "GET", "/load", &authed(), "");
    assert!(last.body.contains("<h2>Last run</h2>"), "{}", last.body);
    assert!(
        last.body
            .contains("<span class=\"state stopped\">stopped</span>"),
        "{}",
        last.body
    );
    assert!(!last.body.contains("http-equiv"));

    // `slow` is not on this host: said so, and the run ends at once.
    let wait = ui(
        &host,
        "POST",
        "/load/start",
        &same_site(),
        "mix=wait&rate=0&connections=1&seconds=60",
    );
    assert_eq!(wait.status, 303, "{}", wait.body);
    wait_until("the run to end", || !host.loader().status().running());
    let missing = ui(&host, "GET", "/load", &authed(), "");
    assert!(
        missing
            .body
            .contains("<li>target app <code>slow</code> is not deployed</li>"),
        "{}",
        missing.body
    );
    assert!(missing
        .body
        .contains("<span class=\"state finished\">finished</span>"));

    // A figure past its cap: the host's reason, the form as posted.
    let capped = ui(
        &host,
        "POST",
        "/load/start",
        &same_site(),
        "mix=cpu&rate=900&connections=1&seconds=5",
    );
    assert_eq!(capped.status, 422, "{}", capped.body);
    assert!(
        capped.body.contains("rate is 0 (closed loop) to 500"),
        "{}",
        capped.body
    );
    assert!(
        capped
            .body
            .contains("<input id=rate name=rate value=\"900\""),
        "{}",
        capped.body
    );

    // Figures of the test's own, shown as the runner's would be.
    host.set_load_status(minicloud::load::Status {
        current: Some(fixed_load_run("running")),
        last: None,
    });
    let fixed = ui(&host, "GET", "/load", &authed(), "");
    for expected in [
        "50 requests a second on up to 4 connections",
        "12.3 s of 20 s",
        "9 × 429, 0 × 503",
        "<span class=big>2.5 ms</span>",
        "p50; p99 181.0 ms",
        "<li>target app <code>crunch</code> is not deployed</li>",
    ] {
        assert!(
            fixed.body.contains(expected),
            "no `{expected}` in:\n{}",
            fixed.body
        );
    }
    // And `/_host/load` says the same.
    let status: serde_json::Value =
        serde_json::from_str(&get(host.addr, "/_host/load").body).unwrap();
    assert_eq!(status["current"]["sent"], 600);
}
