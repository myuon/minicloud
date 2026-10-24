//! The `queue` module an app calls, and the host's delivery of what it
//! sends ([`crate::queue`] is the store beneath it).
//!
//! | operation | answers |
//! | --- | --- |
//! | `queue.send(name, body)` | `Result<Int, Error>`: the message's id |
//! | `queue.sendAfter(name, body, delaySecs)` | `Result<Int, Error>`: the message's id, delivered no sooner than `delaySecs` from now |
//! | `queue.depth(name)` | `Result<Int, Error>`: how many are waiting |
//!
//! A queue is declared in `app.toml` (`[[queues]]`) before it can be sent
//! to, so a misspelt name is an `Err` at the call and not a queue nobody
//! reads. Sending is a write to the app's own SQLite file and nothing else:
//! the message is on disk when `send` answers, so a host that stops between
//! the send and the delivery still has it.
//!
//! **Delivery is a run of the app** ([`crate::internal`]), at the queue's
//! path, with the message as the body and `x-minicloud-queue` and
//! `x-minicloud-attempt` saying which queue and which try. A run that
//! answers 2xx-4xx has taken the message; one that answers 5xx or does not
//! answer has not, and it is tried again after `retry`, doubling each time,
//! until `max_attempts` — then it is a **dead letter**, kept with the
//! reason rather than dropped. Only the app's own answer counts as one
//! ([`crate::convert::Origin`]): when the host answers in a run's place —
//! it was cancelled, or cut off at its deadline — the run did not answer,
//! whatever the status, and a run the host never started (it waited for a
//! worker past its deadline) is not an attempt at all: its message is
//! given back, as one turned away at admission is.
//!
//! **A delivery may happen twice.** A run that takes longer than `lease`
//! is taken to be lost and the message is tried again; if the first run
//! then finishes, the work has been done twice. The alternative — never
//! retrying — loses a message whenever the host stops mid-delivery, which
//! is worse. A consumer must be able to be asked twice, which is a
//! property of the work and not something the host can give it.
//!
//! What the first run's answer no longer does is settle the message. Only
//! the delivery holding the current lease does that ([`crate::queue`]'s
//! "Leases, not deletes"): a run that outlived its lease cannot delete the
//! message from under the delivery that has it now, move that one's lease,
//! or bury it while it is being taken. Its answer is said in the app's log
//! and counted as nothing. A lease that runs out counts as the attempt it
//! was, so a consumer that is always slower than `lease` does not have its
//! message go round for ever: when the lease of the last attempt runs out,
//! the message is a dead letter ("no answer within the lease") instead of
//! a next try. A delivery the host turns away before the app sees it is
//! not an attempt, and gives back the one its lease counted.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cove_runtime::{Effect, HostApi, HostType, ModuleSchema, OperationSchema, RuntimeError, Value};

use tokio::sync::oneshot;

use crate::config::QueuePolicy;
use crate::convert::{Origin, Reply};
use crate::hosts::{AppContext, HostModule};
use crate::logs::LogRing;
use crate::queue::{Message, Queues};
use crate::stats::{AppCounters, Rejection};

/// The capability that opens this module.
pub const CAPABILITY: &str = "queue";

/// The longest a message may be held back: seven days. A delay is in
/// seconds, and a caller that passes milliseconds by mistake would park a
/// message for decades, where nobody would look for it — an `Err` at the
/// call is kinder than a message that never comes.
const MAX_DELAY_SECS: i64 = 7 * 24 * 60 * 60;

const fn op(
    name: &'static str,
    params: &'static [HostType],
    result: HostType,
    effect: Effect,
) -> OperationSchema {
    OperationSchema {
        name,
        params,
        variadic: false,
        result,
        capability: CAPABILITY,
        effect,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

/// The `queue` module.
pub const QUEUE: ModuleSchema = ModuleSchema {
    name: "queue",
    capability: CAPABILITY,
    operations: &[
        op(
            "send",
            &[HostType::String, HostType::String],
            HostType::Result(&HostType::Int, &HostType::Error),
            Effect::ReversibleWrite,
        ),
        op(
            "sendAfter",
            &[HostType::String, HostType::String, HostType::Int],
            HostType::Result(&HostType::Int, &HostType::Error),
            Effect::ReversibleWrite,
        ),
        op(
            "depth",
            &[HostType::String],
            HostType::Result(&HostType::Int, &HostType::Error),
            Effect::Read,
        ),
    ],
    types: &[],
    resources: &[],
};

pub(crate) struct QueueModule;

impl HostModule for QueueModule {
    fn schema(&self) -> ModuleSchema {
        QUEUE
    }

    fn instantiate(&self, app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        // An app not granted `queue` never reaches a store, so no file is
        // made for it, as `kv` does it.
        let store = if !app.granted.contains(CAPABILITY) {
            None
        } else {
            Some(match &app.data {
                Some(dir) => crate::queue::shared(&dir.join("queue.sqlite3"))
                    .map_err(|why| format!("cannot open its queue store: {why}"))?,
                None => Arc::new(Mutex::new(Queues::in_memory()?)),
            })
        };
        Ok(Box::new(QueueHost {
            store,
            queues: app.queues.clone(),
            counters: Arc::clone(&app.counters),
        }))
    }
}

struct QueueHost {
    store: Option<Arc<Mutex<Queues>>>,
    queues: Vec<QueuePolicy>,
    counters: Arc<crate::stats::AppCounters>,
}

impl HostApi for QueueHost {
    fn module_schema(&self) -> ModuleSchema {
        QUEUE
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let name = args[0].as_str().unwrap_or_default();
        let Some(store) = &self.store else {
            return Err(RuntimeError::new("queue: this app has no queue store"));
        };
        let Some(policy) = self.queues.iter().find(|queue| queue.name == name) else {
            return Ok(Value::err(Value::error(format!(
                "queue.{op}: this app declares no queue `{}`; the ones it declares are {}",
                name.chars().take(64).collect::<String>(),
                match self.queues.is_empty() {
                    true => "none".to_string(),
                    false => self
                        .queues
                        .iter()
                        .map(|queue| format!("`{}`", queue.name))
                        .collect::<Vec<_>>()
                        .join(", "),
                }
            ))));
        };
        let store = store.lock().unwrap_or_else(|p| p.into_inner());
        match op {
            "send" | "sendAfter" => {
                let body = args[1].as_str().unwrap_or_default();
                let at = now();
                // A negative delay is no delay: deliver now, as `send` does.
                let delay = match op {
                    "sendAfter" => args.get(2).and_then(Value::as_int).unwrap_or(0).max(0),
                    _ => 0,
                };
                if delay > MAX_DELAY_SECS {
                    self.counters.queue_refused.fetch_add(1, Ordering::Relaxed);
                    return Ok(Value::err(Value::error(format!(
                        "queue.sendAfter: a delay of {delay} seconds is above the limit of \
                         {MAX_DELAY_SECS} seconds (7 days)"
                    ))));
                }
                match store.send(
                    name,
                    body,
                    at,
                    at.saturating_add(delay),
                    policy.max_body_bytes as usize,
                    policy.max_depth,
                ) {
                    Ok(id) => {
                        self.counters.queue_sent.fetch_add(1, Ordering::Relaxed);
                        Ok(Value::ok(Value::int(id)))
                    }
                    Err(why) => {
                        self.counters.queue_refused.fetch_add(1, Ordering::Relaxed);
                        Ok(Value::err(Value::error(format!("queue.{op}: {why}"))))
                    }
                }
            }
            "depth" => match store.depths() {
                Ok(depths) => Ok(Value::ok(Value::int(
                    depths
                        .iter()
                        .find(|(queue, _, _)| queue == name)
                        .map(|(_, waiting, _)| *waiting)
                        .unwrap_or(0),
                ))),
                Err(why) => Ok(Value::err(Value::error(format!("queue.depth: {why}")))),
            },
            other => Err(RuntimeError::new(format!(
                "`queue` declares no operation `{other}`"
            ))),
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

/// How often the host looks for messages that are due.
///
/// A second is short enough that a queue reads as "soon" and long enough
/// that an idle host does nothing measurable: the look is one indexed
/// `SELECT` per app that has a queue.
const EVERY: Duration = Duration::from_secs(1);

/// Delivers what apps have put on their queues, for as long as the host
/// runs.
///
/// Each delivery is started and not waited for: a task of its own settles
/// the message when the run ends. Waiting here would make one slow consumer
/// hold up every other app's queue.
pub(crate) async fn deliver(front: Arc<crate::server::Front>) {
    let clock: Clock = Arc::new(now);
    loop {
        tokio::time::sleep(EVERY).await;
        let Some(data) = front.options.data.clone() else {
            return;
        };
        for slot in front.engine.slots() {
            let Some(app) = slot.current() else { continue };
            if app.queues.is_empty() || !slot.enabled() || app.ready().is_none() {
                continue;
            }
            let Ok(store) = crate::queue::shared(&data.join(&slot.name).join("queue.sqlite3"))
            else {
                continue;
            };
            let deliveries = Deliveries {
                store,
                counters: Arc::clone(&app.counters),
                logs: Arc::clone(&app.logs),
                now: Arc::clone(&clock),
            };
            deliveries.tick(&app.queues, |policy, message| {
                start(&front, &slot, &app, &deliveries, policy, message)
            });
        }
    }
}

/// Admits one message's delivery and settles it when the run ends.
fn start(
    front: &Arc<crate::server::Front>,
    slot: &Arc<crate::sched::Slot>,
    app: &Arc<crate::apps::App>,
    deliveries: &Deliveries<Queues>,
    policy: &QueuePolicy,
    message: Message,
) {
    let admitted = deliveries.admit(policy, &message, |extra| {
        crate::internal::admit_with(
            &front.engine,
            crate::internal::Asked {
                app,
                slot: slot.index,
                forwarding: &front.options.forwarding,
                by: (crate::internal::QUEUE_HEADER, &policy.name),
                method: "POST",
                path: &policy.path,
                body: message.body.clone(),
            },
            extra,
        )
    });
    let Some(answer) = admitted else { return };
    let deliveries = deliveries.clone();
    let policy = policy.clone();
    tokio::spawn(async move {
        let outcome = answer.await.ok();
        deliveries.settle(&policy, &message, outcome);
    });
}

/// The clock a delivery reads: seconds since the epoch, [`now`] in the
/// host. A seam, so that the loop's tests can hold time still, move it in
/// milliseconds, or set it back.
pub(crate) type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// What admitting a delivery's run answers: where its reply will arrive,
/// or why the host turned it away. [`crate::internal::admit_with`]'s
/// answer; a seam, so that a test decides whether a run is admitted and
/// when and how it answers.
pub(crate) type Admitted = Result<oneshot::Receiver<Reply>, Rejection>;

/// What the delivery loop asks of an app's store: [`Queues`]'s own
/// operations, behind a trait so that a test can put a store beneath the
/// loop whose writes fail.
pub(crate) trait Store: Send + 'static {
    fn bury_spent(
        &self,
        queue: &str,
        max_attempts: i64,
        now_unix: i64,
    ) -> Result<Vec<(Message, String)>, String>;
    fn lease(
        &self,
        queue: Option<&str>,
        now_unix: i64,
        limit: usize,
        lease_secs: i64,
        max_attempts: i64,
    ) -> Result<Vec<Message>, String>;
    fn done(&self, id: i64, lease_id: i64) -> Result<bool, String>;
    fn retry(&self, id: i64, lease_id: i64, now_unix: i64, delay_secs: i64)
        -> Result<bool, String>;
    fn give_back(
        &self,
        id: i64,
        lease_id: i64,
        now_unix: i64,
        delay_secs: i64,
    ) -> Result<bool, String>;
    fn bury(&self, id: i64, lease_id: i64, now_unix: i64, why: &str) -> Result<bool, String>;
}

impl Store for Queues {
    fn bury_spent(
        &self,
        queue: &str,
        max_attempts: i64,
        now_unix: i64,
    ) -> Result<Vec<(Message, String)>, String> {
        Queues::bury_spent(self, queue, max_attempts, now_unix)
    }
    fn lease(
        &self,
        queue: Option<&str>,
        now_unix: i64,
        limit: usize,
        lease_secs: i64,
        max_attempts: i64,
    ) -> Result<Vec<Message>, String> {
        Queues::lease(self, queue, now_unix, limit, lease_secs, max_attempts)
    }
    fn done(&self, id: i64, lease_id: i64) -> Result<bool, String> {
        Queues::done(self, id, lease_id)
    }
    fn retry(
        &self,
        id: i64,
        lease_id: i64,
        now_unix: i64,
        delay_secs: i64,
    ) -> Result<bool, String> {
        Queues::retry(self, id, lease_id, now_unix, delay_secs)
    }
    fn give_back(
        &self,
        id: i64,
        lease_id: i64,
        now_unix: i64,
        delay_secs: i64,
    ) -> Result<bool, String> {
        Queues::give_back(self, id, lease_id, now_unix, delay_secs)
    }
    fn bury(&self, id: i64, lease_id: i64, now_unix: i64, why: &str) -> Result<bool, String> {
        Queues::bury(self, id, lease_id, now_unix, why)
    }
}

/// One app's deliveries: what deciding them needs, and nothing of how a
/// run is admitted or awaited. The host fills it from the app
/// ([`deliver`]); the loop's tests fill it with a clock, a store and
/// counters of their own, and drive [`Deliveries::tick`],
/// [`Deliveries::admit`] and [`Deliveries::settle`] a step at a time.
pub(crate) struct Deliveries<S> {
    pub(crate) store: Arc<Mutex<S>>,
    pub(crate) counters: Arc<AppCounters>,
    pub(crate) logs: Arc<LogRing>,
    pub(crate) now: Clock,
}

impl<S> Clone for Deliveries<S> {
    fn clone(&self) -> Deliveries<S> {
        Deliveries {
            store: Arc::clone(&self.store),
            counters: Arc::clone(&self.counters),
            logs: Arc::clone(&self.logs),
            now: Arc::clone(&self.now),
        }
    }
}

impl<S: Store> Deliveries<S> {
    /// One look at the app's queues, in the order `app.toml` declares
    /// them: each one's spent messages buried, then as many leased as it
    /// has room for, each handed to `start` before the next queue's room
    /// is counted (a start the host turns away has given its in-flight
    /// count back by then).
    ///
    /// The clock is read once, and every queue is buried and leased at that
    /// instant. Read twice, a lease that ran out between the two readings
    /// would be too early for `bury_spent` and in time for `lease`, and its
    /// message would be handed out once more than `max_attempts`. `queues`
    /// is the version the tick read when it began, so its `max_attempts` is
    /// the "current" one the contract's S5 means, for the whole tick.
    pub(crate) fn tick(
        &self,
        queues: &[QueuePolicy],
        mut start: impl FnMut(&QueuePolicy, Message),
    ) {
        let now = (self.now)();
        for policy in queues {
            // Before anything is leased: a message that has had its
            // attempts is a dead letter, not a next try.
            let spent = {
                let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
                store.bury_spent(&policy.name, i64::from(policy.max_attempts), now)
            };
            match spent {
                Ok(spent) => {
                    for (message, why) in spent {
                        self.counters.queue_buried.fetch_add(1, Ordering::Relaxed);
                        self.logs.push(
                            "host",
                            &format!(
                                "queue `{}`: message {} is a dead letter after {} \
                                 attempt(s): {why}",
                                policy.name, message.id, message.attempts
                            ),
                        );
                    }
                }
                Err(why) => {
                    self.logs
                        .push("host", &format!("queue `{}`: {why}", policy.name));
                }
            }
            let in_flight = self.counters.queue_in_flight.load(Ordering::Relaxed);
            let room = (policy.batch as u64).saturating_sub(in_flight);
            if room == 0 {
                continue;
            }
            let leased = {
                let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
                store.lease(
                    Some(&policy.name),
                    now,
                    room as usize,
                    policy.lease.as_secs() as i64,
                    i64::from(policy.max_attempts),
                )
            };
            let leased = match leased {
                Ok(leased) => leased,
                Err(why) => {
                    self.logs
                        .push("host", &format!("queue `{}`: {why}", policy.name));
                    continue;
                }
            };
            for message in leased {
                self.counters
                    .queue_in_flight
                    .fetch_add(1, Ordering::Relaxed);
                start(policy, message);
            }
        }
    }

    /// Asks for one leased message's run with `admit`, and answers where
    /// its reply will arrive; `None` for a run the host turned away, whose
    /// message has been given back.
    pub(crate) fn admit(
        &self,
        policy: &QueuePolicy,
        message: &Message,
        admit: impl FnOnce(&[(&str, String)]) -> Admitted,
    ) -> Option<oneshot::Receiver<Reply>> {
        // Which try this is, and how many there will be: a consumer that
        // wants to say something only when the message is about to become
        // a dead letter compares the two. `lease` raised `attempts` to
        // count this one.
        let answer = admit(&[
            (
                crate::internal::ATTEMPT_HEADER,
                message.attempts.to_string(),
            ),
            (
                crate::internal::ATTEMPTS_HEADER,
                policy.max_attempts.to_string(),
            ),
        ]);
        match answer {
            Ok(answer) => Some(answer),
            // The queue was full or the host was busy: give the message back
            // to be tried again, with the attempt `lease` counted taken back,
            // rather than counting it as a failed delivery — or a busy host
            // could spend a message's attempts without its consumer ever being
            // asked. The lease is still asked about: it has not had time to
            // run out, but a give-back that did not ask would be the one
            // settle here that could move another delivery's lease.
            Err(_) => {
                self.counters
                    .queue_in_flight
                    .fetch_sub(1, Ordering::Relaxed);
                self.give_back(policy, message);
                None
            }
        }
    }

    /// Gives a delivery's message back, with the attempt `lease` counted
    /// taken back, for a run the app never saw. The lease is still asked
    /// about: a give-back that did not ask would be the one settle that
    /// could move another delivery's lease.
    fn give_back(&self, policy: &QueuePolicy, message: &Message) {
        let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        if let Ok(false) = store.give_back(
            message.id,
            token(message),
            (self.now)(),
            policy.retry.as_secs() as i64,
        ) {
            outlived(&self.logs, &policy.name, message.id);
        }
    }

    /// Settles one admitted delivery on what its run came to: the reply,
    /// or `None` for a run whose answer never came.
    ///
    /// Who made the reply decides, not its status ([`Origin`]):
    ///
    /// - the **app's own answer** below 500 has taken the message, and 500
    ///   or more has not;
    /// - the **host's word on a run that never started** (it waited for a
    ///   worker past its deadline, or was cancelled while it waited) is not
    ///   an attempt: the message is given back, as one turned away at
    ///   admission is. Taken for the app's answer, the scheduler's 503
    ///   spent an attempt and could bury a message the app never saw, and a
    ///   499 is below 500 and deleted it;
    /// - the **host's word on a run that started** and did not answer (cut
    ///   off at its deadline, cancelled, failed) is no answer, whatever its
    ///   status: an attempt spent, never the message taken.
    pub(crate) fn settle(&self, policy: &QueuePolicy, message: &Message, outcome: Option<Reply>) {
        let (counters, logs) = (&self.counters, &self.logs);
        counters.queue_in_flight.fetch_sub(1, Ordering::Relaxed);
        if let Some(reply) = &outcome {
            if reply.origin == Origin::NotStarted {
                logs.push(
                    "host",
                    &format!(
                        "queue `{}`: message {}'s run never started ({} {}); given back",
                        policy.name,
                        message.id,
                        reply.status,
                        crate::internal::first_line(&reply.body)
                    ),
                );
                self.give_back(policy, message);
                return;
            }
        }
        let taken =
            matches!(&outcome, Some(reply) if reply.origin == Origin::Answer && reply.status < 500);
        let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        if taken {
            match store.done(message.id, token(message)) {
                Ok(false) => outlived(logs, &policy.name, message.id),
                _ => {
                    counters.queue_delivered.fetch_add(1, Ordering::Relaxed);
                }
            }
            return;
        }
        let why = match &outcome {
            Some(reply) if reply.origin == Origin::Answer => format!(
                "{} {}",
                reply.status,
                crate::internal::first_line(&reply.body)
            ),
            Some(reply) => format!(
                "no answer: {} {}",
                reply.status,
                crate::internal::first_line(&reply.body)
            ),
            None => "no answer".to_string(),
        };
        // `attempts` already counts this one, which `lease` raised.
        if message.attempts >= i64::from(policy.max_attempts) {
            if let Ok(false) = store.bury(message.id, token(message), (self.now)(), &why) {
                outlived(logs, &policy.name, message.id);
                return;
            }
            counters.queue_buried.fetch_add(1, Ordering::Relaxed);
            logs.push(
                "host",
                &format!(
                    "queue `{}`: message {} is a dead letter after {} attempt(s): {why}",
                    policy.name, message.id, message.attempts
                ),
            );
            return;
        }
        // Doubling, so a consumer that is down is asked less and less
        // often rather than every `retry` for as long as it is down.
        let delay = policy.retry.as_secs() as i64 * (1 << (message.attempts.min(10) - 1).max(0));
        if let Ok(false) = store.retry(message.id, token(message), (self.now)(), delay) {
            outlived(logs, &policy.name, message.id);
            return;
        }
        counters.queue_retried.fetch_add(1, Ordering::Relaxed);
        logs.push(
            "host",
            &format!(
                "queue `{}`: message {} not taken ({why}); again in {delay}s",
                policy.name, message.id
            ),
        );
    }
}

/// The lease a delivery settles under. Every message the loop delivers came
/// from [`Queues::lease`], which always gives one.
fn token(message: &Message) -> i64 {
    message
        .lease_id
        .expect("a message the loop delivers was leased")
}

/// Says in the app's log that a delivery's answer settled nothing, because
/// its lease had run out and the message had been handed to another.
fn outlived(logs: &LogRing, queue: &str, id: i64) {
    logs.push(
        "host",
        &format!(
            "queue `{queue}`: message {id}'s delivery outlived its lease; \
             another delivery holds it now"
        ),
    );
}

#[cfg(test)]
mod harness;

#[cfg(test)]
mod properties;
