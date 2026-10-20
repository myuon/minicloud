//! What the integration tests share: an apps directory assembled per test, a
//! host started in-process on a free port, and a small HTTP client.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use minicloud::{Host, ServeOptions};
use serde_json::Value as Json;

/// The bundled apps, at the repository's root: only `admin`.
pub fn bundled() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps")
}

/// The example apps, at the repository's root.
pub fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// A shipped app's directory: the bundled `admin`, or an example.
pub fn sample_dir(name: &str) -> PathBuf {
    match name {
        "admin" => bundled().join(name),
        _ => examples().join(name),
    }
}

/// The apps only the tests use.
pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/apps")
}

/// A temporary apps directory, removed when dropped.
pub struct Apps {
    pub root: PathBuf,
    /// The data directory hosts over these apps keep their state in; it
    /// outlives a host, so a second host over the same apps sees it.
    pub data: PathBuf,
}

impl Drop for Apps {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

/// The admin token every test host is started with.
pub const ADMIN_TOKEN: &str = "test-admin-token";

static NEXT: AtomicU64 = AtomicU64::new(0);

/// One app of a test's apps directory: `name`, whose `.cove` files are
/// copied from `from`, and whose `app.toml` is `config` — or the copied
/// one's, when `None`.
pub struct AppSpec<'a> {
    pub name: &'a str,
    pub from: PathBuf,
    pub config: Option<&'a str>,
}

/// A sample app, as it ships.
pub fn sample(name: &str) -> AppSpec<'_> {
    AppSpec {
        name,
        from: sample_dir(name),
        config: None,
    }
}

/// A sample app under a config of the test's own.
pub fn sample_with<'a>(name: &'a str, config: &'a str) -> AppSpec<'a> {
    AppSpec {
        name,
        from: sample_dir(name),
        config: Some(config),
    }
}

/// A test-only app, as it ships.
pub fn fixture(name: &str) -> AppSpec<'_> {
    AppSpec {
        name,
        from: fixtures().join(name),
        config: None,
    }
}

/// Builds an apps directory holding `specs`.
pub fn apps(specs: &[AppSpec]) -> Apps {
    let root = std::env::temp_dir().join(format!(
        "minicloud-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    for spec in specs {
        let dir = root.join(spec.name);
        std::fs::create_dir_all(&dir).unwrap();
        for entry in std::fs::read_dir(&spec.from).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "cove") {
                // Renamed to the app's module where the app is a copy under
                // another name: the main module is the directory's.
                let file = path.file_name().unwrap().to_str().unwrap();
                let source_module = spec.from.file_name().unwrap().to_str().unwrap();
                let file = file.replacen(source_module, spec.name, 1);
                let text = std::fs::read_to_string(&path).unwrap();
                std::fs::write(dir.join(file), text).unwrap();
            } else if path
                .file_name()
                .is_some_and(|name| name == minicloud::assets::DIR)
            {
                // The app's static files, copied whole: the host answers
                // them, so a test over an app sees what it ships.
                copy_tree(&path, &dir.join(path.file_name().unwrap()));
            } else if path.is_dir() {
                // A module of the app's own, copied as it is.
                let module = dir.join(path.file_name().unwrap());
                std::fs::create_dir_all(&module).unwrap();
                for file in std::fs::read_dir(&path).unwrap() {
                    let file = file.unwrap().path();
                    if file.extension().is_some_and(|e| e == "cove") {
                        std::fs::copy(&file, module.join(file.file_name().unwrap())).unwrap();
                    }
                }
            }
        }
        let config = match spec.config {
            Some(config) => config.to_string(),
            None => std::fs::read_to_string(spec.from.join("app.toml")).unwrap(),
        };
        std::fs::write(dir.join("app.toml"), config).unwrap();
    }
    let data = root.with_extension("data");
    let _ = std::fs::remove_dir_all(&data);
    Apps { root, data }
}

/// Copies `from` to `to`, directories and all.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let into = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_tree(&path, &into);
        } else {
            std::fs::copy(&path, &into).unwrap();
        }
    }
}

/// The options a test host starts with: a free port, `workers` workers, the
/// default slice, quiet.
pub fn options(apps: &Apps, workers: usize) -> ServeOptions {
    let mut options = ServeOptions::new(&apps.root);
    options.addr = "127.0.0.1:0".to_string();
    options.workers = workers;
    options.quiet = true;
    options.data = Some(apps.data.clone());
    options.admin = Some("127.0.0.1:0".to_string());
    options.admin_token = Some(ADMIN_TOKEN.to_string());
    // `COVE_HOST_TEST_BACKEND=vm` runs the suite on the encoded VM where the
    // native tier would otherwise be chosen.
    if let Ok(backend) = std::env::var("COVE_HOST_TEST_BACKEND") {
        options.backend = backend.parse().expect("a backend: auto, vm or native");
    }
    options
}

/// Starts a host over `apps`.
pub fn start(apps: &Apps, workers: usize) -> Host {
    Host::start(options(apps, workers)).expect("the host starts")
}

/// An HTTP response, as the tests read it.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The body as text: lossy, for the apps that answer text.
    pub body: String,
    /// The body as it came over the connection, for a static file that is
    /// not UTF-8 ([`minicloud::assets`]).
    pub bytes: Vec<u8>,
}

impl Answer {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Sends `head` (with `Connection: close` added) and `body`, and reads the
/// whole answer.
pub fn send_raw(addr: SocketAddr, raw: &[u8]) -> Answer {
    let mut stream = TcpStream::connect(addr).expect("connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    stream.write_all(raw).unwrap();
    let mut bytes = Vec::new();
    let _ = stream.read_to_end(&mut bytes);
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Answer {
    // Split on the bytes, not on the text: a body that is not UTF-8 would
    // not survive a lossy conversion of the whole answer.
    let at = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("not an HTTP response: {:?}", String::from_utf8_lossy(bytes)));
    let head = String::from_utf8_lossy(&bytes[..at]).into_owned();
    let body = &bytes[at + 4..];
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {head:?}"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(body).into_owned(),
        bytes: body.to_vec(),
    }
}

/// `GET path`.
pub fn get(addr: SocketAddr, path: &str) -> Answer {
    send_raw(
        addr,
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes(),
    )
}

/// `POST path` with `body`.
pub fn post(addr: SocketAddr, path: &str, body: &str) -> Answer {
    send_raw(
        addr,
        format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
}

/// One app's entry in the host's stats.
pub fn app_stats(host: &Host, app: &str) -> Json {
    host.stats()["apps"][app].clone()
}

/// A counter of one app's stats, by its JSON path (`"errors.deadline"`).
pub fn count(host: &Host, app: &str, path: &str) -> u64 {
    let mut value = app_stats(host, app);
    for key in path.split('.') {
        value = value[key].clone();
    }
    value
        .as_u64()
        .unwrap_or_else(|| panic!("`{app}.{path}` is not a count: {value}"))
}

/// Waits until `condition` holds, polling; panics with `what` after a
/// generous bound. Not a timing assertion: the bound is there so that a
/// broken host fails the test instead of hanging it.
pub fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(60);
    while !condition() {
        assert!(Instant::now() < until, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A request timeline of fixed times, for the admin's Live, Timeline and
/// request pages ([`minicloud::timeline::Recorder::fixed`]): two workers, a
/// clock standing at 400 s, and four apps' requests — most of them in the
/// last 8 s, one at 330 s and one at 200 s so that the windows differ.
///
/// | id | app | what it did |
/// | --- | --- | --- |
/// | 80, 90 | notes, hello | answered long ago |
/// | 101 | hello | ran on #0, answered |
/// | 102 | slow | parked on `timer.sleep`, resumed on #1 (moved) |
/// | 103 | proxy | parked on `fetch`, resumed on the same worker |
/// | 104 | notes | yielded twice, continued on #0 and then on #1 (moved) |
/// | 105 | slow | parked on `fetch` until its deadline: 504 |
/// | 106 | hello | parked on `timer.sleep`, cancelled: 499 |
/// | 107 | hello | turned away at its queue: 503 `queue_full` |
/// | 108 | notes | turned away before it: 429 `rate_limited` |
/// | 109 | slow | in flight, parked on `timer.sleep` |
/// | 110 | hello | in flight, running on #1 |
/// | 111 | notes | in flight, waiting for a worker |
///
/// The trackers of the three in flight are answered with the recorder, and
/// must be kept as long as it is looked at: a dropped one ends its request.
pub fn fixed_timeline() -> (
    minicloud::timeline::Recorder,
    Vec<minicloud::timeline::Tracker>,
) {
    use minicloud::timeline::{Began, By, Recorder};
    const MS: u64 = 1_000_000;
    let rec = Recorder::fixed(2, 1_790_000_000_000, 400_000 * MS);
    let hello = rec.app("hello");
    let notes = rec.app("notes");
    let slow = rec.app("slow");
    let proxy = rec.app("proxy");
    let at = |ms: u64| 392_000 * MS + ms * MS;
    let ok = |t: &mut minicloud::timeline::Tracker, end: u64, worker_ms: u64| {
        // A run of a tenth of a second or more collected, one per tenth;
        // every run allocated.
        t.meter(minicloud::timeline::Meter {
            instructions: 1_000 * worker_ms + 345,
            heap_words: 120 + worker_ms,
            gc_collections: worker_ms / 100,
            allocated_words: 4_096 * worker_ms,
        });
        t.end(end, 200, None, Duration::from_millis(worker_ms));
    };

    let mut old = rec.admit(80, notes, "GET", "/notes/old", 200_000 * MS, 200_000 * MS);
    old.run(200_002 * MS, 1, Began::Start);
    ok(&mut old, 200_010 * MS, 8);
    let mut old = rec.admit(90, hello, "GET", "/", 330_000 * MS, 330_000 * MS);
    old.run(330_005 * MS, 0, Began::Start);
    ok(&mut old, 330_020 * MS, 15);

    let mut t = rec.admit(101, hello, "GET", "/", at(100), at(100));
    t.run(at(200), 0, Began::Start);
    ok(&mut t, at(260), 60);

    let mut t = rec.admit(102, slow, "GET", "/slow/sleep", at(300), at(300));
    t.run(at(310), 0, Began::Start);
    t.park(at(330), "timer.sleep");
    t.ready(at(1330), By::Answer);
    t.run(at(1350), 1, Began::Resume);
    ok(&mut t, at(1380), 50);

    let mut t = rec.admit(103, proxy, "GET", "/proxy/fetch", at(500), at(500));
    t.run(at(520), 1, Began::Start);
    t.park(at(540), "fetch");
    t.ready(at(1540), By::Answer);
    t.run(at(1560), 1, Began::Resume);
    ok(&mut t, at(1600), 60);

    let mut t = rec.admit(104, notes, "POST", "/notes/crunch", at(1700), at(1700));
    t.run(at(1710), 0, Began::Start);
    t.yielded(at(2710));
    t.run(at(2730), 0, Began::Continue);
    t.yielded(at(3730));
    t.run(at(3740), 1, Began::Continue);
    ok(&mut t, at(4000), 2260);

    let mut t = rec.admit(105, slow, "GET", "/slow/forever", at(2000), at(2000));
    t.run(at(2010), 1, Began::Start);
    t.park(at(2030), "fetch");
    t.ready(at(4030), By::Deadline);
    t.run(at(4040), 1, Began::Resume);
    t.end(at(4050), 504, Some("deadline"), Duration::from_millis(30));

    let mut t = rec.admit(106, hello, "GET", "/hello/gone", at(2500), at(2500));
    t.run(at(2520), 1, Began::Start);
    t.park(at(2540), "timer.sleep");
    t.ready(at(3000), By::Cancel);
    t.run(at(3010), 0, Began::Resume);
    t.end(at(3020), 499, Some("cancelled"), Duration::from_millis(30));

    let mut t = rec.admit(107, hello, "GET", "/", at(2600), at(2600));
    t.reject(at(2600), 503, "queue_full");
    rec.rejected(
        108,
        notes,
        "GET",
        "/notes/x",
        at(2700),
        at(2700),
        429,
        "rate_limited",
    );

    let parked = rec.admit(109, slow, "GET", "/slow/sleep", at(7000), at(7000));
    parked.run(at(7010), 0, Began::Start);
    parked.park(at(7030), "timer.sleep");
    let running = rec.admit(110, hello, "GET", "/hello/spin", at(7500), at(7500));
    running.run(at(7600), 1, Began::Start);
    let waiting = rec.admit(111, notes, "GET", "/notes/", at(7800), at(7800));
    (rec, vec![parked, running, waiting])
}

/// A request timeline of fixed times like a closed-loop load's, for the
/// Timeline page's merged bars ([`minicloud::timeline::Recorder::fixed`]):
/// two workers, a clock standing at 400 s, and
///
/// - from 300 s to 320 s, `crunch` alone on worker #0: an 8 ms run every
///   20 ms, the worker busy two fifths of the time;
/// - from 330 s to 390 s, each worker taking one request after another,
///   3 ms apart: `hello` (a 1 ms run), `crunch` (8 ms) and `slow` (a 1 ms run,
///   a 30 ms `timer.sleep`, and a 1 ms run on the same worker when it is
///   next free), in turn.
///
/// At five minutes a pixel is more than the 3 ms between runs, so each
/// worker's runs merge into a few bars; at a second, they do not.
pub fn merged_timeline() -> minicloud::timeline::Recorder {
    use minicloud::timeline::{Began, By, Recorder, Tracker};
    const MS: u64 = 1_000_000;
    let rec = Recorder::fixed(2, 1_790_000_000_000, 400_000 * MS);
    let hello = rec.app("hello");
    let crunch = rec.app("crunch");
    let slow = rec.app("slow");
    let mut id = 1_000;
    let done = |t: &mut Tracker, at: u64, worker_ms: u64| {
        t.end(at, 200, None, Duration::from_millis(worker_ms));
    };
    for k in 0..1_000 {
        let at = 300_000 * MS + k * 20 * MS;
        let mut t = rec.admit(id, crunch, "POST", "/crunch/", at, at);
        t.run(at + MS, 0, Began::Start);
        done(&mut t, at + 9 * MS, 8);
        id += 1;
    }
    // Both workers stepped together, whichever is earlier first: the
    // recorder's rings are in arrival order, as on a host.
    struct Worker {
        at: u64,
        turn: u32,
        // Its sleeping requests, each with when its sleep ends.
        sleeping: Vec<(Tracker, u64)>,
    }
    let mut workers: Vec<Worker> = (0..2)
        .map(|worker| Worker {
            at: 330_000 * MS + worker * MS,
            turn: 0,
            sleeping: Vec::new(),
        })
        .collect();
    loop {
        let worker = if workers[0].at <= workers[1].at { 0 } else { 1 };
        let w = &mut workers[worker];
        let at = w.at;
        if at >= 390_000 * MS {
            break;
        }
        if let Some(k) = w.sleeping.iter().position(|(_, ready)| *ready <= at) {
            let (mut t, _) = w.sleeping.remove(k);
            t.run(at, worker, Began::Resume);
            done(&mut t, at + MS, 2);
            w.at += 4 * MS;
            continue;
        }
        let (app, path, length) = match w.turn % 3 {
            0 => (hello, "/hello/", 1),
            1 => (crunch, "/crunch/", 8),
            _ => (slow, "/slow/sleep", 1),
        };
        w.turn += 1;
        let mut t = rec.admit(id, app, "GET", path, at, at);
        id += 1;
        t.run(at, worker, Began::Start);
        if app == slow {
            t.park(at + MS, "timer.sleep");
            t.ready(at + 31 * MS, By::Answer);
            w.sleeping.push((t, at + 31 * MS));
        } else {
            done(&mut t, at + length * MS, length);
        }
        w.at += (length + 3) * MS;
    }
    for (worker, w) in workers.into_iter().enumerate() {
        for (mut t, ready) in w.sleeping {
            t.run(ready, worker, Began::Resume);
            done(&mut t, ready + MS, 2);
        }
    }
    rec
}

/// A load run's status of fixed figures, for the admin's Load page
/// ([`minicloud::Host::set_load_status`]): a mixed run at 50 a second, `crunch`
/// missing.
pub fn fixed_load_run(state: &str) -> minicloud::load::RunStatus {
    minicloud::load::RunStatus {
        mix: "mixed".into(),
        rate: 50,
        connections: 4,
        seconds: 20,
        state: state.into(),
        started_unix_ms: 1_700_000_000_000,
        elapsed_ms: 12_345,
        sent: 600,
        ok: 580,
        client_errors: 0,
        too_many: 9,
        unavailable: 0,
        server_errors: 1,
        other: 0,
        errors: 0,
        p50_us: Some(2_500),
        p99_us: Some(181_000),
        targets: vec!["hello".into(), "crunch".into(), "slow".into()],
        not_deployed: vec!["crunch".into()],
    }
}
