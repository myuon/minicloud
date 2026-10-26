// -*- mode: Bluespec; -*-
//
// The queue store alone (`crates/minicloud/src/queue.rs` at `main`), for
// inductive invariants checked with Apalache (README, "Inductive
// invariants"). Not the delivery loop: deliveries are abstracted to "whoever
// holds a token may call a settle at any time, with any id", so the store is
// checked against any caller.
//
// One queue. Each step is one store call, or one message's share of a call
// that touches several in one transaction (`lease`, `bury_spent`,
// `prune_dead`): a transaction's effect is a run of such steps with nothing in
// between, so every state the code commits is a state of this model.
//
// Nothing is bounded in `store`: ids, tokens, attempts, M and D are
// unbounded integers, the tables are sets of any size. The instances at
// the end bound it for TLC (`storeTlc*`) or give the inductive check the
// size of its arbitrary pre-state (`storeInd*`).
module store {
  // ---------------------------------------------------------- parameters
  // TLC needs finite choices; Apalache takes `Int`.
  const IDS: Set[int]    // ids a settle, revive or prune may name
  const TOKS: Set[int]   // values a settle's token and the observed token range over
  const MS: Set[int]     // values M and D range over (both must be >= 1)
  const MAX_ID: int      // TLC: send only while nextId <= MAX_ID (-1: no bound)
  const MAX_TOK: int     // TLC: lease only while nextTok <= MAX_TOK (-1: no bound)
  // Planted bugs, to see that the checks can fail.
  const BUG_DONE_NO_TOKEN: bool  // `done` deletes by id alone (no `AND lease_id = ?`)
  const BUG_LEASE_NO_M: bool     // `lease` without `attempts < ?4` (the code before 818e6fc, F1)
  // The size of the inductive check's arbitrary pre-state (`indInit`).
  const K: int

  // --------------------------------------------------------------- state
  // `message` and `dead`, by id. A row's columns are the maps below; a
  // map may hold a value for an id on neither table, which means nothing.
  var live: Set[int]       // ids on `message`
  var dead: Set[int]       // ids on `dead`
  var att: int -> int      // `attempts` (a dead letter keeps its own)
  var due: int -> bool     // `due <= now`
  var tok: int -> int      // `lease_id`; 0 is NULL. Only a live row has one.
  var nextId: int          // AUTOINCREMENT: above every id ever given
  var nextTok: int         // `next_lease_id()`: above every token ever given
  var m: int               // the M the caller passes to `lease` / `bury_spent`
  var d: int               // the D the caller passes to `send` / `revive`
  // Ghosts. They never guard a step.
  var leaseM: int -> int   // the M of the `lease` that issued the current token
  var obsTok: int          // an arbitrary token, fixed at the start (S4.3)
  var obsRetired: bool     // obsTok was some row's token and is no longer
  var obsId: int           // an arbitrary id, fixed at the start (S3)
  var obsUsed: bool        // obsId has been on `message` or `dead`
  var s3ok: bool           // no `send` gave an id that had been used
  var s4ok: bool           // every settle changed the store iff its token was the row's
  var s5ok: bool           // every `lease` was of a row with attempts < M

  type St = { live: Set[int], dead: Set[int], att: int -> int, due: int -> bool,
              tok: int -> int, leaseM: int -> int }
  val st: St = { live: live, dead: dead, att: att, due: due, tok: tok, leaseM: leaseM }

  // obsTok stops being current between s and t: replaced, cleared, or its
  // row left `message`. This is the contract's "replaced or removed".
  pure def retires(s: St, t: St, ot: int): bool =
    s.live.exists(i => s.tok.get(i) == ot and not(t.live.contains(i) and t.tok.get(i) == ot))

  // A step of the store: the new store, and the ghosts that follow it.
  action go(t: St): bool = all {
    live' = t.live, dead' = t.dead, att' = t.att, due' = t.due, tok' = t.tok, leaseM' = t.leaseM,
    obsTok' = obsTok, obsId' = obsId,
    obsRetired' = (obsRetired or retires(st, t, obsTok)),
    obsUsed' = (obsUsed or t.live.contains(obsId) or t.dead.contains(obsId)),
    m' = m, d' = d,
  }

  // ------------------------------------------------------------ actions
  // send: `SELECT count(*) FROM message WHERE queue = ?1` < max_depth, then
  // `INSERT INTO message (queue, body, attempts, due, sent) VALUES (?1, ?2, 0, ?3, ?4)`;
  // the id is AUTOINCREMENT's. `later`: sendAfter, not due yet.
  action send = {
    nondet later = Set(true, false).oneOf()
    val i = nextId
    all {
      MAX_ID < 0 or nextId <= MAX_ID,
      live.size() < d,
      go({ live: live.union(Set(i)), dead: dead, att: att.put(i, 0), due: due.put(i, not(later)),
           tok: tok.put(i, 0), leaseM: leaseM.put(i, 0) }),
      nextId' = nextId + 1, nextTok' = nextTok,
      s3ok' = (s3ok and not(obsUsed and obsId == i)),
      s4ok' = s4ok, s5ok' = s5ok,
    }
  }

  // lease, for one row it takes:
  // `SELECT ... WHERE due <= ?1 AND attempts < ?4` (one queue), then
  // `UPDATE message SET attempts = attempts + 1, due = ?2, lease_id = ?3 WHERE id = ?1`
  // with ?3 = next_lease_id(). `due` moves to now + lease (lease >= 1 s).
  // The code also passes over a row with a settle held in memory
  // (`answered`); leaving that out only adds behaviours.
  action lease = {
    nondet i = live.oneOf()
    all {
      MAX_TOK < 0 or nextTok <= MAX_TOK,
      due.get(i),
      BUG_LEASE_NO_M or att.get(i) < m,
      go({ ...st, att: att.set(i, att.get(i) + 1), due: due.set(i, false),
           tok: tok.set(i, nextTok), leaseM: leaseM.set(i, m) }),
      nextId' = nextId, nextTok' = nextTok + 1,
      s3ok' = s3ok, s4ok' = s4ok,
      s5ok' = (s5ok and att.get(i) < m),
    }
  }

  // The settles: `op` on row `i` with token `t`. `hit` is the SQL's
  // `WHERE id = ?1 AND lease_id = ?`.
  //   done:      DELETE FROM message WHERE id = ?1 AND lease_id = ?2
  //   retry:     UPDATE message SET due = ?2, lease_id = NULL WHERE id = ?1 AND lease_id = ?3
  //   give_back: UPDATE message SET due = ?2, lease_id = NULL,
  //              attempts = max(attempts - 1, 0) WHERE id = ?1 AND lease_id = ?3
  //   bury:      INSERT OR REPLACE INTO dead ... SELECT ... FROM message WHERE id = ?1 AND lease_id = ?4;
  //              DELETE FROM message WHERE id = ?1 AND lease_id = ?2   (one transaction)
  // `back`: whether the new due (now + delay) has come already.
  pure def settled(s: St, op: str, i: int, t: int, back: bool): St = {
    val hit = s.live.contains(i) and
              (s.tok.get(i) == t or (BUG_DONE_NO_TOKEN and op == "done"))
    if (not(hit)) s
    else if (op == "done") { ...s, live: s.live.exclude(Set(i)), tok: s.tok.set(i, 0) }
    else if (op == "bury") { ...s, live: s.live.exclude(Set(i)), dead: s.dead.union(Set(i)),
                              tok: s.tok.set(i, 0) }
    else if (op == "retry") { ...s, due: s.due.set(i, back), tok: s.tok.set(i, 0) }
    else { ...s, due: s.due.set(i, back), tok: s.tok.set(i, 0),
           att: s.att.set(i, if (s.att.get(i) - 1 > 0) s.att.get(i) - 1 else 0) }
  }

  // Any caller, any id, any token ever issued (stale ones included).
  action settle = {
    nondet op = Set("done", "retry", "give_back", "bury").oneOf()
    nondet i = IDS.oneOf()
    nondet t = TOKS.oneOf()
    nondet back = Set(true, false).oneOf()
    val t2 = settled(st, op, i, t, back)
    val holds = live.contains(i) and tok.get(i) == t
    all {
      1 <= t, t < nextTok,
      go(t2),
      nextId' = nextId, nextTok' = nextTok,
      s3ok' = s3ok, s5ok' = s5ok,
      // S4.2: the store changed iff t was the row's token
      s4ok' = (s4ok and ((t2 != st) == holds)),
    }
  }

  // bury_spent, for one row it buries: `... WHERE due <= ?2 AND attempts >= ?3`,
  // whatever its lease; `INSERT OR REPLACE INTO dead ... SELECT ... FROM message
  // WHERE id = ?1` and `DELETE FROM message WHERE id = ?1`. Rows with a held
  // settle are passed over in the code; leaving that out adds behaviours.
  action burySpent = {
    nondet i = live.oneOf()
    all {
      due.get(i), att.get(i) >= m,
      go({ ...st, live: live.exclude(Set(i)), dead: dead.union(Set(i)), tok: tok.set(i, 0) }),
      nextId' = nextId, nextTok' = nextTok, s3ok' = s3ok, s4ok' = s4ok, s5ok' = s5ok,
    }
  }

  // revive: the queue holds fewer than max_depth (a declared queue), then
  // `INSERT INTO message (id, queue, body, attempts, due, sent) SELECT id, queue,
  // body, 0, ?2, sent FROM dead WHERE id = ?1` and `DELETE FROM dead WHERE id = ?1`.
  // The id is the dead letter's own; there is no lease_id (NULL).
  action revive = {
    nondet i = IDS.oneOf()
    all {
      dead.contains(i), live.size() < d,
      go({ ...st, live: live.union(Set(i)), dead: dead.exclude(Set(i)),
           att: att.set(i, 0), due: due.set(i, true), tok: tok.set(i, 0) }),
      nextId' = nextId, nextTok' = nextTok, s3ok' = s3ok, s4ok' = s4ok, s5ok' = s5ok,
    }
  }

  // prune_dead, for one row it deletes: `DELETE FROM dead WHERE buried < ?1`.
  action prune = {
    nondet i = IDS.oneOf()
    all {
      dead.contains(i),
      go({ ...st, dead: dead.exclude(Set(i)) }),
      nextId' = nextId, nextTok' = nextTok, s3ok' = s3ok, s4ok' = s4ok, s5ok' = s5ok,
    }
  }

  // Time passes: a row's due time comes.
  action timePasses = {
    nondet i = live.oneOf()
    all {
      not(due.get(i)),
      go({ ...st, due: due.set(i, true) }),
      nextId' = nextId, nextTok' = nextTok, s3ok' = s3ok, s4ok' = s4ok, s5ok' = s5ok,
    }
  }

  // A redeploy: the caller passes another M or D from now on.
  action redeploy = {
    nondet m2 = MS.oneOf()
    nondet d2 = MS.oneOf()
    all {
      m2 >= 1, d2 >= 1,
      live' = live, dead' = dead, att' = att, due' = due, tok' = tok, leaseM' = leaseM,
      obsTok' = obsTok, obsId' = obsId, obsRetired' = obsRetired, obsUsed' = obsUsed,
      m' = m2, d' = d2,
      nextId' = nextId, nextTok' = nextTok, s3ok' = s3ok, s4ok' = s4ok, s5ok' = s5ok,
    }
  }

  action init = {
    nondet m0 = MS.oneOf()
    nondet d0 = MS.oneOf()
    nondet ot = TOKS.oneOf()
    nondet oi = IDS.oneOf()
    all {
      m0 >= 1, d0 >= 1, ot >= 1, oi >= 1,
      live' = Set(), dead' = Set(), att' = Map(), due' = Map(), tok' = Map(), leaseM' = Map(),
      nextId' = 1, nextTok' = 1, m' = m0, d' = d0,
      obsTok' = ot, obsRetired' = false, obsId' = oi, obsUsed' = false,
      s3ok' = true, s4ok' = true, s5ok' = true,
    }
  }

  action step = any { send, lease, settle, burySpent, revive, prune, timePasses, redeploy }

  // --------------------------------------------------------- properties
  // The whole state as one value, so a predicate can be applied to the
  // arbitrary pre-state `indInit` makes as well as to the current one.
  type Full = { s: St, nextId: int, nextTok: int, m: int, d: int,
                obsTok: int, obsRetired: bool, obsId: int, obsUsed: bool,
                s3ok: bool, s4ok: bool, s5ok: bool }
  val full: Full = { s: st, nextId: nextId, nextTok: nextTok, m: m, d: d,
                     obsTok: obsTok, obsRetired: obsRetired, obsId: obsId, obsUsed: obsUsed,
                     s3ok: s3ok, s4ok: s4ok, s5ok: s5ok }

  // S2: no id on both tables.
  pure def pS2(f: Full): bool = f.s.live.intersect(f.s.dead) == Set()
  // S3: a new message never gets an id any message has had (monitor).
  pure def pS3(f: Full): bool = f.s3ok
  // S4.1 holds by the schema: a row has one `lease_id`, so the tokens that
  // settle row i are {tok(i)} or none. Across rows: a token settles at
  // most one row.
  pure def pS4_1(f: Full): bool =
    f.s.live.forall(i => f.s.live.forall(j =>
      i == j or f.s.tok.get(i) == 0 or f.s.tok.get(i) != f.s.tok.get(j)))
  // S4.2: a settle changed the store iff its token was the row's (monitor).
  pure def pS4_2(f: Full): bool = f.s4ok
  // S4.3: a token once replaced or removed is never current again
  // (for obsTok, which is any token).
  pure def pS4_3(f: Full): bool =
    f.obsRetired implies f.s.live.forall(i => f.s.tok.get(i) != f.obsTok)
  // S5 (store): every lease was of a row with attempts < the M it was given
  // (monitor), and a row holding a token has attempts <= that lease's M.
  pure def pS5(f: Full): bool = f.s5ok
  pure def pS5state(f: Full): bool =
    f.s.live.forall(i => f.s.tok.get(i) != 0 implies f.s.att.get(i) <= f.s.leaseM.get(i))

  pure def props(f: Full): bool = and {
    pS2(f), pS3(f), pS4_1(f), pS4_2(f), pS4_3(f), pS5(f), pS5state(f),
  }
  val S2 = pS2(full)
  val S3 = pS3(full)
  val S4_1 = pS4_1(full)
  val S4_2 = pS4_2(full)
  val S4_3 = pS4_3(full)
  val S5 = pS5(full)
  val S5state = pS5state(full)
  val Props = props(full)

  // ------------------------------------------------------------- lemmas
  // What had to be added to the properties to make them inductive, one
  // atomic lemma each. README ("Inductive invariants") gives the
  // counterexample to induction that forced each, in the order of `ORDER`.
  pure def lemma(n: str, f: Full): bool =
    // shape: every row has its columns (anyState assumes it of the
    // pre-state; IndInv asks it of the post-state, so the assumption is
    // itself inductive)
    if (n == "domLive") f.s.live.forall(i =>
      f.s.att.keys().contains(i) and f.s.due.keys().contains(i) and
      f.s.tok.keys().contains(i) and f.s.leaseM.keys().contains(i))
    else if (n == "domDead") f.s.dead.forall(i =>
      f.s.att.keys().contains(i) and f.s.due.keys().contains(i) and
      f.s.tok.keys().contains(i) and f.s.leaseM.keys().contains(i))
    // every id on a table is below AUTOINCREMENT's counter
    else if (n == "idsBelow") f.s.live.union(f.s.dead).forall(i => i < f.nextId)
    // so is the observed id, once it has been used
    else if (n == "obsIdBelow") (f.obsUsed implies f.obsId < f.nextId)
    // the observed token is a token, not NULL
    else if (n == "obsTokPos") f.obsTok >= 1
    // every live row's token (NULL included) is below next_lease_id's counter
    else if (n == "toksBelow") f.s.live.forall(i => f.s.tok.get(i) < f.nextTok)
    // ... which is above NULL
    else if (n == "nextTokPos") f.nextTok >= 1
    // a retired token is below the counter: no lease can issue it again
    else if (n == "obsTokBelow") (f.obsRetired implies f.obsTok < f.nextTok)
    else false

  pure val SHAPE: List[str] = ["domLive", "domDead"]
  pure val ORDER: List[str] = ["idsBelow", "obsIdBelow", "obsTokPos", "toksBelow", "nextTokPos", "obsTokBelow"]
  pure def all_(ns: List[str], f: Full): bool = ns.indices().forall(k => lemma(ns[k], f))

  pure def indP(f: Full): bool = and { props(f), all_(SHAPE, f), all_(ORDER, f) }
  val IndInv = indP(full)
  action indInit = anyState(indP)
  // Not vacuous: indInit can start from three rows, two of them live and
  // holding tokens, with obsTok retired. Checked to fail (README).
  val NoRichStart = not(and {
    obsRetired, live.size() == 2, dead.size() == 1,
    live.forall(i => tok.get(i) != 0), obsUsed,
  })

  // The iterations: the properties and the first n lemmas of ORDER
  // (without SHAPE, as they were run). Iter0..Iter5 each have a
  // counterexample to induction; Iter6 is inductive.
  pure def iterP(n: int, f: Full): bool =
    and { props(f), ORDER.indices().forall(k => k >= n or lemma(ORDER[k], f)) }
  val Iter0 = iterP(0, full)
  action iter0Init = anyState(f => iterP(0, f))
  val Iter1 = iterP(1, full)
  action iter1Init = anyState(f => iterP(1, f))
  val Iter2 = iterP(2, full)
  action iter2Init = anyState(f => iterP(2, f))
  val Iter3 = iterP(3, full)
  action iter3Init = anyState(f => iterP(3, f))
  val Iter4 = iterP(4, full)
  action iter4Init = anyState(f => iterP(4, f))
  val Iter5 = iterP(5, full)
  action iter5Init = anyState(f => iterP(5, f))
  val Iter6 = iterP(6, full)
  action iter6Init = anyState(f => iterP(6, f))

  // Each lemma is needed: IndInv without the n-th lemma of ORDER is not
  // inductive (each has a counterexample to induction).
  pure def dropP(n: int, f: Full): bool =
    and { props(f), all_(SHAPE, f), ORDER.indices().forall(k => k == n or lemma(ORDER[k], f)) }
  val Drop0 = dropP(0, full)
  action drop0Init = anyState(f => dropP(0, f))
  val Drop1 = dropP(1, full)
  action drop1Init = anyState(f => dropP(1, f))
  val Drop2 = dropP(2, full)
  action drop2Init = anyState(f => dropP(2, f))
  val Drop3 = dropP(3, full)
  action drop3Init = anyState(f => dropP(3, f))
  val Drop4 = dropP(4, full)
  action drop4Init = anyState(f => dropP(4, f))
  val Drop5 = dropP(5, full)
  action drop5Init = anyState(f => dropP(5, f))

  // ------------------------------------------ the inductive check's start
  // Any state of at most K rows (on `message` or `dead`) with arbitrary
  // integer ids, tokens, attempts, counters, M and D that satisfies `pre`.
  action anyState(pre: Full => bool): bool = {
    nondet slot = 1.to(K).setOfMaps(Int).oneOf()
    val U = 1.to(K).map(k => slot.get(k))
    nondet L = U.powerset().oneOf()
    nondet Dd = U.powerset().oneOf()
    nondet A = U.setOfMaps(Int).oneOf()
    nondet Du = U.setOfMaps(Set(true, false)).oneOf()
    nondet T = U.setOfMaps(Int).oneOf()
    nondet LM = U.setOfMaps(Int).oneOf()
    nondet ni = Int.oneOf()
    nondet nt = Int.oneOf()
    nondet m0 = Int.oneOf()
    nondet d0 = Int.oneOf()
    nondet ot = Int.oneOf()
    nondet oi = Int.oneOf()
    nondet orr = Set(true, false).oneOf()
    nondet ou = Set(true, false).oneOf()
    nondet b3 = Set(true, false).oneOf()
    nondet b4 = Set(true, false).oneOf()
    nondet b5 = Set(true, false).oneOf()
    val f: Full = { s: { live: L, dead: Dd, att: A, due: Du, tok: T, leaseM: LM },
                    nextId: ni, nextTok: nt, m: m0, d: d0, obsTok: ot, obsRetired: orr,
                    obsId: oi, obsUsed: ou, s3ok: b3, s4ok: b4, s5ok: b5 }
    all {
      m0 >= 1, d0 >= 1,
      pre(f),
      live' = L, dead' = Dd, att' = A, due' = Du, tok' = T, leaseM' = LM,
      nextId' = ni, nextTok' = nt, m' = m0, d' = d0,
      obsTok' = ot, obsRetired' = orr, obsId' = oi, obsUsed' = ou,
      s3ok' = b3, s4ok' = b4, s5ok' = b5,
    }
  }
}

// ---------------------------------------------------------------- instances
// Apalache, unbounded values; K rows in the inductive check's pre-state.
module storeInd2 {
  import store(IDS = Int, TOKS = Int, MS = Int, MAX_ID = -1, MAX_TOK = -1,
               BUG_DONE_NO_TOKEN = false, BUG_LEASE_NO_M = false, K = 2).* from "./store"
}
module storeInd3 {
  import store(IDS = Int, TOKS = Int, MS = Int, MAX_ID = -1, MAX_TOK = -1,
               BUG_DONE_NO_TOKEN = false, BUG_LEASE_NO_M = false, K = 3).* from "./store"
}
module storeInd4 {
  import store(IDS = Int, TOKS = Int, MS = Int, MAX_ID = -1, MAX_TOK = -1,
               BUG_DONE_NO_TOKEN = false, BUG_LEASE_NO_M = false, K = 4).* from "./store"
}
module storeInd6 {
  import store(IDS = Int, TOKS = Int, MS = Int, MAX_ID = -1, MAX_TOK = -1,
               BUG_DONE_NO_TOKEN = false, BUG_LEASE_NO_M = false, K = 6).* from "./store"
}
module storeIndBugDone {
  import store(IDS = Int, TOKS = Int, MS = Int, MAX_ID = -1, MAX_TOK = -1,
               BUG_DONE_NO_TOKEN = true, BUG_LEASE_NO_M = false, K = 3).* from "./store"
}
module storeIndBugLease {
  import store(IDS = Int, TOKS = Int, MS = Int, MAX_ID = -1, MAX_TOK = -1,
               BUG_DONE_NO_TOKEN = false, BUG_LEASE_NO_M = true, K = 3).* from "./store"
}
// TLC: ids 1..2, tokens 1..4, M and D in 1..2.
module storeTlc {
  import store(IDS = 1.to(2), TOKS = 1.to(4), MS = 1.to(2), MAX_ID = 2, MAX_TOK = 4,
               BUG_DONE_NO_TOKEN = false, BUG_LEASE_NO_M = false, K = 0).* from "./store"
}
module storeTlcBugDone {
  import store(IDS = 1.to(2), TOKS = 1.to(4), MS = 1.to(2), MAX_ID = 2, MAX_TOK = 4,
               BUG_DONE_NO_TOKEN = true, BUG_LEASE_NO_M = false, K = 0).* from "./store"
}
module storeTlcBugLease {
  import store(IDS = 1.to(2), TOKS = 1.to(4), MS = 1.to(2), MAX_ID = 2, MAX_TOK = 4,
               BUG_DONE_NO_TOKEN = false, BUG_LEASE_NO_M = true, K = 0).* from "./store"
}
