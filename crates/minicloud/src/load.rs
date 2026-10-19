//! Load the host puts on its own apps, so that the operations views — Live
//! and Timeline — have something to show.
//!
//! Two users: `minicloud-load`, the command-line generator of the
//! performance check, which shares [`mix`], [`request`], [`Conn`] and
//! [`percentile`] from here; and the host itself ([`Runner`]), whose admin
//! page and `POST /_host/load` start a run of real HTTP requests over
//! loopback to its own public listener — through the router, the queues and
//! the workers like any other request, so they appear in the timeline.
//!
//! A run is one of four [`Mix`]es of the example apps — `light` (`hello`),
//! `cpu` (`crunch`), `wait` (`slow`) and `mixed` (the three in turn) — open
//! loop at `rate` requests a second (latency from when each was due, as
//! `minicloud-load --rate`), or closed loop (`rate` 0) on `connections`
//! kept-alive connections, for `seconds`. One at a time, bounded by
//! [`MAX_RATE`], [`MAX_CONNECTIONS`] and [`MAX_SECONDS`]. The connections
//! are tasks on the host's I/O runtime: a run holds no Cove worker of its
//! own, only the ones its requests are served on.
//!
//! An app a mix names that the host does not have answers 404 `no app named`;
//! the run stops asking it, says so in its status, and ends early if no
//! target is left.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

// ------------------------------------------------------------ shared with minicloud-load

/// The weights `--mix` names, by app: `hello`, `crunch`, `crunch20k`,
/// `slow`, `proxy`, `algo`, `algo-sat`, `algo-anneal`, `cpu-io`, or weights
/// like `hello=3,crunch=1`.
pub fn mix(text: &str) -> Result<Vec<(&'static str, u32)>, String> {
    let text = match text {
        "cpu-io" => "crunch=35,slow=30,proxy=10,hello=25",
        other => other,
    };
    text.split(',')
        .map(|part| {
            let (name, weight) = part.split_once('=').unwrap_or((part, "1"));
            let name: &'static str = match name {
                "hello" => "hello",
                "crunch" => "crunch",
                "crunch20k" => "crunch20k",
                "slow" => "slow",
                "proxy" => "proxy",
                "algo" => "algo",
                "algo-sat" => "algo-sat",
                "algo-anneal" => "algo-anneal",
                other => {
                    return Err(format!(
                        "`{other}` is not hello, crunch, crunch20k, slow, proxy, algo, algo-sat, \
                         algo-anneal or cpu-io"
                    ))
                }
            };
            let weight = weight
                .parse()
                .map_err(|e| format!("a whole-number weight: {e}"))?;
            Ok((name, weight))
        })
        .collect()
}

/// Which app request `i` asks, and its path: chosen from `i` by the weights,
/// so the same command asks the same sequence.
pub fn request(i: u64, mix: &[(&'static str, u32)], port: u16) -> (&'static str, String) {
    let total: u64 = mix.iter().map(|(_, w)| u64::from(*w)).sum();
    // A multiplicative hash spreads consecutive indices over the weights.
    let mut pick = i.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 11;
    pick %= total;
    let mut app = mix[0].0;
    for (name, weight) in mix {
        if pick < u64::from(*weight) {
            app = name;
            break;
        }
        pick -= u64::from(*weight);
    }
    (
        match app {
            "crunch20k" => "crunch",
            "algo-sat" | "algo-anneal" => "algo",
            other => other,
        },
        path(app, i, port),
    )
}

/// The path request `i` asks of `name` (a `--mix` name).
pub fn path(name: &str, i: u64, port: u16) -> String {
    match name {
        "hello" => "/hello/?name=load".to_string(),
        "crunch" => format!(
            "/crunch/?n={}",
            [20000, 50000, 100000, 150000][(i % 4) as usize]
        ),
        "crunch20k" => "/crunch/?n=20000".to_string(),
        "slow" => "/slow/?ms=60&times=3".to_string(),
        "algo" => "/algo/matching?example=large&algorithm=augmenting&part=result".to_string(),
        "algo-sat" => "/algo/sat?example=hard&part=result".to_string(),
        "algo-anneal" => "/algo/anneal?example=cooling&part=result".to_string(),
        _ => format!("/proxy/?url=http://127.0.0.1:{port}/hello/"),
    }
}

/// An answer, as a load generator reads it.
pub struct Answered {
    pub status: u16,
    /// The body, which only a 404 is looked at for.
    pub body: Vec<u8>,
}

/// A kept-alive connection.
pub struct Conn {
    addr: String,
    /// A header line sent with every request (`name: value\r\n`), or `""`.
    extra: &'static str,
    stream: Option<BufReader<TcpStream>>,
}

impl Conn {
    /// A connection to `addr`, opened at the first request.
    pub fn new(addr: impl Into<String>) -> Conn {
        Conn {
            addr: addr.into(),
            extra: "",
            stream: None,
        }
    }

    /// Sends `line` (`name: value\r\n`) with every request.
    pub fn with_header(mut self, line: &'static str) -> Conn {
        self.extra = line;
        self
    }

    /// Asks `path`; the status, or `None` for a connection that failed (it is
    /// reopened for the next request).
    pub async fn ask(&mut self, path: &str) -> Option<u16> {
        self.answer(path).await.map(|answered| answered.status)
    }

    /// Asks `path`; the answer, or `None` as [`Conn::ask`].
    pub async fn answer(&mut self, path: &str) -> Option<Answered> {
        if self.stream.is_none() {
            let stream = TcpStream::connect(&self.addr).await.ok()?;
            let _ = stream.set_nodelay(true);
            self.stream = Some(BufReader::new(stream));
        }
        let answer = Self::exchange(self.stream.as_mut()?, path, self.extra).await;
        match answer {
            Some((answered, close)) => {
                if close {
                    self.stream = None;
                }
                Some(answered)
            }
            None => {
                self.stream = None;
                None
            }
        }
    }

    async fn exchange(
        stream: &mut BufReader<TcpStream>,
        path: &str,
        extra: &str,
    ) -> Option<(Answered, bool)> {
        let head = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{extra}\r\n");
        stream.get_mut().write_all(head.as_bytes()).await.ok()?;
        let mut line = String::new();
        stream.read_line(&mut line).await.ok()?;
        let status: u16 = line.split(' ').nth(1)?.parse().ok()?;
        let mut length = 0usize;
        let mut close = false;
        loop {
            line.clear();
            if stream.read_line(&mut line).await.ok()? == 0 {
                return None;
            }
            if line == "\r\n" {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                length = value.trim().parse().ok()?;
            }
            if lower.starts_with("connection:") && lower.contains("close") {
                close = true;
            }
        }
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.ok()?;
        Some((Answered { status, body }, close))
    }
}

/// The `p` quantile of `sorted`, in milliseconds; NaN for none.
pub fn percentile(sorted: &[Duration], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let at = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[at].as_secs_f64() * 1e3
}

// ------------------------------------------------------------ the host's own runs

/// The most requests a second an open-loop run may ask for.
pub const MAX_RATE: u32 = 500;
/// The most connections a run may keep.
pub const MAX_CONNECTIONS: u32 = 64;
/// The longest a run may last.
pub const MAX_SECONDS: u32 = 60;
/// How long a run that is out of time waits for the requests it has in
/// flight before it drops them.
pub const GRACE: Duration = Duration::from_secs(10);
/// The header every request of a run carries, so that it can be told apart.
pub const HEADER: &str = "x-minicloud-load";
const HEADER_LINE: &str = "x-minicloud-load: 1\r\n";

/// What a run asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mix {
    /// `hello`: next to no work, the host's own overhead.
    Light,
    /// `crunch`: CPU, n = 20,000 to 150,000 in turn; holds a worker.
    Cpu,
    /// `slow`: three 60 ms sleeps, parked; holds no worker while it waits.
    Wait,
    /// The three in turn.
    Mixed,
}

impl Mix {
    pub fn name(self) -> &'static str {
        match self {
            Mix::Light => "light",
            Mix::Cpu => "cpu",
            Mix::Wait => "wait",
            Mix::Mixed => "mixed",
        }
    }

    /// The apps it asks, in the order it asks them.
    pub fn targets(self) -> &'static [&'static str] {
        match self {
            Mix::Light => &["hello"],
            Mix::Cpu => &["crunch"],
            Mix::Wait => &["slow"],
            Mix::Mixed => &["hello", "crunch", "slow"],
        }
    }
}

impl std::str::FromStr for Mix {
    type Err = String;
    fn from_str(text: &str) -> Result<Mix, String> {
        match text {
            "light" => Ok(Mix::Light),
            "cpu" => Ok(Mix::Cpu),
            "wait" => Ok(Mix::Wait),
            "mixed" => Ok(Mix::Mixed),
            other => Err(format!(
                "mix `{other}` is none of light, cpu, wait or mixed"
            )),
        }
    }
}

/// What a run is asked to do, within the caps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub mix: Mix,
    /// Requests a second, open loop; 0 is closed loop.
    pub rate: u32,
    pub connections: u32,
    pub seconds: u32,
}

impl Params {
    /// Checks each figure against its cap; the first that is out is the
    /// error.
    pub fn new(mix: &str, rate: i64, connections: i64, seconds: i64) -> Result<Params, String> {
        let mix = mix.trim().parse()?;
        if !(0..=i64::from(MAX_RATE)).contains(&rate) {
            return Err(format!(
                "rate is 0 (closed loop) to {MAX_RATE} requests a second, not {rate}"
            ));
        }
        if !(1..=i64::from(MAX_CONNECTIONS)).contains(&connections) {
            return Err(format!(
                "connections is 1 to {MAX_CONNECTIONS}, not {connections}"
            ));
        }
        if !(1..=i64::from(MAX_SECONDS)).contains(&seconds) {
            return Err(format!("seconds is 1 to {MAX_SECONDS}, not {seconds}"));
        }
        Ok(Params {
            mix,
            rate: rate as u32,
            connections: connections as u32,
            seconds: seconds as u32,
        })
    }

    /// From `POST /_host/load`'s body: `{"mix", "rate", "connections",
    /// "seconds"}`, each optional — `light`, 0, 8 and 10 by default.
    pub fn from_json(body: &[u8]) -> Result<Params, String> {
        let value: Json = if body.iter().all(u8::is_ascii_whitespace) {
            json!({})
        } else {
            serde_json::from_slice(body).map_err(|e| format!("the body is not JSON: {e}"))?
        };
        if !value.is_object() {
            return Err("the body is a JSON object".to_string());
        }
        let number = |key: &str, default: i64| match value.get(key) {
            None | Some(Json::Null) => Ok(default),
            Some(n) => n
                .as_i64()
                .ok_or_else(|| format!("`{key}` is a whole number")),
        };
        let mix = match value.get("mix") {
            None | Some(Json::Null) => "light",
            Some(text) => text.as_str().ok_or("`mix` is a string")?,
        };
        Params::new(
            mix,
            number("rate", 0)?,
            number("connections", 8)?,
            number("seconds", 10)?,
        )
    }

    /// Which app request `i` asks, skipping the ones in `missing` (by their
    /// place in [`Mix::targets`]); `None` when every target is missing.
    fn target(&self, i: u64, missing: &[AtomicBool]) -> Option<(usize, &'static str)> {
        let targets = self.mix.targets();
        let n = targets.len();
        (0..n)
            .map(|k| ((i as usize) + k) % n)
            .find(|at| !missing[*at].load(Ordering::Relaxed))
            .map(|at| (at, targets[at]))
    }
}

/// How long requests took, for a run's p50 and p99: counts in buckets 1.8%
/// wide from a microsecond to well past a minute, so that a quantile is
/// within 2% and a run of any length costs the same few kilobytes.
struct Quantiles {
    buckets: Vec<AtomicU64>,
}

/// Buckets per factor of e: each is e^(1/57), 1.8%, wider than the last.
const PER_E: f64 = 57.0;
const QUANTILE_BUCKETS: usize = 1100;

impl Quantiles {
    fn new() -> Quantiles {
        Quantiles {
            buckets: (0..QUANTILE_BUCKETS).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn observe(&self, micros: u64) {
        let at = (((micros.max(1)) as f64).ln() * PER_E) as usize;
        self.buckets[at.min(QUANTILE_BUCKETS - 1)].fetch_add(1, Ordering::Relaxed);
    }

    /// The `q` quantile in microseconds, as the top of its bucket; `None`
    /// for nothing observed.
    fn quantile_us(&self, q: f64) -> Option<u64> {
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return None;
        }
        let rank = ((total as f64 * q).ceil() as u64).clamp(1, total);
        let mut seen = 0;
        for (at, count) in counts.iter().enumerate() {
            seen += count;
            if seen >= rank {
                return Some(((at + 1) as f64 / PER_E).exp().round() as u64);
            }
        }
        None
    }
}

/// What a run has counted so far.
struct Tally {
    sent: AtomicU64,
    ok: AtomicU64,
    client_errors: AtomicU64,
    too_many: AtomicU64,
    unavailable: AtomicU64,
    server_errors: AtomicU64,
    other: AtomicU64,
    errors: AtomicU64,
    /// By the place of each target in [`Mix::targets`]: the host has no app
    /// of that name.
    missing: [AtomicBool; 3],
    latency: Quantiles,
}

impl Tally {
    fn new() -> Tally {
        Tally {
            sent: AtomicU64::new(0),
            ok: AtomicU64::new(0),
            client_errors: AtomicU64::new(0),
            too_many: AtomicU64::new(0),
            unavailable: AtomicU64::new(0),
            server_errors: AtomicU64::new(0),
            other: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            missing: Default::default(),
            latency: Quantiles::new(),
        }
    }

    fn count(&self, status: u16) {
        let counter = match status {
            200..=299 => &self.ok,
            429 => &self.too_many,
            400..=499 => &self.client_errors,
            503 => &self.unavailable,
            500..=599 => &self.server_errors,
            _ => &self.other,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Whether an answer is the host's own 404 for an app it does not have
/// ([`crate::server`]'s `no app named`), and not one of the app's.
fn not_deployed(answered: &Answered) -> bool {
    answered.status == 404 && answered.body.starts_with(b"no app named `")
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// Its time was up (or no target was left).
    Finished,
    /// Somebody stopped it.
    Stopped,
}

/// One run.
struct Run {
    params: Params,
    started: Instant,
    started_unix_ms: u64,
    tally: Arc<Tally>,
    stop: tokio::sync::watch::Sender<bool>,
    /// Set by [`Runner::stop`].
    stopped: AtomicBool,
    /// When it ended, and how; `None` while it runs.
    ended: Mutex<Option<(Duration, Ended)>>,
}

impl Run {
    fn status(&self) -> RunStatus {
        let tally = &self.tally;
        let ended = *self.ended.lock().unwrap_or_else(|p| p.into_inner());
        let (elapsed, state) = match ended {
            Some((elapsed, Ended::Finished)) => (elapsed, "finished"),
            Some((elapsed, Ended::Stopped)) => (elapsed, "stopped"),
            None => (self.started.elapsed(), "running"),
        };
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let targets = self.params.mix.targets();
        RunStatus {
            mix: self.params.mix.name().to_string(),
            rate: self.params.rate,
            connections: self.params.connections,
            seconds: self.params.seconds,
            state: state.to_string(),
            started_unix_ms: self.started_unix_ms,
            elapsed_ms: elapsed.as_millis() as u64,
            sent: get(&tally.sent),
            ok: get(&tally.ok),
            client_errors: get(&tally.client_errors),
            too_many: get(&tally.too_many),
            unavailable: get(&tally.unavailable),
            server_errors: get(&tally.server_errors),
            other: get(&tally.other),
            errors: get(&tally.errors),
            p50_us: tally.latency.quantile_us(0.5),
            p99_us: tally.latency.quantile_us(0.99),
            targets: targets.iter().map(|t| t.to_string()).collect(),
            not_deployed: targets
                .iter()
                .zip(&tally.missing)
                .filter(|(_, missing)| missing.load(Ordering::Relaxed))
                .map(|(t, _)| t.to_string())
                .collect(),
        }
    }
}

/// What a run has done, as `GET /_host/load` and `host.loadStatus` say it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStatus {
    pub mix: String,
    pub rate: u32,
    pub connections: u32,
    pub seconds: u32,
    /// `running`, `finished` or `stopped`.
    pub state: String,
    pub started_unix_ms: u64,
    pub elapsed_ms: u64,
    /// Requests sent, including those still unanswered.
    pub sent: u64,
    /// 2xx.
    pub ok: u64,
    /// 4xx but 429.
    pub client_errors: u64,
    /// 429: a queue full, or a rate limit.
    pub too_many: u64,
    /// 503: disabled, shutting down, or the host's in-flight limit.
    pub unavailable: u64,
    /// 5xx but 503: a 504 at the deadline, a 500 for a failed run.
    pub server_errors: u64,
    /// Any other status.
    pub other: u64,
    /// No answer: a connection refused, reset or cut short.
    pub errors: u64,
    /// Of the answered, from when each was due (open loop) or sent (closed).
    pub p50_us: Option<u64>,
    pub p99_us: Option<u64>,
    /// The apps the mix asks.
    pub targets: Vec<String>,
    /// Those the host answered `no app named` for.
    pub not_deployed: Vec<String>,
}

impl RunStatus {
    /// Every request answered, whatever its status.
    pub fn answered(&self) -> u64 {
        self.ok
            + self.client_errors
            + self.too_many
            + self.unavailable
            + self.server_errors
            + self.other
    }

    pub fn to_json(&self) -> Json {
        json!({
            "mix": self.mix,
            "rate": self.rate,
            "connections": self.connections,
            "seconds": self.seconds,
            "state": self.state,
            "started_unix_ms": self.started_unix_ms,
            "elapsed_ms": self.elapsed_ms,
            "sent": self.sent,
            "answered": self.answered(),
            "ok": self.ok,
            "client_errors": self.client_errors,
            "too_many": self.too_many,
            "unavailable": self.unavailable,
            "server_errors": self.server_errors,
            "other": self.other,
            "errors": self.errors,
            "p50_us": self.p50_us,
            "p99_us": self.p99_us,
            "targets": self.targets,
            "not_deployed": self.not_deployed,
            "problems": self
                .not_deployed
                .iter()
                .map(|app| format!("target app {app} is not deployed"))
                .collect::<Vec<_>>(),
        })
    }
}

/// The run going on, if one is, and the last that ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub current: Option<RunStatus>,
    pub last: Option<RunStatus>,
}

impl Status {
    pub fn running(&self) -> bool {
        self.current.is_some()
    }

    pub fn to_json(&self) -> Json {
        json!({
            "running": self.running(),
            "current": self.current.as_ref().map(RunStatus::to_json),
            "last": self.last.as_ref().map(RunStatus::to_json),
            "limits": {
                "max_rate": MAX_RATE,
                "max_connections": MAX_CONNECTIONS,
                "max_seconds": MAX_SECONDS,
            },
        })
    }
}

/// Why a run did not start.
#[derive(Debug, PartialEq, Eq)]
pub enum StartError {
    /// One is running.
    Running,
    /// This host cannot be asked over plain HTTP.
    Unavailable(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Running => {
                f.write_str("a load run is going on; stop it or wait for it to end")
            }
            StartError::Unavailable(why) => f.write_str(why),
        }
    }
}

#[derive(Default)]
struct Runs {
    current: Option<Arc<Run>>,
    last: Option<Arc<Run>>,
}

/// The host's load runs: one at a time.
pub struct Runner {
    /// The public listener, as a loopback address.
    target: SocketAddr,
    io: tokio::runtime::Handle,
    /// Why the host cannot be asked (a public listener that terminates TLS).
    unavailable: Option<String>,
    runs: Mutex<Runs>,
}

impl Runner {
    /// A runner that asks `listening` — the address the public listener
    /// bound, the loopback of its family when it bound every address — on
    /// `io`. `tls` is a listener this cannot speak to.
    pub fn new(listening: SocketAddr, io: tokio::runtime::Handle, tls: bool) -> Arc<Runner> {
        let mut target = listening;
        if target.ip().is_unspecified() {
            target.set_ip(match target {
                SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
            });
        }
        Arc::new(Runner {
            target,
            io,
            unavailable: tls.then(|| {
                "the public listener terminates TLS, and a load run speaks plain HTTP to it"
                    .to_string()
            }),
            runs: Mutex::new(Runs::default()),
        })
    }

    /// Where a run's requests go.
    pub fn target(&self) -> SocketAddr {
        self.target
    }

    /// Starts a run, unless one is going on.
    pub fn start(self: &Arc<Self>, params: Params) -> Result<RunStatus, StartError> {
        if let Some(why) = &self.unavailable {
            return Err(StartError::Unavailable(why.clone()));
        }
        let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        if runs.current.is_some() {
            return Err(StartError::Running);
        }
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let run = Arc::new(Run {
            params,
            started: Instant::now(),
            started_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64),
            tally: Arc::new(Tally::new()),
            stop,
            stopped: AtomicBool::new(false),
            ended: Mutex::new(None),
        });
        runs.current = Some(Arc::clone(&run));
        drop(runs);
        self.io
            .spawn(Arc::clone(self).drive(Arc::clone(&run), stopped));
        Ok(run.status())
    }

    /// Stops the run going on: its connections are dropped, and what they
    /// had in flight with them. `Err` when none is.
    pub fn stop(&self) -> Result<RunStatus, String> {
        let runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        let Some(run) = &runs.current else {
            return Err("no load run is going on".to_string());
        };
        run.stopped.store(true, Ordering::Relaxed);
        let _ = run.stop.send(true);
        Ok(run.status())
    }

    /// What the run going on has done, and the last one that ended.
    pub fn status(&self) -> Status {
        let runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        Status {
            current: runs.current.as_ref().map(|run| run.status()),
            last: runs.last.as_ref().map(|run| run.status()),
        }
    }

    /// Runs `run` to its end: its connections, then the record of it.
    async fn drive(self: Arc<Self>, run: Arc<Run>, stopped: tokio::sync::watch::Receiver<bool>) {
        let params = run.params;
        let started = tokio::time::Instant::now();
        let end = started + Duration::from_secs(u64::from(params.seconds));
        let next = Arc::new(AtomicU64::new(0));
        let mut connections = tokio::task::JoinSet::new();
        for _ in 0..params.connections {
            connections.spawn(connection(
                self.target,
                params,
                started,
                end,
                Arc::clone(&next),
                Arc::clone(&run.tally),
                stopped.clone(),
            ));
        }
        // Out of time, what is still in flight has `GRACE` to be answered.
        let cut = tokio::time::sleep_until(end + GRACE);
        tokio::pin!(cut);
        let mut cutting = false;
        loop {
            tokio::select! {
                joined = connections.join_next() => {
                    if joined.is_none() {
                        break;
                    }
                }
                _ = &mut cut, if !cutting => {
                    cutting = true;
                    let _ = run.stop.send(true);
                }
            }
        }
        let how = if run.stopped.load(Ordering::Relaxed) {
            Ended::Stopped
        } else {
            Ended::Finished
        };
        *run.ended.lock().unwrap_or_else(|p| p.into_inner()) = Some((run.started.elapsed(), how));
        let mut runs = self.runs.lock().unwrap_or_else(|p| p.into_inner());
        runs.current = None;
        runs.last = Some(run);
    }
}

/// One kept-alive connection of a run, asking until the run's time is up,
/// it is stopped, or no target is left.
async fn connection(
    target: SocketAddr,
    params: Params,
    started: tokio::time::Instant,
    end: tokio::time::Instant,
    next: Arc<AtomicU64>,
    tally: Arc<Tally>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut conn = Conn::new(target.to_string()).with_header(HEADER_LINE);
    let port = target.port();
    loop {
        if *stop.borrow() {
            return;
        }
        let i = next.fetch_add(1, Ordering::Relaxed);
        let due = if params.rate > 0 {
            let due = started + Duration::from_secs_f64(i as f64 / f64::from(params.rate));
            if due >= end {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep_until(due) => {}
                _ = stop.changed() => return,
            }
            due.into_std()
        } else {
            let now = Instant::now();
            if tokio::time::Instant::from_std(now) >= end {
                return;
            }
            now
        };
        let Some((at, app)) = params.target(i, &tally.missing) else {
            return;
        };
        let path = path(app, i, port);
        tally.sent.fetch_add(1, Ordering::Relaxed);
        let answered = tokio::select! {
            answered = conn.answer(&path) => answered,
            _ = stop.changed() => return,
        };
        match answered {
            Some(answered) => {
                if not_deployed(&answered) {
                    tally.missing[at].store(true, Ordering::Relaxed);
                }
                tally.count(answered.status);
                tally
                    .latency
                    .observe(due.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
            }
            None => {
                tally.errors.fetch_add(1, Ordering::Relaxed);
                // A host that refuses connections is not asked again at
                // once, so a closed loop does not spin on it.
                if params.rate == 0 {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                        _ = stop.changed() => return,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_are_checked_against_their_caps() {
        assert_eq!(
            Params::new("cpu", 0, 4, 10),
            Ok(Params {
                mix: Mix::Cpu,
                rate: 0,
                connections: 4,
                seconds: 10
            })
        );
        assert!(Params::new("light", 500, 64, 60).is_ok());
        assert!(Params::new("wait", 1, 1, 1).is_ok());
        assert!(Params::new(" mixed ", 0, 1, 1).is_ok());
        for (mix, rate, connections, seconds, says) in [
            ("heavy", 0, 4, 10, "none of light, cpu, wait or mixed"),
            ("light", 501, 4, 10, "rate is 0 (closed loop) to 500"),
            ("light", -1, 4, 10, "rate is"),
            ("light", 0, 0, 10, "connections is 1 to 64"),
            ("light", 0, 65, 10, "connections is 1 to 64"),
            ("light", 0, 4, 0, "seconds is 1 to 60"),
            ("light", 0, 4, 61, "seconds is 1 to 60"),
        ] {
            let why = Params::new(mix, rate, connections, seconds).unwrap_err();
            assert!(why.contains(says), "{why}");
        }
    }

    #[test]
    fn a_json_body_has_defaults_and_its_types_checked() {
        assert_eq!(
            Params::from_json(b""),
            Ok(Params {
                mix: Mix::Light,
                rate: 0,
                connections: 8,
                seconds: 10
            })
        );
        assert_eq!(
            Params::from_json(br#"{"mix":"wait","rate":50,"connections":2,"seconds":3}"#),
            Ok(Params {
                mix: Mix::Wait,
                rate: 50,
                connections: 2,
                seconds: 3
            })
        );
        assert!(Params::from_json(b"[1]").unwrap_err().contains("object"));
        assert!(Params::from_json(b"{").unwrap_err().contains("not JSON"));
        assert!(Params::from_json(br#"{"rate":"9"}"#)
            .unwrap_err()
            .contains("`rate` is a whole number"));
        assert!(Params::from_json(br#"{"mix":3}"#)
            .unwrap_err()
            .contains("`mix` is a string"));
        assert!(Params::from_json(br#"{"seconds":600}"#)
            .unwrap_err()
            .contains("seconds is 1 to 60"));
    }

    #[test]
    fn mixed_takes_its_targets_in_turn_and_skips_the_missing() {
        let params = Params::new("mixed", 0, 1, 1).unwrap();
        let missing: [AtomicBool; 3] = Default::default();
        let apps: Vec<_> = (0..4)
            .map(|i| params.target(i, &missing).unwrap().1)
            .collect();
        assert_eq!(apps, ["hello", "crunch", "slow", "hello"]);
        missing[1].store(true, Ordering::Relaxed);
        let apps: Vec<_> = (0..3)
            .map(|i| params.target(i, &missing).unwrap().1)
            .collect();
        assert_eq!(apps, ["hello", "slow", "slow"]);
        missing[0].store(true, Ordering::Relaxed);
        missing[2].store(true, Ordering::Relaxed);
        assert_eq!(params.target(0, &missing), None);
        let light = Params::new("light", 0, 1, 1).unwrap();
        let none: [AtomicBool; 3] = Default::default();
        assert_eq!(light.target(7, &none), Some((0, "hello")));
    }

    #[test]
    fn statuses_are_counted_by_class() {
        let tally = Tally::new();
        for status in [200, 204, 404, 429, 429, 503, 504, 500, 302] {
            tally.count(status);
        }
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        assert_eq!(get(&tally.ok), 2);
        assert_eq!(get(&tally.client_errors), 1);
        assert_eq!(get(&tally.too_many), 2);
        assert_eq!(get(&tally.unavailable), 1);
        assert_eq!(get(&tally.server_errors), 2);
        assert_eq!(get(&tally.other), 1);
    }

    #[test]
    fn only_the_hosts_own_404_is_an_app_not_deployed() {
        let answer = |status, body: &str| Answered {
            status,
            body: body.as_bytes().to_vec(),
        };
        assert!(not_deployed(&answer(404, "no app named `slow`\n")));
        assert!(!not_deployed(&answer(404, "no such page\n")));
        assert!(!not_deployed(&answer(200, "no app named `slow`\n")));
    }

    #[test]
    fn quantiles_are_within_two_percent() {
        let q = Quantiles::new();
        assert_eq!(q.quantile_us(0.5), None);
        for micros in 1..=1000u64 {
            q.observe(micros * 100);
        }
        let p50 = q.quantile_us(0.5).unwrap() as f64;
        let p99 = q.quantile_us(0.99).unwrap() as f64;
        assert!((p50 / 50_000.0 - 1.0).abs() < 0.02, "{p50}");
        assert!((p99 / 99_000.0 - 1.0).abs() < 0.02, "{p99}");
        // Past the last bucket is the last bucket, not a panic.
        q.observe(u64::MAX);
    }

    #[test]
    fn a_status_answers_its_counts_and_problems() {
        let run = RunStatus {
            mix: "mixed".into(),
            rate: 0,
            connections: 2,
            seconds: 5,
            state: "finished".into(),
            started_unix_ms: 1,
            elapsed_ms: 5000,
            sent: 12,
            ok: 6,
            client_errors: 2,
            too_many: 1,
            unavailable: 1,
            server_errors: 1,
            other: 0,
            errors: 1,
            p50_us: Some(900),
            p99_us: None,
            targets: vec!["hello".into(), "crunch".into(), "slow".into()],
            not_deployed: vec!["crunch".into()],
        };
        assert_eq!(run.answered(), 11);
        let json = Status {
            current: None,
            last: Some(run),
        }
        .to_json();
        assert_eq!(json["running"], false);
        assert_eq!(json["current"], Json::Null);
        assert_eq!(json["last"]["answered"], 11);
        assert_eq!(json["last"]["p99_us"], Json::Null);
        assert_eq!(
            json["last"]["problems"][0],
            "target app crunch is not deployed"
        );
        assert_eq!(json["limits"]["max_rate"], 500);
    }

    #[test]
    fn mix_names_parse_as_minicloud_load_has_them() {
        assert_eq!(mix("hello").unwrap(), [("hello", 1)]);
        assert_eq!(mix("cpu-io").unwrap().len(), 4);
        assert_eq!(
            mix("hello=3,crunch=1").unwrap(),
            [("hello", 3), ("crunch", 1)]
        );
        assert!(mix("nope").unwrap_err().contains("is not hello"));
        assert!(mix("hello=x").unwrap_err().contains("whole-number"));
        assert_eq!(request(5, &[("crunch20k", 1)], 80).0, "crunch");
        assert_eq!(request(5, &[("crunch20k", 1)], 80).1, "/crunch/?n=20000");
        assert_eq!(
            request(0, &[("proxy", 1)], 8080).1,
            "/proxy/?url=http://127.0.0.1:8080/hello/"
        );
    }
}
