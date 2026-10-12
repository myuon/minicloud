//! `defer`: work that outlives the request that asked for it.
//!
//! An app answers a request and is done: its isolate ends with its answer,
//! so anything it has not finished by then it has not done. Work that
//! should not make the client wait — writing an audit line, warming a
//! cache, telling something else that this happened — had nowhere to go.
//!
//! | operation | answers |
//! | --- | --- |
//! | `defer.run(path, body)` | `Result<Unit, Error>`: the host will run this app at `path` |
//!
//! **It is a run of its own, not the rest of this one.** Cloudflare's
//! `waitUntil` hands the runtime a promise and the isolate is kept alive
//! until it settles; here the answer *is* the end of the isolate, and
//! keeping one alive past its answer would mean a request's budget outliving
//! the request. So the host starts another run of the same app at the path
//! the call names — the app routes it as it routes anything else — with
//! `x-minicloud-deferred` saying who asked ([`crate::internal`]).
//!
//! The deferred run is a run like any other: its own deadline, its own
//! host-call budget, its own place in the app's queue, counted in `served`
//! and the errors. It is **not** waited for: the call answers as soon as the
//! work is accepted, and the request that asked goes on to answer its
//! client.
//!
//! # What bounds it
//!
//! `[defer] max_in_flight` (8 by default) is how many deferred runs of one
//! app may be outstanding at once; a call past it is the app's `Err` rather
//! than a queue that grows without end. A deferred run may defer again —
//! nothing here forbids it — and an app that does so in a loop spends its
//! own turns on it: the scheduler serves apps round robin, so the loop
//! costs that app and not its neighbours. The outstanding count is the
//! bound that keeps it from costing memory.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use cove_runtime::{Effect, HostApi, HostType, ModuleSchema, OperationSchema, RuntimeError, Value};

use crate::hosts::{AppContext, HostModule};
use crate::stats::AppCounters;

/// The capability that opens this module.
pub const CAPABILITY: &str = "defer";

/// The longest path a call may name.
const MAX_PATH: usize = 1024;

/// What one call asks the host to run.
#[derive(Clone, Debug)]
pub struct Deferred {
    /// The app that asked, which is the app that will run.
    pub app: String,
    pub path: String,
    pub body: String,
}

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

/// The `defer` module.
pub const DEFER: ModuleSchema = ModuleSchema {
    name: "defer",
    capability: CAPABILITY,
    operations: &[op(
        "run",
        &[HostType::String, HostType::String],
        HostType::Result(&HostType::Unit, &HostType::Error),
        // Irreversible: the host may have started the run before the
        // call that asked for it has finished, and nothing can take it
        // back.
        Effect::IrreversibleWrite,
    )],
    types: &[],
    resources: &[],
};

pub(crate) struct DeferModule;

impl HostModule for DeferModule {
    fn schema(&self) -> ModuleSchema {
        DEFER
    }

    fn instantiate(&self, app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        Ok(Box::new(DeferHost {
            app: app.app.clone(),
            max_in_flight: app.defer.max_in_flight,
            counters: Arc::clone(&app.counters),
            asking: app.defer_to.clone(),
        }))
    }
}

struct DeferHost {
    app: String,
    max_in_flight: u64,
    counters: Arc<AppCounters>,
    /// Where the host takes the work; `None` for a host that runs none
    /// (`minicloud test`, which has no engine to run it on).
    asking: Option<tokio::sync::mpsc::Sender<Deferred>>,
}

impl HostApi for DeferHost {
    fn module_schema(&self) -> ModuleSchema {
        DEFER
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        if op != "run" {
            return Err(RuntimeError::new(format!(
                "`defer` declares no operation `{op}`"
            )));
        }
        let path = args[0].as_str().unwrap_or_default();
        let body = args[1].as_str().unwrap_or_default();
        if !path.starts_with('/')
            || path.len() > MAX_PATH
            || path.contains(|c: char| c.is_control())
        {
            return Ok(Value::err(Value::error(format!(
                "defer.run: `{}` is not a path: it starts with `/` and holds no control \
                 character",
                path.chars().take(64).collect::<String>()
            ))));
        }
        let Some(asking) = &self.asking else {
            return Ok(Value::err(Value::error(
                "defer.run: this host runs no deferred work".to_string(),
            )));
        };
        // The count is taken before the send and given back when the run
        // ends, so the bound holds even while the host is still starting
        // the work.
        let outstanding = self.counters.deferred_in_flight.load(Ordering::Relaxed);
        if outstanding >= self.max_in_flight {
            self.counters
                .deferred_refused
                .fetch_add(1, Ordering::Relaxed);
            return Ok(Value::err(Value::error(format!(
                "defer.run: {outstanding} deferred run(s) of this app have not finished, \
                 which is its `[defer] max_in_flight`"
            ))));
        }
        self.counters
            .deferred_in_flight
            .fetch_add(1, Ordering::Relaxed);
        let work = Deferred {
            app: self.app.clone(),
            path: path.to_string(),
            body: body.to_string(),
        };
        match asking.try_send(work) {
            Ok(()) => Ok(Value::ok(Value::unit())),
            Err(_) => {
                self.counters
                    .deferred_in_flight
                    .fetch_sub(1, Ordering::Relaxed);
                self.counters
                    .deferred_refused
                    .fetch_add(1, Ordering::Relaxed);
                Ok(Value::err(Value::error(
                    "defer.run: the host has as much deferred work as it will hold".to_string(),
                )))
            }
        }
    }
}

/// Runs what apps have deferred, one at a time off the channel they are put
/// on, for as long as the host runs.
///
/// Each run is started and **not** waited for here: a task of its own
/// counts it when it ends. Waiting would make one app's slow deferred work
/// hold up every other app's.
pub(crate) async fn deliver(
    front: Arc<crate::server::Front>,
    mut asked: tokio::sync::mpsc::Receiver<Deferred>,
) {
    while let Some(work) = asked.recv().await {
        let Some(slot) = front.engine.slot_named(&work.app) else {
            continue;
        };
        let Some(app) = slot.current() else { continue };
        // The app may have been disabled or refused since the call: give
        // the count back and say so where the app can see it.
        let runnable = slot.enabled() && app.ready().is_some();
        let answer = if runnable {
            crate::internal::admit(
                &front.engine,
                crate::internal::Asked {
                    app: &app,
                    slot: slot.index,
                    forwarding: &front.options.forwarding,
                    by: (crate::internal::DEFERRED_HEADER, &work.path),
                    method: "POST",
                    path: &work.path,
                    body: work.body,
                },
            )
            .ok()
        } else {
            None
        };
        let what = format!("deferred POST {}", work.path);
        let Some(answer) = answer else {
            app.counters
                .deferred_in_flight
                .fetch_sub(1, Ordering::Relaxed);
            app.counters
                .deferred_refused
                .fetch_add(1, Ordering::Relaxed);
            app.logs.push("host", &format!("{what}: not run"));
            continue;
        };
        let counters = Arc::clone(&app.counters);
        let logs = Arc::clone(&app.logs);
        tokio::spawn(async move {
            let outcome = answer.await;
            counters.deferred_in_flight.fetch_sub(1, Ordering::Relaxed);
            counters.deferred_runs.fetch_add(1, Ordering::Relaxed);
            match outcome {
                Ok(reply) if reply.status < 500 => {}
                Ok(reply) => {
                    counters.deferred_failed.fetch_add(1, Ordering::Relaxed);
                    logs.push(
                        "host",
                        &format!(
                            "{what}: {} {}",
                            reply.status,
                            crate::internal::first_line(&reply.body)
                        ),
                    );
                }
                Err(_) => {
                    counters.deferred_failed.fetch_add(1, Ordering::Relaxed);
                    logs.push("host", &format!("{what}: no answer"));
                }
            }
        });
    }
}
