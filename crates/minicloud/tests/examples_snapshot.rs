//! A snapshot of every page of four example apps — `ledger`, `algo`,
//! `webhooks` and `watch` — to hold a refactor of how their pages are
//! written to the same bytes. `admin_snapshot.rs` is the same for the admin
//! app; this follows it.
//!
//! Not a test of what the pages say — `ledger.rs`, `algo.rs`, `webhooks.rs`
//! and the others are — and ignored in the suite: one test per app writes
//! the body of every answer of that app that carries markup, query-string
//! variants, forms and error pages included, to
//! `$EXAMPLES_SNAPSHOT_DIR/<app>/<name>.html`, with
//! `$EXAMPLES_SNAPSHOT_DIR/<app>/MANIFEST` saying, a line each and in the
//! order they were taken, the name, the method, the path and the status. Run
//! it on two trees and compare the two directories with
//! `examples_snapshot_diff.py`:
//!
//! ```text
//! EXAMPLES_SNAPSHOT_DIR=/tmp/ex-snap/base1 \
//!   cargo test --profile checked -p minicloud --test examples_snapshot -- --ignored
//! python3 -I crates/minicloud/tests/examples_snapshot_diff.py /tmp/ex-snap/base1 /tmp/ex-snap/after
//! ```
//!
//! One app's alone is `-- --ignored ledger::` (or `algo::`, `webhooks::`,
//! `watch::`), and the script takes one app's directory as well as the
//! whole.
//!
//! The data is fixed and so is the order of everything done to each host,
//! so two runs differ only where a page shows the clock. Nothing is
//! normalised here: the script does that, as narrowly as it can, so that
//! what it hides is visible in one place.

mod common;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;

use common::*;

/// What escaping is tried with, wherever a page shows something a request
/// or the data put there.
const HOSTILE: &str = "<script>&\"'";
/// The same, percent-encoded for a query string or a form.
const HOSTILE_ENCODED: &str = "%3Cscript%3E%26%22%27";

/// `text` as a form or query value.
fn form_value(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A pause before every request, so that no two requests of a run are in
/// the same millisecond and whatever an app orders by time is in the order
/// the requests were made. Not a timing assertion.
fn apart() {
    std::thread::sleep(std::time::Duration::from_millis(2));
}

/// A request to `addr` as `Host: host`, with `headers` (each `Name: value`)
/// and `body`.
fn request(
    addr: SocketAddr,
    host: &str,
    method: &str,
    path: &str,
    headers: &[&str],
    body: &str,
) -> Answer {
    apart();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for header in headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    send_raw(addr, head.as_bytes())
}

/// The directory one app's pages are written to, and what has been written.
struct Snapshot {
    app: &'static str,
    /// The `Host:` every request is made with.
    host: &'static str,
    dir: PathBuf,
    names: BTreeSet<String>,
    manifest: String,
}

impl Snapshot {
    fn new(app: &'static str, host: &'static str) -> Snapshot {
        let dir = std::env::var_os("EXAMPLES_SNAPSHOT_DIR").unwrap_or_else(|| {
            panic!(
                "set EXAMPLES_SNAPSHOT_DIR to the directory the pages are written to \
                 (each app's go to a directory of its name in it, made if it is not \
                 there; files of the same name are replaced)"
            )
        });
        let dir = PathBuf::from(dir).join(app);
        std::fs::create_dir_all(&dir).unwrap();
        Snapshot {
            app,
            host,
            dir,
            names: BTreeSet::new(),
            manifest: String::new(),
        }
    }

    /// Makes the request and keeps the body as `<name>.html`.
    fn take(
        &mut self,
        addr: SocketAddr,
        name: &str,
        method: &str,
        path: &str,
        headers: &[&str],
        body: &str,
    ) -> Answer {
        assert!(self.names.insert(name.to_string()), "`{name}` twice");
        let answer = request(addr, self.host, method, path, headers, body);
        std::fs::write(self.dir.join(format!("{name}.html")), &answer.bytes).unwrap();
        self.manifest
            .push_str(&format!("{name}\t{method}\t{path}\t{}\n", answer.status));
        answer
    }

    /// `GET path`, with no header of its own.
    fn page(&mut self, addr: SocketAddr, name: &str, path: &str) -> Answer {
        self.take(addr, name, "GET", path, &[], "")
    }

    fn finish(self) {
        std::fs::write(self.dir.join("MANIFEST"), &self.manifest).unwrap();
        eprintln!(
            "{} snapshot: {} pages in {}",
            self.app,
            self.names.len(),
            self.dir.display()
        );
    }
}

/// The bench ledger (`examples/ledger`): every page it builds, empty and
/// filled in. The runs are posted with the `post` secret; what the pages
/// read back from them is all there is to the pages, so the runs are fixed
/// and each is there for something a page shows differently:
///
/// - `base-a` and `base-b`: the same machine and conditions, `b` on another
///   toolchain (a warning) and faster on one backend, within the spread on
///   another; each has a case, a metric and a measurement the other lacks.
///   `base-a` has a note and a source, and what `base-a` shows of a poster's
///   text is hostile.
/// - `other-machine`: the same case on another machine, so that it is not
///   comparable with `base-a` (its commit is `base-b`'s, for a side of two
///   runs).
/// - `hostile-run`: every text a run can hold, hostile.
/// - `cove-593-capacity`: one of the real runs in `samples/`.
/// - `fill-01` .. `fill-48`: older and posted last, to fill the run list
///   past its page of 50.
///
/// Answers that are JSON (the API, `format=json`) or plain text (the
/// run, trend and route that are not there) carry no markup and are not
/// taken.
mod ledger {
    use super::*;
    use serde_json::{json, Value as Json};

    const SECRET: &str = "snapshot-ledger-secret";

    /// The sample's `app.toml` with the post secret given literally.
    fn config() -> String {
        let shipped = std::fs::read_to_string(examples().join("ledger/app.toml")).unwrap();
        let config = shipped.replace(
            "post = { env = \"LEDGER_TOKEN\" }",
            &format!("post = {{ value = \"{SECRET}\" }}"),
        );
        assert!(
            config.contains(SECRET),
            "the shipped app.toml changed shape"
        );
        config
    }

    fn post(addr: SocketAddr, run: &Json) {
        let bearer = format!("Authorization: Bearer {SECRET}");
        let answer = request(
            addr,
            "ledger.test",
            "POST",
            "/ledger/api/runs",
            &[&bearer, "Content-Type: application/json"],
            &run.to_string(),
        );
        assert_eq!(answer.status, 201, "{}: {answer:?}", run["id"]);
    }

    /// One measurement; `conditions` is its own.
    fn result(
        case: &str,
        input: Option<&str>,
        backend: &str,
        load: &[f64],
        conditions: Json,
        metrics: Json,
    ) -> Json {
        let mut one = json!({
            "case": case,
            "backend": backend,
            "load": load,
            "conditions": conditions,
            "metrics": metrics,
        });
        if let Some(input) = input {
            one["input"] = json!(input);
            one["inputSize"] = json!(64);
        }
        one
    }

    fn metric(unit: &str, values: &[f64]) -> Json {
        json!({"unit": unit, "values": values})
    }

    fn base_a() -> Json {
        json!({
            "id": "base-a",
            "repository": "myuon/cove",
            "commit": "aaa1111",
            "measuredAt": "2026-10-05T03:13:54Z",
            "source": format!("results/{HOSTILE}.jsonl"),
            "note": format!("a note: {HOSTILE}"),
            "environment": {"cpu": format!("test {HOSTILE} cpu"), "os": "test os"},
            "toolchain": {"rustc": "1.98.1"},
            "conditions": {"workers": "4"},
            "results": [
                // Every kind of metric: a rate, a time sent in µs, memory sent
                // in KiB, and one that is not one of the usual names.
                result("hello", Some("c=64"), "vm", &[1.5, 2.0], json!({}), json!({
                    "throughput": metric("req/s", &[1000.0, 1100.0, 1050.0]),
                    "p99": metric("µs", &[850.0, 900.5, 870.0]),
                    "memory": metric("KiB", &[2048.0, 2048.0]),
                    "zz.queue-depth": metric("count", &[3.0]),
                })),
                // No load average taken, and a condition of its own.
                result("hello", Some("c=64"), "go", &[], json!({"latency": "from the send"}), json!({
                    "throughput": metric("req/s", &[900.0]),
                    "p99": metric("ms", &[1.2]),
                })),
                // No input at all, and only here.
                result("tail", None, "vm", &[0.0], json!({}), json!({
                    "throughput": metric("req/s", &[10.0]),
                })),
                // The same load twice, and everything a result holds, hostile.
                result(HOSTILE, Some(HOSTILE), HOSTILE, &[0.25, 0.25], json!({}), json!({
                    "throughput": metric("req/s", &[5.0, 6.0]),
                })),
            ],
        })
    }

    fn base_b() -> Json {
        json!({
            "id": "base-b",
            "repository": "myuon/cove",
            "commit": "bbb2222",
            "measuredAt": "2026-10-06T10:00:00+09:00",
            "environment": {"cpu": format!("test {HOSTILE} cpu"), "os": "test os"},
            "toolchain": {"rustc": "1.99.0"},
            "conditions": {"workers": "4"},
            "results": [
                // Faster, with the ranges apart, and a metric `base-a` lacks.
                result("hello", Some("c=64"), "vm", &[1.5], json!({}), json!({
                    "throughput": metric("req/s", &[1200.0, 1250.0]),
                    "p99": metric("ms", &[0.5, 0.6]),
                    "p50": metric("ms", &[0.2]),
                    "memory": metric("KiB", &[2048.0]),
                    "zz.queue-depth": metric("count", &[4.0]),
                })),
                // Slower, within the ranges, and a load far from `base-a`'s.
                result("hello", Some("c=64"), "go", &[9.0], json!({"latency": "from the send"}), json!({
                    "throughput": metric("req/s", &[880.0, 950.0]),
                    "p99": metric("ms", &[1.3, 1.4]),
                })),
                // Only here.
                result("newcase", Some("c=8"), "vm", &[], json!({}), json!({
                    "throughput": metric("req/s", &[77.0]),
                })),
                result(HOSTILE, Some(HOSTILE), HOSTILE, &[0.25], json!({}), json!({
                    "throughput": metric("req/s", &[5.5]),
                })),
            ],
        })
    }

    fn other_machine() -> Json {
        json!({
            "id": "other-machine",
            "repository": "myuon/cove",
            "commit": "bbb2222",
            "measuredAt": "2026-10-07T00:00:00Z",
            "environment": {"cpu": format!("other {HOSTILE} cpu"), "os": "test os"},
            "toolchain": {"rustc": "1.98.1"},
            "conditions": {"workers": "8"},
            "results": [
                result("hello", Some("c=64"), "vm", &[1.0], json!({}), json!({
                    "throughput": metric("req/s", &[500.0]),
                    "p99": metric("ms", &[4.0]),
                })),
            ],
        })
    }

    /// Every text a run holds that a poster chooses, hostile; its ID is the
    /// one thing that cannot be.
    fn hostile_run() -> Json {
        json!({
            "id": "hostile-run",
            "repository": format!("{HOSTILE}/repo"),
            "commit": format!("c{HOSTILE}"),
            "measuredAt": "2026-10-04T12:00:00Z",
            "source": HOSTILE,
            "note": HOSTILE,
            "environment": {HOSTILE: HOSTILE},
            "toolchain": {HOSTILE: HOSTILE},
            "conditions": {HOSTILE: HOSTILE},
            "results": [
                result(HOSTILE, Some(HOSTILE), HOSTILE, &[1.0, 3.0], json!({HOSTILE: HOSTILE}), json!({
                    "throughput": metric("req/s", &[1.0]),
                })),
            ],
        })
    }

    fn filler(index: usize) -> Json {
        json!({
            "id": format!("fill-{index:02}"),
            "repository": "myuon/cove",
            "commit": format!("f{index:06}"),
            "measuredAt": format!("2026-09-01T00:{index:02}:00Z"),
            "environment": {"cpu": "fill cpu"},
            "results": [
                result("fill", None, "vm", &[], json!({}), json!({
                    "throughput": metric("req/s", &[index as f64]),
                })),
            ],
        })
    }

    /// `/ledger/compare?a=…&b=…`, the sides and the rest already encoded.
    fn compare(a: &str, b: &str, more: &str) -> String {
        format!("/ledger/compare?a={a}&b={b}{more}")
    }

    /// Writes a snapshot of every ledger page to `$EXAMPLES_SNAPSHOT_DIR/ledger`.
    ///
    /// First the pages before any run is there; then every page of the runs
    /// above, each query-string variant of each: `before`, `metric`, `all`,
    /// a side that is a run, a commit, one that is not there, and one that
    /// is missing; last, with 48 more runs, the list past its first page.
    #[test]
    #[ignore = "writes a snapshot of every ledger page to $EXAMPLES_SNAPSHOT_DIR/ledger; see the module's doc"]
    fn every_page() {
        let config = config();
        let apps = apps(&[AppSpec {
            name: "ledger",
            from: examples().join("ledger"),
            config: Some(Box::leak(config.into_boxed_str())),
        }]);
        let host = start(&apps, 1);
        let addr = host.addr;
        let mut snap = Snapshot::new("ledger", "ledger.test");
        let ok = |snap: &mut Snapshot, name: &str, path: &str| {
            let answer = snap.page(addr, name, path);
            assert_eq!(answer.status, 200, "{name}: {answer:?}");
        };
        let refused = |snap: &mut Snapshot, name: &str, path: &str| {
            let answer = snap.page(addr, name, path);
            assert_eq!(answer.status, 404, "{name}: {answer:?}");
            assert!(answer.body.contains("<html"), "{name}: {answer:?}");
        };

        // Nothing posted yet.
        ok(&mut snap, "empty-runs", "/ledger/");
        ok(&mut snap, "empty-cases", "/ledger/cases");
        ok(&mut snap, "empty-compare", "/ledger/compare");
        ok(
            &mut snap,
            "empty-compare-a-only",
            "/ledger/compare?a=base-a",
        );
        refused(
            &mut snap,
            "empty-compare-no-run",
            &compare("base-a", "base-b", ""),
        );
        refused(
            &mut snap,
            "empty-compare-no-commit",
            &compare("commit:aaa1111", "commit:bbb2222", ""),
        );

        post(addr, &base_a());
        post(addr, &base_b());
        post(addr, &other_machine());
        post(addr, &hostile_run());
        let sample =
            std::fs::read_to_string(examples().join("ledger/samples/cove-593-capacity.json"))
                .unwrap();
        post(addr, &serde_json::from_str(&sample).unwrap());

        // The list.
        ok(&mut snap, "runs", "/ledger/");
        ok(
            &mut snap,
            "runs-before-hostile",
            &format!("/ledger/?before={HOSTILE_ENCODED}"),
        );
        ok(&mut snap, "runs-before-empty", "/ledger/?before=");
        ok(&mut snap, "runs-before-first", "/ledger/?before=index:");

        // Each run.
        for id in [
            "base-a",
            "base-b",
            "other-machine",
            "hostile-run",
            "cove-593-capacity",
        ] {
            ok(
                &mut snap,
                &format!("run-{id}"),
                &format!("/ledger/runs/{id}"),
            );
        }

        // The cases, and each one's trend.
        ok(&mut snap, "cases", "/ledger/cases");
        let hello = "/ledger/cases/trend?case=hello&input=c%3D64";
        ok(&mut snap, "trend-hello", hello);
        for metric in ["throughput", "p99", "zz.queue-depth", "nothing"] {
            ok(
                &mut snap,
                &format!("trend-hello-metric-{metric}"),
                &format!("{hello}&metric={metric}"),
            );
        }
        ok(
            &mut snap,
            "trend-hello-metric-hostile",
            &format!("{hello}&metric={HOSTILE_ENCODED}"),
        );
        ok(
            &mut snap,
            "trend-tail-no-input",
            "/ledger/cases/trend?case=tail&input=",
        );
        ok(
            &mut snap,
            "trend-tail-input-absent",
            "/ledger/cases/trend?case=tail",
        );
        ok(
            &mut snap,
            "trend-hostile",
            &format!("/ledger/cases/trend?case={HOSTILE_ENCODED}&input={HOSTILE_ENCODED}"),
        );
        ok(
            &mut snap,
            "trend-sample",
            "/ledger/cases/trend?case=crunch&input=c%3D16",
        );

        // The comparison.
        ok(&mut snap, "compare-form", "/ledger/compare");
        ok(
            &mut snap,
            "compare-form-metric-all",
            "/ledger/compare?metric=p99&all=1",
        );
        ok(&mut snap, "compare-a-only", "/ledger/compare?a=base-a");
        ok(
            &mut snap,
            "compare-a-only-other",
            "/ledger/compare?a=other-machine",
        );
        ok(
            &mut snap,
            "compare-a-only-alone",
            "/ledger/compare?a=cove-593-capacity",
        );
        ok(
            &mut snap,
            "compare-a-only-commit",
            "/ledger/compare?a=commit:aaa1111",
        );
        ok(&mut snap, "compare-b-only", "/ledger/compare?b=base-b");
        ok(
            &mut snap,
            "compare-blank-sides",
            "/ledger/compare?a=%20&b=%20",
        );
        ok(&mut snap, "compare-ab", &compare("base-a", "base-b", ""));
        ok(
            &mut snap,
            "compare-ab-metric-p99",
            &compare("base-a", "base-b", "&metric=p99"),
        );
        ok(
            &mut snap,
            "compare-ab-metric-nothing",
            &compare("base-a", "base-b", "&metric=nothing"),
        );
        ok(
            &mut snap,
            "compare-ab-metric-hostile",
            &compare("base-a", "base-b", &format!("&metric={HOSTILE_ENCODED}")),
        );
        ok(
            &mut snap,
            "compare-ab-all",
            &compare("base-a", "base-b", "&all=1"),
        );
        ok(&mut snap, "compare-ba", &compare("base-b", "base-a", ""));
        ok(
            &mut snap,
            "compare-a-other",
            &compare("base-a", "other-machine", ""),
        );
        ok(
            &mut snap,
            "compare-a-other-metric-hostile",
            &compare(
                "base-a",
                "other-machine",
                &format!("&metric={HOSTILE_ENCODED}"),
            ),
        );
        ok(
            &mut snap,
            "compare-a-other-all",
            &compare("base-a", "other-machine", "&all=1"),
        );
        ok(
            &mut snap,
            "compare-a-other-all-metric-p99",
            &compare("base-a", "other-machine", "&metric=p99&all=1"),
        );
        ok(
            &mut snap,
            "compare-commit-run",
            &compare("commit:bbb2222", "base-a", ""),
        );
        ok(
            &mut snap,
            "compare-commit-run-all",
            &compare("commit:bbb2222", "base-a", "&all=1"),
        );
        ok(
            &mut snap,
            "compare-run-commit",
            &compare("base-a", "commit:bbb2222", ""),
        );
        ok(
            &mut snap,
            "compare-commit-commit",
            &compare("commit:aaa1111", "commit:bbb2222", ""),
        );
        ok(
            &mut snap,
            "compare-commit-prefix",
            &compare("commit:bbb2222", "commit:aaa1111", ""),
        );
        ok(
            &mut snap,
            "compare-hostile-a",
            &compare("hostile-run", "base-a", ""),
        );
        ok(
            &mut snap,
            "compare-hostile-commit",
            &compare(&format!("commit:c{HOSTILE_ENCODED}"), "base-b", "&all=1"),
        );
        ok(
            &mut snap,
            "compare-hostile-self",
            &compare("hostile-run", "hostile-run", ""),
        );
        ok(
            &mut snap,
            "compare-sample-self",
            &compare("cove-593-capacity", "cove-593-capacity", ""),
        );
        ok(
            &mut snap,
            "compare-sample-base",
            &compare("cove-593-capacity", "base-a", ""),
        );
        ok(
            &mut snap,
            "compare-sample-base-all",
            &compare("cove-593-capacity", "base-a", "&all=1"),
        );
        refused(
            &mut snap,
            "compare-no-run-a",
            &compare("nothing", "base-b", ""),
        );
        refused(
            &mut snap,
            "compare-no-run-b",
            &compare("base-a", "nothing", ""),
        );
        refused(
            &mut snap,
            "compare-no-run-hostile",
            &compare(
                HOSTILE_ENCODED,
                "base-b",
                &format!("&metric={HOSTILE_ENCODED}&all=1"),
            ),
        );
        refused(
            &mut snap,
            "compare-no-commit",
            &compare("base-a", "commit:zzzzzzz", ""),
        );
        refused(
            &mut snap,
            "compare-no-commit-hostile",
            &compare("base-a", &format!("commit:{HOSTILE_ENCODED}"), ""),
        );
        refused(
            &mut snap,
            "compare-commit-too-short",
            &compare("base-a", "commit:aaa", ""),
        );

        // The list past its first page: 48 runs more make 53.
        for index in 1..=48 {
            post(addr, &filler(index));
        }
        let first = snap.page(addr, "runs-full", "/ledger/");
        assert_eq!(first.status, 200);
        let older = first
            .body
            .split("?before=")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("a link to the older runs")
            .to_string();
        ok(&mut snap, "runs-older", &format!("/ledger/?before={older}"));
        ok(&mut snap, "compare-form-full", "/ledger/compare?a=fill-07");
        ok(&mut snap, "run-fill-01", "/ledger/runs/fill-01");
        ok(&mut snap, "cases-full", "/ledger/cases");

        snap.finish();
    }
}

/// The algorithm playground (`examples/algo`): every page it builds.
///
/// The app has no data of its own: every answer is a function of the request,
/// and every random input is seeded by a number in it, so nothing here needs
/// setting up and two runs differ only where a result shows a time it
/// measured itself (`read in 12 µs`, `3.4 ms`, from the app's clock: the app
/// has no way to be asked not to). Routes:
///
/// - `GET /` the index; `GET /app.js` the script (JavaScript, taken as is);
///   anything else is a plain-text 404, asserted and not kept.
/// - `GET|POST /matching`, `/sat`, `/anneal`: the form page, with the result
///   section pending (`data-autorun`) for a `GET` with an input, the result
///   in it for a `POST`, and with `part=result` the result alone.
///
/// The examples of each page are taken as their page and as their result
/// fragment, but for the `heavy` ones of matching and annealing: those are
/// stopped by the host's deadline, and what answers then is the host's text,
/// not the app's markup. SAT's `heavy` stops at its own decision budget, and
/// is taken. Hostile text goes where the request puts it on a page: vertex
/// names, the formula, the fields of the annealing form, the error that
/// quotes a word, and the `x-forwarded-prefix` every link is built from.
mod algo {
    use super::*;

    /// The one app on the host, mounted at `/algo`: the host says so to the
    /// app in `x-forwarded-prefix`, whatever the request sends, so every link
    /// of every page is `/algo/...` and a hostile prefix cannot be sent.
    const HOST: &str = "localhost";

    /// A `POST` of `body` as a form, which is what the page's own form sends
    /// (without `part`, the answer is the whole page).
    fn post_form(
        snap: &mut Snapshot,
        addr: SocketAddr,
        name: &str,
        path: &str,
        body: &str,
    ) -> Answer {
        snap.take(
            addr,
            name,
            "POST",
            path,
            &["Content-Type: application/x-www-form-urlencoded"],
            body,
        )
    }

    /// A page that must have been answered, with `needle` in it.
    fn ok(answer: Answer, needle: &str) {
        assert_eq!(answer.status, 200, "{answer:?}");
        assert!(answer.body.contains(needle), "no `{needle}` in {answer:?}");
    }

    #[test]
    #[ignore = "writes a snapshot of every algo page to $EXAMPLES_SNAPSHOT_DIR/algo; see the module's doc"]
    fn every_page() {
        let apps = apps(&[sample("algo")]);
        let host = start(&apps, 2);
        let addr = host.addr;
        let mut snap = Snapshot::new("algo", HOST);

        // The index, the script, the nav marked on each page, a 404.
        ok(snap.page(addr, "index", "/algo/"), "Algorithm playground");
        ok(
            snap.page(addr, "index_no_slash", "/algo"),
            "Algorithm playground",
        );
        let script = snap.page(addr, "app_js", "/algo/app.js");
        assert_eq!(script.status, 200);
        assert!(script.body.contains("AbortController"));
        let missing = request(addr, HOST, "GET", "/algo/nowhere", &[], "");
        assert_eq!(missing.status, 404);
        assert_eq!(missing.body, "no such page\n");
        let wrong_method = request(addr, HOST, "POST", "/algo/", &[], "");
        assert_eq!(wrong_method.status, 404);

        matching(&mut snap, addr);
        sat(&mut snap, addr);
        anneal(&mut snap, addr);
        snap.finish();
    }

    /// The matching page: each example (the form with its pending result,
    /// and the result alone) and each algorithm; posts; every input it
    /// refuses; hostile vertex names.
    fn matching(snap: &mut Snapshot, addr: SocketAddr) {
        // The bare page asks to pick an example; `?graph=` fills the form.
        ok(
            snap.page(addr, "matching_blank", "/algo/matching"),
            "Pick an example",
        );
        ok(
            snap.page(
                addr,
                "matching_algorithm_only",
                "/algo/matching?algorithm=both",
            ),
            "value=both checked",
        );
        // The examples. `heavy` is stopped by the host's deadline: its page
        // (the form, pending) is the app's, its result is not.
        for example in ["jobs", "seminars", "greedy", "random", "large", "heavy"] {
            ok(
                snap.page(
                    addr,
                    &format!("matching_example_{example}"),
                    &format!("/algo/matching?example={example}"),
                ),
                "data-autorun",
            );
        }
        for example in ["jobs", "seminars", "greedy", "random", "large"] {
            ok(
                snap.page(
                    addr,
                    &format!("matching_result_{example}"),
                    &format!("/algo/matching?example={example}&part=result"),
                ),
                "data-proved=true",
            );
        }
        // Each algorithm on the example that is drawn, and on the one that is
        // not.
        for algorithm in ["hopcroft-karp", "augmenting", "both"] {
            ok(
                snap.page(
                    addr,
                    &format!("matching_result_seminars_{algorithm}"),
                    &format!("/algo/matching?example=seminars&algorithm={algorithm}&part=result"),
                ),
                "data-proved=true",
            );
            ok(
                snap.page(
                    addr,
                    &format!("matching_page_random_{algorithm}"),
                    &format!("/algo/matching?example=random&algorithm={algorithm}"),
                ),
                "data-autorun",
            );
        }
        ok(
            snap.page(
                addr,
                "matching_result_large_both",
                "/algo/matching?example=large&algorithm=both&part=result",
            ),
            "The two agree",
        );
        // An algorithm the page does not know is Hopcroft-Karp.
        ok(
            snap.page(
                addr,
                "matching_result_unknown_algorithm",
                "/algo/matching?example=greedy&algorithm=nonsense&part=result",
            ),
            "data-proved=true",
        );
        // An example that does not exist is no example: the blank page.
        ok(
            snap.page(
                addr,
                "matching_example_missing",
                "/algo/matching?example=nope",
            ),
            "Pick an example",
        );
        // A graph in the query string; a generated one with another seed, and
        // one with a matching smaller than a side (unmatched and cover names).
        ok(
            snap.page(
                addr,
                "matching_graph_query",
                "/algo/matching?graph=a+x%0Aa+y%0Ab+x%0A",
            ),
            "data-autorun",
        );
        ok(
            snap.page(
                addr,
                "matching_result_generated_seed",
                "/algo/matching?graph=random+8+8+14+3&algorithm=both&part=result",
            ),
            "data-proved=true",
        );
        ok(
            snap.page(
                addr,
                "matching_result_unbalanced",
                "/algo/matching?graph=left%3A+a+b+c%0Aright%3A+x%0Aa+x%0Ab+x%0A&part=result",
            ),
            "data-proved=true",
        );
        // The form posted without the script: the whole page, with the
        // result in it.
        for (name, graph, algorithm) in [
            (
                "jobs",
                "ann: cook clean\nbob: cook\ncat: clean drive\n",
                "both",
            ),
            ("greedy", "a x\na y\nb x\n", "augmenting"),
            ("no_edges", "left: a b c\nright: x\n", "hopcroft-karp"),
        ] {
            let body = format!("graph={}&algorithm={algorithm}", form_value(graph));
            ok(
                post_form(
                    snap,
                    addr,
                    &format!("matching_post_{name}"),
                    "/algo/matching",
                    &body,
                ),
                "data-proved=true",
            );
        }
        // Hostile names: in the graph, the matching, the table and the
        // cover, in the vertices a side lists and in one an edge names.
        let hostile_graph = format!("{HOSTILE} x{HOSTILE}\n{HOSTILE}: y z\nb: x{HOSTILE}\n");
        let body = format!(
            "graph={}&algorithm=both&part=result",
            form_value(&hostile_graph)
        );
        ok(
            post_form(
                snap,
                addr,
                "matching_result_hostile",
                "/algo/matching",
                &body,
            ),
            "&lt;script&gt;",
        );
        let body = format!("graph={}&algorithm=both", form_value(&hostile_graph));
        ok(
            post_form(snap, addr, "matching_post_hostile", "/algo/matching", &body),
            "&lt;script&gt;",
        );
        // Declared sides with hostile names, one left over on each side.
        let declared =
            format!("left: {HOSTILE} b c\nright: x {HOSTILE} z\n{HOSTILE} x\nb {HOSTILE}\n");
        let body = format!(
            "graph={}&algorithm=hopcroft-karp&part=result",
            form_value(&declared)
        );
        ok(
            post_form(
                snap,
                addr,
                "matching_result_hostile_sides",
                "/algo/matching",
                &body,
            ),
            "&lt;script&gt;",
        );
        // Hostile text in the query: the form shows what was asked.
        ok(
            snap.page(
                addr,
                "matching_page_hostile_query",
                &format!("/algo/matching?graph={HOSTILE_ENCODED}+x"),
            ),
            "&lt;script&gt;",
        );
        // More than 60 names listed: the list is cut and counted.
        let wide: String = (0..70).map(|n| format!("l{n}: r0\n")).collect();
        let body = format!(
            "graph={}&algorithm=hopcroft-karp&part=result",
            form_value(&wide)
        );
        ok(
            post_form(snap, addr, "matching_result_wide", "/algo/matching", &body),
            "in all)",
        );
        // A graph too large to be drawn but small enough to list is the
        // `large` example; one with more pairs than the table shows would
        // need over 500 and is left out for its size.

        // What it refuses, each in the result and in a page.
        for (name, graph) in [
            ("three_words", "a b c\n".to_string()),
            ("three_words_hostile", format!("{HOSTILE} b c\n")),
            ("empty", String::new()),
            ("comments_only", "# nothing\n".to_string()),
            ("random_word", format!("random {HOSTILE} 5 1\n")),
            ("random_short", "random 5\n".to_string()),
            (
                "random_too_many_vertices",
                "random 40000 10 10 1\n".to_string(),
            ),
            ("random_zero_side", "random 0 5 3 1\n".to_string()),
            ("random_too_many_edges", "random 2 2 5 1\n".to_string()),
            ("random_and_more", "random 3 3 4 1\na x\n".to_string()),
        ] {
            let body = format!("graph={}&algorithm=both&part=result", form_value(&graph));
            ok(
                post_form(
                    snap,
                    addr,
                    &format!("matching_refused_{name}"),
                    "/algo/matching",
                    &body,
                ),
                "data-stop=app",
            );
        }
        let body = format!(
            "graph={}&algorithm=both",
            form_value(&format!("{HOSTILE} b c\n"))
        );
        ok(
            post_form(
                snap,
                addr,
                "matching_post_refused_hostile",
                "/algo/matching",
                &body,
            ),
            "data-stop=app",
        );
        // A GET with an empty graph runs nothing yet: pending, not refused.
        ok(
            snap.page(addr, "matching_graph_empty_page", "/algo/matching?graph="),
            "data-autorun",
        );
        ok(
            snap.page(
                addr,
                "matching_graph_empty_result",
                "/algo/matching?graph=&part=result",
            ),
            "data-stop=app",
        );
    }

    /// The SAT page: each example, formula kinds, verdicts, budgets, errors.
    fn sat(snap: &mut Snapshot, addr: SocketAddr) {
        ok(snap.page(addr, "sat_blank", "/algo/sat"), "Pick an example");
        for example in [
            "tiny",
            "contradiction",
            "pigeonhole",
            "random",
            "sudoku",
            "hard",
            "heavy",
        ] {
            ok(
                snap.page(
                    addr,
                    &format!("sat_example_{example}"),
                    &format!("/algo/sat?example={example}"),
                ),
                "data-autorun",
            );
        }
        // The results, one per example: satisfiable with the assignment
        // drawn, the sudoku as its grid, unsatisfiable, and the heavy one
        // giving up at the budget its link fills in. The `hard` one is the
        // app's longest deterministic search.
        for (example, verdict) in [
            ("tiny", "data-verdict=sat"),
            ("contradiction", "data-verdict=unsat"),
            ("pigeonhole", "data-verdict=unsat"),
            ("random", "data-verdict=sat"),
            ("sudoku", "data-verdict=sat"),
            ("hard", "data-verdict=unsat"),
            ("heavy", "data-verdict=unknown"),
        ] {
            ok(
                snap.page(
                    addr,
                    &format!("sat_result_{example}"),
                    &format!("/algo/sat?example={example}&part=result"),
                ),
                verdict,
            );
        }
        // The heavy example's page carries its budget in the form.
        ok(
            snap.page(
                addr,
                "sat_example_heavy_budget_set",
                "/algo/sat?example=heavy&budget=77",
            ),
            "value=\"77\"",
        );
        // Formulas by hand, posted for the whole page and as the fragment.
        for (name, formula, budget) in [
            ("unique_model", "1 2 0\n-1 2 0\n1 -2 0\n", ""),
            ("unit_conflict", "1 0\n-1 0\n", ""),
            ("random_unsat", "random 3 60 256 2", ""),
            ("random_other_k", "random 2 10 12 5", ""),
            ("pigeonhole_fits", "pigeonhole 5 5", ""),
            (
                "tautology_and_note",
                "c a tautology is dropped\np cnf 3 4\n1 -1 0\n2 3 0\n-2 0\n-3 1 0\n",
                "",
            ),
            ("variables_unused", "p cnf 9 1\n1 2 0\n", ""),
            ("big_assignment_not_drawn", "random 3 1100 100 1", ""),
            ("budget_small", "pigeonhole 8 7", "100"),
            ("budget_one", "pigeonhole 6 5", "1"),
            ("budget_zero", "1 2 0\n", "0"),
            ("budget_too_big", "1 2 0\n", "100000001"),
            ("budget_not_a_number", "1 2 0\n", "many"),
            ("budget_negative", "1 2 0\n", "-5"),
        ] {
            let body = format!(
                "formula={}&budget={budget}&part=result",
                form_value(formula)
            );
            ok(
                post_form(
                    snap,
                    addr,
                    &format!("sat_result_{name}"),
                    "/algo/sat",
                    &body,
                ),
                "data-",
            );
        }
        for (name, formula, budget) in [
            ("tiny", "p cnf 3 2\n1 -3 0\n2 3 -1 0\n", ""),
            ("budget_small", "pigeonhole 8 7", "100"),
            ("budget_not_a_number", "1 2 0\n", "many"),
        ] {
            let body = format!("formula={}&budget={budget}", form_value(formula));
            let answer = post_form(snap, addr, &format!("sat_post_{name}"), "/algo/sat", &body);
            assert_eq!(answer.status, 200);
            assert!(answer.body.starts_with("<!doctype html>"));
        }
        // A formula in the query string.
        ok(
            snap.page(
                addr,
                "sat_formula_query",
                "/algo/sat?formula=1+2+0%0A-1+0%0A",
            ),
            "data-autorun",
        );
        ok(
            snap.page(
                addr,
                "sat_result_formula_query",
                "/algo/sat?formula=1+2+0%0A-1+0%0A&part=result",
            ),
            "data-verdict=sat",
        );
        ok(
            snap.page(
                addr,
                "sat_formula_query_empty",
                "/algo/sat?formula=&part=result",
            ),
            "data-stop=app",
        );
        // Hostile text: in the formula the form shows back, and in what each
        // error quotes (a word, a line).
        ok(
            snap.page(
                addr,
                "sat_page_hostile_query",
                &format!(
                    "/algo/sat?formula=1+2+0%0Ac+{HOSTILE_ENCODED}%0A&budget={HOSTILE_ENCODED}"
                ),
            ),
            "&lt;script&gt;",
        );
        let commented = format!("c {HOSTILE}\n1 2 0\n-1 0\n");
        let body = format!("formula={}&budget=&part=result", form_value(&commented));
        ok(
            post_form(snap, addr, "sat_result_hostile_comment", "/algo/sat", &body),
            "data-verdict=sat",
        );
        let body = format!("formula={}&budget=", form_value(&commented));
        ok(
            post_form(snap, addr, "sat_post_hostile_comment", "/algo/sat", &body),
            "&lt;script&gt;",
        );
        // What it refuses.
        for (name, formula) in [
            ("angle_word", "1 2 0\n3 <x> 0\n".to_string()),
            ("hostile_word", format!("1 2 0\n{HOSTILE} 0\n")),
            ("empty", String::new()),
            ("no_clauses", "p cnf 3 0\n".to_string()),
            ("bad_problem_line", "p cnf x y\n1 0\n".to_string()),
            ("minus_alone", "1 - 0\n".to_string()),
            ("minus_zero", "1 -0\n".to_string()),
            ("empty_clause", "1 2 0\n0\n".to_string()),
            ("huge_number", "99999999999999999999 0\n".to_string()),
            ("variable_over_declared", "p cnf 2 1\n3 0\n".to_string()),
            ("variable_over_bound", "5001 0\n".to_string()),
            ("pigeonhole_word", format!("pigeonhole {HOSTILE} 3")),
            ("pigeonhole_short", "pigeonhole 3".to_string()),
            ("pigeonhole_big", "pigeonhole 41 3".to_string()),
            ("random_word", format!("random 3 {HOSTILE} 5 1")),
            ("random_short", "random 3 5".to_string()),
            ("random_k_big", "random 11 20 5 1".to_string()),
            ("random_over_bound", "random 3 6000 5 1".to_string()),
            ("sudoku_short", "sudoku 53..7".to_string()),
            ("sudoku_bad_cell", format!("sudoku x{}", ".".repeat(80))),
            (
                "sudoku_hostile_cell",
                format!("sudoku {HOSTILE}{}", ".".repeat(73)),
            ),
        ] {
            let body = format!("formula={}&budget=&part=result", form_value(&formula));
            ok(
                post_form(
                    snap,
                    addr,
                    &format!("sat_refused_{name}"),
                    "/algo/sat",
                    &body,
                ),
                "data-stop=app",
            );
        }
        let body = format!(
            "formula={}&budget=",
            form_value(&format!("1 2 0\n{HOSTILE} 0\n"))
        );
        ok(
            post_form(snap, addr, "sat_post_refused_hostile", "/algo/sat", &body),
            "data-stop=app",
        );
    }

    /// The annealing page: each example, each setting, every refusal.
    fn anneal(snap: &mut Snapshot, addr: SocketAddr) {
        ok(
            snap.page(addr, "anneal_blank", "/algo/anneal"),
            "Pick an example",
        );
        // The examples' pages; `heavy` is the deadline's, but its form is
        // the app's.
        for example in ["cooling", "schedules", "seeds", "circle", "long", "heavy"] {
            ok(
                snap.page(
                    addr,
                    &format!("anneal_example_{example}"),
                    &format!("/algo/anneal?example={example}"),
                ),
                "data-autorun",
            );
        }
        for example in ["cooling", "schedules", "seeds", "circle", "long"] {
            ok(
                snap.page(
                    addr,
                    &format!("anneal_result_{example}"),
                    &format!("/algo/anneal?example={example}&part=result"),
                ),
                "data-checked=true",
            );
        }
        // Settings: seeds, the problem seed, the problem, linear cooling,
        // very short runs, and one where the runs tie.
        for (name, query) in [
            ("reseeded", "example=cooling&a_seed=9"),
            ("problem_seed", "example=cooling&pseed=5"),
            ("small_circle", "problem=circle&n=8&a_iterations=500&b_iterations=500"),
            ("small_random", "n=4&a_iterations=1&b_iterations=2"),
            (
                "both_linear",
                "n=12&a_schedule=linear&b_schedule=linear&a_iterations=3000&b_iterations=3000",
            ),
            (
                "tie",
                "n=10&a_iterations=2000&b_iterations=2000&b_start=0.5&b_end=0.001&b_schedule=geometric",
            ),
            ("hot", "n=10&a_start=1000&b_start=0.001&a_iterations=2000&b_iterations=2000"),
            ("only_a_seed", "a_seed=3&a_iterations=1000&b_iterations=1000"),
        ] {
            ok(
                snap.page(
                    addr,
                    &format!("anneal_result_{name}"),
                    &format!("/algo/anneal?{query}&part=result"),
                ),
                "data-checked=true",
            );
            ok(
                snap.page(
                    addr,
                    &format!("anneal_page_{name}"),
                    &format!("/algo/anneal?{query}"),
                ),
                "data-autorun",
            );
        }
        // An example with a setting over it.
        ok(
            snap.page(
                addr,
                "anneal_result_example_overridden",
                "/algo/anneal?example=circle&n=12&part=result",
            ),
            "data-checked=true",
        );
        ok(
            snap.page(addr, "anneal_example_missing", "/algo/anneal?example=nope"),
            "Pick an example",
        );
        // Posted: the whole page, with the result in it.
        for (name, body) in [
            (
                "defaults_short",
                "n=10&pseed=1&a_seed=1&a_iterations=1000&a_start=0.5&a_end=0.001&a_schedule=geometric&b_seed=1&b_iterations=1000&b_start=0.002&b_end=0.001&b_schedule=linear",
            ),
            (
                "circle",
                "problem=circle&n=10&a_iterations=1000&b_iterations=1000",
            ),
        ] {
            ok(
                post_form(snap, addr, &format!("anneal_post_{name}"), "/algo/anneal", body),
                "data-checked=true",
            );
        }
        // Hostile values in the form's fields: shown back in the form
        // (`value="..."`), and refused where a field cannot be a number.
        for field in [
            "n",
            "pseed",
            "a_seed",
            "a_iterations",
            "a_start",
            "a_end",
            "b_seed",
            "b_iterations",
            "b_start",
            "b_end",
        ] {
            ok(
                snap.page(
                    addr,
                    &format!("anneal_page_hostile_{field}"),
                    &format!("/algo/anneal?{field}={HOSTILE_ENCODED}"),
                ),
                "&lt;script&gt;",
            );
            ok(
                snap.page(
                    addr,
                    &format!("anneal_refused_hostile_{field}"),
                    &format!("/algo/anneal?{field}={HOSTILE_ENCODED}&part=result"),
                ),
                "data-stop=app",
            );
        }
        let body = format!("n=10&a_seed={HOSTILE_ENCODED}");
        ok(
            post_form(
                snap,
                addr,
                "anneal_post_refused_hostile",
                "/algo/anneal",
                &body,
            ),
            "&lt;script&gt;",
        );
        // The schedule and the problem as hostile strings: not a known
        // value, so the defaults' radio and option are chosen.
        ok(
            snap.page(
                addr,
                "anneal_result_hostile_schedule",
                &format!(
                    "/algo/anneal?n=6&a_iterations=100&b_iterations=100&a_schedule={HOSTILE_ENCODED}&problem={HOSTILE_ENCODED}&part=result"
                ),
            ),
            "data-checked=true",
        );
        // What it refuses, each with its words.
        for (name, query) in [
            ("points_few", "n=3"),
            ("points_many", "n=501"),
            ("points_text", "n=many"),
            ("problem_seed_negative", "pseed=-1"),
            ("a_seed_negative", "a_seed=-1"),
            ("a_seed_text", "a_seed=x"),
            ("b_seed_text", "b_seed=x"),
            ("a_iterations_zero", "a_iterations=0"),
            ("b_iterations_zero", "b_iterations=0"),
            ("a_iterations_over", "a_iterations=50000001"),
            ("b_iterations_over", "b_iterations=50000001"),
            ("a_start_zero", "a_start=0"),
            ("b_start_negative", "b_start=-1"),
            ("a_end_text", "a_end=cold"),
            ("b_end_over", "b_end=1001"),
            ("a_start_empty", "a_start="),
        ] {
            ok(
                snap.page(
                    addr,
                    &format!("anneal_refused_{name}"),
                    &format!("/algo/anneal?{query}&part=result"),
                ),
                "data-stop=app",
            );
        }
        ok(
            snap.page(addr, "anneal_page_refused_points_few", "/algo/anneal?n=3"),
            "data-autorun",
        );
    }
}

/// The webhook lab (`examples/webhooks`): every page of its admin area, as
/// the admin secret reaches it, over data made here.
///
/// Routes of the app (all under the mount `/webhooks`), and what is taken:
///
/// - `ANY /in/<endpoint>[/...]`: the receive URL. JSON receipt or the
///   endpoint's own configured answer, or plain text for an unknown
///   endpoint. No markup: used to put data in, never written (the unknown
///   endpoint's text is).
/// - `GET /` redirects to `/admin`: no body.
/// - `GET /admin`: the endpoint list and the form that makes one: HTML.
/// - `POST /admin/endpoints`: makes an endpoint; `303`, or JSON with
///   `Accept: application/json`. No markup.
/// - `GET /admin/e/<id>`: an endpoint's history page, `?before=<event id>`
///   for the next page: HTML. `?format=json`: JSON, skipped.
/// - `POST /admin/e/<id>/settings`: `303`, or a plain-text `400`.
/// - `POST /admin/e/<id>/clear`, `.../delete`: `303`.
/// - `GET /admin/e/<id>/events/<event>`: one request in full: HTML.
///   `?view=json`, `?view=raw`, `?view=resends`: JSON or text, skipped.
/// - `POST .../events/<event>/delete`: `303`.
/// - `POST .../events/<event>/resend`: `303` to the request's page, or JSON
///   with `Accept: application/json`; a plain-text `400` without a `url`.
/// - Refusals and misses are plain text (`401` with `www-authenticate`,
///   `403` for a cross-site form, `404`s). The app builds those itself, so
///   the ones that say something about the request are written too, under
///   a name that begins `text_`.
///
/// What is not reachable here: the `403` for an Access deployment (it needs
/// `[access] team` and a signed token).
///
/// Ids are random (an endpoint's sixteen hex digits, an event's microsecond
/// time and eight hex digits), so this test takes them from the answers and
/// never writes one itself. The list of endpoints is in id order, which is
/// random, so it is only ever written while one endpoint exists. What is
/// left that differs between two runs is what a page shows of the clock and
/// of the ports: see `examples_snapshot_diff.py`.
mod webhooks {
    use super::*;

    const SECRET: &str = "webhooks-snapshot-secret";
    const HOST: &str = "lab.test";
    const FORM: &str = "Content-Type: application/x-www-form-urlencoded";

    /// The shipped `app.toml`, with the admin secret literal and the fetch
    /// allowlist at any port of 127.0.0.1 (the host listens on a free one).
    fn config() -> String {
        let shipped = std::fs::read_to_string(examples().join("webhooks/app.toml")).unwrap();
        let mut config = shipped.replace(
            "admin = { env = \"WEBHOOKS_ADMIN_TOKEN\" }",
            &format!("admin = {{ value = \"{SECRET}\" }}"),
        );
        assert!(
            config.contains(SECRET),
            "the shipped app.toml changed shape"
        );
        let start = config.find("allow = [").expect("an allowlist");
        let end = start + config[start..].find(']').unwrap() + 1;
        config.replace_range(start..end, "allow = [\"http://127.0.0.1:*\"]");
        config
    }

    fn bearer() -> String {
        format!("Authorization: Bearer {SECRET}")
    }

    /// Makes an endpoint as the page's form does (`303` to its page) from
    /// `form`; its id.
    fn make(addr: SocketAddr, form: &str) -> String {
        let made = request(
            addr,
            HOST,
            "POST",
            "/webhooks/admin/endpoints",
            &[&bearer(), FORM],
            form,
        );
        assert_eq!(made.status, 303, "{made:?}");
        let location = made.header("location").expect("a location");
        let id = location
            .strip_prefix("/webhooks/admin/e/")
            .unwrap_or_else(|| panic!("{location}"));
        assert_eq!(id.len(), 16, "{location}");
        id.to_string()
    }

    /// Sends a request to endpoint `id`'s receive URL, `rest` being what
    /// follows it (a path and a query); the id of the event stored.
    fn send(
        addr: SocketAddr,
        id: &str,
        method: &str,
        rest: &str,
        headers: &[&str],
        body: &str,
    ) -> String {
        let answer = request(
            addr,
            HOST,
            method,
            &format!("/webhooks/in/{id}{rest}"),
            headers,
            body,
        );
        assert!(
            matches!(answer.status, 200 | 202 | 503),
            "{method} {rest}: {answer:?}"
        );
        answer
            .header("x-webhook-event")
            .unwrap_or_else(|| panic!("{answer:?}"))
            .to_string()
    }

    /// Takes the admin page at `path` (under `/webhooks/admin`), logged in.
    fn page(snap: &mut Snapshot, addr: SocketAddr, name: &str, path: &str, status: u16) {
        let answer = snap.take(
            addr,
            name,
            "GET",
            &format!("/webhooks/admin{path}"),
            &[&bearer()],
            "",
        );
        assert_eq!(answer.status, status, "{name}: {answer:?}");
    }

    /// Posts `form` to `path` (under `/webhooks/admin`), logged in, and
    /// takes the answer: for the ones that answer with text.
    fn post_taken(
        snap: &mut Snapshot,
        addr: SocketAddr,
        name: &str,
        path: &str,
        form: &str,
        status: u16,
    ) {
        let answer = snap.take(
            addr,
            name,
            "POST",
            &format!("/webhooks/admin{path}"),
            &[&bearer(), FORM],
            form,
        );
        assert_eq!(answer.status, status, "{name}: {answer:?}");
    }

    /// Posts `form` to `path` (under `/webhooks/admin`), logged in, and
    /// expects the redirect a change answers with; nothing is written.
    fn post(addr: SocketAddr, path: &str, form: &str) {
        let answer = request(
            addr,
            HOST,
            "POST",
            &format!("/webhooks/admin{path}"),
            &[&bearer(), FORM],
            form,
        );
        assert_eq!(answer.status, 303, "{path}: {answer:?}");
    }

    /// Resends event `event` of endpoint `id` to `url`; nothing is written
    /// (the page that shows the attempt is).
    fn resend(addr: SocketAddr, id: &str, event: &str, url: &str) {
        post(
            addr,
            &format!("/e/{id}/events/{event}/resend"),
            &format!("url={}", form_value(url)),
        );
    }

    /// Saves response settings for endpoint `id`.
    fn settings(addr: SocketAddr, id: &str, form: &str) {
        post(addr, &format!("/e/{id}/settings"), form);
    }

    #[test]
    #[ignore = "writes a snapshot of every webhooks page to $EXAMPLES_SNAPSHOT_DIR/webhooks; see the module's doc"]
    fn every_page() {
        let apps = apps(&[
            AppSpec {
                name: "webhooks",
                from: examples().join("webhooks"),
                config: Some(Box::leak(config().into_boxed_str())),
            },
            // What a resend is sent to: `/echo`, `/big`, and a greeting.
            sample("hello"),
        ]);
        let host = start(&apps, 2);
        let addr = host.addr;
        let port = addr.port();
        let mut snap = Snapshot::new("webhooks", HOST);

        // ---- Refused and missing, before anything exists.
        let refused = snap.take(
            addr,
            "text_refused_no_login",
            "GET",
            "/webhooks/admin",
            &[],
            "",
        );
        assert_eq!(refused.status, 401);
        let refused = snap.take(
            addr,
            "text_refused_wrong_secret",
            "GET",
            "/webhooks/admin",
            &["Authorization: Bearer wrong"],
            "",
        );
        assert_eq!(refused.status, 401);
        let refused = snap.take(
            addr,
            "text_refused_cross_site",
            "POST",
            "/webhooks/admin/endpoints",
            &[&bearer(), "Origin: https://evil.example"],
            "name=x",
        );
        assert_eq!(refused.status, 403);
        let missing = snap.take(
            addr,
            "text_root_missing",
            "GET",
            "/webhooks/nothing",
            &[],
            "",
        );
        assert_eq!(missing.status, 404);
        let missing = snap.take(
            addr,
            "text_receive_unknown_endpoint",
            "POST",
            &format!("/webhooks/in/{HOSTILE_ENCODED}"),
            &[],
            "x",
        );
        assert_eq!(missing.status, 404);
        page(&mut snap, addr, "text_admin_no_such_page", "/nowhere", 404);
        page(
            &mut snap,
            addr,
            "text_admin_unknown_endpoint",
            &format!("/e/{HOSTILE_ENCODED}"),
            404,
        );
        page(&mut snap, addr, "list_empty", "", 200);

        // ---- Endpoint one: the hostile name, default limits, masking on,
        // and every kind of delivery.
        let one = make(
            addr,
            &format!(
                "name={}&retention=100&maxBody=65536&masking=present&mask=on",
                form_value(&format!("hostile {HOSTILE}"))
            ),
        );
        page(&mut snap, addr, "list_one_endpoint_empty", "", 200);
        page(
            &mut snap,
            addr,
            "endpoint_hostile_empty",
            &format!("/e/{one}"),
            200,
        );

        // The first six: they end up on the history's second page.
        let mut sent: Vec<String> = Vec::new();
        let json_body = r#"{"text":"<script>&\"'","list":[1,2.5,null,true,"x"],"nested":{"unicode":"é😀","empty":{},"none":[]}}"#;
        let json = send(
            addr,
            &one,
            "POST",
            &format!("/hooks/{HOSTILE_ENCODED}?a=1&b={HOSTILE_ENCODED}&{HOSTILE_ENCODED}=x"),
            &[
                "Content-Type: application/json",
                "X-GitHub-Event: push",
                &format!("X-Hostile: {HOSTILE}"),
                "Authorization: Bearer ghs_sender_token",
                "Cookie: session=abc; other=<&>",
                "X-Api-Key: k-123",
                "User-Agent: GitHub-Hookshot/abc123",
                "Accept: */*",
                "X-Request-Id: 0f9e8d7c",
                "X-Hub-Signature-256: sha256=00112233445566778899aabbccddeeff",
            ],
            json_body,
        );
        sent.push(json.clone());
        let plain = send(
            addr,
            &one,
            "PUT",
            "/plain/text",
            &["Content-Type: text/plain"],
            &format!("first line\n{HOSTILE}\n\tindented & <b>last</b>"),
        );
        sent.push(plain.clone());
        let form = send(
            addr,
            &one,
            "POST",
            "/form",
            &[FORM],
            &format!("a=1&b={HOSTILE_ENCODED}&c=two+words"),
        );
        sent.push(form.clone());
        let ping = send(addr, &one, "GET", "/ping", &[], "");
        sent.push(ping.clone());
        let dup = send(
            addr,
            &one,
            "POST",
            "/duplicate-field",
            &["Content-Type: application/json"],
            r#"{"a":1,"a":2}"#,
        );
        sent.push(dup.clone());
        let bare = send(addr, &one, "POST", "", &[], "hi");
        sent.push(bare.clone());
        // Filling: enough to make a second page of history.
        for n in 0..45 {
            sent.push(send(
                addr,
                &one,
                "POST",
                &format!("/bulk/{n}"),
                &["Content-Type: text/plain"],
                &format!("n={n}"),
            ));
        }
        // The last five: the first page.
        let array = send(
            addr,
            &one,
            "POST",
            "/array",
            &["Content-Type: application/json"],
            &format!(
                "[1, \"{}\", null, true, {{\"k\": \"v\"}}]",
                "<script>&\\\"'"
            ),
        );
        sent.push(array.clone());
        let blank = send(
            addr,
            &one,
            "POST",
            "/blank",
            &["Content-Type: application/json"],
            "   \n",
        );
        sent.push(blank.clone());
        // Not text: controls, a DEL, a replacement character, astral and
        // accented letters.
        let binary = send(
            addr,
            &one,
            "POST",
            "/binary",
            &["Content-Type: application/octet-stream"],
            "\u{0}\u{1}\u{2}\r\n\t\u{7f}\u{80}é😀\u{fffd}",
        );
        sent.push(binary.clone());
        // Past the endpoint's 64 KiB limit: stored truncated.
        let large_body: String = (0..3200)
            .map(|n| format!("{n:05} abcdefghij <&> \"q\"\n"))
            .collect();
        assert!(large_body.len() > 65536);
        let large = send(
            addr,
            &one,
            "POST",
            "/large",
            &["Content-Type: text/plain"],
            &large_body,
        );
        sent.push(large.clone());
        // Many headers.
        let many: Vec<String> = (0..30)
            .map(|n| format!("X-Many-{n:02}: value {n} <&> \"'"))
            .collect();
        let many_refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let headers = send(addr, &one, "POST", "/many-headers", &many_refs, "x");
        sent.push(headers.clone());
        assert_eq!(sent.len(), 56);

        page(&mut snap, addr, "list_one_endpoint_populated", "", 200);
        // The newest 50, with the link to the rest; and the rest.
        page(
            &mut snap,
            addr,
            "endpoint_hostile_first_page",
            &format!("/e/{one}"),
            200,
        );
        page(
            &mut snap,
            addr,
            "endpoint_hostile_second_page",
            &format!("/e/{one}?before={}", sent[6]),
            200,
        );
        page(
            &mut snap,
            addr,
            "endpoint_hostile_before_the_oldest",
            &format!("/e/{one}?before={}", sent[0]),
            200,
        );
        page(
            &mut snap,
            addr,
            "endpoint_hostile_before_hostile",
            &format!("/e/{one}?before={HOSTILE_ENCODED}"),
            200,
        );
        for (name, event) in [
            ("json", &json),
            ("plain", &plain),
            ("form", &form),
            ("ping", &ping),
            ("duplicate_field", &dup),
            ("bare", &bare),
            ("array", &array),
            ("blank", &blank),
            ("binary", &binary),
            ("large_truncated", &large),
            ("many_headers", &headers),
        ] {
            page(
                &mut snap,
                addr,
                &format!("event_{name}"),
                &format!("/e/{one}/events/{event}"),
                200,
            );
        }
        // An unknown `view` is the page.
        page(
            &mut snap,
            addr,
            "event_json_unknown_view",
            &format!("/e/{one}/events/{json}?view=other&x={HOSTILE_ENCODED}"),
            200,
        );
        page(
            &mut snap,
            addr,
            "text_event_unknown",
            &format!("/e/{one}/events/{HOSTILE_ENCODED}"),
            404,
        );

        // Settings the form refuses, each said in text.
        for (name, form) in [
            ("status", "status=99".to_string()),
            ("delay", "delayMs=9000".to_string()),
            ("header_line", format!("headers={}", form_value(HOSTILE))),
            ("header_name", "headers=Bad+Name%3A+x".to_string()),
            ("error_every", "errorEvery=lots".to_string()),
            ("several", "status=1&delayMs=-1&errorEvery=x".to_string()),
        ] {
            post_taken(
                &mut snap,
                addr,
                &format!("text_settings_refused_{name}"),
                &format!("/e/{one}/settings"),
                &form,
                400,
            );
        }

        // Resends of the JSON request (masked headers are not sent), to
        // this host's other app, and what comes of each.
        post_taken(
            &mut snap,
            addr,
            "text_resend_without_url",
            &format!("/e/{one}/events/{json}/resend"),
            "url=+",
            400,
        );
        post_taken(
            &mut snap,
            addr,
            "text_resend_unknown_event",
            &format!("/e/{one}/events/{HOSTILE_ENCODED}/resend"),
            "url=http%3A%2F%2Fexample.org%2F",
            404,
        );
        resend(
            addr,
            &one,
            &json,
            &format!("http://127.0.0.1:{port}/hello/echo"),
        );
        page(
            &mut snap,
            addr,
            "event_json_resent_to_echo",
            &format!("/e/{one}/events/{json}"),
            200,
        );
        resend(
            addr,
            &one,
            &json,
            &format!("http://127.0.0.1:{port}/hello/?name={HOSTILE_ENCODED}"),
        );
        // A response that is not a success, from the host itself: not the lab's own
        // receive URL, whose `x-cove-app-version` would be the lab's source hash.
        resend(
            addr,
            &one,
            &json,
            &format!("http://127.0.0.1:{port}/no-such-app/"),
        );
        // Nothing listens at port 1.
        resend(addr, &one, &json, "http://127.0.0.1:1/");
        page(
            &mut snap,
            addr,
            "event_json_resent_four_times",
            &format!("/e/{one}/events/{json}"),
            200,
        );
        // Off the allowlist, with a hostile URL: the newest, so it is the
        // form's value as well.
        resend(
            addr,
            &one,
            &json,
            &format!("http://example.org/hook?q={HOSTILE}"),
        );
        page(
            &mut snap,
            addr,
            "event_json_resent_refused",
            &format!("/e/{one}/events/{json}"),
            200,
        );
        // A request with no body, no masked header and no query.
        resend(
            addr,
            &one,
            &ping,
            &format!("http://127.0.0.1:{port}/hello/"),
        );
        page(
            &mut snap,
            addr,
            "event_ping_resent",
            &format!("/e/{one}/events/{ping}"),
            200,
        );

        // ---- Endpoint two: small limits, headers kept as sent.
        let two = make(
            addr,
            &format!(
                "name={}&retention=2&maxBody=10&mask=off",
                form_value(&format!("small-{HOSTILE}"))
            ),
        );
        let mut kept = Vec::new();
        for n in 0..4 {
            kept.push(send(
                addr,
                &two,
                "POST",
                &format!("/n/{n}"),
                &[
                    "Authorization: Bearer sender-token",
                    "Cookie: session=abc",
                    "Content-Type: text/plain",
                ],
                &format!("body number {n}"),
            ));
        }
        page(&mut snap, addr, "endpoint_small", &format!("/e/{two}"), 200);
        page(
            &mut snap,
            addr,
            "event_small_truncated",
            &format!("/e/{two}/events/{}", kept[3]),
            200,
        );
        // Past the retention count.
        page(
            &mut snap,
            addr,
            "text_event_past_retention",
            &format!("/e/{two}/events/{}", kept[0]),
            404,
        );
        // A response past the 10 bytes it may store.
        resend(
            addr,
            &two,
            &kept[3],
            &format!("http://127.0.0.1:{port}/hello/big?bytes=100"),
        );
        page(
            &mut snap,
            addr,
            "event_small_resent_truncated",
            &format!("/e/{two}/events/{}", kept[3]),
            200,
        );
        // What the settings summary says in each state.
        settings(addr, &two, "status=201");
        page(
            &mut snap,
            addr,
            "endpoint_small_status_only",
            &format!("/e/{two}"),
            200,
        );
        settings(addr, &two, "errorEvery=2&errorStatus=429&delayMs=3");
        page(
            &mut snap,
            addr,
            "endpoint_small_errors_and_delay",
            &format!("/e/{two}"),
            200,
        );
        // One request deleted, then the history.
        post(addr, &format!("/e/{two}/events/{}/delete", kept[2]), "");
        page(
            &mut snap,
            addr,
            "endpoint_small_one_deleted",
            &format!("/e/{two}"),
            200,
        );
        post(addr, &format!("/e/{two}/clear"), "");
        page(
            &mut snap,
            addr,
            "endpoint_small_cleared",
            &format!("/e/{two}"),
            200,
        );

        // ---- Endpoint three: no name, a configured answer that fails every
        // third request, with hostile text in each field of it.
        let three = make(addr, "name=+++&retention=5");
        settings(
            addr,
            &three,
            &format!(
                "status=202&delayMs=1&headers={}&body={}&errorEvery=3&errorStatus=503&errorBody={}",
                form_value(&format!("x-hostile: {HOSTILE}\r\nX-Lab: yes\n\n")),
                form_value(&format!("</textarea>{HOSTILE}")),
                form_value(&format!("</textarea>try later {HOSTILE}")),
            ),
        );
        page(
            &mut snap,
            addr,
            "endpoint_flaky_configured",
            &format!("/e/{three}"),
            200,
        );
        for n in 0..6 {
            send(addr, &three, "POST", &format!("/{n}"), &[], &format!("{n}"));
        }
        page(
            &mut snap,
            addr,
            "endpoint_flaky",
            &format!("/e/{three}"),
            200,
        );

        // ---- Endpoints gone.
        post(addr, &format!("/e/{one}/delete"), "");
        post(addr, &format!("/e/{three}/delete"), "");
        page(&mut snap, addr, "list_one_small_endpoint", "", 200);
        page(
            &mut snap,
            addr,
            "text_endpoint_deleted",
            &format!("/e/{one}"),
            404,
        );
        post(addr, &format!("/e/{two}/delete"), "");
        page(&mut snap, addr, "list_empty_again", "", 200);

        snap.finish();
    }
}

/// `examples/watch`: the dashboard, one target's history, the not-found
/// page and the store's error page — every page the app writes — and, for
/// the refactor's sake, the plain-text answers of its forms and of the
/// paths only the host may ask for, since they are written by the same
/// code.
///
/// What a page shows comes from the app's own SQL store, which is written
/// here straight into, at fixed times, rather than by running checks: a
/// real check's latency is measured and its time is the clock's. The
/// dashboard's "a day" column counts the checks since a day before *now*
/// and has no upper bound, so the checks that count are dated in 2100
/// (always within the last day) and the ones that do not are dated in 2023
/// (never within it): the counts and percentages are the same every run.
///
/// The host's own background work is kept out of the run: the minute's
/// `[[triggers]]` schedule is rewritten to one that names only the first
/// minute of the year (and the test asserts nothing was fired), no check
/// runs before the pages are taken — so nothing is queued and nothing
/// deferred — and the run asserts both before the forms that do start
/// checks. Those forms come last, and no page is taken after them.
mod watch {
    use super::*;

    use minicloud::sql::Store;
    use std::io::{Read, Write};

    /// What `[secrets] admin` is here.
    const SECRET: &str = "watch-secret";

    /// 2100-01-01T00:00:00Z, in milliseconds: a check at or after it is
    /// within the dashboard's last day whenever the test runs.
    const RECENT: i64 = 4_102_444_800_000;
    /// 2023-11-14T22:13:20Z, in milliseconds: a check at it is in the
    /// history and never in the last day.
    const OLD: i64 = 1_700_000_000_000;
    const MINUTE: i64 = 60_000;

    /// The shipped `app.toml`, with
    ///
    /// - the admin secret a literal;
    /// - `[access]` only its token: no team, aud or emails, so Access is off
    ///   whatever the environment says and the bearer token is the way in;
    /// - the fetch allowlist any port of 127.0.0.1, which is where the test
    ///   host and its other app are;
    /// - `[rate]` well above what the run asks for;
    /// - the schedule one that names 00:00 on 1 January only.
    fn config() -> String {
        let shipped = std::fs::read_to_string(examples().join("watch/app.toml")).unwrap();
        let mut config = shipped;
        for (from, to) in [
            (
                "admin = { env = \"WATCH_ADMIN_TOKEN\" }".to_string(),
                format!("admin = {{ value = \"{SECRET}\" }}"),
            ),
            (
                "team = { env = \"ACCESS_TEAM_DOMAIN\" }\n".to_string(),
                String::new(),
            ),
            (
                "aud = { env = \"COVTOOLS_ACCESS_AUD\" }\n".to_string(),
                String::new(),
            ),
            (
                "emails = { env = \"ACCESS_ALLOWED_EMAILS\" }\n".to_string(),
                String::new(),
            ),
            (
                "requests = 120".to_string(),
                "requests = 100000".to_string(),
            ),
            (
                "cron = \"* * * * *\"".to_string(),
                "cron = \"0 0 1 1 *\"".to_string(),
            ),
        ] {
            assert!(
                config.contains(&from),
                "the shipped app.toml changed shape: no {from:?}"
            );
            config = config.replace(&from, &to);
        }
        let start = config
            .find("allow = [")
            .expect("the shipped app.toml has an allowlist");
        let end = start + config[start..].find(']').unwrap() + 1;
        config.replace_range(start..end, "allow = [\"http://127.0.0.1:*\"]");
        config
    }

    fn bearer() -> String {
        format!("Authorization: Bearer {SECRET}")
    }

    /// A form, posted as the admin.
    fn form(snap: &mut Snapshot, addr: SocketAddr, name: &str, path: &str, body: &str) -> Answer {
        snap.take(
            addr,
            name,
            "POST",
            path,
            &[&bearer(), "Content-Type: application/x-www-form-urlencoded"],
            body,
        )
    }

    /// The app's own SQL store, as a second connection beside the host's.
    fn store(apps: &Apps) -> Store {
        Store::open(
            &apps.data.join("watch").join("sql.sqlite3"),
            minicloud::config::SqlLimits::default(),
        )
        .unwrap()
    }

    fn exec(store: &mut Store, statement: &str, params: &[String]) {
        store
            .exec(statement, params)
            .unwrap_or_else(|why| panic!("{statement}: {why:?}"));
    }

    fn add(store: &mut Store, id: &str, url: &str, notify: &str) {
        exec(
            store,
            "INSERT INTO target (id, url, notify, added) VALUES (?, ?, ?, ?)",
            &[id.into(), url.into(), notify.into(), OLD.to_string()],
        );
    }

    /// A check as `record` writes one: `status` 0 is one that got no answer.
    fn check(store: &mut Store, target: &str, at: i64, status: i64, ms: i64, why: &str) {
        let ok = status > 0 && status < 400;
        exec(
            store,
            "INSERT INTO checks (target, at, ok, status, ms, why) VALUES (?, ?, ?, ?, ?, ?)",
            &[
                target.into(),
                at.to_string(),
                if ok { "1" } else { "0" }.into(),
                status.to_string(),
                ms.to_string(),
                why.into(),
            ],
        );
    }

    /// What `changed` would have left: the latch, one row a target.
    fn latch(store: &mut Store, target: &str, ok: bool) {
        exec(
            store,
            "INSERT INTO state (target, ok) VALUES (?, ?)",
            &[target.into(), if ok { "1" } else { "0" }.into()],
        );
    }

    /// The targets, each a different line of the dashboard (they are shown
    /// by id, so in this order: `<script>…`, `api`, `busy`, `db`,
    /// `flapped`, `fresh`, `stale`):
    ///
    /// - `api`: up, its last day 3 of 4 good (75.0%), and one check from
    ///   2023 that the day does not count;
    /// - `busy`: 105 checks in 2023 — the history shows the newest 100 —
    ///   and 3 in the day, 2 of them good (66.6%: tenths, truncated);
    /// - `db`: down, the last answer a failure whose text is hostile, its
    ///   url hostile; 1 of 3 good (33.3%);
    /// - `flapped`: down, then up — a target that changed; 1 of 2 (50.0%);
    /// - `fresh`: never checked ("not yet", no day);
    /// - `stale`: checked only in 2023, last down with a status: a last
    ///   answer and a time, and nothing in the day;
    /// - `HOSTILE`: an id the form refuses, put in the store as it is, its
    ///   url and notify hostile too, and one failed check with a hostile
    ///   reason.
    fn plant(apps: &Apps) {
        let store = &mut store(apps);
        let notify = "http://127.0.0.1:8790/hooks/watch";
        add(store, "api", "http://127.0.0.1:8080/api/health", notify);
        check(store, "api", OLD, 200, 31, "");
        check(store, "api", RECENT, 200, 18, "");
        check(store, "api", RECENT + MINUTE, 200, 25, "");
        check(store, "api", RECENT + 2 * MINUTE, 503, 120, "answered 503");
        check(store, "api", RECENT + 3 * MINUTE, 200, 42, "");
        latch(store, "api", true);

        add(store, "busy", "https://covtools.ramda.io/busy", "");
        for i in 0..105 {
            check(store, "busy", OLD + i * MINUTE, 200, 10 + i, "");
        }
        check(store, "busy", RECENT, 204, 3, "");
        check(store, "busy", RECENT + MINUTE, 500, 9, "answered 500");
        check(store, "busy", RECENT + 2 * MINUTE, 301, 4, "");
        latch(store, "busy", true);

        add(
            store,
            "db",
            &format!("http://127.0.0.1:8080/db?q={HOSTILE}"),
            notify,
        );
        check(store, "db", RECENT, 200, 12, "");
        check(store, "db", RECENT + MINUTE, 500, 77, "answered 500");
        check(
            store,
            "db",
            RECENT + 2 * MINUTE,
            0,
            5000,
            &format!("cannot connect: {HOSTILE}"),
        );
        latch(store, "db", false);

        add(store, "flapped", "http://localhost:8790/", notify);
        check(store, "flapped", RECENT, 502, 8, "answered 502");
        check(store, "flapped", RECENT + MINUTE, 200, 7, "");
        latch(store, "flapped", true);

        add(store, "fresh", "http://localhost:8080/fresh", "");

        add(store, "stale", "http://127.0.0.1:8080/stale", "");
        check(store, "stale", OLD, 200, 15, "");
        check(store, "stale", OLD + MINUTE, 404, 6, "answered 404");
        latch(store, "stale", false);

        add(store, HOSTILE, HOSTILE, HOSTILE);
        check(store, HOSTILE, RECENT, 0, 0, HOSTILE);
        latch(store, HOSTILE, false);
    }

    /// Makes the store answer every page with an error, keeping the
    /// checks aside: `checks` becomes a table without the columns the
    /// pages ask for (the app's `CREATE … IF NOT EXISTS` leaves it, and its
    /// index went with the old table, under the same name, so that is left
    /// too).
    fn break_store(apps: &Apps) {
        let mut store = store(apps);
        exec(&mut store, "ALTER TABLE checks RENAME TO checks_kept", &[]);
        exec(
            &mut store,
            "CREATE TABLE checks (id INTEGER PRIMARY KEY)",
            &[],
        );
    }

    fn mend_store(apps: &Apps) {
        let mut store = store(apps);
        exec(&mut store, "DROP TABLE checks", &[]);
        exec(&mut store, "ALTER TABLE checks_kept RENAME TO checks", &[]);
    }

    /// The first event of `GET path`'s server-sent events, kept as
    /// `<name>.html`: from its `data: ` to the blank line that ends it, the
    /// chunk framing around it left out. The stream goes on for the run's
    /// deadline, so the connection is closed once the event is in.
    fn first_event(snap: &mut Snapshot, addr: SocketAddr, name: &str, path: &str) {
        assert!(snap.names.insert(name.to_string()), "`{name}` twice");
        apart();
        let mut stream = std::net::TcpStream::connect(addr).expect("connects");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: {}\r\n\r\n", snap.host).as_bytes())
            .unwrap();
        let mut seen = Vec::new();
        let mut buffer = [0u8; 4096];
        let event = loop {
            let text = String::from_utf8_lossy(&seen).into_owned();
            if let Some(at) = text.find("data: ") {
                if let Some(end) = text[at..].find("\n\n") {
                    break text[at..at + end + 2].to_string();
                }
            }
            match stream.read(&mut buffer) {
                Ok(0) => panic!("the stream closed before its first event:\n{text}"),
                Ok(n) => seen.extend_from_slice(&buffer[..n]),
                Err(why) => panic!("no first event ({why}):\n{text}"),
            }
        };
        let text = String::from_utf8_lossy(&seen).into_owned();
        let status: u16 = text
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status line: {text}"));
        assert!(text.contains("content-type: text/event-stream"), "{text}");
        std::fs::write(snap.dir.join(format!("{name}.html")), &event).unwrap();
        snap.manifest
            .push_str(&format!("{name}\tGET\t{path}\t{status}\n"));
    }

    /// A port nothing listens on: one the system just gave out and took
    /// back.
    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    #[ignore = "writes a snapshot of every watch page to $EXAMPLES_SNAPSHOT_DIR/watch; see the module's doc"]
    fn every_page() {
        let mut snap = Snapshot::new("watch", "watch.test");
        let config: &'static str = Box::leak(config().into_boxed_str());
        let apps = apps(&[sample_with("watch", config), sample("hello")]);
        let host = minicloud::Host::start(options(&apps, 4)).unwrap();
        let addr = host.addr;

        // ------------------------------------------------- the pages, empty

        // The first run makes the tables, which `plant` writes into.
        snap.page(addr, "dashboard-empty", "/watch/");
        let events = "/watch/events";
        first_event(&mut snap, addr, "events-empty", events);

        // ---------------------------------------------- the pages, watching

        plant(&apps);
        snap.page(addr, "dashboard", "/watch/");
        first_event(&mut snap, addr, "events", events);
        for id in ["api", "busy", "db", "flapped", "fresh", "stale"] {
            snap.page(
                addr,
                &format!("history-{id}"),
                &format!("/watch/history?target={id}"),
            );
        }
        snap.page(
            addr,
            "history-hostile",
            &format!("/watch/history?target={HOSTILE_ENCODED}"),
        );
        snap.page(addr, "history-unknown", "/watch/history?target=nobody");
        snap.page(
            addr,
            "history-unknown-hostile",
            &format!("/watch/history?target=nobody{HOSTILE_ENCODED}"),
        );
        snap.page(addr, "history-no-target", "/watch/history");
        snap.page(addr, "history-empty-target", "/watch/history?target=");

        // Not a page of the app's: its not-found page, whatever the path.
        for (name, path) in [
            ("not-found", "/watch/nowhere".to_string()),
            ("not-found-history-deeper", "/watch/history/api".to_string()),
            ("not-found-hostile", format!("/watch/{HOSTILE_ENCODED}")),
        ] {
            snap.page(addr, name, &path);
        }
        // `/live` without a WebSocket handshake: the app's `400`.
        snap.page(addr, "live-not-a-websocket", "/watch/live");

        // The host's own paths, asked for by a client: refused.
        for (name, path) in [
            ("cron-check-refused", "/watch/cron/check"),
            ("queue-recheck-refused", "/watch/queue/recheck"),
            ("queue-notify-refused", "/watch/queue/notify"),
            ("prune-refused", "/watch/prune"),
        ] {
            snap.take(addr, name, "POST", path, &[], "api");
        }

        // The store failing: the error page of each page that reads it.
        break_store(&apps);
        snap.page(addr, "dashboard-store-error", "/watch/");
        snap.page(addr, "history-store-error", "/watch/history?target=api");
        mend_store(&apps);
        snap.page(addr, "dashboard-mended", "/watch/");

        // Nothing has run but what was asked for.
        assert_eq!(count(&host, "watch", "trigger_runs"), 0);
        assert_eq!(count(&host, "watch", "deferred_runs"), 0);
        assert_eq!(count(&host, "watch", "queue_sent"), 0);

        // ------------------------------------------------- the forms, refused

        let add = "/watch/targets";
        for (name, path) in [
            ("targets-unauthorized", add),
            (
                "targets-remove-unauthorized",
                "/watch/targets/remove?target=api",
            ),
            ("check-unauthorized", "/watch/check?target=api"),
        ] {
            snap.take(addr, name, "POST", path, &[], "id=x&url=http%3A%2F%2Fx");
        }
        snap.take(
            addr,
            "targets-wrong-token",
            "POST",
            add,
            &["Authorization: Bearer wrong"],
            "id=x&url=http%3A%2F%2Fx",
        );
        let long = "a".repeat(65);
        for (name, body) in [
            ("targets-empty", String::new()),
            ("targets-no-id", "url=http%3A%2F%2F127.0.0.1%2F".to_string()),
            ("targets-no-url", "id=nourl".to_string()),
            ("targets-blank", "id=+++&url=+".to_string()),
            (
                "targets-id-spaces",
                "id=two+words&url=http%3A%2F%2Fx".to_string(),
            ),
            (
                "targets-id-too-long",
                format!("id={long}&url=http%3A%2F%2Fx"),
            ),
            (
                "targets-id-hostile",
                format!("id={HOSTILE_ENCODED}&url=http%3A%2F%2Fx"),
            ),
        ] {
            let answer = form(&mut snap, addr, name, add, &body);
            assert_eq!(answer.status, 400, "{name}: {}", answer.body);
        }
        for (name, path) in [
            ("check-unknown", "/watch/check?target=nobody".to_string()),
            ("check-no-target", "/watch/check".to_string()),
            (
                "check-hostile-unknown",
                format!("/watch/check?target=nobody{HOSTILE_ENCODED}"),
            ),
        ] {
            let answer = form(&mut snap, addr, name, &path, "");
            assert_eq!(answer.status, 404, "{name}: {}", answer.body);
        }

        // ----------------------------------------- the forms, that go through
        //
        // From here checks run — the ones these ask for and the ones adding
        // a target defers — so no page is taken after.

        let up = format!("http://{addr}/hello/");
        let down = format!("http://127.0.0.1:{}/", closed_port());
        for (name, body) in [
            ("targets-up", format!("id=up&url={}", form_value(&up))),
            (
                "targets-down",
                format!(
                    "id=down&url={}&notify={}",
                    form_value(&down),
                    form_value(&up)
                ),
            ),
            (
                "targets-not-allowed",
                "id=elsewhere&url=https%3A%2F%2Fexample.com%2F".to_string(),
            ),
            ("targets-not-a-url", "id=nonsense&url=not+a+url".to_string()),
            (
                "targets-url-hostile",
                format!("id=hostile-url&url={HOSTILE_ENCODED}&notify={HOSTILE_ENCODED}"),
            ),
            ("targets-again", format!("id=+up+&url={}", form_value(&up))),
        ] {
            let answer = form(&mut snap, addr, name, add, &body);
            assert_eq!(answer.status, 201, "{name}: {}", answer.body);
        }
        for (name, id, status) in [
            ("check-up", "up", 200),
            ("check-down", "down", 200),
            ("check-not-allowed", "elsewhere", 200),
            ("check-not-a-url", "nonsense", 200),
            ("check-hostile", HOSTILE_ENCODED, 200),
        ] {
            let answer = form(
                &mut snap,
                addr,
                name,
                &format!("/watch/check?target={id}"),
                "",
            );
            assert_eq!(answer.status, status, "{name}: {}", answer.body);
        }
        for (name, id) in [
            ("targets-remove", "up"),
            ("targets-remove-unknown", "nobody"),
            ("targets-remove-hostile", HOSTILE_ENCODED),
            ("targets-remove-none", ""),
        ] {
            let path = if id.is_empty() {
                "/watch/targets/remove".to_string()
            } else {
                format!("/watch/targets/remove?target={id}")
            };
            let answer = form(&mut snap, addr, name, &path, "");
            assert_eq!(answer.status, 200, "{name}: {}", answer.body);
        }

        assert_eq!(count(&host, "watch", "trigger_runs"), 0);
        snap.finish();
    }
}
