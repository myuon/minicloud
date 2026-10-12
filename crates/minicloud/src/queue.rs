//! `queue`: each app's persistent message queues.
//!
//! An app puts messages on a named queue; the host takes them off, delivers
//! them, and either finishes them, tries again later, or gives up and leaves
//! them among the dead letters for a person to read and, if they like, put
//! back. This module is the store only: the host module's schema and the
//! delivery loop live elsewhere.
//!
//! # Storage
//!
//! SQLite (bundled, through `rusqlite`), like the other per-app stores: one
//! file, a format that outlives this program, WAL mode with
//! `synchronous = NORMAL`. A committed write survives the host process
//! crashing; a power loss may lose the last transactions but never corrupts
//! the file. A message that was sent a moment before the power went may be
//! sent again by the app, which an at-least-once queue has to expect anyway.
//!
//! Message ids are never reused (`AUTOINCREMENT`). A dead letter keeps the id
//! it had, and reviving it puts that id back on the queue; if a new message
//! could be given the same id in the meantime, reviving would collide.
//!
//! # Leases, not deletes
//!
//! Taking a message does not remove it. [`Queues::lease`] pushes its due time
//! out by the lease and counts an attempt, so a delivery that crashes or never
//! answers is simply tried again when the lease runs out, rather than lost.
//! Only [`Queues::done`] or [`Queues::bury`] ends a message's life on the
//! queue.
//!
//! # Retention
//!
//! This layer never discards a dead letter by itself: how long they are kept
//! is the caller's decision, carried out with [`Queues::prune_dead`].
//!
//! Error messages never carry the file path: they reach HTTP responses and
//! logs that outsiders may read, and the data directory's layout is not
//! theirs to learn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use rusqlite::{params, Connection, Transaction, TransactionBehavior};

/// The most messages one `lease` takes.
pub const MAX_LEASE: usize = 1000;

/// The longest queue name, in bytes.
pub const MAX_QUEUE_NAME: usize = 64;

/// One message waiting to be delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: i64,
    /// The queue's name within the app: an app may have several.
    pub queue: String,
    pub body: String,
    /// How many times delivery has been tried and failed.
    pub attempts: i64,
    /// When it may next be tried, as seconds since the epoch.
    pub due_unix: i64,
    /// When it was first put on the queue.
    pub sent_unix: i64,
}

/// Why a message left the queue for good.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dropped {
    Delivered,
    Failed,
}

/// An app's queues.
pub struct Queues {
    conn: Connection,
}

fn fail(e: rusqlite::Error) -> String {
    e.to_string()
}

/// Whether `name` is something a queue may be called.
fn valid_name(name: &str) -> bool {
    (1..=MAX_QUEUE_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

fn message_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        queue: row.get(1)?,
        body: row.get(2)?,
        attempts: row.get(3)?,
        due_unix: row.get(4)?,
        sent_unix: row.get(5)?,
    })
}

impl Queues {
    /// Opens (and creates) the file, and the directory it lives in.
    pub fn open(path: &Path) -> Result<Queues, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create the queue store's directory: {e}"))?;
        }
        let conn = Connection::open(path).map_err(|_| "cannot open the queue store".to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(fail)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(fail)?;
        Queues::with(conn)
    }

    /// Queues that live and die with the process.
    pub fn in_memory() -> Result<Queues, String> {
        Queues::with(Connection::open_in_memory().map_err(fail)?)
    }

    fn with(conn: Connection) -> Result<Queues, String> {
        // `due` is indexed because `lease` asks for what is due on every
        // tick; `queue` because `send` counts a queue's depth on every call.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS message (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                queue TEXT NOT NULL,
                body TEXT NOT NULL,
                attempts INTEGER NOT NULL,
                due INTEGER NOT NULL,
                sent INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS message_due ON message (due);
            CREATE INDEX IF NOT EXISTS message_queue ON message (queue);
            CREATE TABLE IF NOT EXISTS dead (
                id INTEGER PRIMARY KEY,
                queue TEXT NOT NULL,
                body TEXT NOT NULL,
                attempts INTEGER NOT NULL,
                due INTEGER NOT NULL,
                sent INTEGER NOT NULL,
                why TEXT NOT NULL,
                buried INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS dead_buried ON dead (buried);",
        )
        .map_err(fail)?;
        Ok(Queues { conn })
    }

    /// A write transaction that takes the write lock at once, so that a
    /// read followed by a write cannot be interleaved with another writer's.
    fn write(&self) -> Result<Transaction<'_>, String> {
        Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate).map_err(fail)
    }

    /// Puts a message on `queue`, due now, and answers its id.
    ///
    /// Refuses (`Err`) a body past `max_body_bytes` or a queue that already
    /// holds `max_depth` messages, naming the limit. Dead letters do not
    /// count towards the depth: they are no longer waiting.
    pub fn send(
        &self,
        queue: &str,
        body: &str,
        now_unix: i64,
        max_body_bytes: usize,
        max_depth: i64,
    ) -> Result<i64, String> {
        if !valid_name(queue) {
            return Err(format!(
                "a queue name is 1 to {MAX_QUEUE_NAME} characters of a-z, 0-9, `_` and `-`"
            ));
        }
        if body.len() > max_body_bytes {
            return Err(format!(
                "a message of {} bytes is above this app's max_body_bytes of {max_body_bytes}",
                body.len()
            ));
        }
        let tx = self.write()?;
        let depth: i64 = tx
            .query_row(
                "SELECT count(*) FROM message WHERE queue = ?1",
                [queue],
                |row| row.get(0),
            )
            .map_err(fail)?;
        if depth >= max_depth {
            return Err(format!(
                "queue `{queue}` holds {depth} messages already, its max_depth of {max_depth}"
            ));
        }
        tx.execute(
            "INSERT INTO message (queue, body, attempts, due, sent) VALUES (?1, ?2, 0, ?3, ?3)",
            params![queue, body, now_unix],
        )
        .map_err(fail)?;
        let id = tx.last_insert_rowid();
        tx.commit().map_err(fail)?;
        Ok(id)
    }

    /// Takes up to `limit` messages that are due at `now_unix`, oldest
    /// first, and marks them as being tried: their `attempts` goes up by
    /// one and their `due_unix` moves to `now_unix + lease_secs`, so a
    /// delivery that never answers is tried again rather than lost.
    ///
    /// `limit` is held to 1 through [`MAX_LEASE`]. The choosing and the
    /// marking are one transaction: no message is missed or handed out twice.
    pub fn lease(
        &self,
        now_unix: i64,
        limit: usize,
        lease_secs: i64,
    ) -> Result<Vec<Message>, String> {
        let limit = limit.clamp(1, MAX_LEASE) as i64;
        let until = now_unix.saturating_add(lease_secs);
        let tx = self.write()?;
        let mut taken = {
            let mut select = tx
                .prepare_cached(
                    "SELECT id, queue, body, attempts, due, sent FROM message \
                     WHERE due <= ?1 ORDER BY sent, id LIMIT ?2",
                )
                .map_err(fail)?;
            let rows = select
                .query_map(params![now_unix, limit], message_row)
                .map_err(fail)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(fail)?
        };
        {
            let mut mark = tx
                .prepare_cached(
                    "UPDATE message SET attempts = attempts + 1, due = ?2 WHERE id = ?1",
                )
                .map_err(fail)?;
            for message in &mut taken {
                mark.execute(params![message.id, until]).map_err(fail)?;
                message.attempts += 1;
                message.due_unix = until;
            }
        }
        tx.commit().map_err(fail)?;
        Ok(taken)
    }

    /// Takes the message off the queue for good.
    pub fn done(&self, id: i64) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM message WHERE id = ?1", [id])
            .map_err(fail)?;
        Ok(())
    }

    /// Puts it back, due `delay_secs` from `now_unix`, for another try.
    pub fn retry(&self, id: i64, now_unix: i64, delay_secs: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE message SET due = ?2 WHERE id = ?1",
                params![id, now_unix.saturating_add(delay_secs)],
            )
            .map_err(fail)?;
        Ok(())
    }

    /// Moves it to the dead-letter table, with the reason.
    pub fn bury(&self, id: i64, now_unix: i64, why: &str) -> Result<(), String> {
        let tx = self.write()?;
        tx.execute(
            "INSERT OR REPLACE INTO dead (id, queue, body, attempts, due, sent, why, buried) \
             SELECT id, queue, body, attempts, due, sent, ?2, ?3 FROM message WHERE id = ?1",
            params![id, why, now_unix],
        )
        .map_err(fail)?;
        tx.execute("DELETE FROM message WHERE id = ?1", [id])
            .map_err(fail)?;
        tx.commit().map_err(fail)
    }

    /// The dead letters of `queue` (or all, for `None`), newest first, at
    /// most `limit`, each with the reason it was buried.
    pub fn dead(
        &self,
        queue: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(Message, String)>, String> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = self
            .conn
            .prepare_cached(
                "SELECT id, queue, body, attempts, due, sent, why FROM dead \
                 WHERE ?1 IS NULL OR queue = ?1 ORDER BY buried DESC, id DESC LIMIT ?2",
            )
            .map_err(fail)?;
        let rows = statement
            .query_map(params![queue, limit], |row| {
                Ok((message_row(row)?, row.get::<_, String>(6)?))
            })
            .map_err(fail)?;
        rows.collect::<Result<_, _>>().map_err(fail)
    }

    /// Puts a dead letter back on its queue, due now, with its attempts
    /// counted afresh; answers whether there was one with that id.
    pub fn revive(&self, id: i64, now_unix: i64) -> Result<bool, String> {
        let tx = self.write()?;
        let moved = tx
            .execute(
                "INSERT INTO message (id, queue, body, attempts, due, sent) \
                 SELECT id, queue, body, 0, ?2, sent FROM dead WHERE id = ?1",
                params![id, now_unix],
            )
            .map_err(fail)?;
        tx.execute("DELETE FROM dead WHERE id = ?1", [id])
            .map_err(fail)?;
        tx.commit().map_err(fail)?;
        Ok(moved > 0)
    }

    /// Deletes the dead letters buried before `before_unix`; answers how
    /// many. How long to keep them is the caller's to decide.
    pub fn prune_dead(&self, before_unix: i64) -> Result<u64, String> {
        let gone = self
            .conn
            .execute("DELETE FROM dead WHERE buried < ?1", [before_unix])
            .map_err(fail)?;
        Ok(gone as u64)
    }

    /// How many messages each queue holds, and how many dead letters, as
    /// `(queue name, waiting, dead)`, sorted by name.
    pub fn depths(&self) -> Result<Vec<(String, i64, i64)>, String> {
        let mut statement = self
            .conn
            .prepare_cached(
                "SELECT queue, sum(waiting), sum(buried) FROM (
                    SELECT queue, 1 AS waiting, 0 AS buried FROM message
                    UNION ALL
                    SELECT queue, 0, 1 FROM dead
                 ) GROUP BY queue ORDER BY queue",
            )
            .map_err(fail)?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(fail)?;
        rows.collect::<Result<_, _>>().map_err(fail)
    }
}

/// Every queue store open now, by file, held weakly: one `Queues` per
/// file however many versions of the app are alive, so a message sent by
/// the version that is going away is seen by the one that is coming.
fn open_stores() -> &'static Mutex<HashMap<PathBuf, Weak<Mutex<Queues>>>> {
    static OPEN: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<Queues>>>>> = OnceLock::new();
    OPEN.get_or_init(Mutex::default)
}

/// The store at `path`: the one already open, or newly opened.
pub fn shared(path: &Path) -> Result<Arc<Mutex<Queues>>, String> {
    let mut open = open_stores().lock().unwrap();
    if let Some(store) = open.get(path).and_then(Weak::upgrade) {
        return Ok(store);
    }
    let store = Arc::new(Mutex::new(Queues::open(path)?));
    open.insert(path.to_path_buf(), Arc::downgrade(&store));
    Ok(store)
}

/// What each queue of an app holds now, for the operations views; `None`
/// when no version of the app has its store open.
pub fn usage(path: &Path) -> Option<Vec<(String, i64, i64)>> {
    let store = open_stores().lock().unwrap().get(path)?.upgrade()?;
    let depths = store.lock().unwrap().depths().ok()?;
    Some(depths)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_000_000;
    const BODY: usize = 1024;
    const DEPTH: i64 = 100;

    fn send(q: &Queues, queue: &str, body: &str, at: i64) -> i64 {
        q.send(queue, body, at, BODY, DEPTH).unwrap()
    }

    fn ids(messages: &[Message]) -> Vec<i64> {
        messages.iter().map(|m| m.id).collect()
    }

    #[test]
    fn send_lease_done_goes_round_once() {
        let q = Queues::in_memory().unwrap();
        let id = send(&q, "mail", "hello", T);
        let got = q.lease(T, 10, 30).unwrap();
        assert_eq!(
            got,
            vec![Message {
                id,
                queue: "mail".into(),
                body: "hello".into(),
                attempts: 1,
                due_unix: T + 30,
                sent_unix: T,
            }]
        );
        q.done(id).unwrap();
        assert!(q.lease(T + 1000, 10, 30).unwrap().is_empty());
        assert_eq!(q.depths().unwrap(), vec![]);
    }

    #[test]
    fn lease_holds_back_what_is_not_due_and_what_is_leased() {
        let q = Queues::in_memory().unwrap();
        let id = send(&q, "a", "x", T);
        assert!(q.lease(T - 1, 10, 30).unwrap().is_empty());
        assert_eq!(ids(&q.lease(T, 10, 30).unwrap()), [id]);
        // Leased: not handed out again while the lease runs.
        assert!(q.lease(T + 29, 10, 30).unwrap().is_empty());
        let again = q.lease(T + 30, 10, 30).unwrap();
        assert_eq!(ids(&again), [id]);
        assert_eq!(again[0].attempts, 2);
        assert_eq!(again[0].due_unix, T + 60);
    }

    #[test]
    fn lease_takes_the_oldest_first_up_to_the_limit() {
        let q = Queues::in_memory().unwrap();
        let c = send(&q, "a", "c", T + 3);
        let a = send(&q, "a", "a", T + 1);
        let b = send(&q, "b", "b", T + 2);
        assert_eq!(ids(&q.lease(T + 10, 2, 60).unwrap()), [a, b]);
        assert_eq!(ids(&q.lease(T + 10, 2, 60).unwrap()), [c]);
        // A limit of 0 is held to 1.
        let d = send(&q, "a", "d", T + 11);
        let e = send(&q, "a", "e", T + 12);
        assert_eq!(ids(&q.lease(T + 20, 0, 60).unwrap()), [d]);
        assert_eq!(ids(&q.lease(T + 20, 5, 60).unwrap()), [e]);
    }

    #[test]
    fn retry_moves_due_and_the_message_comes_back_in_time() {
        let q = Queues::in_memory().unwrap();
        let id = send(&q, "a", "x", T);
        q.lease(T, 1, 30).unwrap();
        q.retry(id, T + 5, 100).unwrap();
        assert!(q.lease(T + 104, 1, 30).unwrap().is_empty());
        let got = q.lease(T + 105, 1, 30).unwrap();
        assert_eq!(ids(&got), [id]);
        assert_eq!(got[0].attempts, 2);
    }

    #[test]
    fn bury_moves_to_dead_and_revive_brings_it_back() {
        let q = Queues::in_memory().unwrap();
        let id = send(&q, "a", "x", T);
        q.lease(T, 1, 30).unwrap();
        q.bury(id, T + 1, "gave up").unwrap();
        assert!(q.lease(T + 1000, 10, 30).unwrap().is_empty());
        let dead = q.dead(None, 10).unwrap();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].1, "gave up");
        assert_eq!((dead[0].0.id, dead[0].0.attempts), (id, 1));
        assert_eq!(q.dead(Some("other"), 10).unwrap(), vec![]);
        assert_eq!(q.dead(Some("a"), 10).unwrap().len(), 1);

        assert!(q.revive(id, T + 50).unwrap());
        assert!(q.dead(None, 10).unwrap().is_empty());
        let got = q.lease(T + 50, 10, 30).unwrap();
        assert_eq!(ids(&got), [id]);
        // Counted afresh, then once for this lease.
        assert_eq!(got[0].attempts, 1);
        assert_eq!(got[0].sent_unix, T);
        assert!(!q.revive(id, T + 60).unwrap());
        assert!(!q.revive(9999, T + 60).unwrap());
    }

    #[test]
    fn dead_comes_newest_first_and_ids_are_not_reused() {
        let q = Queues::in_memory().unwrap();
        let a = send(&q, "a", "a", T);
        q.bury(a, T + 1, "first").unwrap();
        // The queue is empty, but the buried id must not be handed out again.
        let b = send(&q, "a", "b", T + 2);
        assert!(b > a);
        q.bury(b, T + 3, "second").unwrap();
        let dead = q.dead(None, 10).unwrap();
        assert_eq!(
            dead.iter().map(|(_, why)| why.as_str()).collect::<Vec<_>>(),
            ["second", "first"]
        );
        assert_eq!(q.dead(None, 1).unwrap().len(), 1);
    }

    #[test]
    fn send_refuses_bad_names_and_says_what_is_allowed() {
        let q = Queues::in_memory().unwrap();
        for bad in ["", "Mail", "a b", "a/b", "é", &"a".repeat(65)] {
            let why = q.send(bad, "x", T, BODY, DEPTH).unwrap_err();
            assert!(why.contains("a-z") && why.contains("64"), "{why}");
        }
        for good in ["a", "mail_out-2", &"a".repeat(64)] {
            q.send(good, "x", T, BODY, DEPTH).unwrap();
        }
    }

    #[test]
    fn send_refuses_past_the_depth_and_the_body_limit() {
        let q = Queues::in_memory().unwrap();
        q.send("a", "1", T, 4, 2).unwrap();
        q.send("a", "2", T, 4, 2).unwrap();
        let why = q.send("a", "3", T, 4, 2).unwrap_err();
        assert!(
            why.contains("max_depth of 2") && why.contains("2 messages"),
            "{why}"
        );
        // Another queue has its own depth.
        q.send("b", "1", T, 4, 2).unwrap();
        let why = q.send("b", "12345", T, 4, 2).unwrap_err();
        assert!(
            why.contains("max_body_bytes of 4") && why.contains("5 bytes"),
            "{why}"
        );
        // Bytes, not characters.
        assert!(q.send("b", "ééé", T, 4, 2).is_err());
        // Room opens when a message is done.
        let first = q.lease(T, 1, 10).unwrap()[0].id;
        q.done(first).unwrap();
        q.send("a", "3", T, 4, 2).unwrap();
    }

    #[test]
    fn depths_count_waiting_and_dead_by_name() {
        let q = Queues::in_memory().unwrap();
        send(&q, "zeta", "1", T);
        let a1 = send(&q, "alpha", "1", T);
        send(&q, "alpha", "2", T);
        let only_dead = send(&q, "mid", "1", T);
        q.bury(a1, T, "x").unwrap();
        q.bury(only_dead, T, "y").unwrap();
        assert_eq!(
            q.depths().unwrap(),
            vec![
                ("alpha".to_string(), 1, 1),
                ("mid".to_string(), 0, 1),
                ("zeta".to_string(), 1, 0),
            ]
        );
    }

    #[test]
    fn prune_dead_drops_only_the_old_and_counts_them() {
        let q = Queues::in_memory().unwrap();
        for (i, at) in [T, T + 10, T + 20].into_iter().enumerate() {
            let id = send(&q, "a", &i.to_string(), T);
            q.bury(id, at, "why").unwrap();
        }
        let live = send(&q, "a", "live", T);
        assert_eq!(q.prune_dead(T + 10).unwrap(), 1);
        assert_eq!(q.dead(None, 10).unwrap().len(), 2);
        assert_eq!(q.prune_dead(T + 100).unwrap(), 2);
        assert_eq!(q.prune_dead(T + 100).unwrap(), 0);
        assert_eq!(ids(&q.lease(T, 10, 10).unwrap()), [live]);
    }

    #[test]
    fn a_queue_survives_reopening_and_errors_carry_no_path() {
        let dir = std::env::temp_dir().join(format!("minicloud-queue-{}", std::process::id()));
        let path = dir.join("app").join("queue.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        let id = {
            let q = Queues::open(&path).unwrap();
            send(&q, "a", "kept", T)
        };
        let q = Queues::open(&path).unwrap();
        assert_eq!(ids(&q.lease(T, 10, 10).unwrap()), [id]);
        let _ = std::fs::remove_dir_all(&dir);

        // A path that cannot be a database: the error must not name it.
        let blocker =
            std::env::temp_dir().join(format!("minicloud-queue-f-{}", std::process::id()));
        std::fs::write(&blocker, "not a directory").unwrap();
        let why = Queues::open(&blocker.join("secret-dir").join("q.sqlite3"))
            .err()
            .unwrap();
        assert!(
            !why.contains("secret-dir") && !why.contains("minicloud-queue-f"),
            "{why}"
        );
        let _ = std::fs::remove_file(&blocker);
    }
}
