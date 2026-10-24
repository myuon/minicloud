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
//! reason rather than dropped.
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
//! and counted as nothing.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cove_runtime::{Effect, HostApi, HostType, ModuleSchema, OperationSchema, RuntimeError, Value};

use crate::config::QueuePolicy;
use crate::hosts::{AppContext, HostModule};
use crate::queue::Queues;

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
            for policy in &app.queues {
                let in_flight = app.counters.queue_in_flight.load(Ordering::Relaxed);
                let room = (policy.batch as u64).saturating_sub(in_flight);
                if room == 0 {
                    continue;
                }
                let leased = {
                    let store = store.lock().unwrap_or_else(|p| p.into_inner());
                    store.lease(now(), room as usize, policy.lease.as_secs() as i64)
                };
                let leased = match leased {
                    Ok(leased) => leased,
                    Err(why) => {
                        app.logs
                            .push("host", &format!("queue `{}`: {why}", policy.name));
                        continue;
                    }
                };
                for message in leased {
                    if message.queue != policy.name {
                        continue;
                    }
                    app.counters.queue_in_flight.fetch_add(1, Ordering::Relaxed);
                    start(&front, &slot, &app, policy, message, Arc::clone(&store));
                }
            }
        }
    }
}

/// Admits one message's delivery and settles it when the run ends.
fn start(
    front: &Arc<crate::server::Front>,
    slot: &Arc<crate::sched::Slot>,
    app: &Arc<crate::apps::App>,
    policy: &QueuePolicy,
    message: crate::queue::Message,
    store: Arc<Mutex<Queues>>,
) {
    let answer = crate::internal::admit_with(
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
        // Which try this is, and how many there will be: a consumer that
        // wants to say something only when the message is about to become
        // a dead letter compares the two. `lease` raised `attempts` to
        // count this one.
        &[
            (
                crate::internal::ATTEMPT_HEADER,
                message.attempts.to_string(),
            ),
            (
                crate::internal::ATTEMPTS_HEADER,
                policy.max_attempts.to_string(),
            ),
        ],
    );
    let counters = Arc::clone(&app.counters);
    let logs = Arc::clone(&app.logs);
    let policy = policy.clone();
    let answer = match answer {
        Ok(answer) => answer,
        // The queue was full or the host was busy: give the message back
        // to be tried again rather than counting it as a failed delivery.
        // The lease is still asked about: it has not had time to run out,
        // but a retry that did not ask would be the one settle here that
        // could move another delivery's lease.
        Err(_) => {
            counters.queue_in_flight.fetch_sub(1, Ordering::Relaxed);
            let store = store.lock().unwrap_or_else(|p| p.into_inner());
            if let Ok(false) = store.retry(
                message.id,
                message.lease_id,
                now(),
                policy.retry.as_secs() as i64,
            ) {
                outlived(&logs, &policy.name, message.id);
            }
            return;
        }
    };
    tokio::spawn(async move {
        let outcome = answer.await;
        counters.queue_in_flight.fetch_sub(1, Ordering::Relaxed);
        let taken = matches!(&outcome, Ok(reply) if reply.status < 500);
        let store = store.lock().unwrap_or_else(|p| p.into_inner());
        if taken {
            match store.done(message.id, message.lease_id) {
                Ok(false) => outlived(&logs, &policy.name, message.id),
                _ => {
                    counters.queue_delivered.fetch_add(1, Ordering::Relaxed);
                }
            }
            return;
        }
        let why = match &outcome {
            Ok(reply) => format!(
                "{} {}",
                reply.status,
                crate::internal::first_line(&reply.body)
            ),
            Err(_) => "no answer".to_string(),
        };
        // `attempts` already counts this one, which `lease` raised.
        if message.attempts >= i64::from(policy.max_attempts) {
            if let Ok(false) = store.bury(message.id, message.lease_id, now(), &why) {
                outlived(&logs, &policy.name, message.id);
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
        if let Ok(false) = store.retry(message.id, message.lease_id, now(), delay) {
            outlived(&logs, &policy.name, message.id);
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
    });
}

/// Says in the app's log that a delivery's answer settled nothing, because
/// its lease had run out and the message had been handed to another.
fn outlived(logs: &crate::logs::LogRing, queue: &str, id: i64) {
    logs.push(
        "host",
        &format!(
            "queue `{queue}`: message {id}'s delivery outlived its lease; \
             another delivery holds it now"
        ),
    );
}
