//! The queue store held to its contract (`docs/queue-properties.md`).
//!
//! A real [`Queues`], on a file so that a crash is a real reopen, is driven
//! by random sequences of operations, and after every one compared with a
//! model of the contract: what each call answers, every row of both tables,
//! what [`Queues::dead`], [`Queues::depths`] and the window's reads say, and
//! the invariants S1–S10 as far as the store alone can show them.
//!
//! The model is written from the doc, not from the SQL. Where the two are
//! known to disagree (F5: `revive` does not look at `max_depth`),
//! [`Contract`] says whether the model holds the store to the contract —
//! those runs fail on the code as it is, and are ignored with the
//! finding's id — or takes the finding as the code has it, so that
//! everything else is still checked on every run.
//!
//! Every settle names a token (N1): one a delivery holds, current or
//! stale. A settle that named none could not tell a message never leased
//! from one whose lease a `retry` or `give_back` ended, and the store no
//! longer takes one.
//!
//! The lease ids are the real `next_lease_id`'s, which is process-global:
//! a crash here reopens the file but not the process, so the counter goes
//! on rather than starting again from the clock, and the tests running
//! beside this one draw from it too. That keeps tokens unique (S4.3) as
//! the code means them; a real restart's fresh start is not exercised.
//!
//! The regressions below are the cases the model found, each cut down to
//! the calls that show it.
//!
//! `PROPTEST_CASES` sets how many sequences a run tries (64 by default, to
//! keep `cargo test` quick). A failing case is shrunk and printed, not
//! saved: the cases worth keeping are the named regressions.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestCaseResult, TestError, TestRunner};

use super::*;

const T: i64 = 1_000_000;
const BODY: usize = 1024;
const DEPTH: i64 = 100;

fn ids(messages: &[Message]) -> Vec<i64> {
    messages.iter().map(|m| m.id).collect()
}

// ------------------------------------------------------------ regressions

/// S5, F1: a message waiting for its retry when `max_attempts` is lowered to
/// its `attempts` is handed out again, past the new bound.
///
/// Sent; leased under `M = 2` (attempts 1); the delivery fails and retries,
/// which ends the lease; a redeploy lowers `M` to 1; the retry comes due.
/// The tick's `bury_spent(M = 1)` leaves it alone, since it holds no lease,
/// and `lease` hands it out with attempts 2 > 1. The contract: no lease is
/// issued at `attempts >= M`, and the message is a dead letter at the next
/// tick, with a reason that is not "no answer within the lease".
#[test]
fn lowering_max_attempts_stops_a_message_waiting_for_its_retry() {
    let q = Queues::in_memory().unwrap();
    let id = q.send("a", "x", T, T, BODY, DEPTH).unwrap();
    // M = 2: the tick, then the lease.
    assert!(q.bury_spent("a", 2, T).unwrap().is_empty());
    let taken = q.lease(Some("a"), T, 1, 30, 2).unwrap();
    assert_eq!((taken[0].id, taken[0].attempts), (id, 1));
    assert!(q.retry(id, taken[0].lease_id.unwrap(), T + 1, 10).unwrap());
    // Redeployed with M = 1; the retry comes due.
    let spent = q.bury_spent("a", 1, T + 11).unwrap();
    let leased = q.lease(Some("a"), T + 11, 1, 30, 1).unwrap();
    assert!(
        leased.is_empty(),
        "leased at attempts {} with max_attempts 1",
        leased[0].attempts
    );
    let spent: Vec<i64> = spent.iter().map(|(m, _)| m.id).collect();
    assert_eq!(spent, [id], "a dead letter at the next tick");
    let dead = q.dead(None, 10).unwrap();
    assert_eq!(dead[0].0.id, id);
    assert_ne!(dead[0].1, OUTLIVED, "its lease did not run out");
}

/// S5, F1 (the same root, without the tick): `lease` alone hands out a
/// message whose lease ran out on its last attempt.
///
/// The delivery loop always calls `bury_spent` first, which buries this
/// one, so the loop does not show it; the store does, since `lease` takes
/// no `M` and the bound is kept only by the caller's order of calls. Under
/// `M = 1`: sent, leased (attempts 1), the lease runs out, leased again
/// with attempts 2.
#[test]
fn lease_alone_does_not_hand_out_a_message_past_max_attempts() {
    let q = Queues::in_memory().unwrap();
    let id = q.send("a", "x", T, T, BODY, DEPTH).unwrap();
    assert_eq!(ids(&q.lease(Some("a"), T, 1, 30, 1).unwrap()), [id]);
    // M = 1, and nobody has called bury_spent.
    let again = q.lease(None, T + 31, 1, 30, 1).unwrap();
    assert!(
        again.is_empty(),
        "leased at attempts {} with max_attempts 1",
        again[0].attempts
    );
}

/// S10, F5: `revive` puts a dead letter back on a queue that already holds
/// `max_depth` messages.
///
/// `max_depth` 4: four sent, the fifth refused; one buried, one more sent,
/// so the queue is full again; reviving the dead one makes five.
#[test]
#[ignore = "F5: revive ignores max_depth; fixed when revive refuses a full queue"]
fn revive_does_not_fill_a_queue_past_max_depth() {
    let q = Queues::in_memory().unwrap();
    let first = q.send("a", "1", T, T, BODY, 4).unwrap();
    for body in ["2", "3", "4"] {
        q.send("a", body, T, T, BODY, 4).unwrap();
    }
    assert!(q.send("a", "5", T, T, BODY, 4).is_err());
    assert!(q.bury_unleased(first, T, "500 boom").unwrap());
    q.send("a", "5", T, T, BODY, 4).unwrap();
    let revived = q.revive(first, T);
    assert_eq!(q.depths().unwrap(), vec![("a".to_string(), 4, 1)]);
    assert!(!matches!(revived, Ok(true)), "revived into a full queue");
}

/// S1, S4, N1 (found by the model): a settle once took away a message whose
/// lease a `give_back` had ended, by naming no token.
///
/// `lease_id IS NULL` is both "never leased" and "lease ended by `retry` or
/// `give_back`", so `done(id, None)` matched a message a delivery had taken
/// and given back, and deleted it. A settle now names the lease it holds,
/// and the one a `give_back` ended matches nothing.
#[test]
fn a_settle_after_a_give_back_does_not_take_the_message() {
    let q = Queues::in_memory().unwrap();
    let id = q.send("b", "x", T, T, BODY, DEPTH).unwrap();
    let taken = q.lease(Some("b"), T, 1, 30, 3).unwrap();
    let token = taken[0].lease_id.unwrap();
    assert!(q.give_back(id, token, T, 0).unwrap());
    assert!(
        !q.done(id, token).unwrap(),
        "done after give_back deleted it"
    );
    assert_eq!(q.depths().unwrap(), vec![("b".to_string(), 1, 0)]);
}

/// S6, N1: a `give_back` once took back an attempt whose delivery had
/// reached the app, by naming no token after a `retry` had ended the lease.
/// S6.2, invocations ≤ attempts, then no longer held.
#[test]
fn a_give_back_after_a_retry_does_not_take_back_the_answered_attempt() {
    let q = Queues::in_memory().unwrap();
    let id = q.send("a", "x", T, T, BODY, DEPTH).unwrap();
    let taken = q.lease(Some("a"), T, 1, 30, 3).unwrap();
    let token = taken[0].lease_id.unwrap();
    assert!(q.retry(id, token, T, 0).unwrap());
    assert!(!q.give_back(id, token, T, 0).unwrap());
    let again = q.lease(Some("a"), T, 1, 30, 3).unwrap();
    assert_eq!(again[0].attempts, 2, "the answered attempt still counts");
}

// ------------------------------------------------------------ the model

/// The queues the runs use: two, so that S9 has something to mix.
const QUEUES: [&str; 2] = ["a", "b"];

/// A queue's policy as the current version of the app says it.
#[derive(Clone, Copy, Debug)]
struct Policy {
    max_attempts: i64,
    lease: i64,
    max_depth: i64,
}

const FIRST_POLICY: Policy = Policy {
    max_attempts: 3,
    lease: 30,
    max_depth: 4,
};

/// Which of the known findings the model holds the store to the contract
/// on; `false` takes the finding as the code has it.
#[derive(Clone, Copy, Debug)]
struct Contract {
    /// S10 / F5: `revive` refuses a queue that holds `max_depth` already.
    s10: bool,
}

/// Why a dead letter is dead, as the contract tells the causes apart (S7).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Why {
    /// Buried by the delivery holding its lease, with what it said.
    Said(String),
    /// Its lease ran out on its last attempt (S7 b).
    Outlived,
    /// `max_attempts` was lowered to or below its attempts (S7 c).
    Lowered,
}

#[derive(Clone, Debug)]
struct Live {
    queue: usize,
    body: String,
    attempts: i64,
    due: i64,
    sent: i64,
    token: Option<i64>,
    /// Ghosts, for this life (S6): leases issued, give-backs that took an
    /// attempt back, and leases whose delivery answered and asked to retry.
    leases: i64,
    given_back: i64,
    answered: i64,
}

#[derive(Clone, Debug)]
struct Dead {
    queue: usize,
    body: String,
    attempts: i64,
    due: i64,
    sent: i64,
    why: Why,
    buried: i64,
}

/// A delivery in flight: the message and the token it was leased under.
#[derive(Clone, Debug)]
struct Delivery {
    id: i64,
    token: i64,
}

#[derive(Debug)]
struct Model {
    contract: Contract,
    now: i64,
    policy: [Policy; 2],
    live: BTreeMap<i64, Live>,
    dead: BTreeMap<i64, Dead>,
    /// Every id a send has answered (S1, S3).
    sent_ids: BTreeSet<i64>,
    /// Every id `done` took away, and every dead letter pruned (S1).
    done: BTreeSet<i64>,
    pruned: BTreeSet<i64>,
    /// Every token a lease has handed out (S4.3).
    tokens: BTreeSet<i64>,
    /// The deliveries the process holds, current and stale.
    deliveries: Vec<Delivery>,
    sends: usize,
}

impl Model {
    fn new(contract: Contract) -> Model {
        Model {
            contract,
            now: T,
            policy: [FIRST_POLICY; 2],
            live: BTreeMap::new(),
            dead: BTreeMap::new(),
            sent_ids: BTreeSet::new(),
            done: BTreeSet::new(),
            pruned: BTreeSet::new(),
            tokens: BTreeSet::new(),
            deliveries: Vec::new(),
            sends: 0,
        }
    }

    fn depth(&self, queue: usize) -> i64 {
        self.live.values().filter(|l| l.queue == queue).count() as i64
    }

    /// What a send at `now`, due `delay` later, must do: refuse a full
    /// queue (S10), or add one waiting message.
    fn may_send(&self, queue: usize) -> bool {
        self.depth(queue) < self.policy[queue].max_depth
    }

    /// The ids `bury_spent(queue, M)` at `now` must bury, oldest sent first,
    /// each with the cause.
    fn spent(&self, queue: usize) -> Vec<(i64, Why)> {
        let max = self.policy[queue].max_attempts;
        let mut spent: Vec<_> = self
            .live
            .iter()
            .filter(|(_, l)| l.queue == queue && l.due <= self.now && l.attempts >= max)
            .map(|(id, l)| {
                let why = if l.token.is_some() {
                    Why::Outlived
                } else {
                    Why::Lowered
                };
                ((l.sent, *id), why)
            })
            .collect();
        spent.sort_by_key(|(key, _)| *key);
        spent.into_iter().map(|((_, id), why)| (id, why)).collect()
    }

    /// The ids `lease(queue, now, limit, M)` must hand out: due (S8), of
    /// that queue (S9), below `M` (S5), oldest sent first.
    fn leasable(&self, queue: Option<usize>, limit: usize, max_attempts: i64) -> Vec<i64> {
        let mut due: Vec<_> = self
            .live
            .iter()
            .filter(|(_, l)| l.due <= self.now && queue.is_none_or(|q| l.queue == q))
            .filter(|(_, l)| l.attempts < max_attempts)
            .map(|(id, l)| (l.sent, *id))
            .collect();
        due.sort();
        due.truncate(limit.clamp(1, MAX_LEASE));
        due.into_iter().map(|(_, id)| id).collect()
    }

    /// Whether a settle naming `token` is the current holder's (S4.2).
    fn holds(&self, id: i64, token: i64) -> bool {
        self.live.get(&id).is_some_and(|l| l.token == Some(token))
    }

    fn bury(&mut self, id: i64, why: Why) {
        let l = self.live.remove(&id).unwrap();
        self.dead.insert(
            id,
            Dead {
                queue: l.queue,
                body: l.body,
                attempts: l.attempts,
                due: l.due,
                sent: l.sent,
                why,
                buried: self.now,
            },
        );
    }
}

// ------------------------------------------------------------ operations

#[derive(Clone, Copy, Debug)]
enum Settle {
    Done,
    Retry,
    Bury,
    GiveBack,
}

/// Which token a settle names.
#[derive(Clone, Copy, Debug)]
enum Pick {
    /// One a delivery holds that is still its message's.
    Fresh(usize),
    /// One a delivery holds that a later lease, a settle or a revive has
    /// replaced.
    Stale(usize),
}

#[derive(Clone, Copy, Debug)]
enum Change {
    MaxAttempts(i64),
    Lease(i64),
    MaxDepth(i64),
}

#[derive(Clone, Debug)]
enum Op {
    /// `send` (`delay` 0) or `sendAfter`.
    Send {
        queue: usize,
        delay: i64,
    },
    /// The delivery loop's tick for one queue: `bury_spent`, then `lease`.
    Tick {
        queue: usize,
        limit: usize,
    },
    /// `lease` alone, of one queue or all.
    Lease {
        queue: Option<usize>,
        limit: usize,
    },
    Settle {
        how: Settle,
        pick: Pick,
        delay: i64,
    },
    Revive(usize),
    /// `prune_dead` of what was buried before `now + offset`.
    Prune(i64),
    Forward(i64),
    /// Past the longest lease any queue has.
    PastLease,
    Back(i64),
    /// A redeploy: the queue's policy for every later call.
    Redeploy {
        queue: usize,
        change: Change,
    },
    /// The process dies: the store is reopened from its file, and every
    /// delivery is gone.
    Crash,
    /// The operation with another connection holding the write lock, so
    /// that its write fails (`SQLITE_BUSY`).
    Busy(Box<Op>),
}

fn store_op() -> impl Strategy<Value = Op> {
    let queue = 0..QUEUES.len();
    let pick = prop_oneof![
        3 => any::<usize>().prop_map(Pick::Fresh),
        2 => any::<usize>().prop_map(Pick::Stale),
    ];
    let how = prop_oneof![
        Just(Settle::Done),
        Just(Settle::Retry),
        Just(Settle::Bury),
        Just(Settle::GiveBack),
    ];
    prop_oneof![
        4 => (queue.clone(), prop_oneof![Just(0i64), 0..20i64])
            .prop_map(|(queue, delay)| Op::Send { queue, delay }),
        4 => (queue.clone(), 1..4usize).prop_map(|(queue, limit)| Op::Tick { queue, limit }),
        1 => (proptest::option::of(queue.clone()), 0..4usize)
            .prop_map(|(queue, limit)| Op::Lease { queue, limit }),
        6 => (how, pick, 0..20i64).prop_map(|(how, pick, delay)| Op::Settle { how, pick, delay }),
        1 => any::<usize>().prop_map(Op::Revive),
        1 => (-20..20i64).prop_map(Op::Prune),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    let change = prop_oneof![
        (1..5i64).prop_map(Change::MaxAttempts),
        (1..60i64).prop_map(Change::Lease),
        (1..6i64).prop_map(Change::MaxDepth),
    ];
    prop_oneof![
        16 => store_op(),
        3 => (1..6i64).prop_map(Op::Forward),
        2 => Just(Op::PastLease),
        1 => (1..40i64).prop_map(Op::Back),
        2 => (0..QUEUES.len(), change).prop_map(|(queue, change)| Op::Redeploy { queue, change }),
        1 => Just(Op::Crash),
        1 => store_op().prop_map(|op| Op::Busy(Box::new(op))),
    ]
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(op(), 1..48)
}

// ------------------------------------------------------------ the harness

/// Fails the case, with what was expected, unless `$cond`.
macro_rules! check {
    ($cond:expr, $($why:tt)+) => {
        if !$cond {
            return Err(TestCaseError::fail(format!($($why)+)));
        }
    };
}

fn sql(e: impl std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(format!("store error: {e}"))
}

/// A row of `message`, as the model has it: id, queue, body, attempts, due,
/// sent, lease id.
type MessageRow = (i64, String, String, i64, i64, i64, Option<i64>);
/// A row of `dead`: id, queue, body, attempts, due, sent, why, buried.
type DeadRow = (i64, String, String, i64, i64, i64, String, i64);

struct Harness {
    dir: PathBuf,
    path: PathBuf,
    q: Option<Queues>,
    m: Model,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.q = None;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Harness {
    fn new(contract: Contract) -> Harness {
        static RUN: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "minicloud-queue-model-{}-{}",
            std::process::id(),
            RUN.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("queue.sqlite3");
        let mut harness = Harness {
            dir,
            path,
            q: None,
            m: Model::new(contract),
        };
        harness.reopen();
        harness
    }

    fn reopen(&mut self) {
        self.q = None;
        let q = Queues::open(&self.path).unwrap();
        // Not waiting on a lock, so that a write the test blocks fails at
        // once rather than after rusqlite's default five seconds. The
        // store's own code is unchanged; only its connection's setting is.
        q.conn.busy_timeout(Duration::ZERO).unwrap();
        self.q = Some(q);
    }

    fn q(&self) -> &Queues {
        self.q.as_ref().unwrap()
    }

    fn run(&mut self, ops: &[Op]) -> TestCaseResult {
        for (n, op) in ops.iter().enumerate() {
            self.step(op, false)
                .and_then(|()| self.check())
                .map_err(|e| TestCaseError::fail(format!("step {n} ({op:?}): {e}")))?;
        }
        Ok(())
    }

    /// One operation, on the store and the model. `busy`: another
    /// connection holds the write lock, so every write must fail and change
    /// nothing.
    fn step(&mut self, op: &Op, busy: bool) -> TestCaseResult {
        let now = self.m.now;
        match op {
            Op::Send { queue, delay } => {
                let name = QUEUES[*queue];
                let body = format!("m{}", self.m.sends);
                self.m.sends += 1;
                let policy = self.m.policy[*queue];
                let got = self
                    .q()
                    .send(name, &body, now, now + delay, BODY, policy.max_depth);
                if busy {
                    check!(got.is_err(), "a send under a held lock answered {got:?}");
                    return Ok(());
                }
                if !self.m.may_send(*queue) {
                    check!(
                        got.is_err(),
                        "S10: a send to `{name}` at depth {} of max_depth {} answered {got:?}",
                        self.m.depth(*queue),
                        policy.max_depth
                    );
                    return Ok(());
                }
                let id = got.map_err(sql)?;
                let fresh = !self.m.sent_ids.contains(&id)
                    && !self.m.live.contains_key(&id)
                    && !self.m.dead.contains_key(&id);
                check!(fresh, "S3: send answered id {id}, which a message has had");
                self.m.sent_ids.insert(id);
                self.m.live.insert(
                    id,
                    Live {
                        queue: *queue,
                        body,
                        attempts: 0,
                        due: now + delay,
                        sent: now,
                        token: None,
                        leases: 0,
                        given_back: 0,
                        answered: 0,
                    },
                );
            }
            Op::Tick { queue, limit } => {
                let name = QUEUES[*queue];
                let policy = self.m.policy[*queue];
                let got = self.q().bury_spent(name, policy.max_attempts, now);
                if busy {
                    check!(
                        got.is_err(),
                        "bury_spent under a held lock answered {got:?}"
                    );
                } else {
                    let got = got.map_err(sql)?;
                    let want = self.m.spent(*queue);
                    let got_ids: Vec<_> = got.iter().map(|(m, _)| (m.id, m.attempts)).collect();
                    let want_ids: Vec<_> = want
                        .iter()
                        .map(|(id, _)| (*id, self.m.live[id].attempts))
                        .collect();
                    check!(
                        got_ids == want_ids,
                        "S5/S7: bury_spent(`{name}`, M = {}) buried (id, attempts) {got_ids:?}, \
                         the contract {want_ids:?}",
                        policy.max_attempts
                    );
                    for (message, _) in &got {
                        self.message_is(message)?;
                    }
                    for (id, why) in want {
                        self.m.bury(id, why);
                    }
                }
                self.lease(
                    Some(*queue),
                    *limit,
                    policy.lease,
                    policy.max_attempts,
                    busy,
                )?;
            }
            Op::Lease { queue, limit } => {
                let policy = self.m.policy[queue.unwrap_or(0)];
                self.lease(*queue, *limit, policy.lease, policy.max_attempts, busy)?;
            }
            Op::Settle { how, pick, delay } => {
                let Some((at, id, token)) = self.pick(*pick) else {
                    return Ok(());
                };
                let q = self.q();
                let got = match how {
                    Settle::Done => q.done(id, token),
                    Settle::Retry => q.retry(id, token, now, *delay),
                    Settle::Bury => q.bury(id, token, now, "500 boom"),
                    Settle::GiveBack => q.give_back(id, token, now, *delay),
                };
                if busy {
                    check!(got.is_err(), "{how:?} under a held lock answered {got:?}");
                    return Ok(());
                }
                let got = got.map_err(sql)?;
                // A delivery settles once.
                self.m.deliveries.remove(at);
                let holds = self.m.holds(id, token);
                check!(
                    got == holds,
                    "S4.2: {how:?}({id}, {token:?}) answered {got}, the holder {:?}",
                    self.m.live.get(&id).map(|l| l.token)
                );
                if !holds {
                    return Ok(());
                }
                match how {
                    Settle::Done => {
                        self.m.live.remove(&id);
                        self.m.done.insert(id);
                    }
                    Settle::Bury => self.m.bury(id, Why::Said("500 boom".into())),
                    Settle::Retry | Settle::GiveBack => {
                        let l = self.m.live.get_mut(&id).unwrap();
                        l.due = now + delay;
                        l.token = None;
                        if matches!(how, Settle::GiveBack) {
                            if l.attempts > 0 {
                                l.attempts -= 1;
                                l.given_back += 1;
                            }
                        } else {
                            l.answered += 1;
                        }
                    }
                }
            }
            Op::Revive(pick) => {
                let id = if self.m.dead.is_empty() {
                    match self
                        .m
                        .sent_ids
                        .iter()
                        .nth(pick % self.m.sent_ids.len().max(1))
                    {
                        Some(id) => *id,
                        None => return Ok(()),
                    }
                } else {
                    *self.m.dead.keys().nth(pick % self.m.dead.len()).unwrap()
                };
                let got = self.q().revive(id, now);
                if busy {
                    check!(got.is_err(), "revive under a held lock answered {got:?}");
                    return Ok(());
                }
                let Some(dead) = self.m.dead.get(&id) else {
                    check!(got == Ok(false), "revive({id}) of no dead letter: {got:?}");
                    return Ok(());
                };
                let queue = dead.queue;
                if self.m.contract.s10 && !self.m.may_send(queue) {
                    check!(
                        !matches!(got, Ok(true)),
                        "S10: revive({id}) into `{}` at depth {} of max_depth {}",
                        QUEUES[queue],
                        self.m.depth(queue),
                        self.m.policy[queue].max_depth
                    );
                    return Ok(());
                }
                check!(got == Ok(true), "revive({id}) answered {got:?}");
                let dead = self.m.dead.remove(&id).unwrap();
                self.m.live.insert(
                    id,
                    Live {
                        queue,
                        body: dead.body,
                        attempts: 0,
                        due: now,
                        sent: dead.sent,
                        token: None,
                        leases: 0,
                        given_back: 0,
                        answered: 0,
                    },
                );
            }
            Op::Prune(offset) => {
                let before = now + offset;
                let got = self.q().prune_dead(before);
                if busy {
                    check!(
                        got.is_err(),
                        "prune_dead under a held lock answered {got:?}"
                    );
                    return Ok(());
                }
                let gone: Vec<i64> = self
                    .m
                    .dead
                    .iter()
                    .filter(|(_, d)| d.buried < before)
                    .map(|(id, _)| *id)
                    .collect();
                check!(
                    got == Ok(gone.len() as u64),
                    "prune_dead({before}) answered {got:?}, the contract {}",
                    gone.len()
                );
                for id in gone {
                    self.m.dead.remove(&id);
                    self.m.pruned.insert(id);
                }
            }
            Op::Forward(by) => self.m.now += by,
            Op::PastLease => self.m.now += self.m.policy.iter().map(|p| p.lease).max().unwrap() + 1,
            Op::Back(by) => self.m.now -= by,
            Op::Redeploy { queue, change } => {
                let policy = &mut self.m.policy[*queue];
                match *change {
                    Change::MaxAttempts(m) => policy.max_attempts = m,
                    Change::Lease(l) => policy.lease = l,
                    Change::MaxDepth(d) => policy.max_depth = d,
                }
            }
            Op::Crash => {
                self.reopen();
                self.m.deliveries.clear();
            }
            Op::Busy(inner) => {
                let holder = Connection::open(&self.path).map_err(sql)?;
                holder.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
                let outcome = self.step(inner, true);
                holder.execute_batch("ROLLBACK").map_err(sql)?;
                outcome?;
            }
        }
        Ok(())
    }

    /// `lease`, compared with what the model says it must hand out.
    fn lease(
        &mut self,
        queue: Option<usize>,
        limit: usize,
        lease: i64,
        max_attempts: i64,
        busy: bool,
    ) -> TestCaseResult {
        let now = self.m.now;
        let got = self
            .q()
            .lease(queue.map(|q| QUEUES[q]), now, limit, lease, max_attempts);
        if busy {
            check!(got.is_err(), "lease under a held lock answered {got:?}");
            return Ok(());
        }
        let got = got.map_err(sql)?;
        let want = self.m.leasable(queue, limit, max_attempts);
        check!(
            ids(&got) == want,
            "S5/S8/S9: lease({queue:?}, now {now}, limit {limit}) handed out {:?} \
             (attempts {:?}), the contract {want:?}",
            ids(&got),
            got.iter().map(|m| m.attempts).collect::<Vec<_>>()
        );
        for message in &got {
            let l = &self.m.live[&message.id];
            check!(
                l.due <= now,
                "S8: leased {} due {} at {now}",
                message.id,
                l.due
            );
            let token = message.lease_id;
            check!(
                token.is_some_and(|t| !self.m.tokens.contains(&t)),
                "S4.3: lease handed out {token:?}, a token not new"
            );
            let l = self.m.live.get_mut(&message.id).unwrap();
            l.attempts += 1;
            l.due = now + lease;
            l.token = token;
            l.leases += 1;
            self.m.tokens.insert(token.unwrap());
            self.m.deliveries.push(Delivery {
                id: message.id,
                token: token.unwrap(),
            });
            self.message_is(message)?;
        }
        Ok(())
    }

    /// Whether a message a call answered is the model's, as the live table
    /// has it (`bury_spent`'s before the model buries it, `lease`'s after
    /// the model has leased it).
    fn message_is(&self, message: &Message) -> TestCaseResult {
        let l = &self.m.live[&message.id];
        let want = (QUEUES[l.queue], &l.body, l.attempts, l.due, l.sent);
        let got = (
            message.queue.as_str(),
            &message.body,
            message.attempts,
            message.due_unix,
            message.sent_unix,
        );
        check!(
            got == want,
            "message {} answered {got:?}, the model {want:?}",
            message.id
        );
        Ok(())
    }

    /// The delivery index, id and token a settle names.
    fn pick(&self, pick: Pick) -> Option<(usize, i64, i64)> {
        let current = |d: &Delivery| self.m.holds(d.id, d.token);
        let among = |fresh: bool, n: usize| {
            let found: Vec<usize> = (0..self.m.deliveries.len())
                .filter(|&i| current(&self.m.deliveries[i]) == fresh)
                .collect();
            let at = *found.get(n % found.len().max(1))?;
            let d = &self.m.deliveries[at];
            Some((at, d.id, d.token))
        };
        match pick {
            Pick::Fresh(n) => among(true, n),
            Pick::Stale(n) => among(false, n),
        }
    }

    /// Every table, every read and every invariant, against the model.
    fn check(&self) -> TestCaseResult {
        let q = self.q();
        let m = &self.m;

        // The tables, row for row.
        let rows: Vec<MessageRow> = {
            let mut s = q
                .conn
                .prepare(
                    "SELECT id, queue, body, attempts, due, sent, lease_id FROM message ORDER BY id",
                )
                .map_err(sql)?;
            let rows = s
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)?
        };
        let want: Vec<MessageRow> = m
            .live
            .iter()
            .map(|(id, l)| {
                (
                    *id,
                    QUEUES[l.queue].to_string(),
                    l.body.clone(),
                    l.attempts,
                    l.due,
                    l.sent,
                    l.token,
                )
            })
            .collect();
        check!(rows == want, "message table {rows:?}, the model {want:?}");

        let dead_rows: Vec<DeadRow> = {
            let mut s = q
                .conn
                .prepare(
                    "SELECT id, queue, body, attempts, due, sent, why, buried FROM dead ORDER BY id",
                )
                .map_err(sql)?;
            let rows = s
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                })
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)?
        };
        check!(
            dead_rows.len() == m.dead.len(),
            "dead table {dead_rows:?}, the model {:?}",
            m.dead
        );
        for (row, (id, d)) in dead_rows.iter().zip(&m.dead) {
            let (rid, rqueue, rbody, rattempts, rdue, rsent, rwhy, rburied) = row;
            check!(
                (
                    *rid,
                    rqueue.as_str(),
                    rbody,
                    *rattempts,
                    *rdue,
                    *rsent,
                    *rburied
                ) == (
                    *id,
                    QUEUES[d.queue],
                    &d.body,
                    d.attempts,
                    d.due,
                    d.sent,
                    d.buried
                ),
                "dead row {row:?}, the model {id} {d:?}"
            );
            // S7: the reason is the one that happened.
            let truthful = match &d.why {
                Why::Said(said) => rwhy == said,
                Why::Outlived => rwhy == OUTLIVED,
                Why::Lowered => rwhy.starts_with("max_attempts lowered to "),
            };
            check!(
                truthful,
                "S7: dead letter {id} says {rwhy:?}, the cause {:?}",
                d.why
            );
        }

        // S2: one place.
        let both: i64 = q
            .conn
            .query_row(
                "SELECT count(*) FROM message JOIN dead USING (id)",
                [],
                |r| r.get(0),
            )
            .map_err(sql)?;
        check!(both == 0, "S2: {both} ids are on message and dead at once");

        // S1: every id sent is waiting, dead, done or pruned, and only one.
        for id in &m.sent_ids {
            let places = [
                m.live.contains_key(id),
                m.dead.contains_key(id),
                m.done.contains(id),
                m.pruned.contains(id),
            ];
            check!(
                places.iter().filter(|p| **p).count() == 1,
                "S1: message {id} is in {places:?} (waiting, dead, done, pruned)"
            );
        }

        for (id, l) in &m.live {
            // S6.1: attempts = leases issued in this life − give-backs.
            check!(
                l.attempts == l.leases - l.given_back,
                "S6.1: message {id} has attempts {}, {} leases and {} give-backs",
                l.attempts,
                l.leases,
                l.given_back
            );
            // S6.2, as far as the store sees invocations: deliveries that
            // answered and asked for a retry.
            check!(
                l.answered <= l.attempts,
                "S6.2: message {id} answered {} times on {} attempts",
                l.answered,
                l.attempts
            );
            // S4.1: at most one delivery holds the valid token.
            let holders = m
                .deliveries
                .iter()
                .filter(|d| d.id == *id && Some(d.token) == l.token)
                .count();
            check!(holders <= 1, "S4.1: {holders} deliveries hold message {id}");
        }

        // `dead`, newest buried first, for all and for each queue.
        let order = |queue: Option<usize>| -> Vec<i64> {
            let mut dead: Vec<_> = m
                .dead
                .iter()
                .filter(|(_, d)| queue.is_none_or(|q| d.queue == q))
                .map(|(id, d)| (d.buried, *id))
                .collect();
            dead.sort_by(|a, b| b.cmp(a));
            dead.into_iter().map(|(_, id)| id).collect()
        };
        for queue in [None, Some(0), Some(1)] {
            let got: Vec<i64> = q
                .dead(queue.map(|q| QUEUES[q]), 1000)
                .map_err(sql)?
                .iter()
                .map(|(m, _)| m.id)
                .collect();
            let want = order(queue);
            check!(got == want, "dead({queue:?}) {got:?}, the model {want:?}");
        }

        // `depths` and the window's counts.
        let mut depths = Vec::new();
        let mut window = Vec::new();
        for (n, name) in QUEUES.iter().enumerate() {
            let waiting: Vec<_> = m.live.values().filter(|l| l.queue == n).collect();
            let dead = m.dead.values().filter(|d| d.queue == n).count() as i64;
            if waiting.is_empty() && dead == 0 {
                continue;
            }
            depths.push((name.to_string(), waiting.len() as i64, dead));
            let due = waiting.iter().filter(|l| l.due <= m.now).count() as i64;
            window.push(Depth {
                name: name.to_string(),
                due,
                held: waiting.len() as i64 - due,
                dead,
            });
        }
        let got = q.depths().map_err(sql)?;
        check!(got == depths, "depths {got:?}, the model {depths:?}");
        let got = peek_depths(&self.path, m.now).map_err(sql)?;
        check!(got == window, "peek_depths {got:?}, the model {window:?}");

        // The window's listings, in their orders.
        let mut waiting: Vec<_> = m
            .live
            .iter()
            .map(|(id, l)| {
                let due = l.due <= m.now;
                ((!due, if due { l.sent } else { l.due }, *id), l)
            })
            .collect();
        waiting.sort_by_key(|(key, _)| *key);
        let want: Vec<_> = waiting
            .iter()
            .map(|((held, _, id), l)| (*id, l.attempts, l.due, if *held { "held" } else { "due" }))
            .collect();
        let got: Vec<_> = peek_waiting(&self.path, m.now, PEEK_MESSAGES)
            .map_err(sql)?
            .iter()
            .map(|p| (p.id, p.attempts, p.due_unix, p.state))
            .collect();
        check!(got == want, "peek_waiting {got:?}, the model {want:?}");
        let got: Vec<i64> = peek_dead(&self.path, PEEK_MESSAGES)
            .map_err(sql)?
            .iter()
            .map(|p| p.id)
            .collect();
        let want = order(None);
        check!(got == want, "peek_dead {got:?}, the model {want:?}");
        Ok(())
    }
}

// ------------------------------------------------------------ the runs

fn config() -> Config {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(64);
    Config {
        cases,
        // The cases worth keeping are written out as the regressions above.
        failure_persistence: None,
        ..Config::default()
    }
}

fn hold_to(contract: Contract) {
    let mut runner = TestRunner::new(config());
    let outcome = runner.run(&ops(), |ops| Harness::new(contract).run(&ops));
    match outcome {
        Ok(()) => {}
        Err(TestError::Fail(why, ops)) => panic!("{why}\nshrunk to {ops:#?}"),
        Err(e) => panic!("{e}"),
    }
}

/// Every property the store alone can show, with F5 as the code has it.
#[test]
fn the_store_keeps_its_contract_but_for_the_known_findings() {
    hold_to(Contract { s10: false });
}

/// S10 as the contract says it: `revive` does not fill a queue past `D`.
#[test]
#[ignore = "F5: revive ignores max_depth; fixed when revive refuses a full queue"]
fn the_store_keeps_s10() {
    hold_to(Contract { s10: true });
}
