//! Running an app on a schedule: `[[triggers]]` in its `app.toml`.
//!
//! An app had one way in, a request from outside, so anything periodic had
//! to be a crontab somewhere else calling the host over HTTP. A trigger is
//! the host making that call itself.
//!
//! **A trigger run is an ordinary run.** The host builds a [`AppRequest`]
//! and admits it to the app's queue exactly as the HTTP front does, so a
//! scheduled run is served round robin with everything else, holds a worker
//! for a slice at a time, parks where it waits, and is stopped by the app's
//! own `deadline`, `max_host_calls` and heap. It is pinned to the version
//! that was current when it was admitted, and it is counted in that app's
//! `served` and errors like any other run. Nothing about the scheduler
//! ([`crate::sched`]) knows that this one had no client.
//!
//! **An app cannot be fooled about who called.** The host sets
//! `x-minicloud-trigger` on a trigger's request and strips it from every
//! request that comes off a connection ([`crate::server`]), so a path an
//! app only means to run on a schedule can say so:
//!
//! ```cove
//! if request.headers.get("x-minicloud-trigger") == Option.None { … }
//! ```
//!
//! That is the whole of the protection. The path is an ordinary path of the
//! app's: reachable from outside unless the app refuses it, which is the
//! app's to do and lets a job be run by hand.
//!
//! **A tick that is late does not happen twice, and one that is missed does
//! not happen later.** Schedules are to the minute, in UTC
//! ([`crate::cron`]); each minute is looked at once, and a minute the host
//! was not running for is gone. A trigger whose last run has not finished
//! lets the tick go by and counts it (`trigger_skipped`) rather than
//! running two at once: a job that takes longer than its period would
//! otherwise pile up until it filled its app's queue.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::oneshot;

use crate::convert::AppRequest;
use crate::cron::Minute;
use crate::proxy::Forwarding;
use crate::sched::{Cancel, Engine, Flight, Start};

/// The header a trigger's run carries, naming the path its schedule asked
/// for. The host sets it here and removes it from every request that comes
/// off a connection, so an app that finds it knows the host is the caller.
pub const TRIGGER_HEADER: &str = "x-minicloud-trigger";

/// What the ticking task runs triggers with.
pub struct Triggers {
    engine: Arc<Engine>,
    forwarding: Forwarding,
    /// The I/O runtime, where the task that waits for each run's answer
    /// lives.
    io: tokio::runtime::Handle,
    /// The triggers whose run has not answered, by app and index: the ones
    /// a tick lets go by. Shared with the task that waits for each answer.
    running: Arc<Mutex<BTreeSet<(String, usize)>>>,
    /// The last minute [`Triggers::fire_due`] looked at, so that a timer
    /// that wakes twice in a minute fires nothing twice.
    last: Mutex<Option<i64>>,
}

impl Triggers {
    pub fn new(
        engine: Arc<Engine>,
        forwarding: Forwarding,
        io: tokio::runtime::Handle,
    ) -> Triggers {
        Triggers {
            engine,
            forwarding,
            io,
            running: Arc::new(Mutex::new(BTreeSet::new())),
            last: Mutex::new(None),
        }
    }

    /// Runs every schedule due in the minute `unix_seconds` is in, and
    /// answers how many it started.
    ///
    /// The ticking task calls this a moment after each minute begins; a
    /// test calls it with a time of its own, which is why it takes one
    /// rather than reading the clock.
    pub fn fire_due(&self, unix_seconds: i64) -> usize {
        let minute = unix_seconds.div_euclid(60);
        {
            let mut last = self.last.lock().unwrap();
            if *last == Some(minute) {
                return 0;
            }
            *last = Some(minute);
        }
        let at = Minute::from_unix(minute * 60);
        let mut started = 0;
        for slot in self.engine.slots() {
            let Some(app) = slot.current() else { continue };
            if app.triggers.is_empty() {
                continue;
            }
            // A disabled app is not routed to; a refused one has no program
            // to run. Neither is an error to report every minute.
            if !slot.enabled() || app.ready().is_none() {
                continue;
            }
            for (index, trigger) in app.triggers.iter().enumerate() {
                if !trigger.cron.matches(at) {
                    continue;
                }
                let key = (slot.name.clone(), index);
                if !self.running.lock().unwrap().insert(key.clone()) {
                    app.counters
                        .trigger_skipped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    app.logs.push(
                        "host",
                        &format!(
                            "trigger {} {}: the run before it has not finished; this tick is \
                             let go by",
                            trigger.method, trigger.path
                        ),
                    );
                    continue;
                }
                self.start(&app, slot.index, index, key);
                started += 1;
            }
        }
        started
    }

    /// Admits one trigger's run, and waits for its answer on the I/O
    /// runtime to count it and let the next tick fire.
    fn start(&self, app: &Arc<crate::apps::App>, slot: usize, index: usize, key: (String, usize)) {
        let trigger = &app.triggers[index];
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        headers.insert(TRIGGER_HEADER.to_string(), trigger.path.clone());
        // What the app is told about how it is reached, as a request off a
        // connection is told it: a job that writes a link to itself writes
        // the same one a page would.
        self.forwarding
            .apply(&mut headers, app.hosts.first().map(String::as_str));
        headers.insert(
            "x-forwarded-prefix".to_string(),
            if app.hosts.is_empty() {
                format!("/{}", app.name)
            } else {
                String::new()
            },
        );
        let (reply, answer) = oneshot::channel();
        let start = Box::new(Start {
            request: AppRequest {
                method: trigger.method.clone(),
                path: trigger.path.clone(),
                query: Vec::new(),
                headers: headers.into_iter().collect(),
                body: String::new(),
            },
            flight: Flight {
                id: self.engine.next_id(),
                app: slot,
                version: Arc::clone(app),
                accepted: Instant::now(),
                reply,
                cancel: Cancel::new(),
                tally: Default::default(),
            },
        });
        let what = format!("trigger {} {}", trigger.method, trigger.path);
        if let Err(why) = self.engine.queue.admit(slot, start) {
            app.counters.rejected(why);
            app.counters
                .trigger_failed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            app.logs.push("host", &format!("{what}: not run ({why:?})"));
            self.running.lock().unwrap().remove(&key);
            return;
        }
        let counters = Arc::clone(&app.counters);
        let logs = Arc::clone(&app.logs);
        let running = Arc::clone(&self.running);
        let began = Instant::now();
        self.io.spawn(async move {
            use std::sync::atomic::Ordering::Relaxed;
            let outcome = answer.await;
            let took = began.elapsed().as_millis();
            counters.trigger_runs.fetch_add(1, Relaxed);
            match outcome {
                // A status the app chose: 5xx is the runtime's or the app's
                // failure, and worth saying out loud; anything else is the
                // app answering, which a trigger has no opinion about.
                Ok(reply) if reply.status < 500 => {
                    logs.push("host", &format!("{what}: {} in {took} ms", reply.status));
                }
                Ok(reply) => {
                    counters.trigger_failed.fetch_add(1, Relaxed);
                    logs.push(
                        "host",
                        &format!(
                            "{what}: {} in {took} ms: {}",
                            reply.status,
                            first_line(&reply.body)
                        ),
                    );
                }
                // The scheduler dropped the run without answering: the host
                // is shutting down, or the version was dropped under it.
                Err(_) => {
                    counters.trigger_failed.fetch_add(1, Relaxed);
                    logs.push("host", &format!("{what}: no answer after {took} ms"));
                }
            }
            running.lock().unwrap().remove(&key);
        });
    }
}

/// The first line of a body, cut, for a log line.
fn first_line(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let line = text.lines().next().unwrap_or_default();
    if line.len() <= 200 {
        return line.to_string();
    }
    let mut end = 200;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

/// Ticks a moment after each minute begins, for as long as the host runs.
///
/// The wait is to the next minute of the wall clock rather than a plain
/// sixty seconds, so that `0 * * * *` fires near the top of the hour and
/// not wherever the host happened to start; [`Triggers::fire_due`] looks at
/// each minute once, so waking early or twice costs nothing.
pub async fn tick(triggers: Arc<Triggers>) {
    loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let next = (now / 60 + 1) * 60;
        let wait = next.saturating_sub(now);
        tokio::time::sleep(Duration::from_secs(wait) + Duration::from_millis(200)).await;
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        triggers.fire_due(at as i64);
    }
}
