//! The delivery loop under a test's hand.
//!
//! [`Harness`] puts a store, a clock and counters of its own beneath
//! [`Deliveries`] and drives it a step at a time, the steps
//! `docs/queue-properties.md` names apart: a tick (which buries and
//! leases), admission (accepted or turned away), the app's entry starting,
//! the answer (2xx-4xx, 5xx, none, or the scheduler's own 503 or 499 for a
//! run that never started), and the settle being written (or failing).
//! Nothing waits and nothing reads the real clock: a test says when time
//! moves, by how many milliseconds, and in which direction.
//!
//! A **crash** drops every delivery in flight without settling it, gives
//! the counters back as a fresh process has them, and opens the store
//! again from its file — what a process that died leaves behind.
//!
//! Beside the store the harness keeps the **ghost facts** the contract is
//! stated in and the store cannot say: per message, the leases it was
//! handed out under, the invocations (a run's entry starting), the settles
//! written and the ones that failed, and the deliveries a crash took.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use super::{Deliveries, Store};
use crate::config::QueuePolicy;
use crate::convert::{Origin, Reply};
use crate::logs::LogRing;
use crate::queue::{Message, Peeked, Queues, Settle, Unwritten};
use crate::stats::{AppCounters, Rejection};

/// A settle, by the store operation that writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Done,
    Retry,
    GiveBack,
    Bury,
}

/// What is known of one message beyond what the store holds.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// The lease ids it was handed out under, oldest first.
    pub leases: Vec<i64>,
    /// Admissions the host turned away.
    pub refused: u32,
    /// Runs whose entry started.
    pub invocations: u32,
    /// The lease ids of those runs.
    pub started: Vec<i64>,
    /// Settles that reached the store, and whether the lease still
    /// matched (`true`: the write changed the message).
    pub written: Vec<(Op, bool)>,
    /// Settles whose write failed.
    pub failed: Vec<Op>,
    /// Deliveries a crash took before the app's entry started.
    pub crashed_unstarted: u32,
    /// Their lease ids.
    pub lost_unstarted: Vec<i64>,
    /// Deliveries a crash took after it started.
    pub crashed_started: u32,
    /// The `x-minicloud-attempts` each admitted delivery carried.
    pub told_attempts: Vec<String>,
}

impl Facts {
    /// `give_back`s that took an attempt back.
    pub fn given_back(&self) -> u32 {
        self.written
            .iter()
            .filter(|(op, matched)| *op == Op::GiveBack && *matched)
            .count() as u32
    }
}

type Ghost = Arc<Mutex<BTreeMap<i64, Facts>>>;

/// How the next settles of each message are to go, in order: `false`
/// fails one, `true` lets one be written. Past the plan, each is written.
/// Keyed by message and, for a plan of one delivery's settles, its lease;
/// a delivery's own plan comes before its message's.
type FailPlan = Arc<Mutex<BTreeMap<(i64, Option<i64>), VecDeque<bool>>>>;

/// Every `lease` the loop asked for, as (tick, queue, room).
type LeasesAsked = Arc<Mutex<Vec<(u32, String, usize)>>>;

/// [`Queues`], recording every settle and lease and failing the settles a
/// test asked to fail. A settle that fails reaches no row: it answers
/// `Err` as a SQLite error would.
pub struct Faulty {
    inner: Queues,
    ghost: Ghost,
    /// Settles to fail, per message, before the next one is written.
    fail: FailPlan,
    /// Every `lease` the loop asked for.
    leases_asked: LeasesAsked,
    tick: Arc<AtomicU64>,
    /// The host's clock, and how far each `bury_spent` moves it.
    ms: Arc<AtomicI64>,
    slow_bury_spent: Arc<AtomicI64>,
}

impl Faulty {
    fn settle(
        &self,
        id: i64,
        lease_id: i64,
        op: Op,
        write: impl FnOnce(&Queues) -> Result<bool, String>,
    ) -> Result<bool, String> {
        let mut fail = self.fail.lock().unwrap();
        let takes = fail
            .get_mut(&(id, Some(lease_id)))
            .and_then(VecDeque::pop_front)
            .or_else(|| fail.get_mut(&(id, None)).and_then(VecDeque::pop_front));
        if takes == Some(false) {
            self.ghost
                .lock()
                .unwrap()
                .entry(id)
                .or_default()
                .failed
                .push(op);
            return Err("disk I/O error".to_string());
        }
        drop(fail);
        let written = write(&self.inner);
        if let Ok(matched) = written {
            self.ghost
                .lock()
                .unwrap()
                .entry(id)
                .or_default()
                .written
                .push((op, matched));
        }
        written
    }
}

impl Store for Faulty {
    fn bury_spent(
        &self,
        queue: &str,
        max_attempts: i64,
        now_ms: i64,
    ) -> Result<Vec<(Message, String)>, String> {
        let spent = self.inner.bury_spent(queue, max_attempts, now_ms);
        // The time `bury_spent` took, on the host's clock: what a tick that
        // waited on the store's lock, or a slow disk, would see.
        self.ms.fetch_add(
            self.slow_bury_spent.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        spent
    }
    fn bury_undeclared(
        &self,
        declared: &[&str],
        now_ms: i64,
    ) -> Result<Vec<(Message, String)>, String> {
        self.inner.bury_undeclared(declared, now_ms)
    }
    fn lease(
        &self,
        queue: Option<&str>,
        now_ms: i64,
        limit: usize,
        lease_ms: i64,
        max_attempts: i64,
    ) -> Result<Vec<Message>, String> {
        self.leases_asked.lock().unwrap().push((
            self.tick.load(Ordering::Relaxed) as u32,
            queue.unwrap_or_default().to_string(),
            limit,
        ));
        self.inner
            .lease(queue, now_ms, limit, lease_ms, max_attempts)
    }
    fn done(&self, id: i64, lease_id: i64) -> Result<bool, String> {
        self.settle(id, lease_id, Op::Done, |q| q.done(id, lease_id))
    }
    fn retry(&self, id: i64, lease_id: i64, now_ms: i64, delay_ms: i64) -> Result<bool, String> {
        self.settle(id, lease_id, Op::Retry, |q| {
            q.retry(id, lease_id, now_ms, delay_ms)
        })
    }
    fn give_back(
        &self,
        id: i64,
        lease_id: i64,
        now_ms: i64,
        delay_ms: i64,
    ) -> Result<bool, String> {
        self.settle(id, lease_id, Op::GiveBack, |q| {
            q.give_back(id, lease_id, now_ms, delay_ms)
        })
    }
    fn bury(&self, id: i64, lease_id: i64, now_ms: i64, why: &str) -> Result<bool, String> {
        self.settle(id, lease_id, Op::Bury, |q| {
            q.bury(id, lease_id, now_ms, why)
        })
    }
    fn hold(&mut self, id: i64, lease_id: i64, unwritten: Unwritten) {
        self.inner.hold(id, lease_id, unwritten)
    }
    fn forget(&mut self, id: i64, lease_id: i64) {
        self.inner.forget(id, lease_id)
    }
    fn unwritten(&self) -> Vec<(i64, i64, Unwritten)> {
        self.inner.unwritten()
    }
}

/// Where one delivery has got to.
pub enum Stage {
    /// Leased, and not yet admitted: only while [`Harness::hold`] is set,
    /// so that a crash can fall between the two.
    Leased,
    /// Admitted: `reply` is the run's end of the oneshot, `None` once the
    /// run has answered or gone.
    Admitted {
        reply: Option<oneshot::Sender<Reply>>,
        answer: oneshot::Receiver<Reply>,
        started: bool,
    },
}

/// One delivery in flight: the policy it was started under, the message
/// as it was leased, and where it has got to.
pub struct Pending {
    pub policy: QueuePolicy,
    pub message: Message,
    pub stage: Stage,
}

/// A delivery loop, its store and its clock, under a test's hand.
pub struct Harness {
    dir: PathBuf,
    path: PathBuf,
    /// The host's clock, in milliseconds since the epoch, which the loop
    /// reads as the host's `now_ms()` does.
    pub ms: Arc<AtomicI64>,
    /// The queues as the current version declares them; a redeploy is a
    /// new list.
    pub queues: Vec<QueuePolicy>,
    deliveries: Deliveries<Faulty>,
    pub pending: Vec<Pending>,
    /// Whether the host admits a delivery's run; asked once per admission.
    pub admits: Box<dyn FnMut(&Message) -> bool>,
    /// Leave each leased message unadmitted until [`Harness::admit_held`].
    pub hold: bool,
    pub ghost: Ghost,
    fail: FailPlan,
    leases_asked: LeasesAsked,
    tick: Arc<AtomicU64>,
    /// How many milliseconds of the host's clock each `bury_spent` takes:
    /// 0 unless a test wants a tick slow enough for a lease to run out in
    /// the middle of it.
    pub slow_bury_spent: Arc<AtomicI64>,
    /// `queue_in_flight` as each tick left it, oldest first: what the
    /// next tick's room is counted from.
    pub in_flight_after_tick: Vec<u64>,
    pub crashes: u32,
}

/// The queues an `app.toml` declares, as the host reads them.
pub fn policies(toml: &str) -> Vec<QueuePolicy> {
    crate::config::parse_app(toml, "post")
        .expect("an app.toml the host takes")
        .queues
}

impl Harness {
    /// A loop over `queues`, with an empty store in a file of its own and
    /// the clock at 1 000 000 s exactly.
    pub fn new(name: &str, queues: Vec<QueuePolicy>) -> Harness {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "minicloud-queue-loop-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("queue.sqlite3");
        let ms = Arc::new(AtomicI64::new(1_000_000_000));
        let ghost = Ghost::default();
        let fail = Arc::default();
        let leases_asked = Arc::default();
        let tick = Arc::default();
        let slow_bury_spent = Arc::default();
        let deliveries = Harness::open(
            &path,
            &ms,
            &ghost,
            &fail,
            &leases_asked,
            &tick,
            &slow_bury_spent,
        );
        Harness {
            dir,
            path,
            ms,
            queues,
            deliveries,
            pending: Vec::new(),
            admits: Box::new(|_| true),
            hold: false,
            ghost,
            fail,
            leases_asked,
            tick,
            slow_bury_spent,
            in_flight_after_tick: Vec::new(),
            crashes: 0,
        }
    }

    /// What a starting process has: the store opened from its file,
    /// counters at nothing, an empty log.
    fn open(
        path: &Path,
        ms: &Arc<AtomicI64>,
        ghost: &Ghost,
        fail: &FailPlan,
        leases_asked: &LeasesAsked,
        tick: &Arc<AtomicU64>,
        slow_bury_spent: &Arc<AtomicI64>,
    ) -> Deliveries<Faulty> {
        let clock = Arc::clone(ms);
        Deliveries {
            store: Arc::new(Mutex::new(Faulty {
                inner: Queues::open(path).unwrap(),
                ghost: Arc::clone(ghost),
                fail: Arc::clone(fail),
                leases_asked: Arc::clone(leases_asked),
                tick: Arc::clone(tick),
                ms: Arc::clone(ms),
                slow_bury_spent: Arc::clone(slow_bury_spent),
            })),
            counters: Arc::new(AppCounters::default()),
            logs: Arc::new(LogRing::default()),
            // The host's `now_ms()`.
            now: Arc::new(move || clock.load(Ordering::Relaxed)),
        }
    }

    // -------------------------------------------------------- the clock

    pub fn now_ms(&self) -> i64 {
        self.ms.load(Ordering::Relaxed)
    }

    /// The clock in whole seconds, the fraction cut off, for a test that
    /// thinks in seconds; the window's times are seconds too.
    pub fn now(&self) -> i64 {
        self.now_ms().div_euclid(1000)
    }

    pub fn set_ms(&mut self, ms: i64) {
        self.ms.store(ms, Ordering::Relaxed);
    }

    pub fn advance_ms(&mut self, by: i64) {
        self.ms.fetch_add(by, Ordering::Relaxed);
    }

    pub fn advance(&mut self, secs: i64) {
        self.advance_ms(secs * 1000);
    }

    // ------------------------------------------------------ the app side

    /// `queue.send`, as [`super::QueueHost`] makes it.
    pub fn send(&mut self, queue: &str, body: &str) -> i64 {
        self.send_after(queue, body, 0)
    }

    /// `queue.sendAfter`, as [`super::QueueHost`] makes it: sent at the
    /// host's `now()`, due `delay` seconds after that.
    pub fn send_after(&mut self, queue: &str, body: &str, delay: i64) -> i64 {
        self.try_send_after(queue, body, delay).unwrap()
    }

    /// [`Harness::send_after`], answering the store's refusal (the queue
    /// is full) rather than panicking.
    pub fn try_send_after(&mut self, queue: &str, body: &str, delay: i64) -> Result<i64, String> {
        let policy = self
            .queues
            .iter()
            .find(|policy| policy.name == queue)
            .expect("a declared queue")
            .clone();
        let at = self.now_ms();
        let store = self.deliveries.store.lock().unwrap();
        store.inner.send(
            queue,
            body,
            at,
            at.saturating_add(delay.max(0) * 1000),
            policy.max_body_bytes as usize,
            policy.max_depth,
        )
    }

    /// The admin's revive of dead letter `id`, as `admin.rs` makes it: at
    /// the host's clock, bounded by the `max_depth` the current version
    /// gives its queue, unbounded for a queue it does not declare.
    pub fn revive(&mut self, id: i64) -> crate::queue::Revived {
        let now = self.now_ms();
        let queues = self.queues.clone();
        let store = self.deliveries.store.lock().unwrap();
        store
            .inner
            .revive(id, now, |queue| {
                queues
                    .iter()
                    .find(|policy| policy.name == queue)
                    .map(|policy| policy.max_depth)
            })
            .unwrap()
    }

    // ------------------------------------------------------ the loop side

    /// One tick of the loop, at the clock as it stands.
    pub fn tick(&mut self) {
        self.tick.fetch_add(1, Ordering::Relaxed);
        let deliveries = self.deliveries.clone();
        let queues = self.queues.clone();
        let mut leased = Vec::new();
        let hold = self.hold;
        // `start` admits at once, as the host does, so that a refusal
        // gives back its in-flight count before the next queue's room.
        deliveries.tick(&queues, |policy, message| {
            self.ghost
                .lock()
                .unwrap()
                .entry(message.id)
                .or_default()
                .leases
                .push(message.lease_id.expect("a leased message has a lease"));
            if hold {
                leased.push(Pending {
                    policy: policy.clone(),
                    message,
                    stage: Stage::Leased,
                });
            } else if let Some(pending) = self.admit_one(policy, message) {
                leased.push(pending);
            }
        });
        self.pending.extend(leased);
        self.in_flight_after_tick.push(self.in_flight());
        self.check();
    }

    fn admit_one(&mut self, policy: &QueuePolicy, message: Message) -> Option<Pending> {
        let admitted = (self.admits)(&message);
        let mut reply = None;
        let mut told = String::new();
        let answer = self.deliveries.admit(policy, &message, |extra| {
            told = extra
                .iter()
                .find(|(name, _)| *name == crate::internal::ATTEMPTS_HEADER)
                .map(|(_, value)| value.clone())
                .unwrap_or_default();
            if !admitted {
                return Err(Rejection::QueueFull);
            }
            let (sender, answer) = oneshot::channel();
            reply = Some(sender);
            Ok(answer)
        });
        let mut ghost = self.ghost.lock().unwrap();
        let facts = ghost.entry(message.id).or_default();
        match answer {
            Some(answer) => {
                facts.told_attempts.push(told);
                Some(Pending {
                    policy: policy.clone(),
                    message,
                    stage: Stage::Admitted {
                        reply,
                        answer,
                        started: false,
                    },
                })
            }
            None => {
                facts.refused += 1;
                None
            }
        }
    }

    /// Admits every delivery [`Harness::hold`] kept back.
    pub fn admit_held(&mut self) {
        let held = std::mem::take(&mut self.pending);
        for pending in held {
            match pending.stage {
                Stage::Leased => {
                    if let Some(admitted) = self.admit_one(&pending.policy, pending.message) {
                        self.pending.push(admitted);
                    }
                }
                Stage::Admitted { .. } => self.pending.push(pending),
            }
        }
        self.check();
    }

    /// Message `id`'s delivery in flight: the one under `lease`, or (for
    /// `None`) the first.
    fn at(&self, id: i64, lease: Option<i64>) -> usize {
        self.pending
            .iter()
            .position(|pending| {
                pending.message.id == id
                    && lease.is_none_or(|l| pending.message.lease_id == Some(l))
            })
            .unwrap_or_else(|| panic!("message {id} has no delivery in flight under {lease:?}"))
    }

    fn find(&mut self, id: i64, lease: Option<i64>) -> &mut Pending {
        let at = self.at(id, lease);
        &mut self.pending[at]
    }

    /// The app's entry starts for message `id`'s delivery.
    pub fn start_run(&mut self, id: i64) {
        self.start(id, None);
    }

    /// The app's entry starts for the delivery of message `id` under
    /// `lease`, where a message whose lease ran out has several.
    pub fn start_run_of(&mut self, id: i64, lease: i64) {
        self.start(id, Some(lease));
    }

    fn start(&mut self, id: i64, lease: Option<i64>) {
        let pending = self.find(id, lease);
        let token = pending.message.lease_id;
        let Stage::Admitted { started, reply, .. } = &mut pending.stage else {
            panic!("message {id}'s delivery is not admitted");
        };
        assert!(!*started && reply.is_some(), "it starts once");
        *started = true;
        let mut ghost = self.ghost.lock().unwrap();
        let facts = ghost.entry(id).or_default();
        facts.invocations += 1;
        facts.started.extend(token);
    }

    fn reply(&mut self, id: i64, must_have_started: bool, with: Option<Reply>) {
        self.reply_of(id, None, must_have_started, with)
    }

    fn reply_of(
        &mut self,
        id: i64,
        lease: Option<i64>,
        must_have_started: bool,
        with: Option<Reply>,
    ) {
        let Stage::Admitted { started, reply, .. } = &mut self.find(id, lease).stage else {
            panic!("message {id}'s delivery is not admitted");
        };
        assert_eq!(*started, must_have_started, "message {id}'s run");
        let sender = reply.take().expect("a run answers once");
        if let Some(with) = with {
            let _ = sender.send(with);
        }
    }

    /// The app answers `status` to message `id`'s delivery. The settle is
    /// written at the next [`Harness::poll`].
    pub fn answer(&mut self, id: i64, status: u16) {
        self.answer_of(id, None, status);
    }

    /// [`Harness::answer`] for the delivery under `lease`, or the first.
    pub fn answer_of(&mut self, id: i64, lease: Option<i64>, status: u16) {
        self.reply_of(
            id,
            lease,
            true,
            Some(Reply::text(status, format!("answered {status}\n"))),
        );
    }

    /// The run ends with no answer: the flight is dropped (the host
    /// panicked in it, or the engine closed with it queued).
    pub fn drop_answer(&mut self, id: i64) {
        self.drop_answer_of(id, None);
    }

    /// [`Harness::drop_answer`] for the delivery under `lease`, or the
    /// first.
    pub fn drop_answer_of(&mut self, id: i64, lease: Option<i64>) {
        let started = match &self.find(id, lease).stage {
            Stage::Admitted { started, .. } => *started,
            Stage::Leased => panic!("not admitted"),
        };
        self.reply_of(id, lease, started, None);
    }

    /// The run waited for a worker past its deadline and was never
    /// started: the scheduler's own 503 (`sched.rs`, `QueueTimeout`).
    pub fn time_out_in_scheduler(&mut self, id: i64) {
        self.time_out_of(id, None);
    }

    /// [`Harness::time_out_in_scheduler`] for the delivery under `lease`,
    /// or the first.
    pub fn time_out_of(&mut self, id: i64, lease: Option<i64>) {
        let reply = Reply::text(
            503,
            "app `post`: the request waited 1200 ms for a worker, longer than its 1000 ms \
             deadline; it was not run\n",
        )
        .with_header("retry-after", "1")
        .made_by(Origin::NotStarted);
        self.reply_of(id, lease, false, Some(reply));
    }

    /// The run was cancelled while it waited, and never started: the
    /// scheduler's own 499.
    pub fn cancel_before_start(&mut self, id: i64) {
        self.reply(
            id,
            false,
            Some(Reply::text(499, "").made_by(Origin::NotStarted)),
        );
    }

    /// The run was cancelled after its entry started: the scheduler's 499
    /// again, now for a run the app had begun and not answered.
    pub fn cancel_mid_run(&mut self, id: i64) {
        self.reply(
            id,
            true,
            Some(Reply::text(499, "").made_by(Origin::Stopped)),
        );
    }

    /// Settles every delivery whose run has answered or gone, as the
    /// host's task for it would.
    pub fn poll(&mut self) {
        let deliveries = self.deliveries.clone();
        let pending = std::mem::take(&mut self.pending);
        for mut delivery in pending {
            let outcome = match &mut delivery.stage {
                Stage::Admitted { answer, .. } => match answer.try_recv() {
                    Ok(reply) => Some(Some(reply)),
                    Err(oneshot::error::TryRecvError::Closed) => Some(None),
                    Err(oneshot::error::TryRecvError::Empty) => None,
                },
                Stage::Leased => None,
            };
            match outcome {
                Some(outcome) => {
                    deliveries.settle(&delivery.policy, &delivery.message, outcome);
                }
                None => self.pending.push(delivery),
            }
        }
        self.check();
    }

    /// Settles message `id`'s delivery under `lease` (or its first) alone,
    /// as its task would once its run has answered or gone; the others are
    /// left as they are.
    pub fn settle_one(&mut self, id: i64, lease: Option<i64>) {
        let deliveries = self.deliveries.clone();
        let at = self.at(id, lease);
        let mut delivery = self.pending.remove(at);
        let Stage::Admitted { answer, .. } = &mut delivery.stage else {
            panic!("message {id}'s delivery is not admitted");
        };
        let outcome = match answer.try_recv() {
            Ok(reply) => Some(reply),
            Err(oneshot::error::TryRecvError::Closed) => None,
            Err(oneshot::error::TryRecvError::Empty) => {
                panic!("message {id}'s run has not answered")
            }
        };
        deliveries.settle(&delivery.policy, &delivery.message, outcome);
        self.check();
    }

    /// Starts, answers and settles message `id`'s delivery.
    pub fn take(&mut self, id: i64, status: u16) {
        self.start_run(id);
        self.answer(id, status);
        self.poll();
    }

    /// The process dies: every delivery in flight is gone unsettled, the
    /// counters are a fresh process's, and the store is opened again from
    /// its file. Answers the deliveries it took, as (id, lease id).
    pub fn crash(&mut self) -> Vec<(i64, i64)> {
        let lost: Vec<_> = std::mem::take(&mut self.pending);
        // A give-back held because its write failed is a delivery that
        // never started, lost with the process like those in flight.
        let held: Vec<(i64, i64)> = self
            .store()
            .unwritten()
            .into_iter()
            .filter(|(_, _, unwritten)| matches!(unwritten.settle, Settle::GiveBack { .. }))
            .map(|(id, lease, _)| (id, lease))
            .collect();
        {
            let mut ghost = self.ghost.lock().unwrap();
            for pending in &lost {
                let facts = ghost.entry(pending.message.id).or_default();
                match &pending.stage {
                    Stage::Admitted { started: true, .. } => facts.crashed_started += 1,
                    _ => {
                        facts.crashed_unstarted += 1;
                        facts.lost_unstarted.extend(pending.message.lease_id);
                    }
                }
            }
            for (id, lease) in held {
                let facts = ghost.entry(id).or_default();
                facts.crashed_unstarted += 1;
                facts.lost_unstarted.push(lease);
            }
        }
        let taken = lost
            .iter()
            .map(|pending| (pending.message.id, super::token(&pending.message)))
            .collect();
        drop(lost);
        // The old connection goes before the new one comes, as the
        // process's would.
        self.deliveries = Harness::open(
            &self.path,
            &self.ms,
            &self.ghost,
            &self.fail,
            &self.leases_asked,
            &self.tick,
            &self.slow_bury_spent,
        );
        self.crashes += 1;
        self.check();
        taken
    }

    /// The host stops, as `Host`'s drop does it: the engine closes, which
    /// drops every run that has not answered, and each delivery's task —
    /// which the runtime may still run before it stops — finds its answer
    /// never came with the engine closed; then the process ends. A run that
    /// had answered is settled first, as its task would. Answers what the
    /// process's end took, as [`Harness::crash`] does.
    pub fn shut_down(&mut self) -> Vec<(i64, i64)> {
        let deliveries = self.deliveries.clone();
        for mut delivery in std::mem::take(&mut self.pending) {
            let Stage::Admitted {
                reply,
                answer,
                started,
            } = &mut delivery.stage
            else {
                self.pending.push(delivery);
                continue;
            };
            // The engine drops the flight, and its end of the oneshot.
            reply.take();
            match answer.try_recv() {
                Ok(answered) => {
                    deliveries.settle(&delivery.policy, &delivery.message, Some(answered))
                }
                Err(_) => {
                    deliveries.abandon(&delivery.message);
                    let mut ghost = self.ghost.lock().unwrap();
                    let facts = ghost.entry(delivery.message.id).or_default();
                    match started {
                        true => facts.crashed_started += 1,
                        false => {
                            facts.crashed_unstarted += 1;
                            facts.lost_unstarted.extend(delivery.message.lease_id);
                        }
                    }
                }
            }
        }
        self.crash()
    }

    // ------------------------------------------------------- faults

    /// Fails the next `n` settles of message `id`.
    pub fn fail_settles(&mut self, id: i64, n: u32) {
        self.plan_settles(id, &vec![false; n as usize]);
    }

    /// The next settles of message `id`, after those already planned: each
    /// `false` fails, each `true` is written.
    pub fn plan_settles(&mut self, id: i64, outcomes: &[bool]) {
        self.plan(id, None, outcomes);
    }

    /// [`Harness::plan_settles`] for the settles of the delivery under
    /// `lease` alone, where a message has several.
    pub fn plan_settles_of(&mut self, id: i64, lease: i64, outcomes: &[bool]) {
        self.plan(id, Some(lease), outcomes);
    }

    fn plan(&mut self, id: i64, lease: Option<i64>, outcomes: &[bool]) {
        self.fail
            .lock()
            .unwrap()
            .entry((id, lease))
            .or_default()
            .extend(outcomes);
    }

    /// Planned settles no write has come to yet.
    pub fn planned_left(&self) -> usize {
        self.fail.lock().unwrap().values().map(VecDeque::len).sum()
    }

    // ------------------------------------------------------- reading

    pub fn counters(&self) -> &AppCounters {
        &self.deliveries.counters
    }

    pub fn in_flight(&self) -> u64 {
        self.counters().queue_in_flight.load(Ordering::Relaxed)
    }

    pub fn delivered(&self) -> u64 {
        self.counters().queue_delivered.load(Ordering::Relaxed)
    }

    pub fn logs(&self) -> String {
        self.deliveries.logs.tail(200)
    }

    /// The message as the store has it, on the queue or among the dead.
    pub fn row(&self, id: i64) -> Option<Peeked> {
        crate::queue::peek_one(&self.path, id, self.now_ms()).unwrap()
    }

    /// How many dead letters the store holds.
    pub fn dead_count(&self) -> usize {
        crate::queue::peek_dead(&self.path, 1000).unwrap().len()
    }

    /// The store's file, for a test that reads its rows itself.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn facts(&self, id: i64) -> Facts {
        self.ghost
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }

    /// The `lease` calls the loop made, as (tick, queue, room).
    pub fn leases_asked(&self) -> Vec<(u32, String, usize)> {
        self.leases_asked.lock().unwrap().clone()
    }

    /// The store itself, for a probe a test makes by hand.
    pub fn store(&self) -> std::sync::MutexGuard<'_, Faulty> {
        self.deliveries.store.lock().unwrap()
    }

    /// The in-flight count is exactly the deliveries in flight: raised by
    /// each lease, given back by each refusal and each settle, nothing
    /// after a crash — and never below nothing, which on a `u64` would
    /// show as a count near 2^64.
    fn check(&self) {
        let in_flight = self.in_flight();
        assert!(
            in_flight < u64::MAX / 2,
            "queue_in_flight went below 0: {in_flight}"
        );
        assert_eq!(
            in_flight,
            self.pending.len() as u64,
            "queue_in_flight against the deliveries in flight"
        );
        for policy in &self.queues {
            let of_queue = self
                .pending
                .iter()
                .filter(|pending| pending.message.queue == policy.name)
                .count() as u64;
            assert_eq!(
                self.counters().queue_in_flight_of(&policy.name),
                of_queue,
                "`{}`'s in-flight count against its deliveries in flight",
                policy.name
            );
        }
    }

    /// S6 for message `id`, from the store's row and the ghost facts:
    /// `attempts` = leases − give-backs (1), invocations ≤ attempts (2),
    /// and the gap between them is exactly the deliveries a crash took
    /// before their run started (3), beside those in flight now and not
    /// started yet.
    pub fn check_attempts(&self, id: i64) {
        let Some(row) = self.row(id) else { return };
        let facts = self.facts(id);
        let attempts = row.attempts;
        assert_eq!(
            attempts,
            facts.leases.len() as i64 - i64::from(facts.given_back()),
            "S6.1 for message {id}: attempts = leases - give-backs ({facts:?})"
        );
        assert!(
            i64::from(facts.invocations) <= attempts,
            "S6.2 for message {id}: invocations <= attempts ({facts:?})"
        );
        // A delivery in flight that has not started yet is a gap still
        // open, not one left behind.
        let unstarted = self
            .pending
            .iter()
            .filter(|pending| pending.message.id == id)
            .filter(|pending| !matches!(pending.stage, Stage::Admitted { started: true, .. }))
            .count() as i64;
        assert_eq!(
            attempts - i64::from(facts.invocations) - unstarted,
            i64::from(facts.crashed_unstarted),
            "S6.3 for message {id}: attempts - invocations comes from crashes alone ({facts:?})"
        );
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A queue's policy with the defaults `app.toml` gives, changed by `with`.
pub fn policy(name: &str, with: impl FnOnce(&mut QueuePolicy)) -> QueuePolicy {
    let mut policy = policies(&format!("[[queues]]\nname = \"{name}\"\n"))
        .pop()
        .unwrap();
    with(&mut policy);
    policy
}
