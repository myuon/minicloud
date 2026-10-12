//! Counters, per app and for the server, and the `/_host/stats` body.
//!
//! Everything is a relaxed atomic: a counter is read by a person or a test,
//! never used to decide anything, so no ordering between two of them is
//! promised.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// How many recent errors each app keeps.
pub const RECENT_ERRORS: usize = 50;

use serde_json::{json, Map, Value as Json};

/// Why a request that reached its app was not answered by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The run passed its deadline, running or parked (504).
    Deadline,
    /// The run made more host calls than it may (500).
    HostCalls,
    /// The run nested calls deeper than it may (500).
    CallDepth,
    /// The run tried to `spawn` past its task limit (500).
    Concurrency,
    /// The run needed a heap larger than `max_heap_words`, its capacity (500).
    Heap,
    /// Any other runtime error: a failed assertion, an overflow, a host
    /// that failed, a run out of the runtime's own memory (500).
    Runtime,
    /// The handler answered something that is not a valid response (500).
    BadResponse,
    /// The response body was larger than `max_response_bytes` (500).
    ResponseTooLarge,
    /// The request waited in the queue for longer than its deadline (503).
    QueueTimeout,
    /// The host itself failed while running the request (500).
    Internal,
    /// The client went away before the answer: the run was cancelled, or
    /// never started. Nobody reads the status; 499 is nginx's name for it.
    Cancelled,
}

impl ErrorKind {
    /// Every kind, in the order the stats list them.
    pub const ALL: [ErrorKind; 11] = [
        ErrorKind::Deadline,
        ErrorKind::HostCalls,
        ErrorKind::CallDepth,
        ErrorKind::Concurrency,
        ErrorKind::Heap,
        ErrorKind::Runtime,
        ErrorKind::BadResponse,
        ErrorKind::ResponseTooLarge,
        ErrorKind::QueueTimeout,
        ErrorKind::Internal,
        ErrorKind::Cancelled,
    ];

    /// The name the stats use.
    pub fn name(self) -> &'static str {
        match self {
            ErrorKind::Deadline => "deadline",
            ErrorKind::HostCalls => "host_calls",
            ErrorKind::CallDepth => "call_depth",
            ErrorKind::Concurrency => "concurrency",
            ErrorKind::Heap => "heap",
            ErrorKind::Runtime => "runtime",
            ErrorKind::BadResponse => "bad_response",
            ErrorKind::ResponseTooLarge => "response_too_large",
            ErrorKind::QueueTimeout => "queue_timeout",
            ErrorKind::Internal => "internal",
            ErrorKind::Cancelled => "cancelled",
        }
    }

    /// The status a request that failed this way is answered with.
    pub fn status(self) -> u16 {
        match self {
            ErrorKind::Deadline => 504,
            ErrorKind::QueueTimeout => 503,
            ErrorKind::Cancelled => 499,
            _ => 500,
        }
    }
}

/// Why a request was turned away before it reached its app's queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// The app's queue held `max_queued` requests already (429).
    QueueFull,
    /// The server held `--max-in-flight` requests already (503).
    ServerBusy,
    /// The request body was larger than `max_request_bytes` (413).
    TooLarge,
    /// The request was malformed: a body that is not UTF-8 (400).
    BadRequest,
    /// The app is disabled by the administrator (503).
    Disabled,
    /// The caller is past the app's `[rate]` (429, with `Retry-After`).
    RateLimited,
}

/// One app's counters.
#[derive(Default)]
pub struct AppCounters {
    /// Requests the app answered, with any status.
    pub served: AtomicU64,
    /// Requests answered 2xx–4xx by the app itself.
    pub ok: AtomicU64,
    errors: [AtomicU64; ErrorKind::ALL.len()],
    pub rejected_queue_full: AtomicU64,
    pub rejected_server_busy: AtomicU64,
    pub rejected_too_large: AtomicU64,
    pub rejected_bad_request: AtomicU64,
    pub rejected_disabled: AtomicU64,
    /// Requests turned away at the door by `[rate]` ([`crate::rate`]).
    pub rejected_rate_limited: AtomicU64,
    /// Runs parked at a host call, now.
    pub parked: AtomicU64,
    /// Parks, summed over every run.
    pub parks: AtomicU64,
    /// Yields at a safepoint, summed over every run.
    pub yields: AtomicU64,
    /// Times the monitor asked a run of this app to yield.
    pub yield_requests: AtomicU64,
    /// Safepoints that were asked to yield and could not (the runtime's
    /// `OwnedVm::yields_declined`), summed over every answered run.
    pub yields_declined: AtomicU64,
    /// Runs still holding their worker well after being asked to yield: a
    /// run that cannot yield where it is (see `docs/design.md`, "Runtime
    /// constraints, made explicit"). Its deadline still bounds it.
    pub overdue_yields: AtomicU64,
    /// Host calls answered by blocking the worker because the run could not
    /// park where the call was made.
    pub blocking_host_calls: AtomicU64,
    /// Instructions the encoded VM dispatched, summed over answered runs.
    /// Compiled code dispatches none, so on the native tier this undercounts
    /// the work; `worker_ms` does not.
    pub instructions: AtomicU64,
    /// Nanoseconds workers spent running this app's code.
    pub worker_ns: AtomicU64,
    /// The largest heap a run had at a park, a yield or its answer, in words.
    pub heap_peak_words: AtomicU64,
    /// `fetch` calls made, refused by the allowlist or a limit before
    /// anything was sent, and failed (no response: unreachable, too slow,
    /// too large).
    pub fetches: AtomicU64,
    pub fetch_refused: AtomicU64,
    pub fetch_errors: AtomicU64,
    /// Files from the app's `assets/` answered by the host, and those of
    /// them answered `304 Not Modified`. No run happens for either, so
    /// neither is in `served` ([`crate::assets`]).
    pub assets: AtomicU64,
    pub assets_not_modified: AtomicU64,
    /// Runs the host started on a schedule ([`crate::triggers`]): how many
    /// it ran, how many ticks it let go by because the run before had not
    /// finished, and how many ran and did not answer 2xx-4xx.
    pub trigger_runs: AtomicU64,
    pub trigger_skipped: AtomicU64,
    pub trigger_failed: AtomicU64,
    /// How long this app's answered requests took, admission to answer
    /// ([`crate::histogram`]). A run's own headers say what one cost; this
    /// is what they all cost.
    pub latency: crate::histogram::Latency,
    /// Updates installed, and updates refused (the current version kept).
    pub updates: AtomicU64,
    pub updates_refused: AtomicU64,
    /// The last [`RECENT_ERRORS`] errors, newest last.
    recent: Mutex<VecDeque<Json>>,
}

impl AppCounters {
    /// A static file answered: `modified` is false for a `304`.
    pub fn asset(&self, modified: bool) {
        self.assets.fetch_add(1, Ordering::Relaxed);
        if !modified {
            self.assets_not_modified.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn error(&self, kind: ErrorKind) {
        self.served.fetch_add(1, Ordering::Relaxed);
        self.errors[kind as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Notes an error in the app's recent errors: when, which kind, which
    /// version, and the first line of the message.
    pub fn recent_error(&self, kind: ErrorKind, version: &str, message: &str) {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        let mut recent = self.recent.lock().unwrap();
        if recent.len() == RECENT_ERRORS {
            recent.pop_front();
        }
        recent.push_back(json!({
            "unix_ms": millis,
            "kind": kind.name(),
            "status": kind.status(),
            "version": version,
            "message": message.lines().next().unwrap_or_default(),
        }));
    }

    /// The recent errors, oldest first.
    pub fn recent_errors(&self) -> Vec<Json> {
        self.recent.lock().unwrap().iter().cloned().collect()
    }

    pub fn errors(&self, kind: ErrorKind) -> u64 {
        self.errors[kind as usize].load(Ordering::Relaxed)
    }

    pub fn rejected(&self, why: Rejection) {
        let counter = match why {
            Rejection::QueueFull => &self.rejected_queue_full,
            Rejection::ServerBusy => &self.rejected_server_busy,
            Rejection::TooLarge => &self.rejected_too_large,
            Rejection::BadRequest => &self.rejected_bad_request,
            Rejection::Disabled => &self.rejected_disabled,
            Rejection::RateLimited => &self.rejected_rate_limited,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// As the JSON object `/_host/stats` lists under the app, with the
    /// scheduler's gauges beside the counters.
    pub fn to_json(&self, in_flight: usize, queued: usize) -> Json {
        let read = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let mut errors = Map::new();
        for kind in ErrorKind::ALL {
            errors.insert(kind.name().to_string(), json!(self.errors(kind)));
        }
        json!({
            "served": read(&self.served),
            "ok": read(&self.ok),
            "errors": errors,
            "rejected": {
                "queue_full": read(&self.rejected_queue_full),
                "server_busy": read(&self.rejected_server_busy),
                "too_large": read(&self.rejected_too_large),
                "bad_request": read(&self.rejected_bad_request),
                "disabled": read(&self.rejected_disabled),
                "rate_limited": read(&self.rejected_rate_limited),
            },
            "in_flight": in_flight,
            "queued": queued,
            "parked": read(&self.parked),
            "parks": read(&self.parks),
            "yields": read(&self.yields),
            "yield_requests": read(&self.yield_requests),
            "yields_declined": read(&self.yields_declined),
            "overdue_yields": read(&self.overdue_yields),
            "blocking_host_calls": read(&self.blocking_host_calls),
            "instructions": read(&self.instructions),
            "worker_ms": read(&self.worker_ns) as f64 / 1e6,
            "heap_peak_words": read(&self.heap_peak_words),
            "updates": read(&self.updates),
            "updates_refused": read(&self.updates_refused),
            "fetch": {
                "calls": read(&self.fetches),
                "refused": read(&self.fetch_refused),
                "errors": read(&self.fetch_errors),
            },
            "latency_us": {
                "count": self.latency.count(),
                "mean": self.latency.mean_us(),
                "p50": self.latency.quantile_us(0.50),
                "p90": self.latency.quantile_us(0.90),
                "p99": self.latency.quantile_us(0.99),
            },
            "assets_served": read(&self.assets),
            "assets_not_modified": read(&self.assets_not_modified),
            "trigger_runs": read(&self.trigger_runs),
            "trigger_skipped": read(&self.trigger_skipped),
            "trigger_failed": read(&self.trigger_failed),
        })
    }
}

/// The server's own counters.
#[derive(Default)]
pub struct ServerCounters {
    /// Connections accepted.
    pub connections: AtomicU64,
    /// Connections refused because `--max-connections` were open.
    pub rejected_connections: AtomicU64,
    /// Requests that named no app.
    pub not_found: AtomicU64,
}
