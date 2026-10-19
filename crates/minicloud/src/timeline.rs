//! The request timeline: where each request waited, ran, parked and
//! resumed, kept in memory for the last few minutes ([`KEEP`], at most
//! [`KEEP_REQUESTS`] requests), always on.
//!
//! The design is the Cove repository's edge sample's (`examples/edge/host/
//! src/timeline.rs`, `picture.rs`), adapted to a host whose request already
//! travels as one value — the [`crate::sched::Flight`] — from the HTTP side to
//! a worker, to the I/O runtime while it is parked, and back:
//!
//! - **A request's events live with the request.** Its [`Tracker`] is in its
//!   flight, so whoever holds the flight — the HTTP task, a worker, the
//!   task waiting on its host work — is the only writer of its events, into
//!   a mutex nobody else takes except a reader copying it out. There is no
//!   shared buffer per event, no global sequence number, and nothing is
//!   added under the run queue's lock.
//! - **The shared state is touched twice per request**: when it is admitted
//!   (into the *live* map of its shard) and when it is answered (out of the
//!   live map, onto the shard's ring of finished requests). The shard is the
//!   request's id modulo [`SHARDS`], so two workers finishing two requests
//!   rarely meet on one lock, and the HTTP side's admissions do not either.
//! - **Bounded.** A shard keeps its share of [`KEEP_REQUESTS`] finished
//!   requests and none that ended more than [`KEEP`] ago; the oldest go
//!   first. Nothing is written to disk: a restart starts empty.
//!
//! The events, each a time in nanoseconds since the recorder started:
//!
//! | event | where it is taken |
//! | --- | --- |
//! | arrived | the top of the HTTP side's `reply`, before routing; a scheduled run's admission |
//! | queued | when its [`crate::sched::Flight`] is made, just before `RunQueue::admit` |
//! | run (worker, began) | each slice's start on a worker (`Engine::sliced`): its first, a resume after a park, a continue after a yield |
//! | park (op) | a slice that ended at a host call answering pending, with the call (`fetch`, `timer.sleep`, …) |
//! | ready (by) | on the I/O runtime: the host work answered, the deadline came first, or the client left |
//! | yield | a slice that ended at a safepoint, its time up and others waiting (ADR 0084) |
//! | end (status) | the answer, or the error, before it is sent — so a client holding its answer finds it recorded |
//! | rejected (status, reason) | turned away before its app's queue (rate, size, disabled, queue full, server busy) |
//!
//! What is *not* recorded: the query string (only the path), headers and
//! bodies; `kv` and `sql` calls, which are synchronous inside a run and so
//! count as running; files served from an app's `assets/`, which never run.

use std::borrow::Cow;
use std::cell::UnsafeCell;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value as Json};
use smallvec::SmallVec;

/// How long a finished request is kept.
pub const KEEP: Duration = Duration::from_secs(300);
/// The most finished requests kept, over every shard.
pub const KEEP_REQUESTS: usize = 20_000;
/// How many locks the requests are spread over, by id.
pub const SHARDS: usize = 32;
/// The longest path kept; past it the path is cut (at a character).
pub const PATH_BYTES: usize = 256;
/// The most requests the admin's `host.timeline` answers, newest kept: the
/// lanes a page draws.
pub const LANES: usize = 200;
/// The most requests `GET /_host/timeline` answers, newest kept.
pub const JSON_REQUESTS: usize = 2_000;
/// The most worker segments either answers, newest kept.
pub const SEGMENTS: usize = 10_000;
/// The most in-flight requests `live` lists, oldest kept.
pub const LIVE_ROWS: usize = 500;
/// How many buckets the three strips (running, parked, waiting) have.
pub const STRIP_BUCKETS: usize = 200;

/// How a run segment began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Began {
    /// The request's first slice.
    Start,
    /// After a park, with the host's answer (or to be cancelled).
    Resume,
    /// After a yield.
    Continue,
}

impl Began {
    pub fn name(self) -> &'static str {
        match self {
            Began::Start => "start",
            Began::Resume => "resume",
            Began::Continue => "continue",
        }
    }
}

/// What ended a park.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// The host work answered.
    Answer,
    /// The run's deadline came first; it will be cancelled.
    Deadline,
    /// The client went away; it will be cancelled.
    Cancel,
}

impl By {
    pub fn name(self) -> &'static str {
        match self {
            By::Answer => "answer",
            By::Deadline => "deadline",
            By::Cancel => "cancel",
        }
    }
}

/// What happened to a request, between its admission and its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    Run { worker: u32, began: Began },
    Park { op: &'static str },
    Ready { by: By },
    Yield,
}

/// One event.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub at_ns: u64,
    pub what: What,
}

/// How a request ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct End {
    pub at_ns: u64,
    pub status: u16,
    /// The [`crate::stats::ErrorKind`] it stopped with, or the
    /// [`crate::stats::Rejection`] it was turned away for; `None` for an
    /// answer of the app's own.
    pub reason: Option<&'static str>,
    /// Turned away before its app's queue.
    pub rejected: bool,
    pub worker_ns: u64,
    pub instructions: Option<u64>,
    pub heap_words: Option<u64>,
}

/// One request's record.
pub struct Track {
    pub id: u64,
    /// The app's index ([`Recorder::set_app`]): a number, not its name, so
    /// that recording a request touches no counter its neighbours share.
    app: u32,
    method: Cow<'static, str>,
    /// Short paths inline: most are.
    path: SmallVec<[u8; 48]>,
    pub arrived_ns: u64,
    /// When it ended, or [`LIVE`] while it is in flight: what the bounds
    /// and the live view read without taking its latch.
    ended_ns: AtomicU64,
    state: Latch<TrackState>,
}

/// [`Track::ended_ns`] of a request in flight.
const LIVE: u64 = u64::MAX;

impl Track {
    fn blank() -> Track {
        Track {
            id: 0,
            app: 0,
            method: Cow::Borrowed(""),
            path: SmallVec::new(),
            arrived_ns: 0,
            ended_ns: AtomicU64::new(LIVE),
            state: Latch::new(TrackState {
                queued_ns: None,
                events: SmallVec::new(),
                end: None,
            }),
        }
    }

    fn ended_ns(&self) -> Option<u64> {
        match self.ended_ns.load(Ordering::Acquire) {
            LIVE => None,
            at => Some(at),
        }
    }

    fn path(&self) -> String {
        String::from_utf8_lossy(&self.path).into_owned()
    }
}

#[derive(Clone)]
struct TrackState {
    queued_ns: Option<u64>,
    /// A request that neither parks nor yields has one event: inline.
    events: SmallVec<[Event; 4]>,
    end: Option<End>,
}

/// A lock for one request's record. Its writer is whoever holds the
/// request's flight, one thread at a time, and the only other taker is a
/// view copying it out, so it is all but never contended — and unlike
/// `std::sync::Mutex` on macOS, which is a pthread mutex boxed on first
/// use, it costs no allocation to make or to drop, once per request.
struct Latch<T> {
    held: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the value is only reached through `Latch::with`, which holds
// `held` for the closure's whole run, so one thread at a time has it.
unsafe impl<T: Send> Sync for Latch<T> {}

impl<T> Latch<T> {
    fn new(value: T) -> Latch<T> {
        Latch {
            held: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// The value, for the one who holds the only reference.
    fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut spins = 0u32;
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spins += 1;
            if spins < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
        // Released however `f` ends, a panic included.
        struct Release<'a>(&'a AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _release = Release(&self.held);
        // SAFETY: `held` is ours until `_release` drops, after `f`.
        f(unsafe { &mut *self.value.get() })
    }
}

/// A time: an [`Instant`] on the host, nanoseconds since the recorder
/// started in a test.
pub trait At {
    fn ns(self, started: Instant) -> u64;
}

impl At for Instant {
    fn ns(self, started: Instant) -> u64 {
        self.saturating_duration_since(started).as_nanos() as u64
    }
}

impl At for u64 {
    fn ns(self, _: Instant) -> u64 {
        self
    }
}

struct Shard {
    /// Its requests, in the order they arrived, in flight or not.
    ring: VecDeque<Arc<Track>>,
    /// Requests the bounds pushed off the ring while still in flight — a
    /// long-lived stream, a request past `KEEP` — kept until they end.
    aside: Vec<Arc<Track>>,
    /// Records forgotten that nobody holds, for the next requests to reuse:
    /// in a steady state a request allocates nothing for its record, and
    /// none is freed on a thread other than the one that made it.
    spare: Vec<Arc<Track>>,
}

/// The most records a shard keeps for reuse.
const SPARE: usize = 64;

impl Shard {
    /// Takes off what is past the bounds — the oldest past `per_shard`,
    /// and what ended before `oldest_ns` — into `gone`, for the caller to
    /// drop outside the lock. A request still in flight is put aside.
    fn evict(&mut self, per_shard: usize, oldest_ns: u64, gone: &mut Vec<Arc<Track>>) {
        while let Some(front) = self.ring.front() {
            let over = self.ring.len() > per_shard;
            match front.ended_ns() {
                Some(ended) if over || ended < oldest_ns => {
                    let front = self.ring.pop_front().expect("a front");
                    if Arc::strong_count(&front) == 1 && self.spare.len() < SPARE {
                        self.spare.push(front);
                    } else {
                        gone.push(front);
                    }
                }
                None if over || front.arrived_ns < oldest_ns => {
                    let front = self.ring.pop_front().expect("a front");
                    self.aside.push(front);
                }
                _ => break,
            }
        }
        if !self.aside.is_empty() {
            let mut k = 0;
            while k < self.aside.len() {
                if self.aside[k]
                    .ended_ns()
                    .is_some_and(|ended| ended < oldest_ns)
                {
                    gone.push(self.aside.swap_remove(k));
                } else {
                    k += 1;
                }
            }
        }
    }
}

struct Shared {
    started: Instant,
    started_unix_ms: u64,
    workers: usize,
    /// "Now", for a recorder a test filled with times of its own.
    frozen_now: Option<u64>,
    keep_ns: u64,
    per_shard: usize,
    shards: Vec<Mutex<Shard>>,
    /// App names, by [`Track::app`].
    apps: RwLock<Vec<Arc<str>>>,
}

impl Shared {
    fn now_ns(&self) -> u64 {
        self.frozen_now
            .unwrap_or_else(|| Instant::now().ns(self.started))
    }

    fn shard(&self, id: u64) -> MutexGuard<'_, Shard> {
        self.shards[(id % SHARDS as u64) as usize]
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Puts `track` on its shard's ring. Forgetting is left to the end of a
    /// request ([`Shared::forget`]), on a worker, off the HTTP side — unless
    /// the ring is twice its bound, which only rejections with nothing
    /// ending between them can make it.
    ///
    /// The record is a spare one when the shard has one, filled in by
    /// `fill`, and a new one otherwise.
    fn keep(&self, id: u64, now_ns: u64, fill: impl FnOnce(&mut Track)) -> Arc<Track> {
        let mut gone = Vec::new();
        let track = {
            let mut shard = self.shard(id);
            if shard.ring.len() >= 2 * self.per_shard {
                shard.evict(
                    self.per_shard.saturating_sub(1),
                    now_ns.saturating_sub(self.keep_ns),
                    &mut gone,
                );
            }
            let mut track = shard
                .spare
                .pop()
                .unwrap_or_else(|| Arc::new(Track::blank()));
            match Arc::get_mut(&mut track) {
                Some(record) => fill(record),
                // A spare is held by nobody, so this is not reached; a new
                // record is the answer if it were.
                None => {
                    let mut record = Track::blank();
                    fill(&mut record);
                    track = Arc::new(record);
                }
            }
            shard.ring.push_back(Arc::clone(&track));
            track
        };
        drop(gone);
        track
    }

    /// Forgets what is past the bounds in `id`'s shard, as of `now_ns`.
    fn forget(&self, id: u64, now_ns: u64) {
        let mut gone = Vec::new();
        self.shard(id).evict(
            self.per_shard,
            now_ns.saturating_sub(self.keep_ns),
            &mut gone,
        );
        // The records are dropped here, outside the lock.
        drop(gone);
    }
}

/// The recorder: what the engine and the views share.
#[derive(Clone)]
pub struct Recorder {
    shared: Arc<Shared>,
}

impl Recorder {
    /// A recorder starting now, for a host with `workers` workers.
    pub fn new(workers: usize) -> Recorder {
        Recorder::with_bounds(workers, KEEP, KEEP_REQUESTS)
    }

    /// [`Recorder::new`], with bounds of its own.
    pub fn with_bounds(workers: usize, keep: Duration, keep_requests: usize) -> Recorder {
        let started_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        Recorder::build(workers, started_unix_ms, None, keep, keep_requests)
    }

    /// A recorder whose clock stands at `now_ns`, started at
    /// `started_unix_ms`: what a test fills with times of its own, so that
    /// what is drawn of it is the same every run.
    pub fn fixed(workers: usize, started_unix_ms: u64, now_ns: u64) -> Recorder {
        Recorder::build(workers, started_unix_ms, Some(now_ns), KEEP, KEEP_REQUESTS)
    }

    fn build(
        workers: usize,
        started_unix_ms: u64,
        frozen_now: Option<u64>,
        keep: Duration,
        keep_requests: usize,
    ) -> Recorder {
        Recorder {
            shared: Arc::new(Shared {
                started: Instant::now(),
                started_unix_ms,
                workers: workers.max(1),
                frozen_now,
                keep_ns: keep.as_nanos() as u64,
                per_shard: keep_requests.div_ceil(SHARDS).max(1),
                shards: (0..SHARDS)
                    .map(|_| {
                        Mutex::new(Shard {
                            ring: VecDeque::new(),
                            aside: Vec::new(),
                            spare: Vec::new(),
                        })
                    })
                    .collect(),
                apps: RwLock::new(Vec::new()),
            }),
        }
    }

    pub fn workers(&self) -> usize {
        self.shared.workers
    }

    pub fn started_unix_ms(&self) -> u64 {
        self.shared.started_unix_ms
    }

    /// Nanoseconds since the recorder started.
    pub fn now_ns(&self) -> u64 {
        self.shared.now_ns()
    }

    /// `at` as nanoseconds since the recorder started.
    pub fn ns(&self, at: impl At) -> u64 {
        at.ns(self.shared.started)
    }

    /// Names the app whose requests are recorded as `index` — the engine's
    /// slot index, so that a request records the number its flight carries.
    pub fn set_app(&self, index: usize, name: &str) {
        let mut apps = self.shared.apps.write().unwrap_or_else(|p| p.into_inner());
        if apps.len() <= index {
            apps.resize(index + 1, Arc::from(""));
        }
        apps[index] = Arc::from(name);
    }

    /// The index of the app `name`, named now if it has none yet.
    pub fn app(&self, name: &str) -> usize {
        let mut apps = self.shared.apps.write().unwrap_or_else(|p| p.into_inner());
        match apps.iter().position(|known| &**known == name) {
            Some(index) => index,
            None => {
                apps.push(Arc::from(name));
                apps.len() - 1
            }
        }
    }

    /// Keeps a record of a request, a spare one refilled where there is one.
    #[allow(clippy::too_many_arguments)]
    fn track(
        &self,
        id: u64,
        app: usize,
        method: &str,
        path: &str,
        arrived: u64,
        queued: Option<u64>,
        end: Option<End>,
        now_ns: u64,
    ) -> Arc<Track> {
        self.shared.keep(id, now_ns, |track| {
            track.id = id;
            track.app = app as u32;
            track.method = method_name(method);
            track.path.clear();
            track
                .path
                .extend_from_slice(cut(path, PATH_BYTES).as_bytes());
            track.arrived_ns = arrived;
            *track.ended_ns.get_mut() = end.as_ref().map_or(LIVE, |end| end.at_ns);
            let state = track.state.get_mut();
            state.queued_ns = queued;
            state.events.clear();
            state.end = end;
        })
    }

    /// A request admitted to the queue of the app numbered `app` — about
    /// to be: it is live from now. What happens to it next is its
    /// [`Tracker`]'s to record.
    pub fn admit(
        &self,
        id: u64,
        app: usize,
        method: &str,
        path: &str,
        arrived: impl At,
        queued: impl At,
    ) -> Tracker {
        let arrived = self.ns(arrived);
        let queued = self.ns(queued).max(arrived);
        let track = self.track(id, app, method, path, arrived, Some(queued), None, queued);
        Tracker {
            track,
            shared: Arc::clone(&self.shared),
            done: false,
            meter: None,
        }
    }

    /// A request turned away before its app's queue, at `at`.
    #[allow(clippy::too_many_arguments)]
    pub fn rejected(
        &self,
        id: u64,
        app: usize,
        method: &str,
        path: &str,
        arrived: impl At,
        at: impl At,
        status: u16,
        reason: &'static str,
    ) {
        let arrived = self.ns(arrived);
        let at = self.ns(at).max(arrived);
        let end = End {
            at_ns: at,
            status,
            reason: Some(reason),
            rejected: true,
            worker_ns: 0,
            instructions: None,
            heap_words: None,
        };
        self.track(id, app, method, path, arrived, None, Some(end), at);
    }

    /// Every request kept, copied out under each shard's lock in turn
    /// (which also forgets what is past the bounds by now).
    fn all(&self) -> Vec<Arc<Track>> {
        let oldest = self.now_ns().saturating_sub(self.shared.keep_ns);
        let mut all = Vec::new();
        let mut gone = Vec::new();
        for shard in &self.shared.shards {
            let mut shard = shard.lock().unwrap_or_else(|p| p.into_inner());
            shard.evict(self.shared.per_shard, oldest, &mut gone);
            all.extend(shard.aside.iter().cloned());
            all.extend(shard.ring.iter().cloned());
        }
        drop(gone);
        all
    }

    /// How many requests are kept, in flight and finished.
    pub fn len(&self) -> usize {
        self.all().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn names(&self) -> Vec<Arc<str>> {
        self.shared
            .apps
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// One request, assembled; `None` for one not kept (or never seen).
    pub fn request(&self, id: u64) -> Option<Span> {
        let now = self.now_ns();
        let mut gone = Vec::new();
        let track = {
            let mut shard = self.shared.shard(id);
            shard.evict(
                self.shared.per_shard,
                now.saturating_sub(self.shared.keep_ns),
                &mut gone,
            );
            shard
                .ring
                .iter()
                .rev()
                .chain(shard.aside.iter())
                .find(|track| track.id == id)
                .cloned()
        };
        drop(gone);
        let track = track?;
        Some(Span::of(&track, &self.names(), now))
    }

    /// What the requests of `app` (every app's for `None`) did between
    /// `since_ns` and `until_ns`: every request that overlaps it.
    pub fn window(&self, app: Option<&str>, since_ns: u64, until_ns: u64) -> Window {
        let now = self.now_ns();
        let until_ns = until_ns.min(now).max(since_ns);
        let names = self.names();
        let wanted = app.map(|app| names.iter().position(|name| &**name == app));
        let mut requests: Vec<Span> = self
            .all()
            .iter()
            .filter(|track| wanted.is_none_or(|index| index == Some(track.app as usize)))
            .filter(|track| track.arrived_ns <= until_ns)
            .filter(|track| track.ended_ns().is_none_or(|ended| ended >= since_ns))
            .map(|track| Span::of(track, &names, now))
            .collect();
        requests.sort_by_key(|span| (span.arrived_ns, span.id));
        Window::of(self, requests, now, since_ns, until_ns)
    }

    /// The requests in flight now, oldest first, and what each worker runs.
    pub fn live(&self) -> Live {
        let now = self.now_ns();
        let names = self.names();
        let mut rows: Vec<LiveRow> = Vec::new();
        for shard in &self.shared.shards {
            let tracks: Vec<Arc<Track>> = {
                let shard = shard.lock().unwrap_or_else(|p| p.into_inner());
                shard
                    .aside
                    .iter()
                    .chain(shard.ring.iter())
                    .filter(|track| track.ended_ns().is_none())
                    .cloned()
                    .collect()
            };
            rows.extend(tracks.iter().filter_map(|track| LiveRow::of(track, &names)));
        }
        rows.sort_by_key(|row| (row.arrived_ns, row.id));
        let mut workers: Vec<Option<LiveRow>> = vec![None; self.shared.workers];
        for row in &rows {
            if let (LiveState::Running, Some(worker)) = (row.state, row.worker) {
                if let Some(slot) = workers.get_mut(worker as usize) {
                    *slot = Some(row.clone());
                }
            }
        }
        let total = rows.len();
        rows.truncate(LIVE_ROWS);
        Live {
            now_ns: now,
            started_unix_ms: self.shared.started_unix_ms,
            total,
            workers,
            requests: rows,
        }
    }
}

/// `names[app]`, or nothing for an index never named.
fn name_of(names: &[Arc<str>], app: u32) -> String {
    names
        .get(app as usize)
        .map_or_else(String::new, |name| name.to_string())
}

/// A method as kept: the common ones without a copy.
fn method_name(method: &str) -> Cow<'static, str> {
    const KNOWN: [&str; 9] = [
        "GET", "POST", "HEAD", "PUT", "DELETE", "PATCH", "OPTIONS", "CONNECT", "TRACE",
    ];
    match KNOWN.iter().find(|known| **known == method) {
        Some(known) => Cow::Borrowed(known),
        None => Cow::Owned(cut(method, 32).to_string()),
    }
}

/// `text` cut to at most `bytes`, at a character.
fn cut(text: &str, bytes: usize) -> &str {
    if text.len() <= bytes {
        return text;
    }
    let mut end = bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A request's handle on its record, carried in its flight: what records
/// its events. Dropped without an end — the flight went down with a
/// panicking job, or the queue was closed under it — it ends the record
/// itself, as the HTTP side answers it: 500.
pub struct Tracker {
    track: Arc<Track>,
    shared: Arc<Shared>,
    done: bool,
    /// What its run came to: instructions and heap words.
    meter: Option<(u64, u64)>,
}

impl Tracker {
    pub fn id(&self) -> u64 {
        self.track.id
    }

    fn push(&self, at: impl At, what: What) {
        let at_ns = at.ns(self.shared.started);
        self.track
            .state
            .with(|state| state.events.push(Event { at_ns, what }));
    }

    /// A slice began on `worker`.
    pub fn run(&self, at: impl At, worker: usize, began: Began) {
        self.push(
            at,
            What::Run {
                worker: worker as u32,
                began,
            },
        );
    }

    /// The slice ended at a host call answering pending.
    pub fn park(&self, at: impl At, op: &'static str) {
        self.push(at, What::Park { op });
    }

    /// The park's wait is over.
    pub fn ready(&self, at: impl At, by: By) {
        self.push(at, What::Ready { by });
    }

    /// The slice ended at a safepoint, its time up.
    pub fn yielded(&self, at: impl At) {
        self.push(at, What::Yield);
    }

    /// What the run came to, for its end.
    pub fn meter(&mut self, instructions: u64, heap_words: u64) {
        self.meter = Some((instructions, heap_words));
    }

    /// The request is answered `status`, for `reason` (an error kind's
    /// name), or by its app (`None`).
    pub fn end(
        &mut self,
        at: impl At,
        status: u16,
        reason: Option<&'static str>,
        worker: Duration,
    ) {
        self.close(at.ns(self.shared.started), status, reason, false, worker);
    }

    /// The request was turned away at its app's queue.
    pub fn reject(&mut self, at: impl At, status: u16, reason: &'static str) {
        self.close(
            at.ns(self.shared.started),
            status,
            Some(reason),
            true,
            Duration::ZERO,
        );
    }

    fn close(
        &mut self,
        at_ns: u64,
        status: u16,
        reason: Option<&'static str>,
        rejected: bool,
        worker: Duration,
    ) {
        if self.done {
            return;
        }
        self.done = true;
        let (instructions, heap_words) = match self.meter {
            Some((instructions, heap)) => (Some(instructions), Some(heap)),
            None => (None, None),
        };
        let end = End {
            at_ns,
            status,
            reason,
            rejected,
            worker_ns: worker.as_nanos() as u64,
            instructions,
            heap_words,
        };
        self.track.state.with(|state| state.end = Some(end));
        self.track.ended_ns.store(at_ns, Ordering::Release);
        // The bounds are kept here, where a request ends — on a worker —
        // and not where one is admitted, on the HTTP side.
        self.shared.forget(self.track.id, at_ns);
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        if !self.done {
            let reason = if std::thread::panicking() {
                "internal"
            } else {
                "dropped"
            };
            let now = Instant::now().ns(self.shared.started);
            self.close(now, 500, Some(reason), false, Duration::ZERO);
        }
    }
}

// ------------------------------------------------------------ assembled

/// What a stretch of a request was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseKind {
    /// Waiting for a worker: to start, to resume after a park, or to
    /// continue after a yield.
    Wait,
    /// Running on a worker.
    Run,
    /// Parked at a host call, holding no worker.
    Park,
}

impl PhaseKind {
    pub fn name(self) -> &'static str {
        match self {
            PhaseKind::Wait => "wait",
            PhaseKind::Run => "run",
            PhaseKind::Park => "park",
        }
    }
}

/// One stretch of a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phase {
    pub kind: PhaseKind,
    pub from_ns: u64,
    /// `None` while it lasts.
    pub to_ns: Option<u64>,
    /// A run's worker.
    pub worker: Option<u32>,
    /// A run's beginning.
    pub began: Option<Began>,
    /// A run resumed or continued on a worker other than its last.
    pub moved: bool,
    /// A park's host call.
    pub op: Option<&'static str>,
    /// What ended a park.
    pub by: Option<By>,
    /// What a wait came after: `arrival`, `park` or `yield`.
    pub after: Option<&'static str>,
}

impl Phase {
    fn open(kind: PhaseKind, from_ns: u64) -> Phase {
        Phase {
            kind,
            from_ns,
            to_ns: None,
            worker: None,
            began: None,
            moved: false,
            op: None,
            by: None,
            after: None,
        }
    }

    /// Its end, or `now` while it lasts.
    pub fn until(&self, now: u64) -> u64 {
        self.to_ns.unwrap_or(now).max(self.from_ns)
    }
}

/// One request, its events made into phases.
#[derive(Clone, Debug)]
pub struct Span {
    pub id: u64,
    pub app: String,
    pub method: String,
    pub path: String,
    pub arrived_ns: u64,
    pub queued_ns: Option<u64>,
    pub end: Option<End>,
    pub phases: Vec<Phase>,
    pub yields: u32,
    pub parks: u32,
    /// Resumes and continues on another worker than the slice before.
    pub moves: u32,
    /// The clock when it was assembled.
    pub now_ns: u64,
}

impl Span {
    fn of(track: &Track, names: &[Arc<str>], now: u64) -> Span {
        // Copied out, so the latch is held for a copy and no longer.
        let state = track.state.with(|state| state.clone());
        let (phases, yields, parks, moves) =
            assemble(state.queued_ns, &state.events, state.end.as_ref());
        Span {
            id: track.id,
            app: name_of(names, track.app),
            method: track.method.to_string(),
            path: track.path(),
            arrived_ns: track.arrived_ns,
            queued_ns: state.queued_ns,
            end: state.end,
            phases,
            yields,
            parks,
            moves,
            now_ns: now,
        }
    }

    /// From arrival to answer; `None` while in flight.
    pub fn latency_ns(&self) -> Option<u64> {
        self.end
            .as_ref()
            .map(|end| end.at_ns.saturating_sub(self.arrived_ns))
    }

    /// `ok` (answered by its app), `error`, `rejected` or `in_flight`.
    pub fn outcome(&self) -> &'static str {
        match &self.end {
            None => "in_flight",
            Some(end) if end.rejected => "rejected",
            Some(end) if end.reason.is_some() => "error",
            Some(_) => "ok",
        }
    }

    pub fn to_json(&self) -> Json {
        let end = self.end.as_ref();
        json!({
            "id": self.id,
            "app": self.app,
            "method": self.method,
            "path": self.path,
            "arrived_us": us(self.arrived_ns),
            "queued_us": self.queued_ns.map(us),
            "ended_us": end.map(|end| us(end.at_ns)),
            "status": end.map(|end| end.status),
            "outcome": self.outcome(),
            "reason": end.and_then(|end| end.reason),
            "latency_us": self.latency_ns().map(us),
            "worker_us": end.map_or(0, |end| us(end.worker_ns)),
            "instructions": end.and_then(|end| end.instructions),
            "heap_words": end.and_then(|end| end.heap_words),
            "yields": self.yields,
            "parks": self.parks,
            "moves": self.moves,
            "phases": self.phases.iter().map(|phase| phase_json(phase, self.now_ns)).collect::<Vec<_>>(),
        })
    }
}

fn phase_json(phase: &Phase, now: u64) -> Json {
    json!({
        "kind": phase.kind.name(),
        "from_us": us(phase.from_ns),
        "to_us": us(phase.until(now)),
        "open": phase.to_ns.is_none(),
        "worker": phase.worker,
        "began": phase.began.map(Began::name),
        "moved": phase.moved,
        "op": phase.op,
        "by": phase.by.map(By::name),
        "after": phase.after,
    })
}

fn us(ns: u64) -> u64 {
    ns / 1000
}

/// A request's events as phases: waiting for a worker, running on one,
/// parked; and how many yields, parks and moves.
fn assemble(
    queued: Option<u64>,
    events: &[Event],
    end: Option<&End>,
) -> (Vec<Phase>, u32, u32, u32) {
    let mut phases = Vec::with_capacity(events.len() + 1);
    let (mut yields, mut parks, mut moves) = (0, 0, 0);
    let Some(queued) = queued else {
        return (phases, 0, 0, 0);
    };
    let mut open = Phase::open(PhaseKind::Wait, queued);
    open.after = Some("arrival");
    let mut last_worker: Option<u32> = None;
    let close = |open: Phase, at: u64, phases: &mut Vec<Phase>| {
        let mut done = open;
        done.to_ns = Some(at.max(done.from_ns));
        phases.push(done);
    };
    for event in events {
        let at = event.at_ns;
        match event.what {
            What::Run { worker, began } => {
                let mut run = Phase::open(PhaseKind::Run, at);
                run.worker = Some(worker);
                run.began = Some(began);
                run.moved = began != Began::Start && last_worker.is_some_and(|w| w != worker);
                if run.moved {
                    moves += 1;
                }
                last_worker = Some(worker);
                close(std::mem::replace(&mut open, run), at, &mut phases);
            }
            What::Park { op } => {
                parks += 1;
                let mut park = Phase::open(PhaseKind::Park, at);
                park.op = Some(op);
                close(std::mem::replace(&mut open, park), at, &mut phases);
            }
            What::Ready { by } => {
                open.by = Some(by);
                let mut wait = Phase::open(PhaseKind::Wait, at);
                wait.after = Some("park");
                close(std::mem::replace(&mut open, wait), at, &mut phases);
            }
            What::Yield => {
                yields += 1;
                let mut wait = Phase::open(PhaseKind::Wait, at);
                wait.after = Some("yield");
                close(std::mem::replace(&mut open, wait), at, &mut phases);
            }
        }
    }
    match end {
        Some(end) => close(open, end.at_ns, &mut phases),
        None => phases.push(open),
    }
    // A zero-length wait (a run taken up at once) says nothing.
    phases.retain(|phase| {
        !(phase.kind == PhaseKind::Wait && phase.to_ns.is_some_and(|to| to == phase.from_ns))
    });
    (phases, yields, parks, moves)
}

/// One stretch a worker ran one request — or several, merged
/// ([`merge_segments`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub worker: u32,
    /// The request (the first, when merged).
    pub id: u64,
    pub app: String,
    pub from_ns: u64,
    pub to_ns: u64,
    /// How many runs it is.
    pub runs: u32,
}

impl Segment {
    fn to_json(&self) -> Json {
        json!({
            "worker": self.worker,
            "id": self.id,
            "app": self.app,
            "from_us": us(self.from_ns),
            "to_us": us(self.to_ns),
            "runs": self.runs,
        })
    }
}

/// `segments` (in start order), with those of one worker that are less
/// than `gap_ns` apart made one: what a picture a pixel wide per `gap_ns`
/// can draw anyway.
pub fn merge_segments(segments: &[Segment], gap_ns: u64) -> Vec<Segment> {
    let mut last: BTreeMap<u32, usize> = BTreeMap::new();
    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    for segment in segments {
        if let Some(&at) = last.get(&segment.worker) {
            let previous = &mut merged[at];
            if segment.from_ns <= previous.to_ns.saturating_add(gap_ns) {
                previous.to_ns = previous.to_ns.max(segment.to_ns);
                previous.runs += segment.runs;
                continue;
            }
        }
        last.insert(segment.worker, merged.len());
        merged.push(segment.clone());
    }
    merged
}

/// One app's requests in a window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppSummary {
    pub app: String,
    /// Requests that ended in the window, rejected ones included.
    pub requests: u64,
    pub rejected: u64,
    /// Answered with an error of the host's (deadline, cancelled, …).
    pub errors: u64,
    /// From arrival to answer, of those answered (not rejected).
    pub p50_ns: u64,
    pub p99_ns: u64,
}

/// What the workers and the requests did in a window.
#[derive(Clone, Debug)]
pub struct Window {
    pub started_unix_ms: u64,
    pub now_ns: u64,
    pub since_ns: u64,
    pub until_ns: u64,
    pub workers: usize,
    /// Every request overlapping it, in arrival order.
    pub requests: Vec<Span>,
    /// Every run segment overlapping it, in start order, clipped to it.
    pub segments: Vec<Segment>,
    pub bucket_ns: u64,
    /// The mean count, in thousandths, over each of [`STRIP_BUCKETS`]
    /// buckets: workers running, runs parked, requests waiting for a worker.
    pub running: Vec<u64>,
    pub parked: Vec<u64>,
    pub waiting: Vec<u64>,
    /// Worker time inside the window.
    pub busy_ns: u64,
    pub apps: Vec<AppSummary>,
}

impl Window {
    fn of(recorder: &Recorder, requests: Vec<Span>, now: u64, since: u64, until: u64) -> Window {
        let span = (until - since).max(1);
        let bucket_ns = span.div_ceil(STRIP_BUCKETS as u64).max(1);
        let mut strips = [
            vec![0u128; STRIP_BUCKETS],
            vec![0u128; STRIP_BUCKETS],
            vec![0u128; STRIP_BUCKETS],
        ];
        let mut segments = Vec::new();
        let mut busy_ns = 0;
        for request in &requests {
            for phase in &request.phases {
                let from = phase.from_ns.max(since);
                let to = phase.until(now).min(until);
                if to <= from {
                    continue;
                }
                let strip = match phase.kind {
                    PhaseKind::Run => 0,
                    PhaseKind::Park => 1,
                    PhaseKind::Wait => 2,
                };
                add_overlap(&mut strips[strip], since, bucket_ns, from, to);
                if phase.kind == PhaseKind::Run {
                    busy_ns += to - from;
                    segments.push(Segment {
                        worker: phase.worker.unwrap_or(0),
                        id: request.id,
                        app: request.app.clone(),
                        from_ns: from,
                        to_ns: to,
                        runs: 1,
                    });
                }
            }
        }
        segments.sort_by_key(|segment| (segment.from_ns, segment.worker));
        let means = |strip: &Vec<u128>| -> Vec<u64> {
            strip
                .iter()
                .map(|total| (total * 1000 / bucket_ns as u128) as u64)
                .collect()
        };
        let [running, parked, waiting] = &strips;
        let mut by_app: BTreeMap<&str, (AppSummary, Vec<u64>)> = BTreeMap::new();
        for request in &requests {
            let Some(end) = &request.end else { continue };
            if end.at_ns < since || end.at_ns > until {
                continue;
            }
            let (summary, latencies) = by_app.entry(request.app.as_str()).or_default();
            summary.requests += 1;
            if end.rejected {
                summary.rejected += 1;
            } else {
                if end.reason.is_some() {
                    summary.errors += 1;
                }
                latencies.push(end.at_ns.saturating_sub(request.arrived_ns));
            }
        }
        let apps = by_app
            .into_iter()
            .map(|(app, (mut summary, mut latencies))| {
                latencies.sort_unstable();
                summary.app = app.to_string();
                summary.p50_ns = percentile(&latencies, 50);
                summary.p99_ns = percentile(&latencies, 99);
                summary
            })
            .collect();
        Window {
            started_unix_ms: recorder.started_unix_ms(),
            now_ns: now,
            since_ns: since,
            until_ns: until,
            workers: recorder.workers(),
            running: means(running),
            parked: means(parked),
            waiting: means(waiting),
            requests,
            segments,
            bucket_ns,
            busy_ns,
            apps,
        }
    }

    /// Requests that ended in the window turned away, over every app.
    pub fn rejected(&self) -> u64 {
        self.apps.iter().map(|app| app.rejected).sum()
    }

    /// Worker time as thousandths of what the workers had.
    pub fn utilisation_milli(&self) -> u64 {
        let had = (self.until_ns - self.since_ns).max(1) as u128 * self.workers as u128;
        (self.busy_ns as u128 * 1000 / had) as u64
    }

    /// The newest `n` requests, still in arrival order.
    pub fn newest(&self, n: usize) -> &[Span] {
        &self.requests[self.requests.len().saturating_sub(n)..]
    }

    /// `GET /_host/timeline`.
    pub fn to_json(&self) -> Json {
        let segments = &self.segments[self.segments.len().saturating_sub(SEGMENTS)..];
        json!({
            "started_unix_ms": self.started_unix_ms,
            "now_us": us(self.now_ns),
            "since_us": us(self.since_ns),
            "until_us": us(self.until_ns),
            "workers": self.workers,
            "keep": {"secs": KEEP.as_secs(), "requests": KEEP_REQUESTS},
            "summary": {
                "requests": self.requests.len(),
                "rejected": self.rejected(),
                "busy_us": us(self.busy_ns),
                "utilisation_milli": self.utilisation_milli(),
                "apps": self.apps.iter().map(|app| json!({
                    "app": app.app,
                    "requests": app.requests,
                    "rejected": app.rejected,
                    "errors": app.errors,
                    "p50_us": us(app.p50_ns),
                    "p99_us": us(app.p99_ns),
                })).collect::<Vec<_>>(),
            },
            "strips": {
                "buckets": STRIP_BUCKETS,
                "bucket_us": us(self.bucket_ns),
                "running_milli": self.running,
                "parked_milli": self.parked,
                "waiting_milli": self.waiting,
            },
            "segments_total": self.segments.len(),
            "segments": segments.iter().map(Segment::to_json).collect::<Vec<_>>(),
            "requests_total": self.requests.len(),
            "requests": self.newest(JSON_REQUESTS).iter().map(Span::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Adds `[from, to)` to the buckets of `width` from `origin` it covers.
fn add_overlap(strip: &mut [u128], origin: u64, width: u64, from: u64, to: u64) {
    let first = ((from - origin) / width) as usize;
    let last = (((to - origin).saturating_sub(1)) / width) as usize;
    for (k, total) in strip.iter_mut().enumerate().take(last + 1).skip(first) {
        let start = origin + k as u64 * width;
        let end = start + width;
        *total += (to.min(end) - from.max(start)) as u128;
    }
}

/// The nearest-rank `p`th percentile of `sorted`; 0 for none.
fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

// ------------------------------------------------------------------ live

/// Where an in-flight request is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveState {
    /// Waiting for a worker to start it.
    Waiting,
    Running,
    /// Parked at a host call.
    Parked,
    /// Its park is over; waiting for a worker to resume it.
    Resuming,
    /// Yielded; waiting for a worker to continue it.
    Yielded,
}

impl LiveState {
    pub fn name(self) -> &'static str {
        match self {
            LiveState::Waiting => "waiting",
            LiveState::Running => "running",
            LiveState::Parked => "parked",
            LiveState::Resuming => "resuming",
            LiveState::Yielded => "yielded",
        }
    }
}

/// One request in flight.
#[derive(Clone, Debug)]
pub struct LiveRow {
    pub id: u64,
    pub app: String,
    pub method: String,
    pub path: String,
    pub arrived_ns: u64,
    pub state: LiveState,
    /// The worker running it, or the last that did.
    pub worker: Option<u32>,
    /// The host call it is parked at.
    pub op: Option<&'static str>,
    /// Since when it has been in its state.
    pub since_ns: u64,
}

impl LiveRow {
    fn of(track: &Track, names: &[Arc<str>]) -> Option<LiveRow> {
        let state = track.state.with(|state| state.clone());
        if state.end.is_some() {
            return None;
        }
        let mut worker = None;
        for event in &state.events {
            if let What::Run { worker: w, .. } = event.what {
                worker = Some(w);
            }
        }
        let (live, op, since) = match state.events.last() {
            None => (
                LiveState::Waiting,
                None,
                state.queued_ns.unwrap_or(track.arrived_ns),
            ),
            Some(event) => match event.what {
                What::Run { .. } => (LiveState::Running, None, event.at_ns),
                What::Park { op } => (LiveState::Parked, Some(op), event.at_ns),
                What::Ready { .. } => (LiveState::Resuming, None, event.at_ns),
                What::Yield => (LiveState::Yielded, None, event.at_ns),
            },
        };
        Some(LiveRow {
            id: track.id,
            app: name_of(names, track.app),
            method: track.method.to_string(),
            path: track.path(),
            arrived_ns: track.arrived_ns,
            state: live,
            worker,
            op,
            since_ns: since,
        })
    }

    fn to_json(&self) -> Json {
        json!({
            "id": self.id,
            "app": self.app,
            "method": self.method,
            "path": self.path,
            "arrived_us": us(self.arrived_ns),
            "state": self.state.name(),
            "worker": self.worker,
            "op": self.op,
            "since_us": us(self.since_ns),
        })
    }
}

/// The requests in flight, and each worker's.
#[derive(Clone, Debug)]
pub struct Live {
    pub now_ns: u64,
    pub started_unix_ms: u64,
    /// In flight, all of them; `requests` is at most [`LIVE_ROWS`].
    pub total: usize,
    /// What each worker is running, by index.
    pub workers: Vec<Option<LiveRow>>,
    pub requests: Vec<LiveRow>,
}

impl Live {
    /// `GET /_host/live`.
    pub fn to_json(&self) -> Json {
        json!({
            "started_unix_ms": self.started_unix_ms,
            "now_us": us(self.now_ns),
            "total": self.total,
            "workers": self.workers.iter().enumerate().map(|(worker, row)| json!({
                "worker": worker,
                "request": row.as_ref().map(LiveRow::to_json),
            })).collect::<Vec<_>>(),
            "requests": self.requests.iter().map(LiveRow::to_json).collect::<Vec<_>>(),
        })
    }
}

/// A window's length from `over`: seconds (`300`), or with a unit
/// (`10s`, `1m`, `5m`); at most [`KEEP`].
pub fn parse_over(over: &str) -> Option<Duration> {
    let over = over.trim();
    let (number, unit) = match over.strip_suffix('m') {
        Some(minutes) => (minutes, 60),
        None => (over.strip_suffix('s').unwrap_or(over), 1),
    };
    let secs: u64 = number.parse().ok()?;
    (secs > 0).then(|| Duration::from_secs(secs * unit).min(KEEP))
}

/// The window `GET /_host/timeline` asks for: `until` (Unix milliseconds,
/// now if not given), and `since` (the same), or `over` before `until`
/// ([`parse_over`]; a minute if neither is given). Of `app`'s requests, or
/// every app's.
pub fn asked_window(
    recorder: &Recorder,
    app: Option<&str>,
    since_unix_ms: Option<u64>,
    until_unix_ms: Option<u64>,
    over: Option<&str>,
) -> Window {
    let now = recorder.now_ns();
    let started = recorder.started_unix_ms();
    let at = |unix_ms: u64| unix_ms.saturating_sub(started).saturating_mul(1_000_000);
    let until = until_unix_ms.map_or(now, at).min(now);
    let since = match since_unix_ms {
        Some(since) => at(since),
        None => {
            let over = over.and_then(parse_over).unwrap_or(Duration::from_secs(60));
            until.saturating_sub(over.as_nanos() as u64)
        }
    };
    recorder.window(app, since.min(until), until)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn recorder() -> Recorder {
        Recorder::fixed(2, 1_700_000_000_000, 100 * MS)
    }

    #[test]
    fn a_request_that_parks_and_resumes_elsewhere_is_phases() {
        let rec = recorder();
        let mut t = rec.admit(7, rec.app("slow"), "GET", "/slow/", 10 * MS, 11 * MS);
        t.run(12 * MS, 0, Began::Start);
        t.park(13 * MS, "timer.sleep");
        t.ready(23 * MS, By::Answer);
        t.run(25 * MS, 1, Began::Resume);
        t.yielded(26 * MS);
        t.run(27 * MS, 1, Began::Continue);
        t.meter(1234, 56);
        t.end(28 * MS, 200, None, Duration::from_millis(3));
        drop(t);
        let span = rec.request(7).unwrap();
        let kinds: Vec<_> = span
            .phases
            .iter()
            .map(|p| (p.kind, p.from_ns / MS, p.to_ns.map(|t| t / MS)))
            .collect();
        assert_eq!(
            kinds,
            [
                (PhaseKind::Wait, 11, Some(12)),
                (PhaseKind::Run, 12, Some(13)),
                (PhaseKind::Park, 13, Some(23)),
                (PhaseKind::Wait, 23, Some(25)),
                (PhaseKind::Run, 25, Some(26)),
                (PhaseKind::Wait, 26, Some(27)),
                (PhaseKind::Run, 27, Some(28)),
            ]
        );
        assert_eq!(span.phases[2].op, Some("timer.sleep"));
        assert_eq!(span.phases[2].by, Some(By::Answer));
        assert!(
            span.phases[4].moved,
            "resumed on worker 1 after running on 0"
        );
        assert!(!span.phases[6].moved, "continued on the same worker");
        assert_eq!(span.phases[5].after, Some("yield"));
        assert_eq!((span.yields, span.parks, span.moves), (1, 1, 1));
        assert_eq!(span.outcome(), "ok");
        assert_eq!(span.latency_ns(), Some(18 * MS));
        let end = span.end.unwrap();
        assert_eq!((end.instructions, end.heap_words), (Some(1234), Some(56)));
        assert!(rec.live().requests.is_empty());
    }

    #[test]
    fn rejections_and_cancels_are_recorded_with_their_reason() {
        let rec = recorder();
        rec.rejected(
            1,
            rec.app("hello"),
            "GET",
            "/hello/",
            5 * MS,
            5 * MS,
            429,
            "rate_limited",
        );
        let mut queued = rec.admit(2, rec.app("hello"), "POST", "/hello/", 6 * MS, 6 * MS);
        queued.reject(7 * MS, 429, "queue_full");
        let mut parked = rec.admit(3, rec.app("slow"), "GET", "/slow/", 8 * MS, 8 * MS);
        parked.run(9 * MS, 1, Began::Start);
        parked.park(10 * MS, "fetch");
        parked.ready(40 * MS, By::Deadline);
        parked.end(41 * MS, 504, Some("deadline"), Duration::from_millis(1));
        for (id, outcome, status, reason) in [
            (1, "rejected", 429, "rate_limited"),
            (2, "rejected", 429, "queue_full"),
            (3, "error", 504, "deadline"),
        ] {
            let span = rec.request(id).unwrap();
            assert_eq!(span.outcome(), outcome);
            let end = span.end.unwrap();
            assert_eq!((end.status, end.reason), (status, Some(reason)));
        }
        assert!(rec.request(1).unwrap().phases.is_empty());
        let last = rec.request(3).unwrap().phases;
        assert_eq!(last[2].by, Some(By::Deadline));
        let window = rec.window(None, 0, 100 * MS);
        assert_eq!(window.rejected(), 2);
        let slow = window.apps.iter().find(|a| a.app == "slow").unwrap();
        assert_eq!((slow.requests, slow.errors), (1, 1));
    }

    #[test]
    fn a_dropped_tracker_ends_its_request() {
        let rec = recorder();
        let t = rec.admit(4, rec.app("hello"), "GET", "/", MS, MS);
        assert_eq!(rec.live().total, 1);
        drop(t);
        let span = rec.request(4).unwrap();
        assert_eq!(span.end.unwrap().reason, Some("dropped"));
        assert_eq!(rec.live().total, 0);
    }

    #[test]
    fn live_says_where_each_request_is() {
        let rec = recorder();
        let _waiting = rec.admit(1, rec.app("a"), "GET", "/a", MS, MS);
        let running = rec.admit(2, rec.app("a"), "GET", "/b", 2 * MS, 2 * MS);
        running.run(3 * MS, 1, Began::Start);
        let parked = rec.admit(3, rec.app("b"), "GET", "/c", 3 * MS, 3 * MS);
        parked.run(4 * MS, 0, Began::Start);
        parked.park(5 * MS, "fetch");
        let yielded = rec.admit(4, rec.app("b"), "GET", "/d", 4 * MS, 4 * MS);
        yielded.run(5 * MS, 0, Began::Start);
        yielded.yielded(6 * MS);
        let live = rec.live();
        let states: Vec<_> = live
            .requests
            .iter()
            .map(|r| (r.id, r.state, r.worker, r.op))
            .collect();
        assert_eq!(
            states,
            [
                (1, LiveState::Waiting, None, None),
                (2, LiveState::Running, Some(1), None),
                (3, LiveState::Parked, Some(0), Some("fetch")),
                (4, LiveState::Yielded, Some(0), None),
            ]
        );
        assert!(live.workers[0].is_none());
        assert_eq!(live.workers[1].as_ref().map(|r| r.id), Some(2));
        // An in-flight request's detail runs to now.
        let span = rec.request(3).unwrap();
        assert_eq!(span.outcome(), "in_flight");
        assert_eq!(span.phases.last().unwrap().until(span.now_ns), 100 * MS);
    }

    #[test]
    fn the_count_bound_keeps_the_newest() {
        let rec = Recorder::with_bounds(1, KEEP, 64);
        for id in 0..1000 {
            let mut t = rec.admit(id, rec.app("a"), "GET", "/", 0u64, 0u64);
            t.end(0u64, 200, None, Duration::ZERO);
        }
        assert_eq!(rec.len(), 64);
        assert!(rec.request(935).is_none());
        assert!((936..1000).all(|id| rec.request(id).is_some()));
    }

    #[test]
    fn the_age_bound_forgets_what_ended_long_ago() {
        let rec = Recorder::fixed(1, 0, KEEP.as_nanos() as u64 + 50 * MS);
        for (id, ended) in [(1, 10 * MS), (2, 40 * MS), (3, 60 * MS), (4, 200 * MS)] {
            let mut t = rec.admit(id, rec.app("a"), "GET", "/", ended, ended);
            t.end(ended, 200, None, Duration::ZERO);
        }
        // The first two ended more than KEEP before now.
        assert!(rec.request(1).is_none());
        assert_eq!(rec.len(), 2);
        assert!(rec.request(3).is_some());
        // A live request is kept however old.
        let _old = rec.admit(5, rec.app("a"), "GET", "/", 0u64, 0u64);
        assert_eq!(rec.live().total, 1);
    }

    #[test]
    fn the_window_counts_the_workers_and_the_strips() {
        let rec = Recorder::fixed(2, 0, 100 * MS);
        // Two requests: one runs 0–50 ms on worker 0, one waits 0–50 and
        // runs 50–100 on worker 1.
        let mut a = rec.admit(1, rec.app("a"), "GET", "/", 0u64, 0u64);
        a.run(0u64, 0, Began::Start);
        a.end(50 * MS, 200, None, Duration::from_millis(50));
        let mut b = rec.admit(2, rec.app("b"), "GET", "/", 0u64, 0u64);
        b.run(50 * MS, 1, Began::Start);
        b.end(100 * MS, 200, None, Duration::from_millis(50));
        let window = rec.window(None, 0, 100 * MS);
        assert_eq!(window.busy_ns, 100 * MS);
        assert_eq!(window.utilisation_milli(), 500);
        assert_eq!(window.segments.len(), 2);
        assert_eq!(window.running.len(), STRIP_BUCKETS);
        assert_eq!(window.running[0], 1000);
        assert_eq!(window.waiting[0], 1000);
        assert_eq!(window.waiting[STRIP_BUCKETS - 1], 0);
        assert_eq!(window.running[STRIP_BUCKETS - 1], 1000);
        let b = window.apps.iter().find(|a| a.app == "b").unwrap();
        assert_eq!((b.p50_ns, b.p99_ns), (100 * MS, 100 * MS));
        // An app's own window, and a window that misses both.
        assert_eq!(rec.window(Some("a"), 0, 100 * MS).requests.len(), 1);
        assert!(rec.window(None, 101 * MS, 200 * MS).requests.is_empty());
    }

    #[test]
    fn segments_close_together_on_one_worker_merge() {
        let seg = |worker, from, to| Segment {
            worker,
            id: from,
            app: "a".into(),
            from_ns: from,
            to_ns: to,
            runs: 1,
        };
        let merged = merge_segments(
            &[seg(0, 0, 10), seg(1, 5, 8), seg(0, 12, 20), seg(0, 40, 50)],
            5,
        );
        assert_eq!(
            merged,
            [
                Segment {
                    runs: 2,
                    to_ns: 20,
                    ..seg(0, 0, 10)
                },
                seg(1, 5, 8),
                seg(0, 40, 50)
            ]
        );
    }

    #[test]
    fn paths_are_cut_and_windows_parsed() {
        let rec = recorder();
        let long = "/é".repeat(200);
        let _t = rec.admit(1, rec.app("a"), "GET", &long, 0u64, 0u64);
        let path = rec.request(1).unwrap().path;
        assert!(path.len() <= PATH_BYTES && long.starts_with(&path));
        assert_eq!(parse_over("10s"), Some(Duration::from_secs(10)));
        assert_eq!(parse_over("1m"), Some(Duration::from_secs(60)));
        assert_eq!(parse_over("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_over("1h"), None);
        assert_eq!(parse_over("60m"), Some(KEEP));
        assert_eq!(parse_over("0"), None);
    }
}
