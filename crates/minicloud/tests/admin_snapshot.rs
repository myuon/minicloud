//! A snapshot of every page of the admin app (`apps/admin`), to hold a
//! refactor of how the pages are written to the same bytes.
//!
//! Not a test of what the pages say — `admin.rs`, `queue_window.rs` and the
//! others are — and ignored in the suite: it writes the body of every page
//! it can reach, query-string variants and refused forms included, to
//! `$ADMIN_SNAPSHOT_DIR/<name>.html`, with `$ADMIN_SNAPSHOT_DIR/MANIFEST`
//! saying, a line each and in the order they were taken, the name, the
//! method, the path and the status. Run it on two trees and compare the two
//! directories with `admin_snapshot_diff.py`:
//!
//! ```text
//! ADMIN_SNAPSHOT_DIR=/tmp/admin-snap/base1 \
//!   cargo test --profile checked -p minicloud --test admin_snapshot -- --ignored
//! python3 -I crates/minicloud/tests/admin_snapshot_diff.py /tmp/admin-snap/base1 /tmp/admin-snap/after
//! ```
//!
//! The data is fixed and so is the order of everything done to the host, so
//! two runs differ only where a page shows the clock or a measured time.
//! Nothing is normalised here: the script does that, as narrowly as it can,
//! so that what it hides is visible in one place.

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;

use common::*;
use minicloud::Host;

const UI_HOST: &str = "covtools-admin.ramda.io";
/// `Basic base64("owner:ui-secret")`.
const UI_LOGIN: &str = "Basic b3duZXI6dWktc2VjcmV0";

/// What escaping is tried with, wherever a page shows something an app or a
/// form put there.
const HOSTILE: &str = "<script>&\"'";
/// The same, percent-encoded for a query string or a form.
const HOSTILE_ENCODED: &str = "%3Cscript%3E%26%22%27";

/// `apps/admin`'s app.toml, its secret a literal.
fn admin_app_toml() -> String {
    std::fs::read_to_string(bundled().join("admin/app.toml"))
        .unwrap()
        .replace("{ env = \"ADMIN_UI_TOKEN\" }", "{ value = \"ui-secret\" }")
}

/// `post` with two queues: `mail`, which is used, and `spare`, which is
/// declared and never sent to.
const POST_TOML: &str =
    "grant = [\"queue\", \"kv\", \"log\", \"timer\"]\n\n[[queues]]\nname = \"mail\"\n\
max_attempts = 2\nretry = \"3600s\"\nlease = \"120s\"\nmax_depth = 500\nbatch = 4\n\n\
[[queues]]\nname = \"spare\"\n";

/// A pause before every request, so that no two requests of the run are
/// in the same millisecond. Every app's log is one order on `/logs`, and two
/// apps' lines from the same millisecond are put in the order the apps were
/// loaded rather than the order they were said in: without this, which of
/// the two comes first would be up to the clock. Not a timing assertion.
fn apart() {
    std::thread::sleep(std::time::Duration::from_millis(2));
}

/// A request to the admin app, with `extra` header lines.
fn ui(host: &Host, method: &str, path: &str, extra: &str, body: &str) -> Answer {
    apart();
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

/// A request to an app on the public side.
fn public(host: &Host, method: &str, path: &str, body: &str) -> Answer {
    apart();
    send_raw(
        host.addr,
        format!(
            "{method} {path} HTTP/1.1\r\nHost: covtools.ramda.io\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
}

/// The configure form for an app granted nothing, every limit as given but
/// its host calls.
fn limits_form(host_calls: &str) -> String {
    format!(
        "allow=&maxHostCalls={host_calls}&deadlineMs=2000&maxHeapWords=0&maxInFlight=64\
         &maxQueued=256&maxRequestBytes=1048576&maxResponseBytes=4194304"
    )
}

/// The directory the pages are written to, and what has been written.
struct Snapshot {
    dir: PathBuf,
    names: BTreeSet<String>,
    manifest: String,
}

impl Snapshot {
    fn new() -> Snapshot {
        let dir = std::env::var_os("ADMIN_SNAPSHOT_DIR").unwrap_or_else(|| {
            panic!(
                "set ADMIN_SNAPSHOT_DIR to the directory the pages are written to \
                 (it is made if it is not there; files of the same name are replaced)"
            )
        });
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        Snapshot {
            dir,
            names: BTreeSet::new(),
            manifest: String::new(),
        }
    }

    /// Asks the admin app and keeps the body as `<name>.html`.
    fn take(
        &mut self,
        host: &Host,
        name: &str,
        method: &str,
        path: &str,
        extra: &str,
        body: &str,
    ) -> Answer {
        assert!(self.names.insert(name.to_string()), "`{name}` twice");
        let answer = ui(host, method, path, extra, body);
        std::fs::write(self.dir.join(format!("{name}.html")), &answer.bytes).unwrap();
        self.manifest
            .push_str(&format!("{name}\t{method}\t{path}\t{}\n", answer.status));
        answer
    }

    /// A page, with the admin secret.
    fn page(&mut self, host: &Host, name: &str, path: &str) -> Answer {
        self.take(host, name, "GET", path, &authed(), "")
    }

    /// A form posted from the admin app's own pages.
    fn form(&mut self, host: &Host, name: &str, path: &str, body: &str) -> Answer {
        self.take(host, name, "POST", path, &same_site(), body)
    }

    fn finish(self) {
        std::fs::write(self.dir.join("MANIFEST"), &self.manifest).unwrap();
        eprintln!(
            "admin snapshot: {} pages in {}",
            self.names.len(),
            self.dir.display()
        );
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

/// A minute of an app's history, as the host's sampler would have written
/// it.
fn minute(
    app: &str,
    at_unix: i64,
    served: u64,
    errors: u64,
    p50_us: u64,
    p99_us: u64,
) -> minicloud::series::Sample {
    minicloud::series::Sample {
        app: app.to_string(),
        at_unix,
        served,
        ok: served - errors,
        errors,
        rejected: 0,
        worker_ms: served,
        p50_us,
        p99_us,
        // Half the requests collected once each, a third more than once;
        // each allocated 50 000 words.
        gc_collections: served / 2 + served / 3,
        gc_runs: served / 2,
        allocated_words: served * 50_000,
    }
}

/// A minute of the host's own history (the Platform page), its figures a
/// fixed function of `k`.
fn host_minute(at_unix: i64, k: u64) -> minicloud::series::HostRow {
    minicloud::series::HostRow {
        at_unix,
        memory_bytes: Some((600 + 40 * k) << 20),
        memory_max_bytes: Some(2 << 30),
        rss_bytes: Some((300 + 10 * k) << 20),
        cpu_usec: Some(3_000_000 * (k % 5)),
        cpu_throttled_usec: Some(if k % 4 == 3 { 1_500_000 } else { 0 }),
        cpu_throttled_periods: Some(k % 4 / 3),
        cpu_quota_millicores: Some(2000),
        disk_total_bytes: Some(100 << 30),
        disk_available_bytes: Some((60 << 30) - (k << 28)),
        store_bytes: Some(1 << 20),
        load1_milli: Some(200 + 50 * k),
        host_memory_total_bytes: Some(8 << 30),
        host_memory_available_bytes: Some((5 << 30) - (k << 26)),
        in_flight: k % 3,
        queued: k % 4 / 3,
        busy: k % 3,
        workers: 2,
        gc_collections: Some(3 + k % 4),
        gc_runs: Some(1 + k % 3),
        // (20 + 5k) MiB: a word is eight bytes.
        allocated_words: Some((20 + 5 * k) << 17),
    }
}

/// What the host's process and machine are, for every page that shows
/// them now: fixed, so that the machine the snapshot is taken on never
/// reaches a page.
fn fixed_platform() -> minicloud::platform::PlatformSample {
    minicloud::platform::PlatformSample {
        memory_bytes: Some(900 << 20),
        memory_max_bytes: Some(2 << 30),
        rss_bytes: Some(400 << 20),
        cpu_usage_usec: Some(123_456_789),
        cpu_throttled_usec: Some(1_000_000),
        cpu_throttled_periods: Some(3),
        cpu_quota_millicores: Some(2000),
        disk_total_bytes: Some(100 << 30),
        disk_available_bytes: Some(55 << 30),
        store_bytes: Some(1 << 20),
        load1_milli: Some(420),
        host_memory_total_bytes: Some(8 << 30),
        host_memory_available_bytes: Some(5 << 30),
        in_flight: 1,
        queued: 0,
        busy: 1,
        workers: 2,
    }
}

/// Writes the history the Metrics tab draws, straight into the host's own
/// series store, so that the charts are the same every run.
///
/// The host's sampler would write measured times; these are fixed. Each app
/// gets rows whose times are fixed *relative to each other*, so a chart's
/// points — which are placed by time from the first row — do not move; only
/// the "from … to …" stamps follow the clock.
///
/// - `hello`: eight minutes ending two minutes ago, two with errors — the
///   hour and day windows draw both charts; the month window has nothing,
///   because nothing has been rolled into hours for it.
/// - `notes`: three whole hours of minutes, ending two hours before this
///   hour began, rolled up into hours — the month window draws three hours
///   and the day window the minutes; the hour window has nothing.
/// - `catalog`: one minute, which is "one row so far".
/// - every other app: no row at all.
/// - the host itself (the Platform page): a row for every minute of
///   `hello`, which the hour and the day draw, and the minutes of `notes`
///   three days earlier, rolled up into the three hours the month draws.
fn write_history(apps: &Apps) {
    let series =
        minicloud::series::Series::open(&apps.data.join("_host").join("series.sqlite3")).unwrap();
    let now = now();
    // `notes`: hour-aligned, so the hours it rolls up to are the same hours
    // of its minutes every run.
    let this_hour = now / 3600 * 3600;
    let start = this_hour - 5 * 3600;
    let mut rows = Vec::new();
    for hour in 0..3 {
        for m in [0, 15, 30, 45] {
            let at = start + hour * 3600 + m * 60;
            let k = (hour * 4 + m / 15) as u64;
            rows.push(minute("notes", at, 10 + k, 0, 400 + 25 * k, 900 + 50 * k));
            // The host's, three days back: older than the day window, so
            // that the day draws the recent minutes alone and the points of
            // neither depend on when in the hour the run is.
            series.record_host(&host_minute(at - 67 * 3600, k)).unwrap();
        }
    }
    series.record(&rows).unwrap();
    // Every minute before the hour after the last of them is rolled up, and
    // nothing else has been written yet.
    series
        .sweep(
            start + 3 * 3600 - 67 * 3600,
            minicloud::series::KEEP_MINUTES_SECS,
            minicloud::series::KEEP_HOURS_SECS,
        )
        .unwrap();
    series
        .sweep(
            start + 3 * 3600,
            minicloud::series::KEEP_MINUTES_SECS,
            minicloud::series::KEEP_HOURS_SECS,
        )
        .unwrap();
    // `hello` and `catalog`: minute-aligned, within the last hour.
    let this_minute = now / 60 * 60;
    let mut rows = Vec::new();
    for (i, (served, errors, p50, p99)) in [
        (3, 0, 850, 2100),
        (5, 0, 900, 2500),
        (4, 1, 1200, 9800),
        (0, 0, 0, 0),
        (7, 0, 800, 1900),
        (6, 2, 1500, 15000),
        (2, 0, 700, 1600),
        (9, 0, 950, 3000),
    ]
    .into_iter()
    .enumerate()
    {
        // Minute 4 is missing: a row is placed by its time, not its place
        // in the list, and a gap is part of what is drawn.
        let gap = if i >= 4 { 1 } else { 0 };
        let at = this_minute - (10 - i as i64 - gap) * 60;
        rows.push(minute("hello", at, served, errors, p50, p99));
        series.record_host(&host_minute(at, i as u64)).unwrap();
    }
    rows.push(minute("catalog", this_minute - 5 * 60, 4, 0, 600, 1100));
    series.record(&rows).unwrap();
}

/// Rows of `app`'s history the host reads for `over`.
fn rows(host: &Host, app: &str, over: &str) -> usize {
    let answer = get(host.addr, &format!("/_host/series?app={app}&over={over}"));
    assert_eq!(answer.status, 200, "{}", answer.body);
    let json: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    json["rows"].as_array().map_or(0, Vec::len)
}

/// Rows of the host's own history the host reads for `over`.
fn platform_rows(host: &Host, over: &str) -> usize {
    let answer = get(host.addr, &format!("/_host/platform?over={over}"));
    assert_eq!(answer.status, 200, "{}", answer.body);
    let json: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    json["rows"].as_array().map_or(0, Vec::len)
}

/// Every window of every app's history as `write_history` left it — so a
/// run that the host's own sampler wrote into (a run of more than a minute,
/// or one across the top of an hour) fails rather than leaving a snapshot
/// that differs for no reason of the pages'.
fn check_history(host: &Host) {
    let expected = [
        ("hello", "hour", 8),
        ("hello", "day", 8),
        ("hello", "month", 0),
        ("notes", "hour", 0),
        ("notes", "day", 12),
        ("notes", "month", 3),
        ("catalog", "hour", 1),
        ("post", "day", 0),
        ("admin", "day", 0),
    ];
    for (app, over, n) in expected {
        assert_eq!(
            rows(host, app, over),
            n,
            "`{app}` over {over}: the host's own sampler wrote a row while the snapshot \
             was taken, so the Metrics pages are not the fixed history — run it again"
        );
    }
    for (over, n) in [("hour", 8), ("day", 8), ("month", 3)] {
        assert_eq!(
            platform_rows(host, over),
            n,
            "the platform over {over}: the host's own sampler wrote a row while the \
             snapshot was taken, so the Platform pages are not the fixed history — run it again"
        );
    }
}

/// Puts a message on `post`'s queue at fixed times (seconds, which the store
/// keeps as milliseconds), as its own `queue.send` would, and answers its id.
fn put(apps: &Apps, queue: &str, body: &str, sent: i64, due: i64) -> i64 {
    let store = minicloud::queue::shared(&apps.data.join("post").join("queue.sqlite3")).unwrap();
    let store = store.lock().unwrap();
    store
        .send(queue, body, sent * 1000, due * 1000, 64 << 10, 1 << 20)
        .unwrap()
}

/// Buries a message nothing has leased (`post` is disabled). The store
/// settles only what a delivery holds, so the row is moved here, in one
/// transaction, as `Queues::bury` would, with the attempts it has.
fn bury(apps: &Apps, id: i64, at: i64, why: &str) {
    let mut conn =
        rusqlite::Connection::open(apps.data.join("post").join("queue.sqlite3")).unwrap();
    let tx = conn.transaction().unwrap();
    let moved = tx
        .execute(
            "INSERT INTO dead (id, queue, body, attempts, due, sent, why, buried) \
             SELECT id, queue, body, attempts, due, sent, ?2, ?3 FROM message WHERE id = ?1",
            rusqlite::params![id, why, at * 1000],
        )
        .unwrap();
    assert_eq!(moved, 1, "message {id} is on the queue");
    tx.execute("DELETE FROM message WHERE id = ?1", [id])
        .unwrap();
    tx.commit().unwrap();
}

#[test]
#[ignore = "writes a snapshot of every admin page to $ADMIN_SNAPSHOT_DIR; see the module's doc"]
fn every_admin_page() {
    let mut snap = Snapshot::new();
    let admin: &'static str = Box::leak(admin_app_toml().into_boxed_str());
    let apps = apps(&[
        sample_with("admin", admin),
        sample("hello"),
        sample("notes"),
        fixture("catalog"),
        AppSpec {
            name: "post",
            from: fixtures().join("post"),
            config: Some(POST_TOML),
        },
        fixture("noisy"),
        // Refused at load: its secret `keyed-key` is not in the store.
        fixture("keyed"),
        // Refused at load: it spawns a task.
        fixture("spawner"),
    ]);
    let mut options = options(&apps, 2);
    options.forwarding.public_origin = Some("https://covtools.ramda.io".parse().unwrap());
    let host = Host::start(options).unwrap();
    host.set_platform_source(std::sync::Arc::new(fixed_platform));
    // The request timeline the Live, Timeline and request pages draw: fixed
    // times, and not the requests this run makes — the admin's own among
    // them. The trackers of those in flight are kept to the end.
    let (timeline, _in_flight) = fixed_timeline();
    host.set_timeline_source(timeline);

    // ------------------------------------------------------------ the data
    //
    // `post` first, disabled through the form so that nothing is delivered
    // from its queues while they are looked at.
    let disabled = snap.form(&host, "post-apps-post-disable", "/apps/post/disable", "");
    assert_eq!(disabled.status, 303, "{}", disabled.body);
    // Due (in the past), held (due in 2100), and two dead letters, one of
    // them hostile and one with no reason; and a long body, cut in the list.
    let due = put(
        &apps,
        "mail",
        "the first message",
        1_700_000_000,
        1_700_000_060,
    );
    put(
        &apps,
        "mail",
        &"p".repeat(300),
        1_700_000_100,
        1_700_000_100,
    );
    put(&apps, "mail", "held back", 1_700_000_200, 4_102_444_800);
    let dead = put(&apps, "mail", HOSTILE, 1_700_000_300, 1_700_000_300);
    bury(&apps, dead, 1_700_000_400, &format!("500 {HOSTILE}"));
    let quiet = put(&apps, "mail", "", 1_700_000_500, 1_700_000_500);
    bury(&apps, quiet, 1_700_000_600, "");

    // `notes`: more keys than a page, a hostile value, and a key with a
    // space and an `&` in it (the path is the key as it came, encoded).
    for n in 0..60 {
        let put = public(
            &host,
            "PUT",
            &format!("/notes/k{n:02}"),
            &format!("value {n}"),
        );
        assert_eq!(put.status, 201, "{}", put.body);
    }
    for (key, value) in [
        ("hostile", HOSTILE.to_string()),
        ("a%20b%26c", "odd".to_string()),
        ("long", "x".repeat(500)),
    ] {
        let put = public(&host, "PUT", &format!("/notes/{key}"), &value);
        assert_eq!(put.status, 201, "{}", put.body);
    }

    // `catalog`: a table, an index, rows — one of them hostile, one NULL.
    for path in [
        "/catalog/setup".to_string(),
        "/catalog/index".to_string(),
        "/catalog/add?title=Dune&pages=412".to_string(),
        "/catalog/add?title=Emma&pages=474".to_string(),
        format!("/catalog/add?title={HOSTILE_ENCODED}&pages=1"),
    ] {
        let answer = public(&host, "GET", &path, "");
        assert_eq!(answer.status, 200, "{path}: {}", answer.body);
    }

    // `noisy`: lines of its own, one hostile.
    for path in [
        "/noisy/say?text=hello%20there".to_string(),
        format!("/noisy/say?text={HOSTILE_ENCODED}"),
        "/noisy/say?text=chatter&n=3".to_string(),
    ] {
        assert_eq!(public(&host, "GET", &path, "").status, 200, "{path}");
    }

    // `hello`: served, and one request past its deadline.
    for _ in 0..3 {
        assert_eq!(public(&host, "GET", "/hello/", "").status, 200);
    }
    assert_eq!(public(&host, "GET", "/hello/spin", "").status, 504);

    write_history(&apps);

    // ------------------------------------------------- the pages, reading

    snap.page(&host, "index", "/");

    // Every page of every app, each tab, whether it has the thing or not.
    for app in [
        "admin", "hello", "notes", "catalog", "post", "noisy", "keyed", "spawner",
    ] {
        for (tab, path) in [
            ("overview", String::new()),
            ("settings", "/settings".to_string()),
            ("kv", "/kv".to_string()),
            ("logs", "/logs".to_string()),
            ("code", "/code".to_string()),
            ("sql", "/sql".to_string()),
            ("queues", "/queues".to_string()),
            ("metrics", "/metrics".to_string()),
        ] {
            snap.page(
                &host,
                &format!("apps-{app}-{tab}"),
                &format!("/apps/{app}{path}"),
            );
        }
    }

    // Metrics: each window, and one nobody knows (an hour).
    for app in ["hello", "notes", "catalog", "post"] {
        for over in ["hour", "day", "month", "decade"] {
            snap.page(
                &host,
                &format!("apps-{app}-metrics-over-{over}"),
                &format!("/apps/{app}/metrics?over={over}"),
            );
        }
    }
    // The platform: each window, and one nobody knows (an hour).
    for over in ["hour", "day", "month", "decade"] {
        snap.page(
            &host,
            &format!("platform-over-{over}"),
            &format!("/platform?over={over}"),
        );
    }
    snap.page(&host, "platform", "/platform");
    // The request timeline: what is in flight, each window, one app's and
    // an app with nothing, and a window nobody knows (a minute).
    snap.page(&host, "live", "/live");
    // Load: nothing run yet, then figures of the test's own — a run going on
    // with a target missing, and one that ended — never a run of this
    // host's, whose figures are measured.
    snap.page(&host, "load", "/load");
    host.set_load_status(minicloud::load::Status {
        current: Some(fixed_load_run("running")),
        last: Some(fixed_load_run("finished")),
    });
    snap.page(&host, "load-running", "/load");
    host.set_load_status(minicloud::load::Status {
        current: None,
        last: Some(fixed_load_run("stopped")),
    });
    snap.page(&host, "load-stopped", "/load");
    for (name, query) in [
        ("timeline", ""),
        ("timeline-over-10s", "?over=10s"),
        ("timeline-over-1m", "?over=1m"),
        ("timeline-over-5m", "?over=5m"),
        ("timeline-over-decade", "?over=decade"),
        ("timeline-app-slow", "?app=slow&over=10s"),
        ("timeline-app-notes-5m", "?app=notes&over=5m"),
        ("timeline-app-nobody", "?app=nobody"),
        ("timeline-app-hostile", &format!("?app={HOSTILE_ENCODED}")),
        ("timeline-over-15m", "?over=15m"),
        ("timeline-over-1h", "?over=1h"),
        // Windows of the past: two seconds, one app's by `span`, before
        // the host started, and one past now.
        (
            "timeline-past-2s",
            "?from=1790000392000&until=1790000394000",
        ),
        (
            "timeline-past-app-slow",
            "?app=slow&from=1790000392000&span=2",
        ),
        (
            "timeline-past-before-start",
            "?from=1789999000000&until=1789999060000",
        ),
        (
            "timeline-past-into-now",
            "?from=1790000395000&until=1790000999000",
        ),
        ("timeline-past-garbled", "?from=soon&until=later"),
    ] {
        snap.page(&host, name, &format!("/timeline{query}"));
    }
    // One request of each kind: parked and moved, parked on a fetch, yielded
    // and moved, past its deadline, cancelled, rejected at its queue and
    // before it, in flight, and ids the timeline does not have.
    for (name, id) in [
        ("parked-moved", "102"),
        ("fetch", "103"),
        ("yielded", "104"),
        ("deadline", "105"),
        ("cancelled", "106"),
        ("queue-full", "107"),
        ("rate-limited", "108"),
        ("in-flight-parked", "109"),
        ("in-flight-waiting", "111"),
        ("unknown", "424242"),
        ("not-a-number", HOSTILE_ENCODED),
    ] {
        snap.page(
            &host,
            &format!("requests-{name}"),
            &format!("/requests/{id}"),
        );
    }
    // A load's worth of runs, at windows where they merge and where they
    // do not; then the fixed timeline again.
    host.set_timeline_source(merged_timeline());
    for (name, query) in [
        ("timeline-merged-5m", "?over=5m"),
        ("timeline-merged-app-crunch", "?app=crunch&over=5m"),
        (
            "timeline-merged-past-10s",
            "?from=1790000360000&until=1790000370000",
        ),
        (
            "timeline-merged-past-1s",
            "?from=1790000360000&until=1790000361000",
        ),
    ] {
        snap.page(&host, name, &format!("/timeline{query}"));
    }
    let (timeline, _again) = fixed_timeline();
    host.set_timeline_source(timeline);
    check_history(&host);

    // The store: a page, the next, a prefix, a prefix of nothing, keys.
    for (name, query) in [
        ("after-k49", "after=note%3Ak49"),
        ("prefix-k1", "prefix=note%3Ak1"),
        ("prefix-k-after-k20", "prefix=note%3Ak&after=note%3Ak20"),
        ("prefix-none", "prefix=nothing"),
        ("prefix-hostile", &format!("prefix={HOSTILE_ENCODED}")),
        ("key-k07", "key=note%3Ak07"),
        ("key-hostile", "key=note%3Ahostile"),
        ("key-odd", "key=note%3Aa%2520b%2526c"),
        ("key-long", "key=note%3Along"),
        ("key-missing", "key=nothing"),
        ("key-hostile-missing", &format!("key={HOSTILE_ENCODED}")),
    ] {
        snap.page(
            &host,
            &format!("apps-notes-kv-{name}"),
            &format!("/apps/notes/kv?{query}"),
        );
    }
    snap.page(&host, "apps-hello-kv-key", "/apps/hello/kv?key=x");

    // SQL: a read, an aggregate, a NULL, one that fails, one that writes.
    for (name, query) in [
        (
            "q-select",
            "q=SELECT+id%2C+title%2C+pages+FROM+books+ORDER+BY+id",
        ),
        ("q-null", "q=SELECT+NULL+AS+nothing%2C+%27%27+AS+empty"),
        ("q-count", "q=SELECT+count(*)+FROM+books"),
        (
            "q-empty-answer",
            "q=SELECT+title+FROM+books+WHERE+pages+%3E+9999",
        ),
        ("q-fails", "q=SELECT+nope+FROM+nothing"),
        ("q-writes", "q=DELETE+FROM+books"),
        ("q-hostile", &format!("q=SELECT+%27{HOSTILE_ENCODED}%27")),
        ("q-blank", "q=+++"),
    ] {
        snap.page(
            &host,
            &format!("apps-catalog-sql-{name}"),
            &format!("/apps/catalog/sql?{query}"),
        );
    }

    // Code: the tree, and each kind of file.
    for (name, app, file) in [
        ("admin-file-cove", "admin", "admin.cove"),
        ("admin-file-module", "admin", "text/text.cove"),
        ("admin-file-toml", "admin", "app.toml"),
        ("admin-file-css", "admin", "assets/style.css"),
        ("admin-file-svg", "admin", "assets/favicon.svg"),
        ("admin-file-ico", "admin", "assets/favicon.ico"),
        ("admin-file-missing", "admin", "nowhere.cove"),
        ("notes-file-cove", "notes", "notes.cove"),
        ("catalog-file-cove", "catalog", "catalog.cove"),
        ("spawner-file-cove", "spawner", "spawner.cove"),
        ("notes-file-hostile", "notes", HOSTILE_ENCODED),
    ] {
        snap.page(
            &host,
            &format!("apps-{name}"),
            &format!("/apps/{app}/code?file={file}"),
        );
    }

    // Queues: one message of each kind, one that is not there, and a
    // `?message=` that is not a number (the list).
    snap.page(
        &host,
        "apps-post-queues-message-due",
        &format!("/apps/post/queues?message={due}"),
    );
    snap.page(
        &host,
        "apps-post-queues-message-long",
        &format!("/apps/post/queues?message={}", due + 1),
    );
    snap.page(
        &host,
        "apps-post-queues-message-held",
        &format!("/apps/post/queues?message={}", due + 2),
    );
    snap.page(
        &host,
        "apps-post-queues-message-dead",
        &format!("/apps/post/queues?message={dead}"),
    );
    snap.page(
        &host,
        "apps-post-queues-message-dead-quiet",
        &format!("/apps/post/queues?message={quiet}"),
    );
    snap.page(
        &host,
        "apps-post-queues-message-missing",
        "/apps/post/queues?message=9999",
    );
    snap.page(
        &host,
        "apps-post-queues-message-nan",
        "/apps/post/queues?message=abc",
    );
    snap.page(
        &host,
        "apps-notes-queues-message",
        "/apps/notes/queues?message=1",
    );

    // Logs: `?n=`, one app's and every app's, in and out of range.
    for (name, query) in [
        ("n-1", "n=1"),
        ("n-2", "n=2"),
        ("n-50", "n=50"),
        ("n-1000", "n=1000"),
        ("n-0", "n=0"),
        ("n-abc", "n=abc"),
    ] {
        snap.page(&host, &format!("logs-{name}"), &format!("/logs?{query}"));
        snap.page(
            &host,
            &format!("apps-noisy-logs-{name}"),
            &format!("/apps/noisy/logs?{query}"),
        );
    }

    // ---------------------------------------------- the pages, changing

    // Settings after each kind of change, as a redirect lands on it.
    for done in ["enable", "disable", "configure", "reset", "bogus"] {
        snap.page(
            &host,
            &format!("apps-hello-settings-done-{done}"),
            &format!("/apps/hello/settings?done={done}"),
        );
    }
    // A wrong form: the problems by field, the form as posted, escaped.
    snap.form(
        &host,
        "post-apps-hello-configure-problems",
        "/apps/hello/configure",
        &format!(
            "cap.nonsense=on&fuel=3000000&{}",
            limits_form("%22%3E%3Cimg+src%3Dx%3E")
                .replace("allow=", "allow=ftp%3A%2F%2Fx%0Ahttp%3A%2F%2Fok.example")
        ),
    );
    // A form the host refuses: the change with its reason.
    snap.form(
        &host,
        "post-apps-hello-configure-refused",
        "/apps/hello/configure",
        &limits_form("100").replace("maxHeapWords=0", "maxHeapWords=9999999999"),
    );
    // The admin app may not disable itself, nor drop `admin`.
    snap.form(
        &host,
        "post-apps-admin-disable-refused",
        "/apps/admin/disable",
        "",
    );
    snap.form(
        &host,
        "post-apps-admin-configure-refused",
        "/apps/admin/configure",
        &limits_form("1000").replace("deadlineMs=2000", "deadlineMs=60000"),
    );
    // Changes that go through: a limit of hello's, the grant of notes taken
    // away (which refuses it) and given back, hello off and on.
    snap.form(
        &host,
        "post-apps-hello-configure",
        "/apps/hello/configure",
        &limits_form("3000"),
    );
    snap.form(
        &host,
        "post-apps-notes-configure",
        "/apps/notes/configure",
        &format!(
            "cap.log=on&{}",
            limits_form("1000").replace("deadlineMs=2000", "deadlineMs=10000")
        ),
    );
    snap.page(&host, "index-notes-refused", "/");
    snap.page(&host, "apps-notes-overview-refused", "/apps/notes");
    snap.page(&host, "apps-notes-settings-refused", "/apps/notes/settings");
    snap.form(&host, "post-apps-notes-reset", "/apps/notes/reset", "");
    snap.form(&host, "post-apps-hello-disable", "/apps/hello/disable", "");
    snap.page(&host, "apps-hello-overview-disabled", "/apps/hello");
    snap.form(&host, "post-apps-hello-enable", "/apps/hello/enable", "");
    snap.page(&host, "apps-hello-settings-changed", "/apps/hello/settings");

    // Secrets: the store with `keyed-key` used and unset; set one nobody
    // uses; the forms' refusals; set the one keyed needs (which brings it
    // back); delete one in use without `force`, and the unused one.
    snap.page(&host, "secrets", "/secrets");
    snap.form(
        &host,
        "post-secrets-set-empty",
        "/secrets/set",
        "name=&value=",
    );
    snap.form(
        &host,
        "post-secrets-set-hostile-name",
        "/secrets/set",
        &format!("name={HOSTILE_ENCODED}&value=v"),
    );
    snap.form(
        &host,
        "post-secrets-set-unused",
        "/secrets/set",
        "name=spare-token&value=s3cret",
    );
    snap.form(
        &host,
        "post-secrets-set-keyed",
        "/secrets/set",
        "name=keyed-key&value=k3y",
    );
    snap.form(
        &host,
        "post-secrets-delete-unconfirmed",
        "/secrets/delete",
        "name=spare-token",
    );
    snap.form(
        &host,
        "post-secrets-delete-in-use",
        "/secrets/delete",
        "name=keyed-key&confirm=on",
    );
    snap.form(
        &host,
        "post-secrets-delete-unused",
        "/secrets/delete",
        "name=spare-token&confirm=on",
    );
    snap.page(&host, "secrets-after", "/secrets");
    snap.page(&host, "apps-keyed-overview-serving", "/apps/keyed");

    // The history of all of the above, and the list as it ends.
    snap.page(&host, "history", "/history");
    snap.page(&host, "index-after", "/");
    snap.page(&host, "logs-after", "/logs");

    // ------------------------------------------ what is not a page at all

    snap.take(&host, "unauthorized", "GET", "/", "", "");
    snap.take(
        &host,
        "unauthorized-wrong",
        "GET",
        "/apps/hello",
        "Authorization: Bearer wrong\r\n",
        "",
    );
    snap.take(
        &host,
        "cross-site",
        "POST",
        "/apps/hello/disable",
        &format!("{}Origin: https://evil.example\r\n", authed()),
        "",
    );
    snap.take(&host, "method-put", "PUT", "/", &authed(), "");
    for (name, path) in [
        ("not-found-root", "/nowhere"),
        ("not-found-apps", "/apps"),
        ("not-found-app", "/apps/nobody"),
        ("not-found-app-hostile", &format!("/apps/{HOSTILE_ENCODED}")),
        ("not-found-tab", "/apps/hello/nowhere"),
        ("apps-hello-kv-deeper", "/apps/hello/kv/more"),
        ("not-found-secrets", "/secrets/set"),
        ("not-found-logs-deeper", "/logs/hello"),
        ("not-found-history-deeper", "/history/1"),
        ("not-found-requests", "/requests"),
        ("not-found-requests-deeper", "/requests/102/more"),
        ("not-found-live-deeper", "/live/1"),
    ] {
        snap.page(&host, name, path);
    }
    for (name, path) in [
        ("post-not-found-kv", "/apps/notes/kv"),
        ("post-not-found-logs", "/logs"),
        ("post-not-found-action", "/apps/hello/explode"),
        ("post-not-found-secrets", "/secrets/rotate"),
        ("post-not-found-secrets-deep", "/secrets/set/more"),
    ] {
        snap.form(&host, name, path, "");
    }

    snap.finish();
}
