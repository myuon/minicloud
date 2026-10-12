//! The scheduler: per-app queues served round robin, a shared pool of worker
//! threads, and a monitor that slices long runs.
//!
//! ```text
//!   HTTP (tokio) ──admit──▶ ┌ app A: starts ░░░ runs ░ ┐
//!        ▲                  │ app B: starts ░   runs   │ ──take (round robin)──▶ workers × N
//!        │ oneshot          └ app C: …                 ┘                          │
//!        │                          ▲   ▲                                         │
//!        │                          │   └── Continue ◀── Step::Yielded ───────────┤
//!        │                          └────── Resume ◀── PendingWork on tokio ◀─────┤ Step::Parked
//!        └──────────────────────────────────────────────── Step::Answered ────────┘
//! ```
//!
//! **Cove never runs on the async runtime.** The HTTP side admits a request
//! into its app's queue and awaits a oneshot; a worker — a plain thread —
//! takes it, builds a fresh `OwnedVm` over the app's shared
//! `PreparedProgram`, and runs it until it answers, parks or yields.
//!
//! **A parked run holds no thread.** A host that answers pending hands over a
//! [`PendingWork`] future; it is spawned on the I/O runtime, raced against the
//! run's deadline (`ParkedVm::time_left`, ADR 0082), and its answer — or the
//! deadline — puts a resume job on the app's queue, for whichever worker is
//! free. A run whose deadline came first is cancelled (`ParkedVm::cancel`) and
//! answered 504, and its future is dropped, which withdraws the work.
//!
//! **A long run is sliced.** Each worker publishes the run it is running; the
//! monitor raises a run's `YieldRequest` once it has held its worker for a
//! slice (2 ms by default) *and* something else is waiting for a worker
//! (ADR 0084, inside compiled code too by ADR 0085). The yielded run goes to
//! the back of its app's queue.
//!
//! **Apps are served round robin.** Each app has its own queue — runs to
//! resume or continue, and requests waiting to start — and a worker takes one
//! job from the next app in turn that has work it may run. A job is at most
//! one slice of a run while others wait, so an app with a thousand requests
//! queued gets one turn in N like every other busy app: its depth costs
//! itself, not its neighbours. An app at its `max_in_flight` has its starts
//! held back (its runs still continue), and an app at `max_queued` has new
//! requests rejected with 429; the server as a whole rejects with 503 past
//! `--max-in-flight`.
//!
//! **A run that cannot yield is surfaced, not trusted.** A run below an
//! encoded callee of compiled code, inside a host call, or beside a task
//! declines a yield request. The monitor counts a run still holding its
//! worker well after it was asked (`overdue_yields`) and logs it; the
//! runtime's own `yields_declined` is summed per app; the run's deadline
//! still ends it.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cove_diag::render;
use cove_runtime::trace::RunOutcome;
use cove_runtime::{
    Budget, Cancellation, ParkedVm, RuntimeError, Step, Transfer, YieldRequest, YieldedVm,
};
use tokio::sync::oneshot;

use crate::apps::App;
use crate::config::AppLimits;
use crate::convert::{request_value, response_of, AppRequest, BadResponse, Reply};
use crate::hosts::PendingWork;
use crate::logs::LogRing;
use crate::stats::AppCounters;
use crate::stats::{ErrorKind, Rejection};

/// The response header naming the app version that answered.
pub const VERSION_HEADER: &str = "x-cove-app-version";

/// A request in flight: which app, since when, where its answer goes, and
/// how it is called off.
pub struct Flight {
    pub id: u64,
    /// The app's slot: its queue, its counters.
    pub app: usize,
    /// The version of the app the request was admitted to. It runs on this
    /// version to its end, whatever the slot routes to by then; and it keeps
    /// the version — its prepared program, its registry — alive until then.
    pub version: Arc<App>,
    pub accepted: Instant,
    pub reply: oneshot::Sender<Reply>,
    pub cancel: Cancel,
    /// What the run has cost so far, for its answer's [`RUN_HEADERS`].
    pub tally: Tally,
}

/// What one request's run cost, counted across its slices and parks.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tally {
    pub yields: u64,
    pub parks: u64,
    /// Time on a worker, every slice summed.
    pub worker: Duration,
    /// The run's declined yields already added to its app's counters, at
    /// its parks and yields: the runtime's count is the run's whole total so
    /// far, so each point adds what is new since the last.
    pub declined: u64,
}

/// The headers every answer of a run carries, saying what the run cost: the
/// runtime's instruction count, how often it yielded, declined to
/// yield and parked, its time on a worker and from admission to answer (both
/// in microseconds). An app cannot read its own meter, so this is where a page
/// that wants to show "this took N instructions" finds it — `fetch` the page
/// and read the headers.
pub const RUN_HEADERS: [&str; 6] = [
    "x-cove-run-instructions",
    "x-cove-run-yields",
    "x-cove-run-yields-declined",
    "x-cove-run-parks",
    "x-cove-run-worker-us",
    "x-cove-run-wall-us",
];

/// The header a run that a limit stopped is answered with: the
/// [`ErrorKind`]'s name — `deadline`, `cancelled`, `host_calls`,
/// `call_depth`, `heap`, `queue_timeout`, … — so a page can say why without
/// reading the diagnostic.
pub const STOP_HEADER: &str = "x-cove-stop";

/// Calls a request off: raised by the HTTP side when the client goes away
/// before its answer.
///
/// It is the runtime's own [`Cancellation`], in the run's budget, and it
/// reaches the run wherever the run is. A running run stops at its next
/// safepoint; a yielded one stops when it is continued; a parked one's wait
/// on the I/O runtime is woken by [`Cancellation::on_cancel`], registered on
/// the flag the parked run's meter hands back (ADR 0088), which cancels the
/// run and drops its pending host work (an outbound fetch's connection with
/// it). A request still queued is dropped when a worker takes it.
pub type Cancel = Cancellation;

/// A request waiting to start.
pub struct Start {
    pub request: AppRequest,
    pub flight: Flight,
}

struct Resume {
    parked: ParkedVm,
    /// The host's answer, or `None` for a run whose deadline came first,
    /// which is cancelled instead.
    answer: Option<Result<Transfer, RuntimeError>>,
    flight: Flight,
}

struct Continue {
    yielded: YieldedVm,
    flight: Flight,
}

enum Job {
    Start(Box<Start>),
    Resume(Box<Resume>),
    Continue(Box<Continue>),
}

// --------------------------------------------------------------- the queue

struct AppQueue {
    /// Requests admitted and not started.
    starts: VecDeque<Box<Start>>,
    /// Runs already started: resumed after a park, or continued after a
    /// yield. Always eligible. When an app has both, its turns alternate
    /// between a run and a start, so that one long run of an app does not
    /// hold back the app's own new requests either.
    runs: VecDeque<Job>,
    /// Whether this app's next turn, when it has both, is a start.
    start_next: bool,
    /// Runs started and not answered.
    in_flight: usize,
    max_in_flight: usize,
    max_queued: usize,
}

impl AppQueue {
    fn may_start(&self) -> bool {
        !self.starts.is_empty() && self.in_flight < self.max_in_flight
    }
}

impl AppQueue {
    fn eligible(&self) -> bool {
        !self.runs.is_empty() || (!self.starts.is_empty() && self.in_flight < self.max_in_flight)
    }
}

struct QueueState {
    apps: Vec<AppQueue>,
    /// The app the next take looks at first.
    cursor: usize,
    /// Requests admitted and not answered, over every app.
    admitted: usize,
    closed: bool,
}

/// Per-app queues, served round robin.
pub struct RunQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
    max_admitted: usize,
}

impl RunQueue {
    fn new(max_admitted: usize) -> RunQueue {
        RunQueue {
            state: Mutex::new(QueueState {
                apps: Vec::new(),
                cursor: 0,
                admitted: 0,
                closed: false,
            }),
            ready: Condvar::new(),
            max_admitted,
        }
    }

    /// A queue for one more app; its index.
    fn add_app(&self, limits: &AppLimits) -> usize {
        let mut state = self.state.lock().unwrap();
        state.apps.push(AppQueue {
            starts: VecDeque::new(),
            runs: VecDeque::new(),
            start_next: false,
            in_flight: 0,
            max_in_flight: limits.max_in_flight,
            max_queued: limits.max_queued,
        });
        state.apps.len() - 1
    }

    /// An app's limits, from the version it now routes to. What is queued
    /// stays queued.
    fn set_limits(&self, app: usize, limits: &AppLimits) {
        let mut state = self.state.lock().unwrap();
        state.apps[app].max_in_flight = limits.max_in_flight;
        state.apps[app].max_queued = limits.max_queued;
        drop(state);
        self.ready.notify_all();
    }

    /// Queues a request, or says why not.
    pub fn admit(&self, app: usize, start: Box<Start>) -> Result<(), Rejection> {
        let mut state = self.state.lock().unwrap();
        if state.closed || state.admitted >= self.max_admitted {
            return Err(Rejection::ServerBusy);
        }
        let queue = &mut state.apps[app];
        if queue.starts.len() >= queue.max_queued {
            return Err(Rejection::QueueFull);
        }
        queue.starts.push_back(start);
        state.admitted += 1;
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    fn push_run(&self, app: usize, job: Job) {
        let mut state = self.state.lock().unwrap();
        state.apps[app].runs.push_back(job);
        drop(state);
        self.ready.notify_one();
    }

    /// The next job, from the next app in turn that has one it may run; or
    /// `None` once the queue is closed.
    fn take(&self) -> Option<(usize, Job)> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed {
                return None;
            }
            let n = state.apps.len();
            for k in 0..n {
                let at = (state.cursor + k) % n;
                if !state.apps[at].eligible() {
                    continue;
                }
                state.cursor = (at + 1) % n;
                let queue = &mut state.apps[at];
                let start = queue.may_start() && (queue.runs.is_empty() || queue.start_next);
                queue.start_next = !start;
                let job = if start {
                    queue.in_flight += 1;
                    Job::Start(queue.starts.pop_front().expect("eligible"))
                } else {
                    queue.runs.pop_front().expect("eligible")
                };
                return Some((at, job));
            }
            state = self.ready.wait(state).unwrap();
        }
    }

    /// A run of `app` answered: it no longer counts against either limit.
    fn finished(&self, app: usize) {
        let mut state = self.state.lock().unwrap();
        let queue = &mut state.apps[app];
        queue.in_flight = queue.in_flight.saturating_sub(1);
        let wake = !queue.starts.is_empty();
        state.admitted = state.admitted.saturating_sub(1);
        drop(state);
        if wake {
            self.ready.notify_one();
        }
    }

    /// Whether some job is waiting for a worker.
    fn has_waiting(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .apps
            .iter()
            .any(AppQueue::eligible)
    }

    /// `(in_flight, queued)` for `app`.
    pub fn gauges(&self, app: usize) -> (usize, usize) {
        let state = self.state.lock().unwrap();
        let queue = &state.apps[app];
        (queue.in_flight, queue.starts.len())
    }

    /// Requests admitted and not answered, over every app.
    pub fn admitted(&self) -> usize {
        self.state.lock().unwrap().admitted
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        for queue in &mut state.apps {
            queue.starts.clear();
            queue.runs.clear();
        }
        drop(state);
        self.ready.notify_all();
    }

    fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }
}

// -------------------------------------------------------------- the engine

/// The run a worker is running now, for the monitor.
struct Running {
    version: Arc<App>,
    since: Instant,
    signal: YieldRequest,
    asked_at: Option<Instant>,
    overdue: bool,
}

/// One app's place in the host: its name, the version it routes to, and what
/// outlives any one version — its queue (by index), counters and log.
pub struct Slot {
    pub name: String,
    /// The index of the app's queue.
    pub index: usize,
    /// The version new requests go to; `None` once the app is removed.
    current: RwLock<Option<Arc<App>>>,
    /// Every version this slot has routed to, held weakly: a version is
    /// alive while it routes or while any request admitted to it is in
    /// flight, and its program is dropped with it.
    history: Mutex<Vec<VersionRecord>>,
    pub counters: Arc<AppCounters>,
    pub logs: Arc<LogRing>,
    /// The number the next version of this app is given.
    next_version: AtomicU64,
    /// Whether new requests are routed to the app. A disabled app keeps its
    /// current version, its queue, its counters and its data; requests
    /// already admitted finish, as they do across an update.
    enabled: AtomicBool,
}

/// One version a slot has routed to.
struct VersionRecord {
    id: String,
    loaded: SystemTime,
    app: Weak<App>,
    /// The lowered program its `PreparedProgram` holds: gone when the last
    /// run of the version has ended.
    program: Option<Weak<cove_ir::Program>>,
}

impl Slot {
    /// The version new requests go to.
    pub fn current(&self) -> Option<Arc<App>> {
        self.current.read().unwrap().clone()
    }

    /// Whether the app is routed to (when it has a current version).
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Routes to the app, or stops: what is in flight finishes either way.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// The version number to give the next version loaded into this slot.
    pub fn next_version(&self) -> u64 {
        self.next_version.load(Ordering::Relaxed)
    }

    /// Every version this slot has had, as JSON: id, when it was loaded,
    /// whether it is the current one, whether it is still alive (routed to,
    /// or with requests in flight) and whether its program is.
    pub fn versions(&self) -> Vec<serde_json::Value> {
        let current = self.current().map(|app| app.version.clone());
        let mut history = self.history.lock().unwrap();
        // Forget long-gone versions, keeping the last few for the record.
        let gone = history
            .iter()
            .filter(|record| record.app.strong_count() == 0)
            .count();
        if gone > 8 {
            let mut drop_first = gone - 8;
            history.retain(|record| {
                if drop_first > 0 && record.app.strong_count() == 0 {
                    drop_first -= 1;
                    false
                } else {
                    true
                }
            });
        }
        history
            .iter()
            .map(|record| {
                serde_json::json!({
                    "version": record.id,
                    "loaded_unix_s": record.loaded.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
                    "current": current.as_deref() == Some(record.id.as_str()),
                    "alive": record.app.strong_count() > 0,
                    "program_alive": record.program.as_ref().is_some_and(|p| p.strong_count() > 0),
                })
            })
            .collect()
    }

    /// How many of this slot's versions are alive, and how many of their
    /// prepared programs.
    pub fn alive(&self) -> (usize, usize) {
        let history = self.history.lock().unwrap();
        (
            history.iter().filter(|r| r.app.strong_count() > 0).count(),
            history
                .iter()
                .filter(|r| r.program.as_ref().is_some_and(|p| p.strong_count() > 0))
                .count(),
        )
    }
}

/// What installing a version came to.
pub struct Installed {
    pub app: String,
    pub version: String,
    /// The version it replaced, if any.
    pub previous: Option<String>,
}

/// The apps, their queues, and what the workers and the monitor share.
pub struct Engine {
    slots: RwLock<Vec<Arc<Slot>>>,
    /// `[route] hosts` of every current version: hostname to app name.
    hostnames: RwLock<Arc<BTreeMap<String, String>>>,
    pub queue: RunQueue,
    running: Vec<Mutex<Option<Running>>>,
    /// How long a run may hold a worker while others wait; `None` never asks.
    pub slice: Option<Duration>,
    /// Where pending host work runs.
    io: tokio::runtime::Handle,
    requests: AtomicU64,
}

impl Engine {
    pub fn new(
        apps: Vec<App>,
        workers: usize,
        max_in_flight: usize,
        slice: Option<Duration>,
        io: tokio::runtime::Handle,
    ) -> Engine {
        let engine = Engine {
            slots: RwLock::new(Vec::new()),
            hostnames: RwLock::new(Arc::default()),
            queue: RunQueue::new(max_in_flight),
            running: (0..workers.max(1)).map(|_| Mutex::new(None)).collect(),
            slice,
            io,
            requests: AtomicU64::new(0),
        };
        for app in apps {
            engine.install(app);
        }
        engine
    }

    /// Every slot, in the order the apps were first loaded.
    pub fn slots(&self) -> Vec<Arc<Slot>> {
        self.slots.read().unwrap().clone()
    }

    /// The slot named `name`, removed or not.
    pub fn slot_named(&self, name: &str) -> Option<Arc<Slot>> {
        self.slots
            .read()
            .unwrap()
            .iter()
            .find(|slot| slot.name == name)
            .cloned()
    }

    fn slot(&self, index: usize) -> Arc<Slot> {
        Arc::clone(&self.slots.read().unwrap()[index])
    }

    /// Routes `app`'s name to it from now on: a new slot for a new name, or
    /// the next version of an existing one. Requests already admitted to the
    /// version it replaces finish on that version; the old version — and
    /// its prepared program — is dropped when the last of them ends.
    ///
    /// The caller decides whether a version is fit to install: the host
    /// installs only a loaded one, and keeps the current one otherwise.
    pub fn install(&self, app: App) -> Installed {
        let app = Arc::new(app);
        let record = VersionRecord {
            id: app.version.clone(),
            loaded: SystemTime::now(),
            app: Arc::downgrade(&app),
            program: app
                .ready()
                .map(|ready| Arc::downgrade(ready.program.program())),
        };
        let mut slots = self.slots.write().unwrap();
        let slot = match slots.iter().find(|slot| slot.name == app.name) {
            Some(slot) => Arc::clone(slot),
            None => {
                let slot = Arc::new(Slot {
                    name: app.name.clone(),
                    index: self.queue.add_app(&app.limits),
                    current: RwLock::new(None),
                    history: Mutex::new(Vec::new()),
                    counters: Arc::clone(&app.counters),
                    logs: Arc::clone(&app.logs),
                    next_version: AtomicU64::new(1),
                    enabled: AtomicBool::new(true),
                });
                slots.push(Arc::clone(&slot));
                slot
            }
        };
        drop(slots);
        slot.next_version
            .fetch_max(app.number + 1, Ordering::Relaxed);
        self.queue.set_limits(slot.index, &app.limits);
        slot.history.lock().unwrap().push(record);
        let previous = slot
            .current
            .write()
            .unwrap()
            .replace(Arc::clone(&app))
            .map(|old| old.version.clone());
        self.rebuild_hostnames();
        Installed {
            app: app.name.clone(),
            version: app.version.clone(),
            previous,
        }
    }

    /// Stops routing to `name`. Requests in flight finish; its queue, its
    /// counters and its data stay, and loading it again resumes the slot.
    pub fn remove(&self, name: &str) -> Option<String> {
        let slot = self.slot_named(name)?;
        let old = slot.current.write().unwrap().take();
        self.rebuild_hostnames();
        old.map(|old| old.version.clone())
    }

    /// Hostname to app name, for every app's current version: what the
    /// router reads.
    pub fn hostnames(&self) -> Arc<BTreeMap<String, String>> {
        Arc::clone(&self.hostnames.read().unwrap())
    }

    /// The app other than `except` whose current version claims `host`.
    pub fn host_claimed_by(&self, host: &str, except: &str) -> Option<String> {
        self.hostnames()
            .get(host)
            .filter(|name| name.as_str() != except)
            .cloned()
    }

    fn rebuild_hostnames(&self) {
        let mut table = BTreeMap::new();
        for slot in self.slots() {
            if let Some(app) = slot.current() {
                for host in &app.hosts {
                    table
                        .entry(host.clone())
                        .or_insert_with(|| app.name.clone());
                }
            }
        }
        *self.hostnames.write().unwrap() = Arc::new(table);
    }

    /// A request id.
    pub fn next_id(&self) -> u64 {
        self.requests.fetch_add(1, Ordering::Relaxed)
    }

    pub fn workers(&self) -> usize {
        self.running.len()
    }

    /// Starts the worker threads.
    pub fn start_workers(self: &Arc<Self>) -> Result<Vec<JoinHandle<()>>, String> {
        (0..self.running.len())
            .map(|worker| {
                let engine = Arc::clone(self);
                std::thread::Builder::new()
                    .name(format!("cove-worker-{worker}"))
                    .spawn(move || engine.run_worker(worker))
                    .map_err(|e| e.to_string())
            })
            .collect()
    }

    /// Stops taking work. Queued requests are dropped, which answers them
    /// 503 on the HTTP side; a run on a worker finishes its step.
    pub fn close(&self) {
        self.queue.close();
    }

    fn run_worker(self: &Arc<Self>, worker: usize) {
        while let Some((app, job)) = self.queue.take() {
            let ran = catch_unwind(AssertUnwindSafe(|| self.run_job(worker, job)));
            if ran.is_err() {
                // The flight went down with the job, so the HTTP side sees
                // its oneshot dropped and answers 500. The worker lives on.
                *self.running[worker].lock().unwrap() = None;
                let slot = self.slot(app);
                slot.counters.error(ErrorKind::Internal);
                slot.logs
                    .push("host", "a request's run panicked in the host; answered 500");
                self.queue.finished(app);
                eprintln!(
                    "minicloud: [{}] a request's run panicked in the host; answered 500",
                    slot.name
                );
            }
        }
    }

    fn run_job(self: &Arc<Self>, worker: usize, job: Job) {
        match job {
            Job::Start(start) => {
                let Start {
                    request,
                    mut flight,
                } = *start;
                let app = Arc::clone(&flight.version);
                let ready = app.ready().expect("only a loaded app is admitted");
                if flight.cancel.is_cancelled() {
                    // The client left while it waited: nothing to run.
                    self.fail(flight, ErrorKind::Cancelled, Reply::text(499, ""));
                    return;
                }
                if let Some(deadline) = app.limits.run.deadline {
                    let waited = flight.accepted.elapsed();
                    if waited > deadline {
                        let reply = Reply::text(
                            503,
                            format!(
                                "app `{}`: the request waited {} ms for a worker, longer than its \
                                 {} ms deadline; it was not run\n",
                                app.name,
                                waited.as_millis(),
                                deadline.as_millis()
                            ),
                        )
                        .with_header("retry-after", "1");
                        self.fail(flight, ErrorKind::QueueTimeout, reply);
                        return;
                    }
                }
                let vm = ready.isolate(app.limits.max_heap_words);
                let budget =
                    Budget::with_cancellation(app.limits.run.clone(), flight.cancel.clone());
                let signal = vm.yield_request();
                let step = self.sliced(worker, &app, &mut flight.tally, signal, || {
                    let argument = request_value(&request);
                    vm.invoke_within_parkable(
                        budget,
                        &ready.module,
                        &ready.function,
                        vec![argument],
                    )
                });
                self.settle(step, flight);
            }
            Job::Resume(resume) => {
                let Resume {
                    parked,
                    answer,
                    mut flight,
                } = *resume;
                let step = match answer {
                    Some(answer) => {
                        let signal = parked.yield_request();
                        let version = Arc::clone(&flight.version);
                        self.sliced(worker, &version, &mut flight.tally, signal, || {
                            parked.resume(answer)
                        })
                    }
                    None => {
                        let (vm, error) = parked.cancel();
                        Step::Answered(vm, Err(error))
                    }
                };
                self.settle(step, flight);
            }
            Job::Continue(cont) => {
                let Continue {
                    yielded,
                    mut flight,
                } = *cont;
                let signal = yielded.yield_request();
                let version = Arc::clone(&flight.version);
                let step = self.sliced(worker, &version, &mut flight.tally, signal, || {
                    yielded.resume()
                });
                self.settle(step, flight);
            }
        }
    }

    /// Runs `step` as this worker's current run, where the monitor can see it,
    /// and charges the time to the app.
    fn sliced(
        &self,
        worker: usize,
        app: &Arc<App>,
        tally: &mut Tally,
        signal: YieldRequest,
        step: impl FnOnce() -> Step,
    ) -> Step {
        let since = Instant::now();
        if self.slice.is_some() {
            *self.running[worker].lock().unwrap() = Some(Running {
                version: Arc::clone(app),
                since,
                signal,
                asked_at: None,
                overdue: false,
            });
        }
        let step = step();
        if self.slice.is_some() {
            *self.running[worker].lock().unwrap() = None;
        }
        let held = since.elapsed();
        tally.worker += held;
        app.counters
            .worker_ns
            .fetch_add(held.as_nanos() as u64, Ordering::Relaxed);
        step
    }

    /// Answers `flight` with `reply`, counted as `kind`.
    fn fail(&self, flight: Flight, kind: ErrorKind, reply: Reply) {
        self.fail_metered(flight, kind, reply, None);
    }

    /// [`Engine::fail`], for a run that got as far as running: its meter.
    fn fail_metered(
        &self,
        flight: Flight,
        kind: ErrorKind,
        reply: Reply,
        meter: Option<(u64, u64)>,
    ) {
        let app = &flight.version;
        app.counters.error(kind);
        app.counters
            .recent_error(kind, &app.version, &String::from_utf8_lossy(&reply.body));
        let reply = with_run_headers(reply, &flight, meter)
            .with_header(STOP_HEADER, kind.name())
            .with_header(VERSION_HEADER, app.version.as_str());
        let _ = flight.reply.send(reply);
        self.queue.finished(flight.app);
    }

    /// What a run came to: an answer to send, a park to hand on, or a yield
    /// to queue.
    fn settle(self: &Arc<Self>, step: Step, mut flight: Flight) {
        let app = Arc::clone(&flight.version);
        let counters = &app.counters;
        match step {
            Step::Answered(vm, outcome) => {
                let heap = vm.heap_words();
                let meter = (vm.instructions(), vm.yields_declined());
                counters
                    .instructions
                    .fetch_add(vm.instructions(), Ordering::Relaxed);
                note_progress(&mut flight.tally, counters, heap, vm.yields_declined());
                let answered = match outcome {
                    // The heap's capacity is the app's `max_heap_words`
                    // (`Ready::isolate`), so a run that needed more has
                    // already failed its allocation and is an `Err`.
                    Ok(value) => match response_of(&value, app.limits.max_response_bytes) {
                        Ok(reply) => Ok(reply),
                        Err(BadResponse::Invalid(why)) => Err((
                            ErrorKind::BadResponse,
                            format!("app `{}` answered a bad response: {why}\n", app.name),
                        )),
                        Err(BadResponse::TooLarge { bytes, limit }) => Err((
                            ErrorKind::ResponseTooLarge,
                            format!(
                                "app `{}` answered a body of {bytes} bytes, above its \
                                 max_response_bytes of {limit}\n",
                                app.name
                            ),
                        )),
                    },
                    Err(error) => {
                        let ready = app.ready().expect("only a loaded app runs");
                        Err((
                            kind_of(&error),
                            render(&ready.sources, &error.to_diagnostic()),
                        ))
                    }
                };
                drop(vm);
                match answered {
                    Ok(reply) => {
                        counters.served.fetch_add(1, Ordering::Relaxed);
                        counters.ok.fetch_add(1, Ordering::Relaxed);
                        let reply = with_run_headers(reply, &flight, Some(meter))
                            .with_header(VERSION_HEADER, app.version.as_str());
                        let _ = flight.reply.send(reply);
                        self.queue.finished(flight.app);
                    }
                    Err((kind, body)) => {
                        let line = format!(
                            "{} {} ({}): {}",
                            kind.status(),
                            kind.name(),
                            app.version,
                            first_line(&body)
                        );
                        app.logs.push("host", &line);
                        if kind != ErrorKind::Cancelled {
                            eprintln!("minicloud: [{}] {line}", app.name);
                        }
                        self.fail_metered(
                            flight,
                            kind,
                            Reply::text(kind.status(), body),
                            Some(meter),
                        );
                    }
                }
            }
            Step::Parked(mut parked) => {
                note_progress(
                    &mut flight.tally,
                    counters,
                    parked.heap_words(),
                    parked.yields_declined(),
                );
                flight.tally.parks += 1;
                counters.parks.fetch_add(1, Ordering::Relaxed);
                counters.parked.fetch_add(1, Ordering::Relaxed);
                match parked.take_request().map(|r| r.downcast::<PendingWork>()) {
                    Some(Ok(work)) => {
                        // The deadline, or "never" for a run without one.
                        let left = parked.time_left().unwrap_or(Duration::MAX);
                        // The run's own flag tells this wait when the client
                        // leaves — at once if it already has.
                        let (told, cancelled) = oneshot::channel::<()>();
                        parked.meter().cancellation().on_cancel(move || {
                            let _ = told.send(());
                        });
                        let engine = Arc::clone(self);
                        self.io.spawn(async move {
                            let work = *work;
                            // Whichever comes first. The losers are dropped:
                            // a fetch's connection is closed with its future.
                            let answer = tokio::select! {
                                answer = work.answer => Some(answer),
                                () = sleep_at_most(left) => None,
                                Ok(()) = cancelled => None,
                            };
                            engine.resume(parked, answer, flight);
                        });
                    }
                    _ => {
                        let answer = Err(RuntimeError::new(
                            "the host parked with a request minicloud does not know",
                        ));
                        self.resume(parked, Some(answer), flight);
                    }
                }
            }
            Step::Yielded(yielded) => {
                note_progress(
                    &mut flight.tally,
                    counters,
                    yielded.heap_words(),
                    yielded.yields_declined(),
                );
                flight.tally.yields += 1;
                counters.yields.fetch_add(1, Ordering::Relaxed);
                let app = flight.app;
                self.queue
                    .push_run(app, Job::Continue(Box::new(Continue { yielded, flight })));
            }
        }
    }

    /// A parked run, back on its app's queue with its answer — or with none,
    /// to be cancelled.
    fn resume(
        &self,
        parked: ParkedVm,
        answer: Option<Result<Transfer, RuntimeError>>,
        flight: Flight,
    ) {
        let app = flight.app;
        flight
            .version
            .counters
            .parked
            .fetch_sub(1, Ordering::Relaxed);
        self.queue.push_run(
            app,
            Job::Resume(Box::new(Resume {
                parked,
                answer,
                flight,
            })),
        );
    }

    /// The monitor: every quarter slice, asks each run that has held its
    /// worker for a whole slice to yield — only while something is waiting
    /// for a worker — and notes a run that has not yielded well after it was
    /// asked.
    pub async fn monitor(self: Arc<Self>) {
        let Some(slice) = self.slice else {
            return;
        };
        let tick = (slice / 4).clamp(Duration::from_micros(100), Duration::from_millis(5));
        let overdue_after = (slice * 4).max(Duration::from_millis(20));
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if self.queue.is_closed() {
                return;
            }
            let waiting = self.queue.has_waiting();
            let now = Instant::now();
            for running in &self.running {
                let mut running = running.lock().unwrap();
                let Some(run) = running.as_mut() else {
                    continue;
                };
                let counters = &run.version.counters;
                match run.asked_at {
                    None if waiting && now.duration_since(run.since) >= slice => {
                        run.signal.request();
                        run.asked_at = Some(now);
                        counters.yield_requests.fetch_add(1, Ordering::Relaxed);
                    }
                    Some(asked) if !run.overdue && now.duration_since(asked) >= overdue_after => {
                        run.overdue = true;
                        let n = counters.overdue_yields.fetch_add(1, Ordering::Relaxed) + 1;
                        let line = format!(
                            "a run still holds its worker {} ms after it was asked to yield: it \
                             is where the runtime cannot yield (a host call, or below an encoded \
                             callee of compiled code); its deadline still bounds it ({n} so far)",
                            now.duration_since(asked).as_millis()
                        );
                        run.version.logs.push("host", &line);
                        if n.is_power_of_two() {
                            eprintln!("minicloud: [{}] {line}", run.version.name);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The kind of error a run stopped with.
fn kind_of(error: &RuntimeError) -> ErrorKind {
    if error.message == OUT_OF_HEAP {
        return ErrorKind::Heap;
    }
    match error.outcome {
        RunOutcome::Deadline => ErrorKind::Deadline,
        RunOutcome::HostCalls => ErrorKind::HostCalls,
        RunOutcome::CallDepth => ErrorKind::CallDepth,
        RunOutcome::Concurrency => ErrorKind::Concurrency,
        RunOutcome::Cancelled => ErrorKind::Cancelled,
        _ => ErrorKind::Runtime,
    }
}

/// What the runtime says when a run's heap is at its capacity — the app's
/// `max_heap_words` — and an allocation does not fit (ADR 0088).
const OUT_OF_HEAP: &str = "this run has no memory left";

/// Adds what a run has come to so far at one of its parks, yields or its
/// answer: its heap to its app's peak, and the yields it declined since the
/// last such point to its app's count.
fn note_progress(tally: &mut Tally, counters: &AppCounters, heap: u64, declined: u64) {
    counters.heap_peak_words.fetch_max(heap, Ordering::Relaxed);
    counters
        .yields_declined
        .fetch_add(declined.saturating_sub(tally.declined), Ordering::Relaxed);
    tally.declined = tally.declined.max(declined);
}

/// Sleeps for `left`, or forever for a duration no timer can hold.
async fn sleep_at_most(left: Duration) {
    match tokio::time::Instant::now().checked_add(left) {
        Some(at) if left < Duration::from_secs(365 * 24 * 3600) => {
            tokio::time::sleep_until(at).await
        }
        _ => std::future::pending().await,
    }
}

/// `reply` with the [`RUN_HEADERS`] of a run that has `tally` and, if it ran
/// to an answer, `instructions` and `declined` yields.
fn with_run_headers(reply: Reply, flight: &Flight, meter: Option<(u64, u64)>) -> Reply {
    let (instructions, declined) = meter.unwrap_or_default();
    let values = [
        instructions,
        flight.tally.yields,
        declined,
        flight.tally.parks,
        flight.tally.worker.as_micros() as u64,
        flight.accepted.elapsed().as_micros() as u64,
    ];
    let mut reply = reply;
    for (name, value) in RUN_HEADERS.iter().zip(values) {
        reply = reply.with_header(name, value.to_string());
    }
    reply
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}
