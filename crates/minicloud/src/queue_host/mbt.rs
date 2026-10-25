//! The delivery loop against the model's own traces.
//!
//! `spec/queue` (`npm run traces`) has Quint draw traces from the model of
//! the code (`IMPL2` with the fixes, `mbtStep`) and write them in the
//! Informal Trace Format, with the action each step took and its nondet
//! picks (`mbt::actionTaken`, `mbt::nondetPicks`). They are committed under
//! `tests/queue_traces/`, so this needs neither Node nor Quint. Each trace
//! is driven step by step through [`Harness`], and after every step what
//! the implementation shows ([`real`]) is compared with the model's state
//! ([`model`]): the store's rows, the settles held in memory, the in-flight
//! and settle counters, the deliveries and how far each has got, and the
//! ghost counts the contract is stated in. Neither side is computed with the
//! other's rules: [`real`] reads only the implementation, [`model`] only
//! the trace, and the comparison is of states.
//!
//! The two are put side by side as follows.
//!
//! - **Names.** Model ids and tokens are mapped to the store's ids and
//!   lease ids as the replay meets them (`Names`): a send binds an id, a
//!   lease binds (id, token) to the lease id the store gave. A lease id
//!   given twice, or an id the model has never sent, fails the replay: it
//!   would be S3 or S4 broken. A model token is a name reused once nothing
//!   holds it, so a lease rebinds it. A delivery is driven by its lease:
//!   after a stall a message has two.
//! - **Lives.** The model's ghost counts start again at a revive; the
//!   harness's facts are per message, so they are counted from the last
//!   revive on (`Life`).
//! - **Refusals.** A send or revive the queue's depth refuses is a step of
//!   the trace that changes nothing; the code is asked as well and must
//!   refuse.
//! - **Time.** The model counts time in delays (`clk`, a message's
//!   `dueAt`), and every lease and retry here is one delay, [`DELAY_MS`].
//!   "Message m becomes due" moves the clock to m's due time in the store;
//!   that nothing else became due with it that the model says is not, and
//!   the reverse, is part of the comparison (each message's due or not, and
//!   when).
//! - **The tick** is one call into the code, which flushes, buries and
//!   leases at one reading of the clock and admits as it goes; the model
//!   takes it in several steps with nothing in between (`mbtStep`). The
//!   steps of one tick are replayed as that one call, with the model's
//!   picks (which held settle's write fails, which admission is refused)
//!   set up before it, and the states compared when it ends; within it, the
//!   writes the model made, the admissions in the model's order and the
//!   leases it issued are checked against what the code did.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use super::harness::{policy, Harness, Op, Stage};
use super::Store;
use crate::config::QueuePolicy;
use crate::queue::{Revived, Settle, OUTLIVED};

/// [`Harness::new`]'s clock, the model's time 0.
const BASE_MS: i64 = 1_000_000_000;
/// One unit of the model's time: every queue's `lease` and `retry`. With
/// `max_attempts` 2 a retry is only ever the first one, which is not
/// doubled.
const DELAY_MS: i64 = 30_000;

// ------------------------------------------------------------- ITF

fn int(v: &Value) -> i64 {
    match v.get("#bigint").and_then(Value::as_str) {
        Some(n) => n.parse().expect("an ITF #bigint"),
        None => v.as_i64().expect("an ITF integer"),
    }
}

fn elements(v: &Value) -> &[Value] {
    v.get("#set")
        .and_then(Value::as_array)
        .expect("an ITF #set")
}

fn entries(v: &Value) -> impl Iterator<Item = (&Value, &Value)> {
    v.get("#map")
        .and_then(Value::as_array)
        .expect("an ITF #map")
        .iter()
        .map(|pair| (&pair[0], &pair[1]))
}

fn text(v: &Value) -> &str {
    v.as_str().expect("an ITF string")
}

fn flag(v: &Value) -> bool {
    v.as_bool().expect("an ITF bool")
}

/// A state's variable, whatever module path the trace gave it.
fn var<'a>(state: &'a Value, name: &str) -> &'a Value {
    let suffix = format!("::{name}");
    state
        .as_object()
        .expect("an ITF state")
        .iter()
        .find(|(key, _)| key.ends_with(&suffix))
        .map(|(_, value)| value)
        .unwrap_or_else(|| panic!("no variable `{name}` in the trace"))
}

fn action(state: &Value) -> &str {
    text(&state["mbt::actionTaken"])
}

/// A nondet pick of the step that led to `state`, if it made one.
fn pick<'a>(state: &'a Value, name: &str) -> Option<&'a Value> {
    let picked = &state["mbt::nondetPicks"][name];
    (picked["tag"] == "Some").then(|| &picked["value"])
}

fn picked<'a>(state: &'a Value, name: &str) -> &'a Value {
    pick(state, name).unwrap_or_else(|| panic!("`{}` picked no `{name}`", action(state)))
}

/// A delivery of the model (`Del`), as much of it as the replay needs.
struct Del {
    id: i64,
    q: i64,
    tok: i64,
    att: i64,
    m: i64,
    stage: String,
    op: String,
    started: bool,
    at: i64,
}

fn del(v: &Value) -> Del {
    Del {
        id: int(&v["id"]),
        q: int(&v["q"]),
        tok: int(&v["tok"]),
        att: int(&v["att"]),
        m: int(&v["m"]),
        stage: text(&v["stage"]).to_string(),
        op: text(&v["op"]).to_string(),
        started: flag(&v["started"]),
        at: int(&v["at"]),
    }
}

fn dels(state: &Value) -> Vec<Del> {
    elements(var(state, "dels")).iter().map(del).collect()
}

const TICK: [&str; 8] = [
    "tickBeginSettle",
    "tickSettleRetry",
    "tickSettleEnd",
    "tickUndecl",
    "tickBury",
    "tickLease",
    "admitOk",
    "admitRefused",
];

// ---------------------------------------------------- the two states

/// A state as the comparison sees it: one line per thing, by name. What
/// the model and the implementation are compared on, and nothing else.
type Abs = BTreeMap<String, String>;

fn queue_name(q: i64) -> String {
    char::from(b'a' + u8::try_from(q).expect("a small queue index")).to_string()
}

fn token(t: i64) -> String {
    if t == 0 {
        "none".to_string()
    } else {
        format!("t{t}")
    }
}

fn put(abs: &mut Abs, key: String, value: String) {
    let mut key = key;
    while abs.contains_key(&key) {
        key.push('\'');
    }
    abs.insert(key, value);
}

fn delivery_line(
    q: &str,
    attempts: i64,
    m: Option<i64>,
    stage: &str,
    started: bool,
    held: Option<(&str, Option<i64>)>,
) -> String {
    match held {
        Some((op, at)) => match at {
            Some(at) => format!("queue {q}, attempts {attempts}, holds {op} as of {at} ms"),
            None => format!("queue {q}, attempts {attempts}, holds {op}"),
        },
        None => format!(
            "queue {q}, attempts {attempts}, M {}, {stage}{}",
            m.unwrap_or_default(),
            if started { ", started" } else { "" }
        ),
    }
}

/// The model's state, from the trace alone.
fn model(state: &Value) -> Abs {
    let mut abs = Abs::new();
    let clk = int(var(state, "clk"));
    abs.insert("clock".into(), format!("{} ms", clk * DELAY_MS));
    for (i, m) in entries(var(state, "msg")) {
        if !flag(&m["on"]) {
            continue;
        }
        let due = if flag(&m["due"]) {
            "due".to_string()
        } else {
            format!("due at {} ms", int(&m["dueAt"]) * DELAY_MS)
        };
        abs.insert(
            format!("message {}", int(i)),
            format!(
                "queue {}, attempts {}, {due}, lease {}",
                queue_name(int(&m["q"])),
                int(&m["att"]),
                token(int(&m["tok"]))
            ),
        );
    }
    for (i, d) in entries(var(state, "dead")) {
        if !flag(&d["on"]) {
            continue;
        }
        abs.insert(
            format!("dead {}", int(i)),
            format!(
                "queue {}, attempts {}, {}",
                queue_name(int(&d["q"])),
                int(&d["att"]),
                text(&d["why"])
            ),
        );
    }
    for (q, n) in entries(var(state, "inflight")) {
        abs.insert(
            format!("in flight {}", queue_name(int(q))),
            int(n).to_string(),
        );
    }
    for d in dels(state) {
        let q = queue_name(d.q);
        let line = if d.stage == "pending" {
            let at = (d.op != "done").then_some(d.at * DELAY_MS);
            delivery_line(&q, d.att, None, "", false, Some((&d.op, at)))
        } else {
            delivery_line(&q, d.att, Some(d.m), &d.stage, d.started, None)
        };
        put(
            &mut abs,
            format!("delivery {} {}", d.id, token(d.tok)),
            line,
        );
    }
    let counts = var(state, "counts");
    abs.insert(
        "counters".into(),
        format!(
            "delivered {}, retried {}, buried {}",
            int(&counts["delivered"]),
            int(&counts["retried"]),
            int(&counts["buried"])
        ),
    );
    let sent = int(var(state, "nextId"));
    for (i, g) in entries(var(state, "gh")) {
        let i = int(i);
        if i < sent {
            abs.insert(
                format!("ghost {i}"),
                format!(
                    "invocations {}, leases less give-backs {}, unstarted lost to a crash {}",
                    int(&g["invoc"]),
                    int(&g["net"]),
                    int(&g["gap"])
                ),
            );
        }
    }
    abs
}

/// The model's names for the store's ids and lease ids.
#[derive(Default)]
struct Names {
    ids: BTreeMap<i64, i64>,
    rids: BTreeMap<i64, i64>,
    toks: BTreeMap<(i64, i64), i64>,
    leases: BTreeMap<i64, (i64, i64)>,
}

impl Names {
    fn real(&self, id: i64) -> Result<i64, String> {
        self.ids
            .get(&id)
            .copied()
            .ok_or_else(|| format!("model message {id} was never sent"))
    }

    fn id(&self, real: i64) -> String {
        match self.rids.get(&real) {
            Some(id) => id.to_string(),
            None => format!("? (store id {real}, never sent by the model)"),
        }
    }

    fn token(&self, real: i64, lease: Option<i64>) -> String {
        let Some(lease) = lease else {
            return token(0);
        };
        match (self.leases.get(&lease), self.rids.get(&real)) {
            (Some(&(id, tok)), Some(&of)) if id == of => token(tok),
            (Some(&(id, tok)), _) => format!("{} of message {id}", token(tok)),
            (None, _) => format!("? (lease id {lease}, never issued by a lease the model made)"),
        }
    }

    fn bind_id(&mut self, id: i64, real: i64) -> Result<(), String> {
        if let Some(had) = self.ids.get(&id) {
            return Err(format!(
                "model message {id} sent again (it was store id {had})"
            ));
        }
        if let Some(had) = self.rids.get(&real) {
            return Err(format!(
                "S3: the store gave id {real} again (model message {had} had it)"
            ));
        }
        self.ids.insert(id, real);
        self.rids.insert(real, id);
        Ok(())
    }

    fn bind_token(&mut self, id: i64, tok: i64, lease: i64) -> Result<(), String> {
        if let Some(&(had, t)) = self.leases.get(&lease) {
            return Err(format!(
                "S4: lease id {lease} issued again (message {had}'s {} had it)",
                token(t)
            ));
        }
        if let Some(old) = self.toks.insert((id, tok), lease) {
            self.leases.remove(&old);
        }
        self.leases.insert(lease, (id, tok));
        Ok(())
    }
}

fn why(why: &str) -> String {
    if why == OUTLIVED {
        "outlived".into()
    } else if why.starts_with("max_attempts lowered to ") {
        "lowered".into()
    } else if why.ends_with("is no longer declared") {
        "undeclared".into()
    } else if why == "no answer" || why.starts_with("500 ") {
        "fail".into()
    } else {
        format!("? ({why})")
    }
}

/// Where a message's life began in the harness's ghost facts: how many
/// leases and written settles it had had when it was last revived.
#[derive(Clone, Copy, Default)]
struct Life {
    leases: usize,
    written: usize,
}

/// The implementation's state, from what it shows: the store's file, the
/// settles it holds, its counters, the deliveries in flight and the
/// harness's ghost facts (per life: since the message was last revived).
fn real(r: &Replay) -> Abs {
    let (h, rows, names) = (&r.h, &r.rows, &r.names);
    let mut abs = Abs::new();
    let now = h.now_ms();
    let index = |name: &str| name.to_string();
    abs.insert("clock".into(), format!("{} ms", now - BASE_MS));
    let mut select = rows
        .prepare("SELECT id, queue, attempts, due, lease_id FROM message")
        .unwrap();
    let found = select
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .unwrap();
    for row in found {
        let (id, queue, attempts, due, lease) = row.unwrap();
        let due = if due <= now {
            "due".to_string()
        } else {
            format!("due at {} ms", due - BASE_MS)
        };
        put(
            &mut abs,
            format!("message {}", names.id(id)),
            format!(
                "queue {}, attempts {attempts}, {due}, lease {}",
                index(&queue),
                names.token(id, lease)
            ),
        );
    }
    let mut select = rows
        .prepare("SELECT id, queue, attempts, why FROM dead")
        .unwrap();
    let found = select
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap();
    for row in found {
        let (id, queue, attempts, reason) = row.unwrap();
        put(
            &mut abs,
            format!("dead {}", names.id(id)),
            format!(
                "queue {}, attempts {attempts}, {}",
                index(&queue),
                why(&reason)
            ),
        );
    }
    let counters = h.counters();
    for queue in &r.all_queues {
        abs.insert(
            format!("in flight {queue}"),
            counters.queue_in_flight_of(queue).to_string(),
        );
    }
    for pending in &h.pending {
        let (stage, started) = match &pending.stage {
            Stage::Leased => ("leased", false),
            Stage::Admitted {
                reply: Some(_),
                started,
                ..
            } => (if *started { "running" } else { "queued" }, *started),
            Stage::Admitted {
                reply: None,
                started,
                ..
            } => ("answered", *started),
        };
        let message = &pending.message;
        put(
            &mut abs,
            format!(
                "delivery {} {}",
                names.id(message.id),
                names.token(message.id, message.lease_id)
            ),
            delivery_line(
                &index(&message.queue),
                message.attempts,
                Some(i64::from(pending.policy.max_attempts)),
                stage,
                started,
                None,
            ),
        );
    }
    for (id, lease, unwritten) in h.store().unwritten() {
        let (op, at) = match &unwritten.settle {
            Settle::Done => ("done", None),
            Settle::Retry { at, .. } => ("retry", Some(*at)),
            Settle::Bury { at, .. } => ("bury", Some(*at)),
            Settle::GiveBack { at, .. } => ("giveback", Some(*at)),
        };
        put(
            &mut abs,
            format!("delivery {} {}", names.id(id), names.token(id, Some(lease))),
            delivery_line(
                &index(&unwritten.queue),
                unwritten.attempts,
                None,
                "",
                false,
                Some((op, at.map(|at| at - BASE_MS))),
            ),
        );
    }
    abs.insert(
        "counters".into(),
        format!(
            "delivered {}, retried {}, buried {}",
            counters.queue_delivered.load(Ordering::Relaxed),
            counters.queue_retried.load(Ordering::Relaxed),
            counters.queue_buried.load(Ordering::Relaxed)
        ),
    );
    for (&id, &rid) in &names.ids {
        let facts = h.facts(rid);
        let life = r.lives.get(&rid).copied().unwrap_or_default();
        let leases = &facts.leases[life.leases..];
        let given_back = facts.written[life.written..]
            .iter()
            .filter(|&&(op, matched)| op == Op::GiveBack && matched)
            .count();
        let of_life = |of: &[i64]| of.iter().filter(|lease| leases.contains(lease)).count();
        abs.insert(
            format!("ghost {id}"),
            format!(
                "invocations {}, leases less give-backs {}, unstarted lost to a crash {}",
                of_life(&facts.started),
                leases.len() as i64 - given_back as i64,
                of_life(&facts.lost_unstarted)
            ),
        );
    }
    abs
}

fn diff(real: &Abs, model: &Abs) -> String {
    let keys: BTreeSet<&String> = real.keys().chain(model.keys()).collect();
    let mut out = String::new();
    for key in keys {
        let (r, m) = (real.get(key), model.get(key));
        if r != m {
            out.push_str(&format!(
                "    {key}:\n      implementation: {}\n      model:          {}\n",
                r.map_or("(none)", String::as_str),
                m.map_or("(none)", String::as_str)
            ));
        }
    }
    out
}

// ------------------------------------------------------- the replay

/// What the admissions of one tick are to answer, in the model's order,
/// and what they were asked.
#[derive(Default)]
struct Admissions {
    plan: VecDeque<(i64, bool)>,
    asked: Vec<i64>,
}

struct Replay {
    h: Harness,
    rows: Connection,
    names: Names,
    /// Every queue of the model, declared or not.
    all_queues: Vec<String>,
    /// Per store id, where its current life began.
    lives: BTreeMap<i64, Life>,
    admissions: Rc<RefCell<Admissions>>,
}

/// The queues the model's version declares, with every delay one unit.
fn queues_of(state: &Value) -> Vec<QueuePolicy> {
    let pol = var(state, "pol");
    let of = |field: &str, q: &Value| {
        entries(&pol[field])
            .find(|(k, _)| int(k) == int(q))
            .map(|(_, v)| v.clone())
            .expect("a policy for every queue")
    };
    entries(&pol["decl"])
        .filter(|(_, declared)| flag(declared))
        .map(|(q, _)| {
            policy(&queue_name(int(q)), |p| {
                p.max_attempts = u32::try_from(int(&of("m", q))).unwrap();
                p.max_depth = int(&of("d", q));
                p.batch = u32::try_from(int(&of("batch", q))).unwrap();
                p.lease = Duration::from_millis(DELAY_MS as u64);
                p.retry = Duration::from_millis(DELAY_MS as u64);
            })
        })
        .collect()
}

impl Replay {
    fn new(name: &str, init: &Value) -> Replay {
        let mut h = Harness::new(name, queues_of(init));
        assert_eq!(h.now_ms(), BASE_MS);
        let admissions = Rc::new(RefCell::new(Admissions::default()));
        let plan = Rc::clone(&admissions);
        h.admits = Box::new(move |message| {
            let mut plan = plan.borrow_mut();
            plan.asked.push(message.id);
            match plan.plan.front() {
                Some(&(id, admit)) if id == message.id => {
                    plan.plan.pop_front();
                    admit
                }
                _ => true,
            }
        });
        let rows = Connection::open_with_flags(
            h.path(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let all_queues = entries(&var(init, "pol")["decl"])
            .map(|(q, _)| queue_name(int(q)))
            .collect();
        Replay {
            h,
            rows,
            names: Names::default(),
            all_queues,
            lives: BTreeMap::new(),
            admissions,
        }
    }

    fn real_of(&self, d: &Value) -> Result<i64, String> {
        self.names.real(int(&d["id"]))
    }

    /// The store id and lease id of a delivery the model picked.
    fn delivery_of(&self, d: &Value) -> Result<(i64, i64), String> {
        let (id, tok) = (int(&d["id"]), int(&d["tok"]));
        let lease = self
            .names
            .toks
            .get(&(id, tok))
            .copied()
            .ok_or_else(|| format!("message {id}'s {} was never leased", token(tok)))?;
        Ok((self.names.real(id)?, lease))
    }

    /// Whether the code tried every settle write the model made.
    fn writes_done(&self, what: &str) -> Result<(), String> {
        match self.h.planned_left() {
            0 => Ok(()),
            n => Err(format!(
                "{n} settle write(s) the model made in {what} were never tried by the loop"
            )),
        }
    }

    /// The model's next settle write for the delivery picked: written
    /// (`ok`) or failing. A delivery leased in the tick being set up has no
    /// lease id yet: its message's next write is planned instead.
    fn plan_write(&mut self, after: &Value) -> Result<(), String> {
        let ok = flag(picked(after, "ok"));
        let d = picked(after, "d");
        match self.delivery_of(d) {
            Ok((rid, lease)) if text(&d["stage"]) != "leased" => {
                self.h.plan_settles_of(rid, lease, &[ok])
            }
            _ => self.h.plan_settles(self.real_of(d)?, &[ok]),
        }
        Ok(())
    }

    /// One step that is not a tick.
    fn step(&mut self, before: &Value, after: &Value) -> Result<(), String> {
        match action(after) {
            "send" => {
                let q = int(picked(after, "q"));
                let id = int(var(before, "nextId"));
                // sendAfter is one delay
                let later = pick(after, "l").is_some_and(flag);
                let delay = if later { DELAY_MS / 1000 } else { 0 };
                let rid = self.h.send_after(&queue_name(q), &format!("m{id}"), delay);
                self.names.bind_id(id, rid)?;
            }
            "runStart" => {
                let (rid, lease) = self.delivery_of(picked(after, "d"))?;
                self.h.start_run_of(rid, lease);
            }
            "runAnswer" => {
                let (rid, lease) = self.delivery_of(picked(after, "d"))?;
                match text(picked(after, "o")) {
                    "ok" => self.h.answer_of(rid, Some(lease), 200),
                    "fail" => self.h.answer_of(rid, Some(lease), 500),
                    "none" => self.h.drop_answer_of(rid, Some(lease)),
                    other => return Err(format!("no answer `{other}` to drive")),
                }
            }
            "runTimeout" => {
                let (rid, lease) = self.delivery_of(picked(after, "d"))?;
                self.h.time_out_of(rid, Some(lease));
            }
            "settle" => {
                let (rid, lease) = self.delivery_of(picked(after, "d"))?;
                self.plan_write(after)?;
                self.h.settle_one(rid, Some(lease));
                self.writes_done("the settle")?;
            }
            "redeploy" => {
                self.h.queues = queues_of(after);
            }
            "revive" => {
                let rid = self.names.real(int(picked(after, "i")))?;
                let facts = self.h.facts(rid);
                let life = Life {
                    leases: facts.leases.len(),
                    written: facts.written.len(),
                };
                match self.h.revive(rid) {
                    Revived::Back => {
                        self.lives.insert(rid, life);
                    }
                    other => return Err(format!("the store's revive answered {other:?}")),
                }
            }
            // The model's refusals: the store must refuse as well, and
            // change nothing (the comparison after says so).
            "sendRefused" => {
                let q = int(picked(after, "q"));
                if let Ok(rid) = self.h.try_send_after(&queue_name(q), "refused", 0) {
                    return Err(format!(
                        "the store took a send to the full queue {} (store id {rid})",
                        queue_name(q)
                    ));
                }
            }
            "reviveRefused" => {
                let rid = self.names.real(int(picked(after, "i")))?;
                match self.h.revive(rid) {
                    Revived::Full { .. } => {}
                    other => {
                        return Err(format!(
                            "the store's revive answered {other:?}; the model refused it"
                        ))
                    }
                }
            }
            "clockBackTo" => {
                let t = int(picked(after, "t"));
                self.h.set_ms(BASE_MS + t * DELAY_MS);
            }
            "timePasses" => {
                let i = int(picked(after, "i"));
                let rid = self.names.real(i)?;
                let due: Option<i64> = self
                    .rows
                    .query_row("SELECT due FROM message WHERE id = ?1", [rid], |row| {
                        row.get(0)
                    })
                    .ok();
                let Some(due) = due else {
                    return Err(format!("message {i} is not on the store's queue"));
                };
                if due <= self.h.now_ms() {
                    return Err(format!(
                        "the model has message {i} not due yet; the store has it due at {} ms, \
                         and the clock is at {} ms",
                        due - BASE_MS,
                        self.h.now_ms() - BASE_MS
                    ));
                }
                self.h.set_ms(due);
            }
            "crash" => {
                self.h.crash();
            }
            other => return Err(format!("no way to drive `{other}`")),
        }
        Ok(())
    }

    /// The steps of one tick, `steps[0]` the state before it.
    fn tick(&mut self, steps: &[Value]) -> Result<(), String> {
        let mut admissions = VecDeque::new();
        let mut leased = Vec::new();
        for (before, after) in steps.iter().zip(&steps[1..]) {
            match action(after) {
                "tickSettleRetry" => self.plan_write(after)?,
                "admitOk" | "admitRefused" => {
                    let rid = self.real_of(picked(after, "d"))?;
                    let refused = action(after) == "admitRefused";
                    if refused {
                        self.plan_write(after)?;
                    }
                    admissions.push_back((rid, !refused));
                }
                "tickLease" => {
                    let had: BTreeSet<(i64, i64)> =
                        dels(before).iter().map(|d| (d.id, d.tok)).collect();
                    leased.extend(
                        dels(after)
                            .into_iter()
                            .filter(|d| d.stage == "leased" && !had.contains(&(d.id, d.tok)))
                            .map(|d| (d.id, d.tok)),
                    );
                }
                _ => {}
            }
        }
        let expected: Vec<i64> = admissions.iter().map(|&(rid, _)| rid).collect();
        {
            let mut plan = self.admissions.borrow_mut();
            plan.plan = admissions;
            plan.asked.clear();
        }
        let leases_before: BTreeMap<i64, usize> = self
            .names
            .ids
            .values()
            .map(|&rid| (rid, self.h.facts(rid).leases.len()))
            .collect();

        self.h.tick();

        self.writes_done("the tick")?;
        let asked = std::mem::take(&mut self.admissions.borrow_mut().asked);
        if asked != expected {
            return Err(format!(
                "the tick admitted store ids {asked:?}; the model, {expected:?}"
            ));
        }
        let mut new: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for (&rid, &had) in &leases_before {
            let leases = self.h.facts(rid).leases;
            if leases.len() > had {
                new.insert(rid, leases[had..].to_vec());
            }
        }
        let mut by_model = BTreeMap::new();
        for &(id, tok) in &leased {
            by_model.insert(self.names.real(id)?, (id, tok));
        }
        for (rid, leases) in &new {
            let Some(&(id, tok)) = by_model.get(rid) else {
                return Err(format!(
                    "the tick leased message {}; the model leased {:?}",
                    self.names.id(*rid),
                    leased
                ));
            };
            let [lease] = leases[..] else {
                return Err(format!(
                    "the tick leased message {id} {} times",
                    leases.len()
                ));
            };
            self.names.bind_token(id, tok, lease)?;
        }
        for (rid, (id, _)) in &by_model {
            if !new.contains_key(rid) {
                return Err(format!("the model leased message {id}; the tick did not"));
            }
        }
        Ok(())
    }

    fn compare(&self, state: &Value) -> Result<(), String> {
        let real = real(self);
        let model = model(state);
        if real == model {
            Ok(())
        } else {
            Err(format!("the states differ:\n{}", diff(&real, &model)))
        }
    }
}

fn describe(state: &Value) -> String {
    let picks: Vec<String> = state["mbt::nondetPicks"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, v)| v["tag"] == "Some")
        .map(|(name, v)| format!("{name} = {}", brief(&v["value"])))
        .collect();
    format!("{}({})", action(state), picks.join(", "))
}

/// An ITF value, short: a delivery by its id, token and stage.
fn brief(v: &Value) -> String {
    if v.get("stage").is_some() {
        let d = del(v);
        format!("delivery {} {} {}", d.id, token(d.tok), d.stage)
    } else if v.get("#bigint").is_some() {
        int(v).to_string()
    } else {
        v.to_string()
    }
}

/// Replays one trace; on a mismatch, says where and how.
fn replay(file: &str, trace: &Value) -> Result<usize, String> {
    let states = trace["states"].as_array().expect("ITF states");
    let name = Path::new(file)
        .file_name()
        .map(|n| n.to_string_lossy().replace('.', "-"))
        .unwrap_or_default();
    let mut r = Replay::new(&name, &states[0]);
    let at = |k: usize, what: String, why: String| {
        format!("{file}, step {k}: {what}\n  {}", why.replace('\n', "\n  "))
    };
    r.compare(&states[0])
        .map_err(|why| at(0, "init".into(), why))?;
    let mut k = 1;
    while k < states.len() {
        let state = &states[k];
        if TICK.contains(&action(state)) {
            // The tick's steps, to the one after which it is idle again; a
            // trace cut off in the middle of a tick ends before it.
            let Some(end) = (k..states.len()).find(|&e| text(var(&states[e], "tpc")) == "idle")
            else {
                break;
            };
            let what = format!(
                "the tick of steps {k}..={end}: {}",
                states[k..=end]
                    .iter()
                    .map(describe)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                r.tick(&states[k - 1..=end])
            }))
            .unwrap_or_else(|panic| Err(panicked(panic)));
            done.and_then(|()| r.compare(&states[end]))
                .map_err(|why| at(k, what, why))?;
            k = end + 1;
        } else {
            let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                r.step(&states[k - 1], state)
            }))
            .unwrap_or_else(|panic| Err(panicked(panic)));
            done.and_then(|()| r.compare(state))
                .map_err(|why| at(k, describe(state), why))?;
            k += 1;
        }
    }
    Ok(k)
}

fn panicked(panic: Box<dyn std::any::Any + Send>) -> String {
    let said = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    format!("the harness panicked: {said}")
}

fn corpus() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/queue_traces");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".itf.json.gz"))
        .collect();
    files.sort();
    files
}

fn load(path: &Path) -> Value {
    let mut json = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap())
        .read_to_string(&mut json)
        .unwrap();
    serde_json::from_str(&json).unwrap()
}

/// Replays the traces `take` keeps; panics with every mismatch found.
fn replay_corpus(take: impl Fn(usize) -> bool) {
    let started = Instant::now();
    let mut failures = Vec::new();
    let (mut traces, mut steps) = (0, 0);
    for (n, path) in corpus().into_iter().enumerate() {
        if !take(n) {
            continue;
        }
        let file = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(&path)
            .display()
            .to_string();
        traces += 1;
        match replay(&file, &load(&path)) {
            Ok(n) => steps += n,
            Err(why) => failures.push(why),
        }
    }
    eprintln!(
        "replayed {traces} traces, {steps} states, in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    assert!(traces > 0, "no traces under tests/queue_traces");
    assert!(
        failures.is_empty(),
        "{} of {traces} traces do not replay:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Every trace of the corpus.
#[test]
fn the_loop_follows_every_trace_of_the_model() {
    replay_corpus(|_| true);
}
