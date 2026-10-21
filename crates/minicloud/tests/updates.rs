//! Versioned updates through the admin listener. As elsewhere, nothing
//! asserts a duration: a test waits for the host's stats to show the state
//! it needs, then updates, then checks which version answered what.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use common::*;
use minicloud::Host;

/// A way to break the app's directory.
type Breaker<'a> = Box<dyn Fn() + 'a>;

/// Writes the `versioned` fixture's source into the test's apps directory
/// with `VERSION` replaced by `label`, and `app.toml` as `config` (or the
/// fixture's).
fn write_version(apps: &Apps, label: &str, config: Option<&str>) {
    let from = fixtures().join("versioned");
    let source = std::fs::read_to_string(from.join("versioned.cove")).unwrap();
    let dir = apps.root.join("versioned");
    std::fs::write(dir.join("versioned.cove"), source.replace("VERSION", label)).unwrap();
    let config = match config {
        Some(config) => config.to_string(),
        None => std::fs::read_to_string(from.join("app.toml")).unwrap(),
    };
    std::fs::write(dir.join("app.toml"), config).unwrap();
}

/// A request to the admin listener, with `token` as its bearer token.
fn admin(host: &Host, method: &str, path: &str, token: Option<&str>) -> Answer {
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    send_raw(
        host.admin_addr.expect("an admin listener"),
        format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{auth}Content-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    )
}

fn update(host: &Host, app: &str) -> Answer {
    admin(
        host,
        "POST",
        &format!("/apps/{app}/update"),
        Some(ADMIN_TOKEN),
    )
}

fn started(apps: &Apps, workers: usize) -> Host {
    write_version(apps, "one", None);
    start(apps, workers)
}

#[test]
fn in_flight_requests_finish_on_the_old_version_and_new_ones_get_the_new() {
    let apps = apps(&[fixture("versioned"), sample("hello")]);
    let host = started(&apps, 1);
    let addr = host.addr;
    let v1 = get(addr, "/versioned/")
        .header("x-cove-app-version")
        .unwrap()
        .to_string();
    assert!(v1.starts_with("v1-"), "{v1}");

    // Parked: a long sleep. Running and yielded: two long spins on the one
    // worker, taking turns. Queued: a fourth, past max_in_flight = 3.
    let parked = thread::spawn(move || get(addr, "/versioned/sleep?ms=1500"));
    wait_until("the sleep to park", || {
        count(&host, "versioned", "parked") == 1
    });
    let spins: Vec<_> = (0..2)
        .map(|_| thread::spawn(move || get(addr, "/versioned/spin?turns=50000000")))
        .collect();
    wait_until("both spins in flight and sliced", || {
        count(&host, "versioned", "in_flight") == 3 && count(&host, "versioned", "yields") > 0
    });
    let queued = thread::spawn(move || get(addr, "/versioned/"));
    wait_until("the fourth to queue", || {
        count(&host, "versioned", "queued") == 1
    });

    write_version(&apps, "two", None);
    let updated = update(&host, "versioned");
    assert_eq!(updated.status, 200, "{}", updated.body);
    let detail: serde_json::Value = serde_json::from_str(&updated.body).unwrap();
    assert_eq!(detail["previous"], v1.as_str());
    let v2 = detail["version"].as_str().unwrap().to_string();
    assert!(v2.starts_with("v2-"), "{v2}");
    // Both versions are alive while the old one's requests are in flight.
    assert_eq!(count(&host, "versioned", "versions_alive"), 2);

    // Admitted after the update: the new version, once there is room.
    let fresh = thread::spawn(move || get(addr, "/versioned/"));

    let old: Vec<Answer> = std::iter::once(parked)
        .chain(spins)
        .chain(std::iter::once(queued))
        .map(|t| t.join().unwrap())
        .collect();
    for answer in &old {
        assert_eq!(answer.status, 200, "{answer:?}");
        assert!(answer.body.starts_with("one "), "{answer:?}");
        assert_eq!(answer.header("x-cove-app-version"), Some(v1.as_str()));
    }
    assert_eq!(old[1].body, "one 50000000\n");
    let fresh = fresh.join().unwrap();
    assert_eq!(fresh.body, "two 0\n");
    assert_eq!(fresh.header("x-cove-app-version"), Some(v2.as_str()));

    // Drained: the old version, and the program it prepared, are gone.
    wait_until("the old version to be dropped", || {
        count(&host, "versioned", "versions_alive") == 1
            && count(&host, "versioned", "programs_alive") == 1
    });
    let versions = host.app_detail("versioned").unwrap()["versions"].clone();
    assert_eq!(versions[0]["version"], v1.as_str());
    assert_eq!(versions[0]["alive"], false);
    assert_eq!(versions[0]["program_alive"], false);
    assert_eq!(versions[1]["current"], true);
    assert_eq!(count(&host, "versioned", "updates"), 1);
    // The app's counters and its log carried over.
    assert_eq!(count(&host, "versioned", "served"), 6);
    assert!(get(addr, "/_host/apps/versioned/logs")
        .body
        .contains(&format!("updated: {v1} -> {v2}")));
}

#[test]
fn a_failed_update_keeps_the_current_version_and_says_why() {
    let apps = apps(&[fixture("versioned"), sample("hello")]);
    let host = started(&apps, 2);
    let addr = host.addr;
    let v1 = get(addr, "/versioned/")
        .header("x-cove-app-version")
        .unwrap()
        .to_string();
    // Another app answering throughout.
    let stop = Arc::new(AtomicBool::new(false));
    let neighbour = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut answered = 0;
            while !stop.load(Ordering::Relaxed) {
                assert_eq!(get(addr, "/hello/").status, 200);
                answered += 1;
            }
            answered
        })
    };

    let source = || std::fs::read_to_string(fixtures().join("versioned/versioned.cove")).unwrap();
    let dir = apps.root.join("versioned");
    let cases: [(&str, Breaker); 5] = [
        (
            "does not parse",
            Box::new(|| {
                std::fs::write(dir.join("versioned.cove"), "export fn handle( {\n").unwrap();
            }),
        ),
        (
            "requires `log`, which app.toml does not grant",
            Box::new(|| {
                let logged = source()
                    .replace("use timer\n", "use timer\nuse log\n")
                    .replace(
                        "  var spun = 0\n",
                        "  log.info(\"hello\")\n  var spun = 0\n",
                    );
                std::fs::write(dir.join("versioned.cove"), logged).unwrap();
            }),
        ),
        (
            "can spawn a task",
            Box::new(|| {
                let spawning =
                    std::fs::read_to_string(fixtures().join("spawner/spawner.cove")).unwrap();
                std::fs::write(dir.join("versioned.cove"), spawning).unwrap();
            }),
        ),
        (
            "unknown field `deadlin`",
            Box::new(|| {
                write_version(
                    &apps,
                    "bad",
                    Some("grant = [\"timer\"]\n[limits]\ndeadlin = \"5s\"\n"),
                );
            }),
        ),
        // A key Cove removed (ADR 0091) is refused by name when it is put
        // forward, not as an unknown field.
        (
            "`limits.fuel` was removed (Cove ADR 0091",
            Box::new(|| {
                write_version(
                    &apps,
                    "bad",
                    Some("grant = [\"timer\"]\n[limits]\nfuel = 1000000\n"),
                );
            }),
        ),
    ];
    for (n, (reason, break_it)) in cases.iter().enumerate() {
        break_it();
        let refused = update(&host, "versioned");
        assert_eq!(refused.status, 422, "{reason}: {}", refused.body);
        assert!(
            refused.body.contains(&format!("still serving {v1}")),
            "{}",
            refused.body
        );
        assert!(refused.body.contains(reason), "{reason}: {}", refused.body);
        let still = get(addr, "/versioned/");
        assert_eq!(still.body, "one 0\n");
        assert_eq!(still.header("x-cove-app-version"), Some(v1.as_str()));
        assert_eq!(count(&host, "versioned", "updates_refused"), n as u64 + 1);
        write_version(&apps, "one", None);
    }
    assert_eq!(count(&host, "versioned", "updates"), 0);
    assert_eq!(count(&host, "versioned", "versions_alive"), 1);

    // Fixed, it goes through.
    write_version(&apps, "two", None);
    assert_eq!(update(&host, "versioned").status, 200);
    assert_eq!(get(addr, "/versioned/").body, "two 0\n");

    stop.store(true, Ordering::Relaxed);
    assert!(neighbour.join().unwrap() > 0);
    assert_eq!(count(&host, "hello", "errors.internal"), 0);
}

#[test]
fn the_admin_listener_refuses_without_the_token() {
    let apps = apps(&[fixture("versioned")]);
    let host = started(&apps, 1);
    write_version(&apps, "two", None);
    for token in [None, Some("wrong"), Some("")] {
        let refused = admin(&host, "POST", "/apps/versioned/update", token);
        assert_eq!(refused.status, 401, "{token:?}");
        assert_eq!(refused.header("www-authenticate"), Some("Bearer"));
    }
    assert_eq!(admin(&host, "DELETE", "/apps/versioned", None).status, 401);
    assert_eq!(admin(&host, "GET", "/apps", Some("wrong")).status, 401);
    // Nothing changed, and the public listener has no way to change it.
    assert_eq!(get(host.addr, "/versioned/").body, "one 0\n");
    assert_eq!(
        post(host.addr, "/_host/apps/versioned/update", "").status,
        404
    );
    assert_eq!(get(host.addr, "/versioned/").body, "one 0\n");
    assert_eq!(count(&host, "versioned", "updates"), 0);
    // With it, the stats.
    assert_eq!(admin(&host, "GET", "/apps", Some(ADMIN_TOKEN)).status, 200);
}

#[test]
fn an_update_can_add_an_app_and_the_admin_can_remove_one() {
    let apps = apps(&[fixture("versioned")]);
    let host = started(&apps, 1);
    assert_eq!(get(host.addr, "/hello/").status, 404);
    assert_eq!(update(&host, "nobody").status, 404);

    // A directory that was not there at start, loaded as a new app.
    let dir = apps.root.join("hello");
    std::fs::create_dir_all(&dir).unwrap();
    for file in ["hello.cove", "app.toml"] {
        std::fs::copy(examples().join("hello").join(file), dir.join(file)).unwrap();
    }
    let added = update(&host, "hello");
    assert_eq!(added.status, 200, "{}", added.body);
    assert!(added.body.contains("\"previous\": null"), "{}", added.body);
    assert_eq!(
        get(host.addr, "/hello/?name=new").body,
        "Hello, new! (GET /)\n"
    );

    let removed = admin(&host, "DELETE", "/apps/hello", Some(ADMIN_TOKEN));
    assert_eq!(removed.status, 200);
    assert_eq!(get(host.addr, "/hello/").status, 404);
    assert_eq!(host.stats()["apps"]["hello"]["state"], "removed");
    // Loading it again brings it back, as its next version.
    let back = update(&host, "hello");
    assert!(back.body.contains("\"version\": \"v2-"), "{}", back.body);
    assert_eq!(get(host.addr, "/hello/").status, 200);
    assert_eq!(get(host.addr, "/versioned/").body, "one 0\n");
}

#[test]
fn the_operations_page_escapes_what_apps_say() {
    let apps = apps(&[fixture("versioned"), sample("notes")]);
    let host = started(&apps, 1);
    // A version whose log line is markup, and which fails a request.
    let dir = apps.root.join("versioned");
    std::fs::write(
        dir.join("versioned.cove"),
        "use std.web\nuse log\n\n/// Logs markup, then spins past its deadline.\nexport fn handle(request: web.Request) -> web.Response {\n  \
         log.info(\"<script>alert(1)</script>\")\n  var turns = 0\n  while turns >= 0 {\n    \
         turns += 1\n  }\n  web.Response(status: 200, headers: Map.of(), body: \"{turns}\")\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("app.toml"),
        "grant = [\"log\"]\n[limits]\ndeadline = \"300ms\"\n",
    )
    .unwrap();
    let updated = update(&host, "versioned");
    assert_eq!(updated.status, 200, "{}", updated.body);
    assert_eq!(get(host.addr, "/versioned/").status, 504);
    assert_eq!(super_put(host.addr), 201);

    let page = get(host.addr, "/_host/ui");
    assert_eq!(page.status, 200);
    assert_eq!(
        page.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert!(!page.body.contains("<script>"), "{}", page.body);
    assert!(page.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(
        page.body.contains("deadline of 300ms exceeded"),
        "{}",
        page.body
    );
    // The store's usage against its quota.
    assert!(page.body.contains("1 / 10000 keys"), "{}", page.body);

    let detail = host.app_detail("versioned").unwrap();
    assert_eq!(detail["recent_errors"][0]["kind"], "deadline");
    assert!(detail["recent_errors"][0]["version"]
        .as_str()
        .unwrap()
        .starts_with("v2-"));
    let notes = host.app_detail("notes").unwrap();
    assert_eq!(notes["kv"]["keys"], 1);
    assert_eq!(notes["kv"]["max_keys"], 10000);
}

fn super_put(addr: std::net::SocketAddr) -> u16 {
    send_raw(
        addr,
        b"PUT /notes/k HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 1\r\n\r\nv",
    )
    .status
}

#[test]
fn the_app_log_is_written_to_the_data_directory() {
    let apps = apps(&[sample("notes")]);
    let host = start(&apps, 1);
    assert_eq!(super_put(host.addr), 201);
    let file = apps.data.join("notes/log.txt");
    wait_until("the log line to reach its file", || {
        std::fs::read_to_string(&file).is_ok_and(|text| text.contains("info: stored k (1 bytes)"))
    });
}
