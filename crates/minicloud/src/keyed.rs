//! `[keyed]`: one run at a time for one key, and an alarm per key.
//!
//! An app can hold state — `kv`, `sql` — and cannot hold it *safely* when
//! two requests about the same thing run at once. `kv.increment` is atomic
//! and nothing else is: read, decide, write is three calls, and two runs
//! that interleave them lose one of the two. A chat room, a seat booking, a
//! counter that is more than a counter: each wants everything about one key
//! to happen in order.
//!
//! **This is the single-writer half of Cloudflare's Durable Objects, and
//! not the object.** A Durable Object is addressable, long-lived and holds
//! its state in memory between requests. Here a run's heap ends with the
//! run — that is the whole design — so what is offered instead is the
//! guarantee that makes the state safe: **at most one run per key**, with
//! the state in the app's own store. An app reads, decides and writes with
//! nobody else in the middle, which is what the object was for.
//!
//! ```toml
//! [keyed]
//! by = "query:room"      # or "header:x-room"
//! max_waiting = 64       # requests for one key that may be waiting
//! alarm_path = "/alarm"  # where a due alarm runs
//! ```
//!
//! The key is taken from the request **before the app runs**, because the
//! waiting has to happen before the run starts; it is a query parameter or
//! a header the config names. A request with no key is not keyed and runs
//! as any request does.
//!
//! # How the waiting works
//!
//! The HTTP task takes the key's lock before it admits the request and
//! holds it until the answer, so the next request for that key starts when
//! this one has ended. The lock is fair: a key's requests are served in the
//! order they arrived. `max_waiting` bounds the queue behind one key — past
//! it the answer is 429 with `Retry-After`, as a full app queue is — so one
//! hot key cannot fill the host's admission limit by itself.
//!
//! Nothing about the scheduler changes: a keyed run is an ordinary run with
//! an ordinary deadline. An app that calls itself with the same key from
//! inside a keyed run waits for itself and is ended by its deadline; that
//! is the one deadlock there is, and the deadline is what bounds it.
//!
//! # Alarms
//!
//! `keyed.alarm(key, atMillis)` asks the host to run the app at
//! `alarm_path` with that key at that time, and `keyed.clearAlarm(key)`
//! takes it back. One alarm per key — setting it again moves it — and it is
//! kept in the app's own SQLite file, so it survives a restart. A due alarm
//! runs under the key's lock like everything else about the key, so an
//! alarm and a request about the same key never run together.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use cove_runtime::{Effect, HostApi, HostType, ModuleSchema, OperationSchema, RuntimeError, Value};
use rusqlite::{params, Connection};

use crate::hosts::{AppContext, HostModule};

/// The capability that opens the module.
pub const CAPABILITY: &str = "keyed";

/// The longest a key may be. A key is a name, not a document.
pub const MAX_KEY: usize = 256;

/// How many keys the host keeps locks for before it sweeps the ones nobody
/// is holding or waiting for.
const MAX_KEYS: usize = 10_000;

// ------------------------------------------------------------- the locks

/// One key's turn.
#[derive(Debug, Default)]
struct Key {
    /// Behind an `Arc` of its own because the guard outlives the borrow:
    /// a turn is held across the admission and the answer.
    turn: Arc<tokio::sync::Mutex<()>>,
    waiting: AtomicUsize,
}

/// The keys of every app, and whose turn it is.
#[derive(Default)]
pub struct Keys {
    keys: Mutex<HashMap<(String, String), Arc<Key>>>,
}

/// A key's turn, held until it is dropped.
#[derive(Debug)]
pub struct Turn {
    _held: tokio::sync::OwnedMutexGuard<()>,
    /// Kept so the entry cannot be swept while this turn is held.
    _key: Arc<Key>,
}

/// Why a request did not get a turn.
#[derive(Debug, PartialEq, Eq)]
pub enum NoTurn {
    /// `max_waiting` requests are waiting for this key already.
    TooMany(usize),
}

impl Keys {
    /// Waits for this key's turn and answers a guard that holds it.
    ///
    /// The lock is `tokio`'s, which is fair, so a key's requests are served
    /// in the order they arrived rather than whichever wakes first.
    pub async fn take(&self, app: &str, key: &str, max_waiting: usize) -> Result<Turn, NoTurn> {
        let entry = {
            let mut keys = self.keys.lock().unwrap_or_else(|p| p.into_inner());
            if keys.len() >= MAX_KEYS {
                // Only the idle ones: a key nobody is holding or waiting
                // for is the same as a key that was never there.
                keys.retain(|_, key| {
                    key.waiting.load(Ordering::Relaxed) > 0 || key.turn.try_lock().is_err()
                });
            }
            Arc::clone(keys.entry((app.to_string(), key.to_string())).or_default())
        };
        if entry.waiting.load(Ordering::Relaxed) >= max_waiting {
            return Err(NoTurn::TooMany(max_waiting));
        }
        entry.waiting.fetch_add(1, Ordering::Relaxed);
        let held = Arc::clone(&entry.turn).lock_owned().await;
        entry.waiting.fetch_sub(1, Ordering::Relaxed);
        Ok(Turn {
            _held: held,
            _key: entry,
        })
    }

    /// How many keys the host is holding locks for, for the stats.
    pub fn len(&self) -> usize {
        self.keys.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ------------------------------------------------------------ the alarms

/// One app's alarms: at most one per key, kept across a restart.
pub struct Alarms {
    conn: Connection,
}

impl Alarms {
    pub fn open(path: &Path) -> Result<Alarms, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create the app's directory: {e}"))?;
        }
        let conn = Connection::open(path).map_err(|_| "cannot open the alarm store".to_string())?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        Alarms::with(conn)
    }

    pub fn in_memory() -> Result<Alarms, String> {
        Alarms::with(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn with(conn: Connection) -> Result<Alarms, String> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS alarm (
                 key TEXT PRIMARY KEY,
                 at INTEGER NOT NULL
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS alarm_at ON alarm (at);",
        )
        .map_err(|e| format!("cannot prepare the alarm store: {e}"))?;
        Ok(Alarms { conn })
    }

    /// Sets this key's alarm, moving one that was already there: a key has
    /// one alarm, because a key is one thing.
    pub fn set(&self, key: &str, at_millis: i64) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO alarm (key, at) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET at = excluded.at",
                params![key, at_millis],
            )
            .map_err(|e| format!("cannot set the alarm: {e}"))?;
        Ok(())
    }

    /// Takes this key's alarm back; answers whether there was one.
    pub fn clear(&self, key: &str) -> Result<bool, String> {
        let gone = self
            .conn
            .execute("DELETE FROM alarm WHERE key = ?1", params![key])
            .map_err(|e| format!("cannot clear the alarm: {e}"))?;
        Ok(gone > 0)
    }

    /// When this key's alarm is due, if it has one.
    pub fn at(&self, key: &str) -> Result<Option<i64>, String> {
        self.conn
            .query_row("SELECT at FROM alarm WHERE key = ?1", params![key], |row| {
                row.get(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(format!("cannot read the alarm: {other}")),
            })
    }

    /// The keys whose alarm is due at `now_millis`, and takes them: an
    /// alarm goes off once, and an app that wants another sets another.
    pub fn take_due(&self, now_millis: i64, limit: usize) -> Result<Vec<String>, String> {
        let mut statement = self
            .conn
            .prepare("SELECT key FROM alarm WHERE at <= ?1 ORDER BY at LIMIT ?2")
            .map_err(|e| format!("cannot read the alarms: {e}"))?;
        let due: Result<Vec<String>, _> = statement
            .query_map(params![now_millis, limit as i64], |row| row.get(0))
            .map_err(|e| format!("cannot read the alarms: {e}"))?
            .collect();
        let due = due.map_err(|e| format!("cannot read the alarms: {e}"))?;
        for key in &due {
            self.conn
                .execute("DELETE FROM alarm WHERE key = ?1", params![key])
                .map_err(|e| format!("cannot take the alarm: {e}"))?;
        }
        Ok(due)
    }

    /// How many alarms are set, for the operations views.
    pub fn count(&self) -> Result<i64, String> {
        self.conn
            .query_row("SELECT count(*) FROM alarm", [], |row| row.get(0))
            .map_err(|e| format!("cannot count the alarms: {e}"))
    }
}

fn open_stores() -> &'static Mutex<HashMap<PathBuf, Weak<Mutex<Alarms>>>> {
    static OPEN: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<Alarms>>>>> = OnceLock::new();
    OPEN.get_or_init(Mutex::default)
}

/// The alarm store at `path`: the one already open, or newly opened. One
/// per file however many versions of the app are alive.
pub fn shared(path: &Path) -> Result<Arc<Mutex<Alarms>>, String> {
    let mut open = open_stores().lock().unwrap();
    if let Some(store) = open.get(path).and_then(Weak::upgrade) {
        return Ok(store);
    }
    let store = Arc::new(Mutex::new(Alarms::open(path)?));
    open.insert(path.to_path_buf(), Arc::downgrade(&store));
    Ok(store)
}

/// How many alarms the app at `path` has set, if its store is open.
pub fn usage(path: &Path) -> Option<i64> {
    let store = open_stores().lock().unwrap().get(path)?.upgrade()?;
    let count = store.lock().unwrap().count().ok()?;
    Some(count)
}

// ------------------------------------------------------------ the module

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

/// The `keyed` module.
pub const KEYED: ModuleSchema = ModuleSchema {
    name: "keyed",
    capability: CAPABILITY,
    operations: &[
        op(
            "alarm",
            &[HostType::String, HostType::Int],
            HostType::Result(&HostType::Unit, &HostType::Error),
            Effect::ReversibleWrite,
        ),
        op(
            "clearAlarm",
            &[HostType::String],
            HostType::Result(&HostType::Bool, &HostType::Error),
            Effect::ReversibleWrite,
        ),
        op(
            "alarmAt",
            &[HostType::String],
            HostType::Result(&HostType::Int, &HostType::Error),
            Effect::Read,
        ),
    ],
    types: &[],
    resources: &[],
};

pub(crate) struct KeyedModule;

impl HostModule for KeyedModule {
    fn schema(&self) -> ModuleSchema {
        KEYED
    }

    fn instantiate(&self, app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        let alarms = if !app.granted.contains(CAPABILITY) {
            None
        } else {
            Some(match &app.data {
                Some(dir) => shared(&dir.join("alarms.sqlite3"))
                    .map_err(|why| format!("cannot open its alarm store: {why}"))?,
                None => Arc::new(Mutex::new(Alarms::in_memory()?)),
            })
        };
        Ok(Box::new(KeyedHost {
            alarms,
            keyed: app.keyed.is_some(),
        }))
    }
}

struct KeyedHost {
    alarms: Option<Arc<Mutex<Alarms>>>,
    /// Whether the app has a `[keyed]` at all: an alarm with nowhere to run
    /// is a mistake worth saying out loud.
    keyed: bool,
}

impl HostApi for KeyedHost {
    fn module_schema(&self) -> ModuleSchema {
        KEYED
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let Some(alarms) = &self.alarms else {
            return Err(RuntimeError::new("keyed: this app has no alarm store"));
        };
        if !self.keyed {
            return Ok(Value::err(Value::error(format!(
                "keyed.{op}: this app has no `[keyed]`, so an alarm would have nowhere to run"
            ))));
        }
        let key = args.first().and_then(Value::as_str).unwrap_or_default();
        if key.is_empty() || key.len() > MAX_KEY {
            return Ok(Value::err(Value::error(format!(
                "keyed.{op}: a key is 1 to {MAX_KEY} bytes"
            ))));
        }
        let alarms = alarms.lock().unwrap_or_else(|p| p.into_inner());
        match op {
            "alarm" => {
                let at = args.get(1).and_then(Value::as_int).unwrap_or_default();
                match alarms.set(key, at) {
                    Ok(()) => Ok(Value::ok(Value::unit())),
                    Err(why) => Ok(Value::err(Value::error(format!("keyed.alarm: {why}")))),
                }
            }
            "clearAlarm" => match alarms.clear(key) {
                Ok(was) => Ok(Value::ok(Value::bool(was))),
                Err(why) => Ok(Value::err(Value::error(format!("keyed.clearAlarm: {why}")))),
            },
            // `-1` for a key with no alarm: an `Option<Int>` would be the
            // honest type, and the schema's `Result` already carries the
            // failure, so the absent case is a value the app compares.
            "alarmAt" => match alarms.at(key) {
                Ok(at) => Ok(Value::ok(Value::int(at.unwrap_or(-1)))),
                Err(why) => Ok(Value::err(Value::error(format!("keyed.alarmAt: {why}")))),
            },
            other => Err(RuntimeError::new(format!(
                "`keyed` declares no operation `{other}`"
            ))),
        }
    }
}

/// How often the host looks for alarms that are due. A second, as a queue
/// is looked at: an alarm is "about now", not "at this millisecond".
const EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// How many of one app's due alarms the host starts at once.
const AT_ONCE: usize = 8;

/// Runs the alarms that are due, for as long as the host runs.
///
/// A due alarm takes its key's turn like a request does, so an alarm and a
/// request about the same key never run together — which is the whole
/// point of a key having one.
pub(crate) async fn ring(front: Arc<crate::server::Front>) {
    loop {
        tokio::time::sleep(EVERY).await;
        let Some(data) = front.options.data.clone() else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as i64);
        for slot in front.engine.slots() {
            let Some(app) = slot.current() else { continue };
            let Some(policy) = app.keyed.clone() else {
                continue;
            };
            if !app.granted.contains(CAPABILITY) || !slot.enabled() || app.ready().is_none() {
                continue;
            }
            let Ok(alarms) = shared(&data.join(&slot.name).join("alarms.sqlite3")) else {
                continue;
            };
            let due = {
                let alarms = alarms.lock().unwrap_or_else(|p| p.into_inner());
                alarms.take_due(now, AT_ONCE)
            };
            let due = match due {
                Ok(due) => due,
                Err(why) => {
                    app.logs.push("host", &format!("alarms: {why}"));
                    continue;
                }
            };
            for key in due {
                let front = Arc::clone(&front);
                let app = Arc::clone(&app);
                let policy = policy.clone();
                let index = slot.index;
                tokio::spawn(async move {
                    // The key's turn, waited for as a request waits: an
                    // alarm is not more urgent than what is already going
                    // on about that key.
                    let _turn = match front
                        .keys
                        .take(&app.name, &key, policy.max_waiting as usize)
                        .await
                    {
                        Ok(turn) => turn,
                        Err(_) => {
                            app.logs.push(
                                "host",
                                &format!("alarm `{key}`: too many waiting for it; not run"),
                            );
                            return;
                        }
                    };
                    let path = match &policy.by {
                        crate::config::KeyIn::Query(name) => {
                            format!("{}?{name}={key}", policy.alarm_path)
                        }
                        crate::config::KeyIn::Header(_) => policy.alarm_path.clone(),
                    };
                    // The path carries the key for a `query:` policy; a
                    // `header:` one gets it in the header the host sets,
                    // so an app reads it the same way either way.
                    let (path, query) = match path.split_once('?') {
                        Some((path, query)) => (path.to_string(), query.to_string()),
                        None => (path, String::new()),
                    };
                    let answer = crate::internal::admit_with(
                        &front.engine,
                        crate::internal::Asked {
                            app: &app,
                            slot: index,
                            forwarding: &front.options.forwarding,
                            by: (crate::internal::ALARM_HEADER, &key),
                            method: "POST",
                            path: &path,
                            body: String::new(),
                        },
                        &query,
                        match &policy.by {
                            crate::config::KeyIn::Header(name) => {
                                vec![(name.as_str(), key.clone())]
                            }
                            crate::config::KeyIn::Query(_) => Vec::new(),
                        }
                        .as_slice(),
                    );
                    app.counters
                        .alarms_run
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    match answer {
                        Ok(answer) => match answer.await {
                            Ok(reply) if reply.status < 500 => {}
                            Ok(reply) => {
                                app.counters
                                    .alarms_failed
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                app.logs.push(
                                    "host",
                                    &format!(
                                        "alarm `{key}`: {} {}",
                                        reply.status,
                                        crate::internal::first_line(&reply.body)
                                    ),
                                );
                            }
                            Err(_) => {
                                app.counters
                                    .alarms_failed
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                app.logs.push("host", &format!("alarm `{key}`: no answer"));
                            }
                        },
                        Err(_) => {
                            app.counters
                                .alarms_failed
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            app.logs.push("host", &format!("alarm `{key}`: not run"));
                        }
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alarms() -> Alarms {
        Alarms::in_memory().unwrap()
    }

    #[test]
    fn a_key_has_one_alarm_and_setting_it_again_moves_it() {
        let alarms = alarms();
        alarms.set("room-1", 1_000).unwrap();
        assert_eq!(alarms.at("room-1").unwrap(), Some(1_000));
        alarms.set("room-1", 2_000).unwrap();
        assert_eq!(alarms.at("room-1").unwrap(), Some(2_000));
        assert_eq!(alarms.count().unwrap(), 1);
        assert_eq!(alarms.at("room-2").unwrap(), None);
    }

    #[test]
    fn clearing_says_whether_there_was_one() {
        let alarms = alarms();
        assert!(!alarms.clear("room-1").unwrap());
        alarms.set("room-1", 1_000).unwrap();
        assert!(alarms.clear("room-1").unwrap());
        assert_eq!(alarms.at("room-1").unwrap(), None);
    }

    #[test]
    fn a_due_alarm_is_taken_once_and_in_time_order() {
        let alarms = alarms();
        alarms.set("late", 3_000).unwrap();
        alarms.set("early", 1_000).unwrap();
        alarms.set("soon", 2_000).unwrap();
        assert_eq!(alarms.take_due(999, 10).unwrap(), Vec::<String>::new());
        assert_eq!(alarms.take_due(2_000, 10).unwrap(), vec!["early", "soon"]);
        // Taken means taken: the same moment answers nothing twice.
        assert_eq!(alarms.take_due(2_000, 10).unwrap(), Vec::<String>::new());
        assert_eq!(alarms.take_due(9_000, 10).unwrap(), vec!["late"]);
        assert_eq!(alarms.count().unwrap(), 0);
    }

    #[test]
    fn a_limit_takes_the_soonest_first() {
        let alarms = alarms();
        for (key, at) in [("a", 1), ("b", 2), ("c", 3)] {
            alarms.set(key, at).unwrap();
        }
        assert_eq!(alarms.take_due(10, 2).unwrap(), vec!["a", "b"]);
        assert_eq!(alarms.count().unwrap(), 1);
    }

    #[tokio::test]
    async fn one_key_is_one_turn_at_a_time_and_another_key_is_not_waiting() {
        let keys = Keys::default();
        let first = keys.take("app", "room", 8).await.expect("the first turn");
        // Another key is free.
        let other = keys.take("app", "other", 8).await.expect("another key");
        // The same key waits: with the turn held, taking it does not
        // finish.
        let waiting = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            keys.take("app", "room", 8),
        )
        .await;
        assert!(waiting.is_err(), "the second turn waits for the first");
        drop(first);
        drop(other);
        // Once it is given up, the next one has it.
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            keys.take("app", "room", 8),
        )
        .await
        .expect("the turn was given up")
        .expect("the turn");
    }

    #[tokio::test]
    async fn more_waiting_than_max_waiting_is_refused() {
        let keys = Arc::new(Keys::default());
        let held = keys.take("app", "room", 1).await.expect("the turn");
        // One may wait; the next is refused rather than queued.
        let waiting = tokio::spawn({
            let keys = Arc::clone(&keys);
            async move { keys.take("app", "room", 1).await.map(|_| ()) }
        });
        // Give the waiter time to be counted.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            keys.take("app", "room", 1).await.err(),
            Some(NoTurn::TooMany(1))
        );
        drop(held);
        waiting.await.unwrap().expect("the waiter got its turn");
    }
}
