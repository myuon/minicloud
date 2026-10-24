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
//! **Only the delivery holding the current lease settles a message.** A run
//! may take longer than its lease: nothing holds a queue's `lease` to the
//! app's deadline, and a run that is still going when the lease runs out is,
//! as far as `lease` can tell, a delivery that was lost. The message is
//! handed out again, and two deliveries now have it. If the first one could
//! still settle it by id, it would settle the second's: its `done` would
//! delete a message somebody else is working on, its `retry` would move the
//! due time of the second's lease (and so let a third delivery take it), and
//! its `bury` would make a dead letter of a message that is being taken. So
//! every lease writes a fresh `lease_id` beside the message, the delivery
//! carries it ([`Message::lease_id`]), and [`Queues::done`],
//! [`Queues::retry`] and [`Queues::bury`] act only where it still matches,
//! answering whether it did. A delivery that outlived its lease changes
//! nothing; the one that holds the lease now settles the message as if the
//! first had never been. `attempts` cannot be the token: [`Queues::revive`]
//! counts it afresh from 0, so a number from before the revive could come
//! round again.
//!
//! A lease id comes from a counter in the process, started at the clock's
//! nanoseconds when it is first used (`next_lease_id`): unique within the
//! process, and in practice across restarts, since a restart begins
//! further on than the last process could have counted. A message no
//! delivery holds has none, and no settle matches it: every settle names
//! a lease.
//!
//! **No lease past `max_attempts`.** [`Queues::lease`] takes the queue's
//! current `max_attempts` and hands out only a message tried fewer times
//! than that; the bound does not rest on callers asking in the right order.
//! A message that is due and has had its attempts is
//! [`Queues::bury_spent`]'s, which the delivery loop calls first, at the
//! same instant it then leases at (a lease that ran out between two
//! readings of the clock would otherwise be neither buried nor refused).
//! It is a dead letter for one of two reasons, and says which: it still
//! holds a lease, so its last delivery did not answer in time (every
//! settle ends the lease or takes the message away); or it holds none, so
//! it was waiting for a retry when a redeploy lowered `max_attempts`. A
//! delivery the host turns away before the app sees it is not an attempt,
//! and [`Queues::give_back`] takes back the one `lease` counted.
//!
//! **A settle is kept until it is written.** A delivery's settle is put in
//! memory beside the store ([`Queues::hold`]) and then written; a write
//! that fails leaves it there, and the delivery loop writes it again at its
//! next tick. Until then the message is its delivery's: `bury_spent` does
//! not bury it and `lease` does not hand it out, though its lease may run
//! out meanwhile — the app answered, and "no answer within the lease" would
//! be untrue. The kept settles live as long as the process: a crash takes
//! them with every delivery, and the message's lease runs out as any
//! crashed delivery's does.
//!
//! # Retention
//!
//! This layer never discards a dead letter by itself: how long they are kept
//! is the caller's decision, carried out with [`Queues::prune_dead`].
//!
//! # The admin's window
//!
//! [`Queues::lease`] is the only way the delivery loop sees a message, and
//! it is a side effect: it takes the message, counts the try and starts the
//! lease. A page cannot be built on it. So the window the admin app opens
//! (issue 90) has reads of its own — [`peek_depths`], [`peek_waiting`],
//! [`peek_dead`] and [`peek_one`] — which take nothing and change nothing,
//! and go through the store the app has open where one is open
//! (`with_reader`, the shape [`crate::kv::peek`] and
//! [`crate::sql::peek_tables`] have for the same job). Their limits are the
//! window's own ([`PEEK_MESSAGES`], [`PEEK_BODY_CHARS`] and
//! [`READ_BODY_CHARS`]): an app's `[[queues]]` is for the app, and reading
//! its queue should not mean raising anything.
//!
//! **A message is `due` or it is `held`, and the store says no more than
//! that.** `due` is the next tick's work: `lease` takes what is due, oldest
//! sent first. `held` is a message whose `due` is still ahead — which is
//! either a delivery holding it on lease ([`Queues::lease`] pushed `due` out
//! by the queue's `lease`) or a failed delivery waiting for its retry
//! ([`Queues::retry`] pushed it out by the delay), or one sent for later.
//! One column carries all three, and the window says so rather than
//! calling a backoff a lease. (`lease_id` could now tell a lease from the
//! rest, since only a lease sets it; the window does not read it yet.)
//! `attempts` is the useful difference: one with 2 has been tried twice
//! whichever it is, and one with 0 has never been tried — sent for later,
//! or given back by a host too busy to begin it ([`Queues::give_back`]).
//!
//! Error messages never carry the file path: they reach HTTP responses and
//! logs that outsiders may read, and the data directory's layout is not
//! theirs to learn.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};

/// The most messages one `lease` takes.
pub const MAX_LEASE: usize = 1000;

/// The longest queue name, in bytes.
pub const MAX_QUEUE_NAME: usize = 64;

/// The reason a message is buried with when its lease ran out on its last
/// attempt: that delivery did not answer in time, and may still be running,
/// so "no answer" alone would say less than is known.
pub const OUTLIVED: &str = "no answer within the lease";

/// The reason a message is buried with when it held no lease and
/// `max_attempts` had come down to or below the attempts it had had: no
/// delivery failed it, a redeploy did.
pub fn lowered(max_attempts: i64) -> String {
    format!("max_attempts lowered to {max_attempts}")
}

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
    /// The lease this copy of the message was handed out under, which
    /// settling it has to name; `None` for a message read rather than
    /// leased, or one no delivery has taken.
    pub lease_id: Option<i64>,
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
    /// Settles a delivery has decided and the file does not have yet, by
    /// message id and the lease they were decided under. In memory only: a
    /// crash takes them with every delivery, which is what a crash is.
    unwritten: BTreeMap<(i64, i64), Unwritten>,
}

/// What a delivery decided to do with its message.
///
/// `at` is when it was decided — when the run answered, or was turned away
/// — and a settle written later is written as of then: a retry kept over a
/// failed write is still due `delay_secs` after the answer, not after the
/// write that finally took.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settle {
    /// The app took it ([`Queues::done`]).
    Done,
    /// Not taken; tried again `delay_secs` after `at` ([`Queues::retry`]).
    Retry {
        at: i64,
        delay_secs: i64,
        why: String,
    },
    /// Not taken, on its last attempt ([`Queues::bury`]).
    Bury { at: i64, why: String },
    /// The app never saw it ([`Queues::give_back`]); `why` is what to say
    /// in the app's log, if anything.
    GiveBack {
        at: i64,
        delay_secs: i64,
        why: Option<String>,
    },
}

/// A settle decided and not yet written, with what saying it needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unwritten {
    pub queue: String,
    /// The attempts the message had when its delivery settled.
    pub attempts: i64,
    pub settle: Settle,
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
        lease_id: None,
    })
}

/// A lease id no lease of this process has had.
///
/// The counter starts at the clock's nanoseconds since the epoch the first
/// time it is asked and goes up by one a lease, so a process that starts
/// after another has stopped begins past every id the last one handed out:
/// the clock gains a billion a second, and no process leases that fast. Only
/// a clock set back could bring an old id round again, and then only one
/// whose message is still on the queue with that very lease would matter.
fn next_lease_id() -> i64 {
    static NEXT: OnceLock<AtomicI64> = OnceLock::new();
    NEXT.get_or_init(|| {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        AtomicI64::new(i64::try_from(since).unwrap_or(i64::MAX / 2))
    })
    .fetch_add(1, Ordering::Relaxed)
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
        // `lease_id` came after the table did, and `CREATE TABLE IF NOT
        // EXISTS` leaves a store made before it as it was. Its messages
        // have no lease id, which is right: the deliveries that leased them
        // were the last process's, and they are gone.
        let has_lease_id = conn
            .prepare("SELECT 1 FROM pragma_table_info('message') WHERE name = 'lease_id'")
            .and_then(|mut found| found.exists([]))
            .map_err(fail)?;
        if !has_lease_id {
            conn.execute_batch("ALTER TABLE message ADD COLUMN lease_id INTEGER")
                .map_err(fail)?;
        }
        Ok(Queues {
            conn,
            unwritten: BTreeMap::new(),
        })
    }

    /// Keeps the settle a delivery holding lease `lease_id` of message `id`
    /// has decided, before it is written. Until [`Queues::forget`] it is
    /// the delivery's answer, and the message is not the tick's:
    /// [`Queues::bury_spent`] does not bury it, and [`Queues::lease`] does
    /// not hand it out, though its lease may run out meanwhile. Burying it
    /// would say "no answer within the lease" of a message whose app
    /// answered; leasing it would hand out what is as good as settled.
    pub fn hold(&mut self, id: i64, lease_id: i64, unwritten: Unwritten) {
        self.unwritten.insert((id, lease_id), unwritten);
    }

    /// Drops a kept settle: it was written, or found its lease gone.
    pub fn forget(&mut self, id: i64, lease_id: i64) {
        self.unwritten.remove(&(id, lease_id));
    }

    /// Every settle kept and not yet written, oldest message first, as
    /// `(id, lease id, settle)`.
    pub fn unwritten(&self) -> Vec<(i64, i64, Unwritten)> {
        self.unwritten
            .iter()
            .map(|((id, lease_id), unwritten)| (*id, *lease_id, unwritten.clone()))
            .collect()
    }

    /// Whether the lease a message holds now has a settle kept for it.
    fn answered(&self, id: i64, lease_id: Option<i64>) -> bool {
        lease_id.is_some_and(|lease_id| self.unwritten.contains_key(&(id, lease_id)))
    }

    /// A write transaction that takes the write lock at once, so that a
    /// read followed by a write cannot be interleaved with another writer's.
    fn write(&self) -> Result<Transaction<'_>, String> {
        Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate).map_err(fail)
    }

    /// Puts a message on `queue`, to be delivered from `due_unix`, and
    /// answers its id. `now_unix` is when it was sent, kept apart from the
    /// due time so a message held back for a while still says when it was
    /// asked for; `lease` hands it out only once `due_unix` has come.
    ///
    /// Refuses (`Err`) a body past `max_body_bytes` or a queue that already
    /// holds `max_depth` messages, naming the limit. Dead letters do not
    /// count towards the depth: they are no longer waiting. A message held
    /// back for later does count — it is waiting, and a queue that could be
    /// filled with them unseen would have no bound at all.
    pub fn send(
        &self,
        queue: &str,
        body: &str,
        now_unix: i64,
        due_unix: i64,
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
            "INSERT INTO message (queue, body, attempts, due, sent) VALUES (?1, ?2, 0, ?3, ?4)",
            params![queue, body, due_unix, now_unix],
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
    ///
    /// Each message taken is given a lease id of its own, which replaces
    /// whatever lease it had: the delivery that held that one, if it is
    /// still running, can no longer settle it.
    ///
    /// `queue` takes from that queue alone (`None`, from all of them). The
    /// delivery loop leases a queue at a time, with that queue's lease and
    /// room; a message of another queue taken here would have an attempt
    /// counted and no delivery to make it.
    ///
    /// Only a message tried fewer than `max_attempts` times is taken. The
    /// bound is the store's to keep, not the caller's order of calls: a
    /// message that has had its attempts — its last lease ran out, or
    /// `max_attempts` was lowered while it waited for a retry — is
    /// [`Queues::bury_spent`]'s, and handing it out here would be one
    /// delivery more than the queue allows.
    pub fn lease(
        &self,
        queue: Option<&str>,
        now_unix: i64,
        limit: usize,
        lease_secs: i64,
        max_attempts: i64,
    ) -> Result<Vec<Message>, String> {
        let limit = limit.clamp(1, MAX_LEASE);
        let until = now_unix.saturating_add(lease_secs);
        let tx = self.write()?;
        let mut taken = {
            // As many more as there are settles kept, which are passed over
            // (`hold`) and must not leave the batch short.
            let asked = (limit + self.unwritten.len()) as i64;
            let mut select = tx
                .prepare_cached(
                    "SELECT id, queue, body, attempts, due, sent, lease_id FROM message \
                     WHERE due <= ?1 AND (?3 IS NULL OR queue = ?3) AND attempts < ?4 \
                     ORDER BY sent, id LIMIT ?2",
                )
                .map_err(fail)?;
            let rows = select
                .query_map(params![now_unix, asked, queue, max_attempts], |row| {
                    Ok((message_row(row)?, row.get::<_, Option<i64>>(6)?))
                })
                .map_err(fail)?;
            let mut due = Vec::new();
            for row in rows {
                let (message, lease_id) = row.map_err(fail)?;
                if !self.answered(message.id, lease_id) {
                    due.push(message);
                }
            }
            due.truncate(limit);
            due
        };
        {
            let mut mark = tx
                .prepare_cached(
                    "UPDATE message SET attempts = attempts + 1, due = ?2, lease_id = ?3 \
                     WHERE id = ?1",
                )
                .map_err(fail)?;
            for message in &mut taken {
                let lease_id = next_lease_id();
                mark.execute(params![message.id, until, lease_id])
                    .map_err(fail)?;
                message.attempts += 1;
                message.due_unix = until;
                message.lease_id = Some(lease_id);
            }
        }
        tx.commit().map_err(fail)?;
        Ok(taken)
    }

    /// Takes the message off the queue for good, if `lease_id` is still
    /// its lease; answers whether it was.
    ///
    /// `lease_id` is the one [`Queues::lease`] handed out with the message
    /// ([`Message::lease_id`]). A `false` is a delivery that outlived its
    /// lease: another has the message now, and it is that one's to settle.
    ///
    /// There is no settling without a lease. A message with no `lease_id`
    /// is one no delivery holds — never leased, revived, or one whose lease
    /// a `retry` or `give_back` ended — and those cannot be told apart by
    /// the empty column, so a settle that named "no lease" would match a
    /// message somebody had already answered for.
    pub fn done(&self, id: i64, lease_id: i64) -> Result<bool, String> {
        let gone = self
            .conn
            .execute(
                "DELETE FROM message WHERE id = ?1 AND lease_id = ?2",
                params![id, lease_id],
            )
            .map_err(fail)?;
        Ok(gone > 0)
    }

    /// Puts it back, due `delay_secs` from `now_unix`, for another try, if
    /// `lease_id` is still its lease; answers whether it was, as
    /// [`Queues::done`] does.
    ///
    /// The lease ends here: a message waiting for its retry is held by
    /// nobody, which is what tells it apart from one whose lease ran out
    /// ([`Queues::bury_spent`]).
    pub fn retry(
        &self,
        id: i64,
        lease_id: i64,
        now_unix: i64,
        delay_secs: i64,
    ) -> Result<bool, String> {
        let moved = self
            .conn
            .execute(
                "UPDATE message SET due = ?2, lease_id = NULL WHERE id = ?1 AND lease_id = ?3",
                params![id, now_unix.saturating_add(delay_secs), lease_id],
            )
            .map_err(fail)?;
        Ok(moved > 0)
    }

    /// Puts it back as [`Queues::retry`] does, and takes back the attempt
    /// [`Queues::lease`] counted: for a delivery that never began, because
    /// the host turned the run away before the app saw the message.
    ///
    /// A host that is busy is not the consumer failing, and a message given
    /// back this way must not come nearer its `max_attempts` for it — or a
    /// busy hour could make a dead letter of a message no consumer was ever
    /// asked to take.
    pub fn give_back(
        &self,
        id: i64,
        lease_id: i64,
        now_unix: i64,
        delay_secs: i64,
    ) -> Result<bool, String> {
        let moved = self
            .conn
            .execute(
                "UPDATE message SET due = ?2, lease_id = NULL, attempts = max(attempts - 1, 0) \
                 WHERE id = ?1 AND lease_id = ?3",
                params![id, now_unix.saturating_add(delay_secs), lease_id],
            )
            .map_err(fail)?;
        Ok(moved > 0)
    }

    /// Buries the messages of `queue` that are due and have had their
    /// `max_attempts`, and answers each as it was with the reason it was
    /// given.
    ///
    /// A delivery that answers settles its message, and one that fails on
    /// the last attempt buries it. What is left for this is a message
    /// [`Queues::lease`] will no longer hand out, and there are two ways to
    /// become one, which the reason tells apart:
    ///
    /// - it **still holds a lease** that has run out: its last delivery did
    ///   not answer in time, and may still be running ([`OUTLIVED`]). Every
    ///   settle either ends the lease (`retry`, `give_back`) or takes the
    ///   message away, so a lease still held is one nobody settled.
    /// - it **holds none**: it was waiting for a retry (or had been given
    ///   back) when a redeploy lowered `max_attempts` to or below the
    ///   attempts it has had ([`lowered`]).
    ///
    /// Without this a message past its attempts would stay on the queue for
    /// good, neither delivered nor dead, where nobody would look for it.
    ///
    /// A message whose lease has a settle kept for it ([`Queues::hold`]) is
    /// left alone: its delivery answered, and the settle says what becomes
    /// of it once it is written.
    pub fn bury_spent(
        &self,
        queue: &str,
        max_attempts: i64,
        now_unix: i64,
    ) -> Result<Vec<(Message, String)>, String> {
        let tx = self.write()?;
        let spent = {
            let mut select = tx
                .prepare_cached(
                    "SELECT id, queue, body, attempts, due, sent, lease_id \
                     FROM message WHERE queue = ?1 AND due <= ?2 AND attempts >= ?3 \
                     ORDER BY sent, id",
                )
                .map_err(fail)?;
            let rows = select
                .query_map(params![queue, now_unix, max_attempts], |row| {
                    Ok((message_row(row)?, row.get::<_, Option<i64>>(6)?))
                })
                .map_err(fail)?;
            let mut spent = Vec::new();
            for row in rows {
                let (message, lease_id) = row.map_err(fail)?;
                if self.answered(message.id, lease_id) {
                    continue;
                }
                let why = match lease_id {
                    Some(_) => OUTLIVED.to_string(),
                    None => lowered(max_attempts),
                };
                spent.push((message, why));
            }
            spent
        };
        for (message, why) in &spent {
            tx.execute(
                "INSERT OR REPLACE INTO dead (id, queue, body, attempts, due, sent, why, buried) \
                 SELECT id, queue, body, attempts, due, sent, ?2, ?3 FROM message WHERE id = ?1",
                params![message.id, why, now_unix],
            )
            .map_err(fail)?;
            tx.execute("DELETE FROM message WHERE id = ?1", [message.id])
                .map_err(fail)?;
        }
        tx.commit().map_err(fail)?;
        Ok(spent)
    }

    /// Moves it to the dead-letter table, with the reason, if `lease_id` is
    /// still its lease; answers whether it was, as [`Queues::done`] does.
    ///
    /// The lease is asked about by both statements, inside one write
    /// transaction, so a message is never copied to `dead` and then left on
    /// the queue for whoever holds it now.
    pub fn bury(&self, id: i64, lease_id: i64, now_unix: i64, why: &str) -> Result<bool, String> {
        let tx = self.write()?;
        let buried = tx
            .execute(
                "INSERT OR REPLACE INTO dead (id, queue, body, attempts, due, sent, why, buried) \
                 SELECT id, queue, body, attempts, due, sent, ?2, ?3 FROM message \
                 WHERE id = ?1 AND lease_id = ?4",
                params![id, why, now_unix, lease_id],
            )
            .map_err(fail)?;
        tx.execute(
            "DELETE FROM message WHERE id = ?1 AND lease_id = ?2",
            params![id, lease_id],
        )
        .map_err(fail)?;
        tx.commit().map_err(fail)?;
        Ok(buried > 0)
    }

    /// Buries a message no delivery holds, for a test that wants a dead
    /// letter with a reason of its own and the attempts it has. Not a
    /// settle: nothing in the host may bury what it has not leased.
    #[cfg(test)]
    pub(crate) fn bury_unleased(&self, id: i64, now_unix: i64, why: &str) -> Result<bool, String> {
        let tx = self.write()?;
        let buried = tx
            .execute(
                "INSERT OR REPLACE INTO dead (id, queue, body, attempts, due, sent, why, buried) \
                 SELECT id, queue, body, attempts, due, sent, ?2, ?3 FROM message \
                 WHERE id = ?1 AND lease_id IS NULL",
                params![id, why, now_unix],
            )
            .map_err(fail)?;
        tx.execute(
            "DELETE FROM message WHERE id = ?1 AND lease_id IS NULL",
            [id],
        )
        .map_err(fail)?;
        tx.commit().map_err(fail)?;
        Ok(buried > 0)
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
    /// counted afresh and no lease, so no delivery from before it was
    /// buried can settle it; answers whether there was one with that id.
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

// -------------------------------------------------- the admin's window

/// The most messages one of the window's listings answers.
///
/// A page somebody reads, not a queue being drained: a queue's `max_depth`
/// is the app's and may be thousands, and a listing is not paged. The counts
/// beside it ([`peek_depths`]) are whole, so a page that has this many of
/// more than this can say so, rather than letting the first 200 stand for
/// the queue — the one wrong answer a window like this can give.
pub const PEEK_MESSAGES: i64 = 200;

/// How much of a body, or of a reason a message was buried, one row
/// carries, in characters.
///
/// A body is `max_body_bytes` (64 KiB by default) and a listing holds
/// [`PEEK_MESSAGES`] of them, so the whole of each is not what a listing is
/// for. The same length `kv`'s listing carries of a value
/// ([`crate::kv::PEEK_VALUE_CHARS`]), for the same reason: it is a value of
/// an app's of the same kind, and the page cuts it again to what a column
/// shows.
pub const PEEK_BODY_CHARS: usize = 512;

/// How much of a body one message's read carries, in characters.
///
/// A listing shows the start of two hundred bodies; this is one message
/// somebody opened, and a queue carries what an app sent — a webhook's
/// payload is routinely more than [`PEEK_BODY_CHARS`], so a window that
/// could not show the rest of one would be answering a question with half
/// an answer. The length `kv`'s own read carries
/// ([`crate::kv::READ_VALUE_CHARS`]), for the same reason: the app's
/// `max_body_bytes` may be larger still, and a browser wants an end to a
/// `<pre>`.
pub const READ_BODY_CHARS: usize = 64 * 1024;

/// What one queue of an app holds, as the window counts it: whole counts,
/// not a page's worth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Depth {
    pub name: String,
    /// Messages the next delivery tick may take: `due` at or before now.
    pub due: i64,
    /// Messages whose `due` is still ahead — a delivery's lease, or a failed
    /// delivery waiting for its retry. The store keeps one `due` for both,
    /// so this does not say which.
    pub held: i64,
    pub dead: i64,
}

/// One message, as the window sees it.
///
/// Waiting and buried are one type, with `why` and `buried_unix` empty for a
/// message that is still on the queue. A `TypeSchema`'s cases carry no
/// payload (`cove_runtime::TypeSchema`), so the admin module could not
/// declare "waiting, or buried with this reason" as one type anyway; and the
/// fields that would differ are two of eleven, against a page, a host
/// operation and a row renderer that would be written twice. `state` says
/// which it is, and never has to be guessed from an empty `why`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peeked {
    pub id: i64,
    pub queue: String,
    /// The body, cut to [`PEEK_BODY_CHARS`] characters in a listing and to
    /// [`READ_BODY_CHARS`] in one message's read; `bytes` is how long the
    /// whole of it is.
    pub body: String,
    pub bytes: i64,
    /// Whether `body` is shorter than the whole.
    pub cut: bool,
    /// How many deliveries have been tried. `lease` counts the try it hands
    /// out, so a message being delivered now already counts this one.
    pub attempts: i64,
    /// When it may next be tried, in seconds since the epoch. For a dead
    /// letter this is the `due` it had when it was buried, which is past.
    pub due_unix: i64,
    pub sent_unix: i64,
    /// `due`, `held` (see [`Depth`]) or `dead`.
    pub state: &'static str,
    /// Why it was buried, cut as the body is; empty unless `state` is
    /// `dead`.
    pub why: String,
    /// When it was buried; 0 unless `state` is `dead`.
    pub buried_unix: i64,
}

/// What each queue of the store at `path` holds at `now_unix`.
///
/// Only the queues the store has rows for: the queues an app *declares* are
/// in its `[[queues]]`, and joining the two is the caller's (`Front`'s
/// `app_queues`, in [`crate::manage`]), so that a queue declared and never
/// used is shown as empty rather than missing.
///
/// Empty for an app with no store, which is every app not granted `queue`.
pub fn peek_depths(path: &Path, now_unix: i64) -> Result<Vec<Depth>, String> {
    with_reader(path, Vec::new(), |conn| {
        let mut statement = conn.prepare(
            "SELECT queue, sum(due), sum(held), sum(buried) FROM (
                SELECT queue,
                       CASE WHEN due <= ?1 THEN 1 ELSE 0 END AS due,
                       CASE WHEN due <= ?1 THEN 0 ELSE 1 END AS held,
                       0 AS buried
                FROM message
                UNION ALL
                SELECT queue, 0, 0, 1 FROM dead
             ) GROUP BY queue ORDER BY queue",
        )?;
        let rows = statement.query_map([now_unix], |row| {
            Ok(Depth {
                name: row.get(0)?,
                due: row.get(1)?,
                held: row.get(2)?,
                dead: row.get(3)?,
            })
        })?;
        rows.collect()
    })
}

/// The messages still on the queues of the store at `path`, at most `limit`
/// (0–[`PEEK_MESSAGES`]), every queue together.
///
/// What is due first, in the order [`Queues::lease`] will take it — oldest
/// sent first — so the head of the list is the next delivery; then what is
/// held, soonest due first, which is the order it comes back in.
///
/// **This takes nothing.** No `attempts` is counted and no `due` is moved,
/// so a message read here is delivered exactly as if nobody had looked.
pub fn peek_waiting(path: &Path, now_unix: i64, limit: i64) -> Result<Vec<Peeked>, String> {
    let limit = limit.clamp(0, PEEK_MESSAGES);
    with_reader(path, Vec::new(), |conn| {
        let mut statement = conn.prepare(
            "SELECT id, queue, substr(body, 1, ?2), attempts, due, sent, \
                    length(body), length(CAST(body AS BLOB)) \
             FROM message \
             ORDER BY CASE WHEN due <= ?1 THEN 0 ELSE 1 END, \
                      CASE WHEN due <= ?1 THEN sent ELSE due END, id \
             LIMIT ?3",
        )?;
        let rows =
            statement.query_map(params![now_unix, PEEK_BODY_CHARS as i64, limit], |row| {
                let peeked = peeked_row(row)?;
                Ok(Peeked {
                    state: if peeked.due_unix <= now_unix {
                        "due"
                    } else {
                        "held"
                    },
                    ..peeked
                })
            })?;
        rows.collect()
    })
}

/// The dead letters of the store at `path`, newest buried first, at most
/// `limit` (0–[`PEEK_MESSAGES`]), each with why it was buried.
///
/// The same read [`Queues::dead`] does for `minicloud queues <app>`, with
/// the bodies cut and the time of the burial carried: the window shows many
/// at once where the command shows a few.
pub fn peek_dead(path: &Path, limit: i64) -> Result<Vec<Peeked>, String> {
    let limit = limit.clamp(0, PEEK_MESSAGES);
    with_reader(path, Vec::new(), |conn| {
        let mut statement = conn.prepare(
            "SELECT id, queue, substr(body, 1, ?1), attempts, due, sent, \
                    length(body), length(CAST(body AS BLOB)), \
                    substr(why, 1, ?1), buried \
             FROM dead ORDER BY buried DESC, id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![PEEK_BODY_CHARS as i64, limit], |row| {
            // `state`, `why` and `buried` are this read's; the rest is the
            // row every listing of a message reads.
            Ok(Peeked {
                state: "dead",
                why: row.get(8)?,
                buried_unix: row.get(9)?,
                ..peeked_row(row)?
            })
        })?;
        rows.collect()
    })
}

/// One message of the store at `path`, waiting or buried, by id; `None`
/// for an id neither holds.
///
/// Both tables are asked, because a page reaches this from either listing
/// and an id tells the two apart by itself: ids are never reused, a dead
/// letter keeps the id it had, and reviving moves that id back — so a
/// message is in one table or the other and never in both.
///
/// The body is cut at [`READ_BODY_CHARS`] rather than the listing's length:
/// this is the read that shows the whole of what was sent.
pub fn peek_one(path: &Path, id: i64, now_unix: i64) -> Result<Option<Peeked>, String> {
    with_reader(path, None, |conn| {
        let waiting = conn
            .query_row(
                "SELECT id, queue, substr(body, 1, ?2), attempts, due, sent, \
                        length(body), length(CAST(body AS BLOB)) \
                 FROM message WHERE id = ?1",
                params![id, READ_BODY_CHARS as i64],
                |row| {
                    let peeked = read_row(row)?;
                    Ok(Peeked {
                        state: if peeked.due_unix <= now_unix {
                            "due"
                        } else {
                            "held"
                        },
                        ..peeked
                    })
                },
            )
            .optional()?;
        if let Some(found) = waiting {
            return Ok(Some(found));
        }
        conn.query_row(
            "SELECT id, queue, substr(body, 1, ?2), attempts, due, sent, \
                    length(body), length(CAST(body AS BLOB)), \
                    substr(why, 1, ?2), buried \
             FROM dead WHERE id = ?1",
            params![id, READ_BODY_CHARS as i64],
            |row| {
                Ok(Peeked {
                    state: "dead",
                    why: row.get(8)?,
                    buried_unix: row.get(9)?,
                    ..read_row(row)?
                })
            },
        )
        .optional()
    })
}

/// A row of `SELECT id, queue, substr(body, …), attempts, due, sent,
/// length(body), length(CAST(body AS BLOB))` as a listing reads it — the
/// body cut at [`PEEK_BODY_CHARS`] — with `state` and what only a dead
/// letter has left to the caller.
fn peeked_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Peeked> {
    let chars: i64 = row.get(6)?;
    Ok(Peeked {
        id: row.get(0)?,
        queue: row.get(1)?,
        body: row.get(2)?,
        bytes: row.get(7)?,
        cut: chars > PEEK_BODY_CHARS as i64,
        attempts: row.get(3)?,
        due_unix: row.get(4)?,
        sent_unix: row.get(5)?,
        state: "due",
        why: String::new(),
        buried_unix: 0,
    })
}

/// The same row where the body was cut at [`READ_BODY_CHARS`]: only the
/// length `cut` is measured against differs.
fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Peeked> {
    let chars: i64 = row.get(6)?;
    Ok(Peeked {
        cut: chars > READ_BODY_CHARS as i64,
        ..peeked_row(row)?
    })
}

/// Runs `read` on the store at `path`, reading only; `empty` when there is
/// no file there.
///
/// The connection is **the app's own** where a version of the app has the
/// store open, as [`crate::kv::peek`] and [`crate::sql::peek_tables`] do and
/// for the same two reasons. A second connection on a WAL database reads the
/// snapshot its transaction started with, so a message sent a moment ago
/// would be missing from a window whose whole job is to say what is waiting;
/// and two connections on one file take locks against each other, so a read
/// would wait on a delivery's write or be refused as busy. The cost is the
/// other way round — the next `lease` waits for the window's read — which is
/// why these reads are three statements and no more.
///
/// A file that is not there is **no store** rather than an error:
/// [`Queues::open`] would create one, and an app not granted `queue` must
/// not come to have a queue store because somebody looked at it.
fn with_reader<T>(
    path: &Path,
    empty: T,
    read: impl FnOnce(&Connection) -> rusqlite::Result<T>,
) -> Result<T, String> {
    let open = open_stores()
        .lock()
        .unwrap()
        .get(path)
        .and_then(Weak::upgrade);
    let outcome = if let Some(store) = open {
        let store = store.lock().unwrap_or_else(|p| p.into_inner());
        read(&store.conn)
    } else {
        if !path.is_file() {
            return Ok(empty);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "cannot open the queue store".to_string())?;
        read(&conn)
    };
    match outcome {
        Ok(found) => Ok(found),
        // A file that is not a queue store of ours — nothing creates one,
        // but a hand-made file would — has nothing waiting on it.
        Err(e) if e.to_string().contains("no such table") => Ok(empty),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_000_000;
    const BODY: usize = 1024;
    const DEPTH: i64 = 100;

    fn send(q: &Queues, queue: &str, body: &str, at: i64) -> i64 {
        q.send(queue, body, at, at, BODY, DEPTH).unwrap()
    }

    fn ids(messages: &[Message]) -> Vec<i64> {
        messages.iter().map(|m| m.id).collect()
    }

    /// The lease `id` was handed out under in `taken`.
    fn lease_of(taken: &[Message], id: i64) -> i64 {
        let lease = taken.iter().find(|m| m.id == id).unwrap().lease_id;
        lease.expect("a leased message carries its lease")
    }

    #[test]
    fn send_lease_done_goes_round_once() {
        let q = Queues::in_memory().unwrap();
        let id = send(&q, "mail", "hello", T);
        let got = q.lease(None, T, 10, 30, 100).unwrap();
        assert_eq!(
            got,
            vec![Message {
                id,
                queue: "mail".into(),
                body: "hello".into(),
                attempts: 1,
                due_unix: T + 30,
                sent_unix: T,
                lease_id: Some(lease_of(&got, id)),
            }]
        );
        assert!(q.done(id, lease_of(&got, id)).unwrap());
        assert!(q.lease(None, T + 1000, 10, 30, 100).unwrap().is_empty());
        assert_eq!(q.depths().unwrap(), vec![]);
    }

    #[test]
    fn lease_holds_back_what_is_not_due_and_what_is_leased() {
        let q = Queues::in_memory().unwrap();
        let id = send(&q, "a", "x", T);
        assert!(q.lease(None, T - 1, 10, 30, 100).unwrap().is_empty());
        assert_eq!(ids(&q.lease(None, T, 10, 30, 100).unwrap()), [id]);
        // Leased: not handed out again while the lease runs.
        assert!(q.lease(None, T + 29, 10, 30, 100).unwrap().is_empty());
        let again = q.lease(None, T + 30, 10, 30, 100).unwrap();
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
        assert_eq!(ids(&q.lease(None, T + 10, 2, 60, 100).unwrap()), [a, b]);
        assert_eq!(ids(&q.lease(None, T + 10, 2, 60, 100).unwrap()), [c]);
        // A limit of 0 is held to 1.
        let d = send(&q, "a", "d", T + 11);
        let e = send(&q, "a", "e", T + 12);
        assert_eq!(ids(&q.lease(None, T + 20, 0, 60, 100).unwrap()), [d]);
        assert_eq!(ids(&q.lease(None, T + 20, 5, 60, 100).unwrap()), [e]);
    }

    #[test]
    fn retry_moves_due_and_the_message_comes_back_in_time() {
        let q = Queues::in_memory().unwrap();
        let id = send(&q, "a", "x", T);
        let taken = q.lease(None, T, 1, 30, 100).unwrap();
        assert!(q.retry(id, lease_of(&taken, id), T + 5, 100).unwrap());
        assert!(q.lease(None, T + 104, 1, 30, 100).unwrap().is_empty());
        let got = q.lease(None, T + 105, 1, 30, 100).unwrap();
        assert_eq!(ids(&got), [id]);
        assert_eq!(got[0].attempts, 2);
    }

    #[test]
    fn bury_moves_to_dead_and_revive_brings_it_back() {
        let q = Queues::in_memory().unwrap();
        let id = send(&q, "a", "x", T);
        let taken = q.lease(None, T, 1, 30, 100).unwrap();
        assert!(q.bury(id, lease_of(&taken, id), T + 1, "gave up").unwrap());
        assert!(q.lease(None, T + 1000, 10, 30, 100).unwrap().is_empty());
        let dead = q.dead(None, 10).unwrap();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].1, "gave up");
        assert_eq!((dead[0].0.id, dead[0].0.attempts), (id, 1));
        assert_eq!(q.dead(Some("other"), 10).unwrap(), vec![]);
        assert_eq!(q.dead(Some("a"), 10).unwrap().len(), 1);

        assert!(q.revive(id, T + 50).unwrap());
        assert!(q.dead(None, 10).unwrap().is_empty());
        let got = q.lease(None, T + 50, 10, 30, 100).unwrap();
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
        q.bury_unleased(a, T + 1, "first").unwrap();
        // The queue is empty, but the buried id must not be handed out again.
        let b = send(&q, "a", "b", T + 2);
        assert!(b > a);
        q.bury_unleased(b, T + 3, "second").unwrap();
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
            let why = q.send(bad, "x", T, T, BODY, DEPTH).unwrap_err();
            assert!(why.contains("a-z") && why.contains("64"), "{why}");
        }
        for good in ["a", "mail_out-2", &"a".repeat(64)] {
            q.send(good, "x", T, T, BODY, DEPTH).unwrap();
        }
    }

    #[test]
    fn a_message_sent_for_later_is_held_until_it_is_due() {
        let q = Queues::in_memory().unwrap();
        let id = q.send("a", "later", T, T + 60, BODY, DEPTH).unwrap();
        assert!(q.lease(None, T, 10, 30, 100).unwrap().is_empty());
        assert!(q.lease(None, T + 59, 10, 30, 100).unwrap().is_empty());
        let got = q.lease(None, T + 60, 10, 30, 100).unwrap();
        assert_eq!(ids(&got), [id]);
        // It still says when it was asked for, not when it came due.
        assert_eq!(got[0].sent_unix, T);
    }

    #[test]
    fn a_held_message_still_counts_towards_the_depth() {
        let q = Queues::in_memory().unwrap();
        q.send("a", "1", T, T + 60, 4, 2).unwrap();
        q.send("a", "2", T, T + 60, 4, 2).unwrap();
        let why = q.send("a", "3", T, T, 4, 2).unwrap_err();
        assert!(why.contains("max_depth of 2"), "{why}");
        assert_eq!(q.depths().unwrap(), vec![("a".to_string(), 2, 0)]);
    }

    #[test]
    fn send_refuses_past_the_depth_and_the_body_limit() {
        let q = Queues::in_memory().unwrap();
        q.send("a", "1", T, T, 4, 2).unwrap();
        q.send("a", "2", T, T, 4, 2).unwrap();
        let why = q.send("a", "3", T, T, 4, 2).unwrap_err();
        assert!(
            why.contains("max_depth of 2") && why.contains("2 messages"),
            "{why}"
        );
        // Another queue has its own depth.
        q.send("b", "1", T, T, 4, 2).unwrap();
        let why = q.send("b", "12345", T, T, 4, 2).unwrap_err();
        assert!(
            why.contains("max_body_bytes of 4") && why.contains("5 bytes"),
            "{why}"
        );
        // Bytes, not characters.
        assert!(q.send("b", "ééé", T, T, 4, 2).is_err());
        // Room opens when a message is done.
        let taken = q.lease(None, T, 1, 10, 100).unwrap();
        assert!(q.done(taken[0].id, lease_of(&taken, taken[0].id)).unwrap());
        q.send("a", "3", T, T, 4, 2).unwrap();
    }

    #[test]
    fn depths_count_waiting_and_dead_by_name() {
        let q = Queues::in_memory().unwrap();
        send(&q, "zeta", "1", T);
        let a1 = send(&q, "alpha", "1", T);
        send(&q, "alpha", "2", T);
        let only_dead = send(&q, "mid", "1", T);
        q.bury_unleased(a1, T, "x").unwrap();
        q.bury_unleased(only_dead, T, "y").unwrap();
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
            q.bury_unleased(id, at, "why").unwrap();
        }
        let live = send(&q, "a", "live", T);
        assert_eq!(q.prune_dead(T + 10).unwrap(), 1);
        assert_eq!(q.dead(None, 10).unwrap().len(), 2);
        assert_eq!(q.prune_dead(T + 100).unwrap(), 2);
        assert_eq!(q.prune_dead(T + 100).unwrap(), 0);
        assert_eq!(ids(&q.lease(None, T, 10, 10, 100).unwrap()), [live]);
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
        assert_eq!(ids(&q.lease(None, T, 10, 10, 100).unwrap()), [id]);
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

    // ---------------------------------------- who may settle a message

    /// A message's row as the settles can change it: on the queue (with
    /// its due time and lease), among the dead, or neither.
    #[derive(Debug, PartialEq, Eq)]
    enum Row {
        Waiting { due: i64, lease_id: Option<i64> },
        Dead,
        Gone,
    }

    fn row(q: &Queues, id: i64) -> Row {
        let waiting = q
            .conn
            .query_row(
                "SELECT due, lease_id FROM message WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .unwrap();
        let dead = q
            .conn
            .query_row("SELECT 1 FROM dead WHERE id = ?1", [id], |_| Ok(()))
            .optional()
            .unwrap()
            .is_some();
        match (waiting, dead) {
            (Some((due, lease_id)), false) => Row::Waiting { due, lease_id },
            (None, true) => Row::Dead,
            (None, false) => Row::Gone,
            (Some(_), true) => panic!("message {id} is on the queue and among the dead"),
        }
    }

    /// A delivery that ran past its lease, and the one that took the
    /// message when the lease ran out: the first is `stale`, the second
    /// holds the message now.
    fn handed_on(q: &Queues) -> (i64, i64, i64) {
        let id = send(q, "a", "x", T);
        let stale = lease_of(&q.lease(None, T, 1, 30, 100).unwrap(), id);
        let current = lease_of(&q.lease(None, T + 30, 1, 30, 100).unwrap(), id);
        assert_ne!(stale, current);
        (id, stale, current)
    }

    #[test]
    fn a_delivery_that_outlived_its_lease_cannot_take_the_message_away() {
        let q = Queues::in_memory().unwrap();
        let (id, stale, current) = handed_on(&q);
        assert!(!q.done(id, stale).unwrap());
        assert_eq!(
            row(&q, id),
            Row::Waiting {
                due: T + 60,
                lease_id: Some(current)
            }
        );
        assert!(q.done(id, current).unwrap());
        assert_eq!(row(&q, id), Row::Gone);
        // Settling what is gone is not an error, and not a success either.
        assert!(!q.done(id, current).unwrap());
    }

    #[test]
    fn a_delivery_that_outlived_its_lease_cannot_move_the_current_one() {
        let q = Queues::in_memory().unwrap();
        let (id, stale, current) = handed_on(&q);
        // Had it moved `due` to T + 31, a third delivery could take the
        // message while the second still has it.
        assert!(!q.retry(id, stale, T + 31, 0).unwrap());
        assert_eq!(
            row(&q, id),
            Row::Waiting {
                due: T + 60,
                lease_id: Some(current)
            }
        );
        assert!(q.lease(None, T + 59, 10, 30, 100).unwrap().is_empty());
        // The holder's retry moves it, and ends the lease.
        assert!(q.retry(id, current, T + 40, 5).unwrap());
        assert_eq!(
            row(&q, id),
            Row::Waiting {
                due: T + 45,
                lease_id: None
            }
        );
    }

    #[test]
    fn a_delivery_that_outlived_its_lease_cannot_bury_the_message() {
        let q = Queues::in_memory().unwrap();
        let (id, stale, current) = handed_on(&q);
        assert!(!q.bury(id, stale, T + 31, "500 late").unwrap());
        assert_eq!(
            row(&q, id),
            Row::Waiting {
                due: T + 60,
                lease_id: Some(current)
            }
        );
        assert!(q.dead(None, 10).unwrap().is_empty());
        assert!(q.bury(id, current, T + 40, "500 now").unwrap());
        assert_eq!(row(&q, id), Row::Dead);
        assert_eq!(q.dead(None, 10).unwrap()[0].1, "500 now");
    }

    #[test]
    fn a_lease_from_before_a_revive_does_not_settle_the_revived_message() {
        let q = Queues::in_memory().unwrap();
        let id = send(&q, "a", "x", T);
        let before = lease_of(&q.lease(None, T, 1, 30, 100).unwrap(), id);
        assert!(q.bury(id, before, T + 1, "gave up").unwrap());
        assert!(q.revive(id, T + 2).unwrap());
        // Revived, it has no lease, and its attempts start again from 0, so
        // a token made of `attempts` would match here.
        assert!(!q.done(id, before).unwrap());
        assert!(!q.retry(id, before, T + 2, 100).unwrap());
        let after = lease_of(&q.lease(None, T + 2, 1, 30, 100).unwrap(), id);
        assert!(!q.done(id, before).unwrap());
        assert!(!q.bury(id, before, T + 3, "late").unwrap());
        assert!(q.done(id, after).unwrap());
        assert_eq!(row(&q, id), Row::Gone);
    }

    #[test]
    fn a_lease_that_runs_out_on_the_last_attempt_is_a_dead_letter() {
        let q = Queues::in_memory().unwrap();
        let id = send(&q, "a", "x", T);
        // Two attempts allowed; each delivery outlives its lease.
        assert_eq!(ids(&q.lease(Some("a"), T, 1, 30, 2).unwrap()), [id]);
        assert!(q.bury_spent("a", 2, T + 30).unwrap().is_empty());
        assert_eq!(ids(&q.lease(Some("a"), T + 30, 1, 30, 2).unwrap()), [id]);
        // Not while the second lease runs, and not leased a third time.
        assert!(q.bury_spent("a", 2, T + 59).unwrap().is_empty());
        assert!(q.lease(Some("a"), T + 60, 1, 30, 2).unwrap().is_empty());
        let spent = q.bury_spent("a", 2, T + 60).unwrap();
        assert_eq!(spent.len(), 1);
        assert_eq!((spent[0].0.id, spent[0].0.attempts), (id, 2));
        assert_eq!(spent[0].1, OUTLIVED);
        assert_eq!(row(&q, id), Row::Dead);
        assert_eq!(q.dead(None, 10).unwrap()[0].1, OUTLIVED);
        assert!(q.lease(Some("a"), T + 1000, 10, 30, 2).unwrap().is_empty());
    }

    /// S5: what is due and has had its `max_attempts` is buried whether it
    /// holds a lease or not, and the reason says which; what has attempts
    /// left is not, nor is another queue's.
    #[test]
    fn the_tick_buries_every_due_message_that_has_had_its_attempts() {
        let q = Queues::in_memory().unwrap();
        // Retried after its first attempt, and `max_attempts` lowered to 1
        // since: the retry ended its lease, and it is not tried again.
        let retried = send(&q, "a", "retried", T);
        let taken = q.lease(Some("a"), T, 1, 30, 3).unwrap();
        assert!(q.retry(retried, lease_of(&taken, retried), T, 0).unwrap());
        // Revived: no attempts and no lease.
        let revived = send(&q, "a", "revived", T + 1);
        assert!(q.bury_unleased(revived, T + 1, "gave up").unwrap());
        assert!(q.revive(revived, T + 1).unwrap());
        // Sent and never tried.
        let fresh = send(&q, "a", "fresh", T + 2);
        // Another queue's spent message is that queue's to bury.
        let other = send(&q, "b", "other", T + 3);
        q.lease(Some("b"), T + 3, 1, 1, 1).unwrap();
        // `lease` refuses the spent one by itself.
        assert_eq!(
            ids(&q.lease(Some("a"), T + 100, 10, 30, 1).unwrap()),
            [revived, fresh]
        );
        let spent = q.bury_spent("a", 1, T + 100).unwrap();
        assert_eq!(spent.len(), 1);
        assert_eq!(spent[0].0.id, retried);
        assert_eq!(spent[0].1, "max_attempts lowered to 1");
        // The two leased just now are not due, so not spent.
        assert!(q.bury_spent("a", 1, T + 100).unwrap().is_empty());
        let spent = q.bury_spent("b", 1, T + 100).unwrap();
        assert_eq!(spent.len(), 1);
        assert_eq!((spent[0].0.id, spent[0].1.as_str()), (other, OUTLIVED));
    }

    /// A settle kept for the lease a message holds makes the message its
    /// delivery's: when the lease runs out it is neither spent nor leased,
    /// and the batch is filled from behind it. Once forgotten, it is due.
    #[test]
    fn a_message_with_a_kept_settle_is_neither_spent_nor_leased() {
        let mut q = Queues::in_memory().unwrap();
        let id = send(&q, "a", "x", T);
        let token = lease_of(&q.lease(Some("a"), T, 1, 30, 1).unwrap(), id);
        let behind = send(&q, "a", "y", T + 1);
        let kept = Unwritten {
            queue: "a".into(),
            attempts: 1,
            settle: Settle::Done,
        };
        q.hold(id, token, kept.clone());
        assert_eq!(q.unwritten(), vec![(id, token, kept)]);
        assert!(q.bury_spent("a", 1, T + 30).unwrap().is_empty());
        assert_eq!(
            ids(&q.lease(Some("a"), T + 30, 1, 30, 2).unwrap()),
            [behind]
        );
        q.forget(id, token);
        assert!(q.unwritten().is_empty());
        assert_eq!(ids(&q.lease(Some("a"), T + 30, 1, 30, 2).unwrap()), [id]);
    }

    #[test]
    fn a_delivery_the_host_turned_away_is_given_back_without_its_attempt() {
        let q = Queues::in_memory().unwrap();
        let id = send(&q, "a", "x", T);
        // A busy host, ten times over, with one attempt allowed: never a
        // dead letter, since the consumer was never asked.
        for n in 0..10 {
            let at = T + n * 10;
            let taken = q.lease(Some("a"), at, 1, 30, 100).unwrap();
            assert_eq!(taken[0].attempts, 1);
            assert!(q.give_back(id, lease_of(&taken, id), at, 5).unwrap());
            assert_eq!(
                row(&q, id),
                Row::Waiting {
                    due: at + 5,
                    lease_id: None
                }
            );
            assert!(q.bury_spent("a", 1, at + 5).unwrap().is_empty());
        }
        // A stale delivery cannot give back what another holds.
        let (id, stale, current) = handed_on(&q);
        assert!(!q.give_back(id, stale, T + 31, 0).unwrap());
        assert_eq!(
            row(&q, id),
            Row::Waiting {
                due: T + 60,
                lease_id: Some(current)
            }
        );
    }

    #[test]
    fn a_lease_of_one_queue_takes_nothing_of_another() {
        let q = Queues::in_memory().unwrap();
        let a = send(&q, "a", "a", T);
        let b = send(&q, "b", "b", T + 1);
        assert_eq!(ids(&q.lease(Some("b"), T + 10, 10, 30, 100).unwrap()), [b]);
        assert_eq!(
            row(&q, a),
            Row::Waiting {
                due: T,
                lease_id: None
            }
        );
        assert_eq!(ids(&q.lease(Some("a"), T + 10, 10, 30, 100).unwrap()), [a]);
    }

    #[test]
    fn a_store_made_before_lease_ids_is_given_the_column_and_still_settles() {
        let dir = std::env::temp_dir().join(format!("minicloud-queue-old-{}", std::process::id()));
        let path = dir.join("queue.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            // The schema as it was, and a message that was leased under it.
            let old = Connection::open(&path).unwrap();
            old.execute_batch(
                "CREATE TABLE message (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    queue TEXT NOT NULL,
                    body TEXT NOT NULL,
                    attempts INTEGER NOT NULL,
                    due INTEGER NOT NULL,
                    sent INTEGER NOT NULL
                );
                INSERT INTO message (queue, body, attempts, due, sent)
                    VALUES ('a', 'kept', 1, 1000030, 1000000);",
            )
            .unwrap();
        }
        let q = Queues::open(&path).unwrap();
        let taken = q.lease(None, T + 30, 10, 30, 100).unwrap();
        assert_eq!(ids(&taken), [1]);
        assert_eq!(taken[0].attempts, 2);
        assert!(q.done(1, lease_of(&taken, 1)).unwrap());
        assert_eq!(row(&q, 1), Row::Gone);
        drop(q);
        // Opening it again finds the column there and leaves it be.
        let q = Queues::open(&path).unwrap();
        let id = send(&q, "a", "new", T);
        assert!(id > 1);
        let taken = q.lease(None, T, 10, 30, 100).unwrap();
        assert!(q.bury(id, lease_of(&taken, id), T, "why").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every order two deliveries of one message can come in, and every
    /// answer each can give.
    ///
    /// A leases at `T`; its lease runs out at `T + 30`; then B leases what
    /// is due. A's settle may come at any point after its lease, before or
    /// after B's, and B's after its own. What must hold is that the message
    /// is in one place at most, that once B has it A changes nothing, and
    /// that where it ends is what the last settle with the lease said.
    #[test]
    fn two_deliveries_in_any_order_leave_the_message_to_whoever_holds_it() {
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Step {
            ALease,
            Expire,
            BLease,
            ASettle,
            BSettle,
        }
        #[derive(Clone, Copy, Debug)]
        enum Settle {
            Done,
            Retry,
            Bury,
        }
        use Step::*;
        fn orders(left: Vec<Step>, so_far: Vec<Step>, out: &mut Vec<Vec<Step>>) {
            if left.is_empty() {
                out.push(so_far);
                return;
            }
            for (i, step) in left.iter().enumerate() {
                let mut left = left.clone();
                left.remove(i);
                let mut so_far = so_far.clone();
                so_far.push(*step);
                orders(left, so_far, out);
            }
        }
        let mut all = Vec::new();
        orders(
            vec![ALease, Expire, BLease, ASettle, BSettle],
            vec![],
            &mut all,
        );
        let at = |order: &[Step], step| order.iter().position(|s| *s == step).unwrap();
        let possible: Vec<_> = all
            .into_iter()
            .filter(|o| {
                at(o, ALease) < at(o, Expire)
                    && at(o, Expire) < at(o, BLease)
                    && at(o, ALease) < at(o, ASettle)
                    && at(o, BLease) < at(o, BSettle)
            })
            .collect();
        // A's settle before the lease runs out, before B's lease, before
        // B's settle, or after it.
        assert_eq!(possible.len(), 4);

        let settle = |q: &Queues, id, lease, now, how: Settle| match how {
            Settle::Done => q.done(id, lease).unwrap(),
            // No delay, so a retried message is due again at once and B
            // can take it.
            Settle::Retry => q.retry(id, lease, now, 0).unwrap(),
            Settle::Bury => q.bury(id, lease, now, "no").unwrap(),
        };
        let answers = [Settle::Done, Settle::Retry, Settle::Bury];
        for order in &possible {
            for a_how in answers {
                for b_how in answers {
                    let case = format!("{order:?}, A {a_how:?}, B {b_how:?}");
                    let q = Queues::in_memory().unwrap();
                    let id = send(&q, "a", "x", T);
                    let mut now = T;
                    let mut a = None;
                    let mut b = None;
                    // The last settle that took effect.
                    let mut last = None;
                    for step in order {
                        let before = row(&q, id);
                        match step {
                            ALease => {
                                a = Some(lease_of(&q.lease(None, now, 1, 30, 100).unwrap(), id))
                            }
                            Expire => now = T + 30,
                            BLease => {
                                let taken = q.lease(None, now, 1, 30, 100).unwrap();
                                b = taken.first().map(|m| lease_of(&taken, m.id));
                            }
                            ASettle => {
                                let took = settle(&q, id, a.unwrap(), now, a_how);
                                // A holds the lease until B takes it.
                                assert_eq!(took, b.is_none(), "{case}");
                                if took {
                                    last = Some(a_how);
                                } else {
                                    assert_eq!(row(&q, id), before, "A changed it: {case}");
                                }
                            }
                            BSettle => {
                                // B got nothing if A had settled it for good.
                                if let Some(lease) = b {
                                    assert!(settle(&q, id, lease, now, b_how), "{case}");
                                    last = Some(b_how);
                                }
                            }
                        }
                        // `row` panics on a message in both tables.
                        row(&q, id);
                    }
                    let end = row(&q, id);
                    match last.unwrap() {
                        Settle::Done => assert_eq!(end, Row::Gone, "{case}"),
                        Settle::Bury => assert_eq!(end, Row::Dead, "{case}"),
                        Settle::Retry => {
                            assert!(matches!(end, Row::Waiting { .. }), "{case}")
                        }
                    }
                }
            }
        }
    }

    // ---------------------------------------------- the admin's window

    /// An empty directory of this test's own.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("minicloud-queue-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bodies(found: &[Peeked]) -> Vec<&str> {
        found.iter().map(|m| m.body.as_str()).collect()
    }

    fn states(found: &[Peeked]) -> Vec<&str> {
        found.iter().map(|m| m.state).collect()
    }

    #[test]
    fn the_window_says_what_is_due_apart_from_what_is_held() {
        let dir = scratch("window");
        let path = dir.join("queue.sqlite3");
        {
            let q = Queues::open(&path).unwrap();
            send(&q, "mail", "first", T);
            send(&q, "mail", "second", T + 1);
            send(&q, "other", "third", T + 2);
            // Leased: `due` is pushed out to the end of the lease, which is
            // the only mark a delivery in flight leaves.
            q.lease(None, T + 3, 1, 30, 100).unwrap();
        }
        let found = peek_waiting(&path, T + 3, 10).unwrap();
        assert_eq!(bodies(&found), ["second", "third", "first"]);
        assert_eq!(states(&found), ["due", "due", "held"]);
        assert_eq!((found[2].attempts, found[2].due_unix), (1, T + 33));
        // The counts are whole and by queue, and say the same of each.
        assert_eq!(
            peek_depths(&path, T + 3).unwrap(),
            vec![
                Depth {
                    name: "mail".into(),
                    due: 1,
                    held: 1,
                    dead: 0
                },
                Depth {
                    name: "other".into(),
                    due: 1,
                    held: 0,
                    dead: 0
                },
            ]
        );
        // The lease runs out and the message is due again, with nothing
        // changed by the looking.
        let later = peek_waiting(&path, T + 33, 10).unwrap();
        assert_eq!(states(&later), ["due", "due", "due"]);
        assert_eq!(bodies(&later), ["first", "second", "third"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The window is a read, and a read that took a message would be the one
    /// thing it must not be: the delivery that follows must be the delivery
    /// that would have happened.
    #[test]
    fn reading_the_window_takes_nothing() {
        let dir = scratch("takes-nothing");
        let path = dir.join("queue.sqlite3");
        let q = Queues::open(&path).unwrap();
        let id = send(&q, "mail", "untouched", T);
        for _ in 0..3 {
            assert_eq!(peek_waiting(&path, T, 10).unwrap().len(), 1);
            assert_eq!(peek_depths(&path, T).unwrap()[0].due, 1);
        }
        let taken = q.lease(None, T, 10, 30, 100).unwrap();
        assert_eq!(ids(&taken), [id]);
        // The first try, as it would have been had nobody looked.
        assert_eq!(taken[0].attempts, 1);
        assert_eq!(taken[0].body, "untouched");
        assert!(q.done(id, lease_of(&taken, id)).unwrap());
        assert!(peek_waiting(&path, T, 10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed delivery waiting for its retry is `held` like a lease: one
    /// `due` column carries both, and the window does not claim to know
    /// which of the two a message is in.
    #[test]
    fn a_message_waiting_for_its_retry_is_held_and_says_when_it_is_due() {
        let dir = scratch("retry");
        let path = dir.join("queue.sqlite3");
        let q = Queues::open(&path).unwrap();
        let id = send(&q, "mail", "again", T);
        let taken = q.lease(None, T, 1, 30, 100).unwrap();
        q.retry(id, lease_of(&taken, id), T + 5, 100).unwrap();
        let found = peek_waiting(&path, T + 5, 10).unwrap();
        assert_eq!(states(&found), ["held"]);
        assert_eq!((found[0].attempts, found[0].due_unix), (1, T + 105));
        assert_eq!(found[0].sent_unix, T);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_letter_comes_with_why_and_when_it_was_buried() {
        let dir = scratch("dead");
        let path = dir.join("queue.sqlite3");
        let q = Queues::open(&path).unwrap();
        let first = send(&q, "mail", "one", T);
        let second = send(&q, "mail", "two", T);
        let taken = q.lease(None, T, 2, 30, 100).unwrap();
        q.bury(first, lease_of(&taken, first), T + 10, "500 no")
            .unwrap();
        q.bury(second, lease_of(&taken, second), T + 20, "no answer")
            .unwrap();
        let dead = peek_dead(&path, 10).unwrap();
        // Newest buried first, which is the one somebody came to read.
        assert_eq!(bodies(&dead), ["two", "one"]);
        assert_eq!(states(&dead), ["dead", "dead"]);
        assert_eq!(dead[0].why, "no answer");
        assert_eq!(dead[0].buried_unix, T + 20);
        assert_eq!(dead[0].attempts, 1);
        assert_eq!(dead[1].why, "500 no");
        // They are not waiting, and the counts keep the two apart.
        assert!(peek_waiting(&path, T + 100, 10).unwrap().is_empty());
        assert_eq!(
            peek_depths(&path, T + 100).unwrap(),
            vec![Depth {
                name: "mail".into(),
                due: 0,
                held: 0,
                dead: 2
            }]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One message by id, from either table: a page reaches it from the
    /// waiting listing or from the dead letters, and the id is enough.
    #[test]
    fn one_message_is_read_by_id_from_either_table() {
        let dir = scratch("one");
        let path = dir.join("queue.sqlite3");
        let q = Queues::open(&path).unwrap();
        let waiting = send(&q, "mail", "still here", T);
        let buried = send(&q, "mail", "gave up", T);
        let taken = q.lease(None, T, 2, 30, 100).unwrap();
        q.bury(buried, lease_of(&taken, buried), T + 5, "500 no")
            .unwrap();

        let found = peek_one(&path, waiting, T + 60).unwrap().unwrap();
        assert_eq!((found.body.as_str(), found.state), ("still here", "due"));
        assert_eq!((found.attempts, found.why.as_str()), (1, ""));
        // Still leased at this moment, so it is held and not due.
        assert_eq!(peek_one(&path, waiting, T).unwrap().unwrap().state, "held");

        let dead = peek_one(&path, buried, T + 60).unwrap().unwrap();
        assert_eq!((dead.body.as_str(), dead.state), ("gave up", "dead"));
        assert_eq!((dead.why.as_str(), dead.buried_unix), ("500 no", T + 5));
        // An id neither table has is no message, not an error.
        assert!(peek_one(&path, 9999, T).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One message's read carries what a listing cuts: a queue's body is a
    /// payload, and more than the listing's length is the ordinary case.
    #[test]
    fn one_messages_read_carries_more_than_a_listing_does() {
        let dir = scratch("whole");
        let path = dir.join("queue.sqlite3");
        let q = Queues::open(&path).unwrap();
        let long = "p".repeat(PEEK_BODY_CHARS + 100);
        let id = q.send("mail", &long, T, T, 1 << 20, DEPTH).unwrap();
        let listed = &peek_waiting(&path, T, 10).unwrap()[0];
        assert!(listed.cut);
        assert_eq!(listed.body.len(), PEEK_BODY_CHARS);

        let whole = peek_one(&path, id, T).unwrap().unwrap();
        assert!(!whole.cut);
        assert_eq!(whole.body, long);
        // And its own length is a limit as well, which `cut` says.
        let longer = "q".repeat(READ_BODY_CHARS + 10);
        let id = q.send("mail", &longer, T, T, 1 << 20, DEPTH).unwrap();
        let cut = peek_one(&path, id, T).unwrap().unwrap();
        assert!(cut.cut);
        assert_eq!(cut.body.len(), READ_BODY_CHARS);
        assert_eq!(cut.bytes, READ_BODY_CHARS as i64 + 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_body_is_cut_and_says_how_long_the_whole_is() {
        let dir = scratch("cut");
        let path = dir.join("queue.sqlite3");
        let q = Queues::open(&path).unwrap();
        let long = "é".repeat(PEEK_BODY_CHARS + 10);
        q.send("mail", &long, T, T, 1 << 20, DEPTH).unwrap();
        send(&q, "mail", "short", T + 1);
        let found = peek_waiting(&path, T + 1, 10).unwrap();
        assert!(found[0].cut);
        assert_eq!(found[0].body.chars().count(), PEEK_BODY_CHARS);
        assert_eq!(found[0].bytes, 2 * (PEEK_BODY_CHARS as i64 + 10));
        assert!(!found[1].cut);
        assert_eq!((found[1].body.as_str(), found[1].bytes), ("short", 5));
        // A reason is cut the same way; a 500's first line can be anything.
        let id = found[1].id;
        q.bury_unleased(id, T + 2, &"x".repeat(PEEK_BODY_CHARS + 5))
            .unwrap();
        let dead = peek_dead(&path, 10).unwrap();
        assert_eq!(dead[0].why.chars().count(), PEEK_BODY_CHARS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The listing stops at the window's own limit; the counts beside it do
    /// not, so a page can say how many more there are.
    #[test]
    fn the_listing_stops_at_the_windows_limit_and_the_counts_do_not() {
        let dir = scratch("limit");
        let path = dir.join("queue.sqlite3");
        let q = Queues::open(&path).unwrap();
        let more = PEEK_MESSAGES + 5;
        for n in 0..more {
            q.send("mail", &format!("{n}"), T, T, 1024, more * 2)
                .unwrap();
        }
        assert_eq!(
            peek_waiting(&path, T, PEEK_MESSAGES + 100).unwrap().len() as i64,
            PEEK_MESSAGES
        );
        assert_eq!(peek_waiting(&path, T, 3).unwrap().len(), 3);
        // A limit of 0 answers nothing rather than everything.
        assert!(peek_waiting(&path, T, 0).unwrap().is_empty());
        assert_eq!(peek_depths(&path, T).unwrap()[0].due, more);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reader goes through the store the app has open where one is open,
    /// so a message sent a moment ago is in the window: a second connection
    /// on a WAL database would read the snapshot it started with.
    #[test]
    fn the_window_reads_through_the_store_the_app_has_open() {
        let dir = scratch("shared");
        let path = dir.join("queue.sqlite3");
        let store = shared(&path).unwrap();
        store
            .lock()
            .unwrap()
            .send("mail", "just now", T, T, 1024, DEPTH)
            .unwrap();
        assert_eq!(bodies(&peek_waiting(&path, T, 10).unwrap()), ["just now"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_store_that_is_not_there_is_no_queue_and_is_not_created() {
        let dir = scratch("absent");
        let path = dir.join("app").join("queue.sqlite3");
        assert!(peek_depths(&path, T).unwrap().is_empty());
        assert!(peek_waiting(&path, T, 10).unwrap().is_empty());
        assert!(peek_dead(&path, 10).unwrap().is_empty());
        // An app not granted `queue` has no file, and looking must not make
        // one — nor the directory it would live in.
        assert!(!path.exists());
        assert!(!dir.join("app").exists());
        // A file that is not a queue store of ours has nothing waiting.
        let bare = dir.join("bare.sqlite3");
        Connection::open(&bare)
            .unwrap()
            .execute_batch("CREATE TABLE other (x);")
            .unwrap();
        assert!(peek_depths(&bare, T).unwrap().is_empty());
        assert!(peek_waiting(&bare, T, 10).unwrap().is_empty());
        assert!(peek_dead(&bare, 10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// The store driven by random operations against a model of its contract
// (`docs/queue-properties.md`), and the cases that model has found.
#[cfg(test)]
mod model_tests;
