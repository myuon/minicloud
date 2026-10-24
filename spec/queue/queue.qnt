// -*- mode: Bluespec; -*-
//
// A model of minicloud's queue: the store (`crates/minicloud/src/queue.rs`)
// and the delivery loop (`queue_host.rs`), as the code is at `e39f40e`, and
// the fixes `docs/queue-properties.md` proposes, each behind its own flag.
//
// The properties are the contract of `docs/queue-properties.md`. Monitors
// (the `bad` set) record an action property the moment a step breaks it;
// everything else is a state invariant or a temporal formula. The checks
// themselves (bounds, flags) are the modules in `checks.qnt`; how to run
// them is in `README.md`.
//
// Time is abstract: a message is `due` (its due time has come) or not.
// `timePasses(i)` makes one message due; `clockBack` makes due messages
// not due again. The one-second truncation (F4) is arithmetic and is left to
// Rust tests.
module queue {
  type Cfg = {
    NQ: int,
    NID: int,
    NTOK: int,
    MAXATT: int,
    RECYCLE: bool,
    INIT_POL: Pol,
    REDEPLOY_POLS: Set[Pol],
    MAX_CRASH: int,
    MAX_REDEPLOY: int,
    MAX_REVIVE: int,
    MAX_CLOCKBACK: int,
    ENV_SEND_AFTER: bool,
    ENV_OLD_SENDS: bool,
    ENV_REFUSE: bool,
    ENV_TIMEOUT: bool,
    ENV_WRITE_FAIL: bool,
    ENV_PRUNE: bool,
    ENV_OUTCOMES: Set[str],
    FIX_S5: bool,
    FIX_S6: bool,
    FIX_S7: bool,
    FIX_S7_GUARD: bool,
    FIX_S10: bool,
    FIX_L2: bool,
    PRE_167: bool,
    ONE_NOW: bool,
    TRACK: Set[str],
  }
  const C: Cfg

  // ------------------------------------------------------------ bounds
  val NQ = C.NQ  // queues 0 .. NQ-1, in declaration order
  val NID = C.NID  // message ids 1 .. NID
  val NTOK = C.NTOK  // lease tokens per id (see `freshTok`)
  val MAXATT = C.MAXATT  // attempts past this is a bound overflow
  val RECYCLE = C.RECYCLE  // liveness: a free id may be sent again
  val INIT_POL = C.INIT_POL  // the first version's [[queues]]
  val REDEPLOY_POLS = C.REDEPLOY_POLS
  val MAX_CRASH = C.MAX_CRASH
  val MAX_REDEPLOY = C.MAX_REDEPLOY
  val MAX_REVIVE = C.MAX_REVIVE
  val MAX_CLOCKBACK = C.MAX_CLOCKBACK  // ------------------------------------------------------- environment
  // ------------------------------------------------------- environment
  val ENV_SEND_AFTER = C.ENV_SEND_AFTER  // sendAfter with a delay
  val ENV_OLD_SENDS = C.ENV_OLD_SENDS  // runs of the previous version may still send
  val ENV_REFUSE = C.ENV_REFUSE  // admission may be refused
  val ENV_TIMEOUT = C.ENV_TIMEOUT  // a queued run may time out in the scheduler (503)
  val ENV_WRITE_FAIL = C.ENV_WRITE_FAIL  // a settle's write may fail
  val ENV_PRUNE = C.ENV_PRUNE  // the admin may prune dead letters
  val ENV_OUTCOMES = C.ENV_OUTCOMES  // what a started run may answer: ok (<500), fail (5xx), none
  // ------------------------------------------------- the proposed fixes
  val FIX_S5 = C.FIX_S5  // lease takes M; the tick buries attempts >= M whatever the lease
  val FIX_S6 = C.FIX_S6  // a run that never started is given back
  val FIX_S7 = C.FIX_S7  // a failed settle is kept and tried again at the next tick
  val FIX_S7_GUARD = C.FIX_S7_GUARD  // ... and bury_spent leaves alone a message whose settle is pending
  val FIX_S10 = C.FIX_S10  // revive refuses when the queue is full
  val FIX_L2 = C.FIX_L2  // in flight counted per queue
  // ------------------------------------------------------------ sanity
  val PRE_167 = C.PRE_167  // lease(None): the delivery loop before #167
  val ONE_NOW = C.ONE_NOW  // restriction: no time passes between a queue's bury_spent and its lease
  // The ghosts a check needs; the others stay at their initial value, which
  // keeps the state space to what the property can see. Ghosts never guard
  // a step, so leaving one out removes no behaviour.
  val TRACK = C.TRACK

  type Pol = { decl: int -> bool, m: int -> int, d: int -> int, batch: int -> int }
  // `message` row: `tok` 0 is lease_id NULL.
  type Msg = { on: bool, q: int, att: int, due: bool, tok: int }
  // `dead` row; `why` is "fail" (a settle's bury), "outlived" (bury_spent),
  // "lowered" (the S5 fix's burial of a message M was lowered under).
  type Dead = { on: bool, q: int, att: int, why: str }
  // A delivery task. `att` and `m` are its own copy (the message `lease`
  // answered, the policy of the version the tick ran). `stage`: leased
  // (the tick has yet to admit it), queued (admitted, waiting for a
  // worker), running (the app's entry started), answered (the run ended;
  // `out` says how), pending (FIX_S7: a settle `op` whose write failed).
  // `life`, `started` and `valid` are ghosts.
  type Del = { id: int, q: int, tok: int, att: int, m: int, life: int,
               stage: str, out: str, op: str, started: bool, valid: bool, tried: bool }
  // Per id ghosts. `net`: leases issued in this life minus give_backs that
  // were written. `invoc`: invocations by deliveries of this life. `gap`:
  // unstarted deliveries of this life lost to a crash. `invAll`: all
  // invocations, capped at 2. `broken`: L3a's hypothesis no longer holds.
  // `ranOut`: the current lease ran out. `answered`: the holder of the
  // current lease got its app's answer before the lease ran out. `early`:
  // sent with sendAfter and its time has not come. `acked`: a send answered
  // Ok. `legit`: removed as S1 allows. `used`: S3's ids ever given.
  type Ghost = { life: int, net: int, invoc: int, gap: int, invAll: int, broken: bool,
                 ranOut: bool, answered: bool, early: bool, acked: bool, legit: bool, used: bool }

  // ------------------------------------------------------------ state
  var msg: int -> Msg
  var dead: int -> Dead
  var nextId: int
  var inflight: int -> int  // per queue; the code's app-wide count is the sum
  var dels: Set[Del]
  var tpc: str              // the tick: idle, settle (FIX_S7), bury, lease, admit
  var tqi: int              // the queue the tick is at
  var tpol: Pol             // the version the tick read when it began
  var pol: Pol              // the current version
  var prevPol: Pol
  var oldRuns: bool         // a run of the previous version may still be running
  var crashes: int
  var redeploys: int
  var revives: int
  var clockbacks: int
  var gh: int -> Ghost
  var mmax: int -> int      // the highest M each queue has had
  var bad: Set[str]         // monitors: the action properties broken so far

  val vars = (msg, dead, nextId, inflight, dels, tpc, tqi, tpol, pol, prevPol, oldRuns,
              crashes, redeploys, revives, clockbacks, gh, mmax, bad)

  // ---------------------------------------------------------- helpers
  pure def imax(a: int, b: int): int = if (a > b) a else b
  pure def imin(a: int, b: int): int = if (a < b) a else b
  // The least of `s`, or `dflt` for an empty `s`.
  pure def setMin(s: Set[int], dflt: int): int =
    if (s.size() == 0) dflt else s.fold(1000, (a, x) => if (x < a) x else a)
  val QS = 0.to(NQ - 1)
  val IDS = 1.to(NID)
  val TOKS = 1.to(NTOK)
  pure val NOMSG: Msg = { on: false, q: 0, att: 0, due: false, tok: 0 }
  pure val NODEAD: Dead = { on: false, q: 0, att: 0, why: "" }
  pure val G0: Ghost = { life: 0, net: 0, invoc: 0, gap: 0, invAll: 0, broken: false,
                         ranOut: false, answered: false, early: false, acked: false,
                         legit: false, used: false }

  def depth(q: int): int = IDS.filter(i => msg.get(i).on and msg.get(i).q == q).size()
  def appInflight: int = QS.fold(0, (a, q) => a + inflight.get(q))
  // The first queue at or after `from` that `p` declares; NQ for none.
  def nextDecl(p: Pol, from: int): int = setMin(QS.filter(q => q >= from and p.decl.get(q)), NQ)
  // A token no live reference to message `i` has: the message's own and every
  // delivery's. Tokens are only ever compared, so this is the code's fresh
  // `next_lease_id` up to renaming. 0 when the pool is exhausted (overflow).
  def freshTok(i: int): int =
    setMin(TOKS.filter(t => t != msg.get(i).tok and not(dels.exists(d => d.id == i and d.tok == t))), 0)
  pure def invalidate(ds: Set[Del], i: int): Set[Del] =
    ds.map(d => if (d.id == i) { ...d, valid: false } else d)
  def pendingFor(i: int): bool = dels.exists(d => d.id == i and d.stage == "pending")
  def unstarted(i: int): int =
    dels.filter(d => d.id == i and d.life == gh.get(i).life and not(d.started)).size()
  // L3a: the hypothesis no longer holds for these messages
  def breakFor(g: int -> Ghost, ids: Set[int]): int -> Ghost =
    IDS.mapBy(i => if (ids.contains(i)) { ...g.get(i), broken: true } else g.get(i))
  def liveIds: Set[int] = IDS.filter(i => msg.get(i).on)
  def tr(f: str): bool = TRACK.contains(f)
  def mask(g: Ghost): Ghost = {
    life: if (tr("life")) g.life else 0,
    net: if (tr("net")) g.net else 0,
    invoc: if (tr("invoc")) g.invoc else 0,
    gap: if (tr("gap")) g.gap else 0,
    invAll: if (tr("invAll")) g.invAll else 0,
    broken: if (tr("broken")) g.broken else false,
    ranOut: if (tr("ranOut")) g.ranOut else false,
    answered: if (tr("answered")) g.answered else false,
    early: if (tr("early")) g.early else false,
    acked: if (tr("acked")) g.acked else false,
    legit: if (tr("legit")) g.legit else false,
    used: if (tr("used")) g.used else false,
  }
  def maskAll(m: int -> Ghost): int -> Ghost = IDS.mapBy(i => mask(m.get(i)))
  def isNextLeased(d: Del): bool =
    d.stage == "leased" and dels.forall(e => e.stage == "leased" implies e.id >= d.id)

  // ------------------------------------------------- unchanged groups
  action keepStore = all { msg' = msg, dead' = dead, nextId' = nextId }
  action keepTick = all { tpc' = tpc, tqi' = tqi, tpol' = tpol }
  action keepApp = all { pol' = pol, prevPol' = prevPol, oldRuns' = oldRuns }
  action keepEnv = all { crashes' = crashes, redeploys' = redeploys, revives' = revives, clockbacks' = clockbacks }

  // ------------------------------------------------------------ init
  action init = all {
    msg' = IDS.mapBy(i => NOMSG),
    dead' = IDS.mapBy(i => NODEAD),
    nextId' = 1,
    inflight' = QS.mapBy(q => 0),
    dels' = Set(),
    tpc' = "idle", tqi' = 0, tpol' = INIT_POL,
    pol' = INIT_POL, prevPol' = INIT_POL, oldRuns' = false,
    crashes' = 0, redeploys' = 0, revives' = 0, clockbacks' = 0,
    gh' = IDS.mapBy(i => G0),
    mmax' = QS.mapBy(q => INIT_POL.m.get(q)),
    bad' = Set(),
  }

  // ------------------------------------------------------- settling
  // The op the code chooses for an answered delivery (`start`'s task).
  def opFor(d: Del): str =
    if (d.out == "ok") "done"
    else if (FIX_S6 and d.out == "timeout") "giveback"
    else if (d.att >= d.m) "bury"
    else "retry"

  type Eff = { msg: int -> Msg, dead: int -> Dead, dels: Set[Del], gh: int -> Ghost, bad: Set[str] }

  // Delivery `d` writes `op` (done, retry, bury, giveback) and the write
  // `ok`s or fails; `rest` is the deliveries without `d`. Each op asks for
  // `lease_id IS ?` as the code does.
  def settleEff(d: Del, op: str, ok: bool, rest: Set[Del]): Eff = {
    val i = d.id
    val m = msg.get(i)
    val g = gh.get(i)
    val hit = ok and m.on and m.tok == d.tok
    val gone = op == "done" or op == "bury"
    // S4.2: a written settle changes the store iff `d` holds the valid lease
    val b1 = if (ok and hit != d.valid) Set("S4.2") else Set()
    // S7 (a): a settle buries only for the valid holder whose run started
    // and answered 5xx or nothing, on its M-th delivery
    val b2 = if (hit and op == "bury" and
                 not(d.valid and d.started and (d.out == "fail" or d.out == "none") and d.att >= d.m))
               Set("S7") else Set()
    {
      msg: if (not(hit)) msg
           else if (gone) msg.set(i, { ...m, on: false })
           else msg.set(i, { ...m, due: false, tok: 0,
                             att: if (op == "giveback") imax(m.att - 1, 0) else m.att }),
      dead: if (hit and op == "bury") dead.set(i, { on: true, q: m.q, att: m.att, why: "fail" }) else dead,
      dels: if (not(ok) and FIX_S7) rest.union(Set({ ...d, stage: "pending", op: op, tried: true }))
            else if (hit) invalidate(rest, i)
            else rest,
      gh: if (not(hit)) gh
          else gh.set(i, { ...g,
                 // S1 (a): done by the valid holder after an answer below 500
                 legit: g.legit or (op == "done" and d.valid and d.out == "ok"),
                 net: if (op == "giveback") g.net - 1 else g.net }),
      bad: bad.union(b1).union(b2),
    }
  }

  // The delivery task settles an answered run.
  action settle(d: Del, ok: bool): bool = all {
    d.stage == "answered",
    ok or ENV_WRITE_FAIL,
    val e = settleEff(d, opFor(d), ok, dels.exclude(Set(d)))
    all { msg' = e.msg, dead' = e.dead, nextId' = nextId, dels' = e.dels, gh' = maskAll(e.gh), bad' = e.bad },
    inflight' = inflight, mmax' = mmax,
    keepTick, keepApp, keepEnv,
  }

  // ---------------------------------------------------------- the tick
  // Where the tick goes after queue `q`.
  // The tick moves to queue `n`, or ends (and forgets what it read).
  action goTo(n: int): bool =
    if (n < NQ) all { tpc' = "bury", tqi' = n, tpol' = tpol }
    else all { tpc' = "idle", tqi' = 0, tpol' = INIT_POL }
  action advanceFrom(q: int): bool = {
    val n = nextDecl(tpol, q + 1)
    goTo(n)
  }

  // FIX_S7: the tick begins by trying the settles that failed.
  action tickBeginSettle = all {
    FIX_S7, tpc == "idle", nextDecl(pol, 0) < NQ,
    dels.exists(d => d.stage == "pending"),
    tpc' = "settle", tqi' = 0, tpol' = pol,
    dels' = dels.map(d => if (d.stage == "pending") { ...d, tried: false } else d),
    keepStore, inflight' = inflight, keepApp, keepEnv, gh' = gh, mmax' = mmax, bad' = bad,
  }
  action tickSettleRetry(d: Del, ok: bool): bool = all {
    tpc == "settle", d.stage == "pending", not(d.tried),
    ok or ENV_WRITE_FAIL,
    val e = settleEff(d, d.op, ok, dels.exclude(Set(d)))
    all { msg' = e.msg, dead' = e.dead, nextId' = nextId, dels' = e.dels, gh' = maskAll(e.gh), bad' = e.bad },
    inflight' = inflight, mmax' = mmax, keepTick, keepApp, keepEnv,
  }
  action tickSettleEnd = all {
    tpc == "settle", dels.forall(d => d.stage == "pending" implies d.tried),
    val n = nextDecl(tpol, 0)
    goTo(n),
    keepStore, inflight' = inflight, dels' = dels, keepApp, keepEnv, gh' = gh, mmax' = mmax, bad' = bad,
  }

  // bury_spent for one queue. From idle, the tick begins: it reads the
  // current version, and does the first declared queue.
  action tickBury = {
    val begin = tpc == "idle"
    val p = if (begin) pol else tpol
    val q = if (begin) nextDecl(pol, 0) else tqi
    val mq = p.m.get(q)
    val spent = IDS.filter(i => msg.get(i).on and msg.get(i).q == q and msg.get(i).due
                               and msg.get(i).att >= mq and (FIX_S5 or msg.get(i).tok != 0)
                               and not(FIX_S7_GUARD and pendingFor(i)))
    // S7 (b) and (c): "outlived" only if the current lease's holder did not
    // answer within it; "lowered" only if M was once higher
    val lies = spent.filter(i =>
      if (msg.get(i).tok != 0) gh.get(i).answered else not(mmax.get(q) > mq))
    all {
      begin or tpc == "bury",
      not(begin and FIX_S7 and dels.exists(d => d.stage == "pending")),
      q < NQ,
      msg' = IDS.mapBy(i => if (spent.contains(i)) { ...msg.get(i), on: false } else msg.get(i)),
      dead' = IDS.mapBy(i => if (spent.contains(i))
                               { on: true, q: q, att: msg.get(i).att,
                                 why: if (msg.get(i).tok != 0) "outlived" else "lowered" }
                             else dead.get(i)),
      nextId' = nextId,
      dels' = dels.map(d => if (spent.contains(d.id)) { ...d, valid: false } else d),
      tpc' = "lease", tqi' = q, tpol' = p,
      inflight' = inflight, keepApp, keepEnv, gh' = gh, mmax' = mmax,
      bad' = if (lies.size() > 0) bad.union(Set("S7")) else bad,
    }
  }

  // room = batch - in flight, then lease.
  action tickLease = {
    val q = tqi
    val p = tpol
    val inF = if (FIX_L2) inflight.get(q) else appInflight
    val room = imax(0, p.batch.get(q) - inF)
    val cand = IDS.filter(i => msg.get(i).on and msg.get(i).due
                               and (PRE_167 or msg.get(i).q == q)
                               and (not(FIX_S5) or msg.get(i).att < p.m.get(q)))
    // oldest first: ids are sent in order (see README on clock set back)
    val taken = cand.filter(i => cand.filter(j => j < i).size() < room)
    val tokOf = IDS.mapBy(i => freshTok(i))
    val over = taken.exists(i => tokOf.get(i) == 0 or msg.get(i).att + 1 > MAXATT)
    val b5 = if (taken.exists(i => msg.get(i).att >= p.m.get(q))) Set("S5") else Set()
    val b5s = if (taken.exists(i => msg.get(i).att >= pol.m.get(q))) Set("S5strict") else Set()
    val b8 = if (taken.exists(i => gh.get(i).early)) Set("S8") else Set()
    val b9 = if (taken.exists(i => msg.get(i).q != q)) Set("S9") else Set()
    val bo = if (over) Set("overflow") else Set()
    all {
      tpc == "lease",
      msg' = IDS.mapBy(i => if (taken.contains(i))
                              { ...msg.get(i), att: msg.get(i).att + 1, due: false, tok: tokOf.get(i) }
                            else msg.get(i)),
      dead' = dead, nextId' = nextId,
      dels' = invalidateAll(dels, taken).union(taken.map(i =>
                { id: i, q: q, tok: tokOf.get(i), att: msg.get(i).att + 1, m: p.m.get(q),
                  life: gh.get(i).life, stage: "leased", out: "", op: "",
                  started: false, valid: true, tried: false })),
      if (taken.size() > 0) all { tpc' = "admit", tqi' = q, tpol' = tpol } else advanceFrom(q),
      inflight' = inflight, keepApp, keepEnv,
      gh' = maskAll(IDS.mapBy(i => if (taken.contains(i))
                             { ...gh.get(i), net: gh.get(i).net + 1, ranOut: false, answered: false }
                           else gh.get(i))),
      mmax' = mmax,
      bad' = bad.union(b5).union(b5s).union(b8).union(b9).union(bo),
    }
  }
  pure def invalidateAll(ds: Set[Del], ids: Set[int]): Set[Del] =
    ds.map(d => if (ids.contains(d.id)) { ...d, valid: false } else d)

  // After admitting `d`: the next leased message, or the next queue.
  action afterAdmit(d: Del, ds: Set[Del]): bool =
    if (ds.exists(e => e.stage == "leased")) keepTick else advanceFrom(d.q)

  // `start`: in_flight + 1, then admit_with.
  action admitOk(d: Del): bool = {
    val ds = dels.exclude(Set(d)).union(Set({ ...d, stage: "queued" }))
    all {
      tpc == "admit", isNextLeased(d),
      dels' = ds,
      inflight' = inflight.set(d.q, inflight.get(d.q) + 1),
      afterAdmit(d, ds),
      keepStore, keepApp, keepEnv, gh' = gh, mmax' = mmax, bad' = bad,
    }
  }
  // Admission refused: in_flight - 1, give_back.
  action admitRefused(d: Del, ok: bool): bool = {
    val e = settleEff(d, "giveback", ok, dels.exclude(Set(d)))
    all {
      ENV_REFUSE, tpc == "admit", isNextLeased(d),
      ok or ENV_WRITE_FAIL,
      msg' = e.msg, dead' = e.dead, nextId' = nextId, dels' = e.dels,
      gh' = maskAll(breakFor(e.gh, Set(d.id))), bad' = e.bad,
      inflight' = inflight,
      afterAdmit(d, e.dels),
      keepApp, keepEnv, mmax' = mmax,
    }
  }

  action tick = any {
    tickBeginSettle,
    tickSettleRetryAny,
    tickSettleEnd,
    tickBury,
    tickLease,
    admitOkAny,
    admitRefusedAny,
  }
  action tickSettleRetryAny = { nondet d = dels.oneOf() nondet ok = Set(true, false).oneOf() tickSettleRetry(d, ok) }
  action admitOkAny = { nondet d = dels.oneOf() admitOk(d) }
  action admitRefusedAny = { nondet d = dels.oneOf() nondet ok = Set(true, false).oneOf() admitRefused(d, ok) }

  // ----------------------------------------------------- the run
  action replaceDel(d: Del, d2: Del): bool = dels' = dels.exclude(Set(d)).union(Set(d2))

  // The app's entry starts: an invocation.
  action runStart(d: Del): bool = {
    val g = gh.get(d.id)
    all {
      d.stage == "queued",
      replaceDel(d, { ...d, stage: "running", started: true }),
      gh' = maskAll(gh.set(d.id, { ...g, invoc: if (d.life == g.life) g.invoc + 1 else g.invoc,
                                 invAll: imin(g.invAll + 1, 2) })),
      keepStore, inflight' = inflight, keepTick, keepApp, keepEnv, mmax' = mmax, bad' = bad,
    }
  }
  // The scheduler answers 503: the run waited past its deadline and was never started.
  action runTimeout(d: Del): bool = all {
    ENV_TIMEOUT, d.stage == "queued",
    replaceDel(d, { ...d, stage: "answered", out: "timeout" }),
    inflight' = inflight.set(d.q, inflight.get(d.q) - 1),
    gh' = maskAll(breakFor(gh, Set(d.id))),
    keepStore, keepTick, keepApp, keepEnv, mmax' = mmax, bad' = bad,
  }
  // The run ends: ok (2xx-4xx), fail (5xx), none (no answer).
  action runAnswer(d: Del, o: str): bool = {
    val g = gh.get(d.id)
    val g2 = { ...g, broken: g.broken or o != "ok",
                     answered: g.answered or (d.valid and not(g.ranOut)) }
    all {
      d.stage == "running", ENV_OUTCOMES.contains(o),
      replaceDel(d, { ...d, stage: "answered", out: o }),
      inflight' = inflight.set(d.q, inflight.get(d.q) - 1),
      gh' = maskAll(gh.set(d.id, g2)),
      keepStore, keepTick, keepApp, keepEnv, mmax' = mmax, bad' = bad,
    }
  }

  // --------------------------------------------------- environment
  // Time passes until message `i` is due.
  action timePasses(i: int): bool = {
    val m = msg.get(i)
    val g = gh.get(i)
    all {
      m.on, not(m.due),
      not(ONE_NOW and tpc == "lease"),
      msg' = msg.set(i, { ...m, due: true }),
      gh' = maskAll(gh.set(i, { ...g, early: false, ranOut: g.ranOut or m.tok != 0,
                              broken: g.broken or m.tok != 0 })),
      dead' = dead, nextId' = nextId,
      inflight' = inflight, dels' = dels, keepTick, keepApp, keepEnv, mmax' = mmax, bad' = bad,
    }
  }
  // The clock is set back past the due times of `s`.
  action clockBack(s: Set[int]): bool = all {
    clockbacks < MAX_CLOCKBACK, s.size() > 0,
    s.forall(i => msg.get(i).on and msg.get(i).due),
    msg' = IDS.mapBy(i => if (s.contains(i)) { ...msg.get(i), due: false } else msg.get(i)),
    gh' = maskAll(breakFor(gh, liveIds)),
    clockbacks' = clockbacks + 1,
    dead' = dead, nextId' = nextId, inflight' = inflight, dels' = dels, keepTick, keepApp,
    crashes' = crashes, redeploys' = redeploys, revives' = revives, mmax' = mmax, bad' = bad,
  }
  // S6.3's gap: the unstarted deliveries of this life a crash takes away.
  def crashGhost(i: int): Ghost =
    val g = gh.get(i)
    val lost = dels.filter(d => d.id == i and d.life == g.life and not(d.started)).size()
    { ...g, gap: g.gap + lost, broken: g.broken or msg.get(i).on }
  // The host process dies and comes back: the store keeps what was committed.
  action crash = all {
    crashes < MAX_CRASH,
    crashes' = crashes + 1,
    dels' = Set(),
    inflight' = QS.mapBy(q => 0),
    tpc' = "idle", tqi' = 0, tpol' = INIT_POL,
    oldRuns' = false, pol' = pol, prevPol' = prevPol,
    gh' = maskAll(IDS.mapBy(i => crashGhost(i))),
    keepStore, redeploys' = redeploys, revives' = revives, clockbacks' = clockbacks,
    mmax' = mmax, bad' = bad,
  }
  // A new version. Deliveries keep the policy they were started under.
  action redeploy(p: Pol): bool = all {
    redeploys < MAX_REDEPLOY,
    redeploys' = redeploys + 1,
    pol' = p,
    prevPol' = if (ENV_OLD_SENDS) pol else prevPol, oldRuns' = ENV_OLD_SENDS,
    mmax' = if (TRACK.contains("mmax")) QS.mapBy(q => imax(mmax.get(q), p.m.get(q))) else mmax,
    gh' = maskAll(breakFor(gh, liveIds)),
    keepStore, inflight' = inflight, dels' = dels, keepTick,
    crashes' = crashes, revives' = revives, clockbacks' = clockbacks, bad' = bad,
  }
  // The previous version's last run ends.
  action oldDrain = all {
    oldRuns, oldRuns' = false, pol' = pol, prevPol' = prevPol,
    keepStore, inflight' = inflight, dels' = dels, keepTick, keepEnv, gh' = gh, mmax' = mmax, bad' = bad,
  }
  // queue.send / queue.sendAfter from a run of the current version, or of
  // the previous one while it still runs. A send the depth check refuses
  // changes nothing and is not a step.
  action send(q: int, later: bool, old: bool): bool = {
    val p = if (old) prevPol else pol
    val free = IDS.filter(i => not(msg.get(i).on) and not(dead.get(i).on))
    val i = if (RECYCLE) setMin(free, 0) else nextId
    val g = gh.get(i)
    val b3 = if (not(RECYCLE) and g.used) Set("S3") else Set()
    // S10: an operation adds a waiting message only below the current D
    val b10 = if (depth(q) >= pol.d.get(q)) Set("S10") else Set()
    all {
      p.decl.get(q), not(old) or (ENV_OLD_SENDS and oldRuns), not(later) or ENV_SEND_AFTER,
      if (RECYCLE) free.size() > 0 else nextId <= NID,
      depth(q) < p.d.get(q),
      msg' = msg.set(i, { on: true, q: q, att: 0, due: not(later), tok: 0 }),
      nextId' = if (RECYCLE) nextId else nextId + 1,
      dead' = dead,
      gh' = maskAll(gh.set(i, { ...G0, life: g.life, acked: true, early: later, used: true })),
      bad' = bad.union(b3).union(b10),
      inflight' = inflight, dels' = dels, keepTick, keepApp, keepEnv, mmax' = mmax,
    }
  }
  // Admin: revive a dead letter (a new life).
  action revive(i: int): bool = {
    val dl = dead.get(i)
    val g = gh.get(i)
    val b10 = if (depth(dl.q) >= pol.d.get(dl.q)) Set("S10") else Set()
    all {
      revives < MAX_REVIVE, dl.on,
      not(FIX_S10) or depth(dl.q) < pol.d.get(dl.q),
      revives' = revives + 1,
      msg' = msg.set(i, { on: true, q: dl.q, att: 0, due: true, tok: 0 }),
      dead' = dead.set(i, NODEAD),
      gh' = maskAll(gh.set(i, { ...G0, life: g.life + 1, acked: g.acked, used: g.used, broken: true })),
      bad' = bad.union(b10),
      nextId' = nextId, inflight' = inflight, dels' = dels, keepTick, keepApp,
      crashes' = crashes, redeploys' = redeploys, clockbacks' = clockbacks, mmax' = mmax,
    }
  }
  // Admin: prune a dead letter.
  action prune(i: int): bool = all {
    ENV_PRUNE, dead.get(i).on,
    dead' = dead.set(i, NODEAD),
    gh' = maskAll(gh.set(i, { ...gh.get(i), legit: true })),
    msg' = msg, nextId' = nextId, inflight' = inflight, dels' = dels, keepTick, keepApp, keepEnv,
    mmax' = mmax, bad' = bad,
  }

  // ------------------------------------------------------------ step
  action runOf(d: Del): bool = any {
    runStart(d),
    runTimeout(d),
    { nondet o = ENV_OUTCOMES.oneOf() runAnswer(d, o) },
  }
  action runs = { nondet d = dels.oneOf() runOf(d) }
  action settleOf(d: Del): bool = { nondet ok = Set(true, false).oneOf() settle(d, ok) }
  action settleAny = { nondet d = dels.oneOf() settleOf(d) }
  action env = any {
    { nondet i = IDS.oneOf() timePasses(i) },
    { nondet s = IDS.powerset().oneOf() clockBack(s) },
    crash,
    { nondet p = REDEPLOY_POLS.oneOf() redeploy(p) },
    oldDrain,
    { nondet q = QS.oneOf() nondet l = Set(false, true).oneOf() nondet o = Set(false, true).oneOf() send(q, l, o) },
    { nondet i = IDS.oneOf() revive(i) },
    { nondet i = IDS.oneOf() prune(i) },
  }
  action step = any { tick, runs, settleAny, env }

  // ------------------------------------------------ safety properties
  val S1 = IDS.forall(i => gh.get(i).acked implies
                             (msg.get(i).on or dead.get(i).on or gh.get(i).legit))
  val S2 = IDS.forall(i => not(msg.get(i).on and dead.get(i).on))
  val S3 = not(bad.contains("S3"))
  val S4_1 = IDS.forall(i => dels.filter(d => d.id == i and d.valid).size() <= 1)
  val S4_2 = not(bad.contains("S4.2"))
  val S4_3 = dels.forall(d => not(d.valid) implies
                               not(msg.get(d.id).on and msg.get(d.id).tok == d.tok))
  val S4 = S4_1 and S4_2 and S4_3
  val S5 = not(bad.contains("S5"))
  val S5strict = not(bad.contains("S5strict"))
  val S6_1 = IDS.forall(i => msg.get(i).on implies msg.get(i).att == gh.get(i).net)
  val S6_2 = IDS.forall(i => msg.get(i).on implies gh.get(i).invoc <= msg.get(i).att)
  val S6_3 = IDS.forall(i => msg.get(i).on implies
                              msg.get(i).att - gh.get(i).invoc == gh.get(i).gap + unstarted(i))
  val S6 = S6_1 and S6_2 and S6_3
  val S7 = not(bad.contains("S7"))
  val S8 = not(bad.contains("S8"))
  val S9 = not(bad.contains("S9"))
  val S10 = not(bad.contains("S10"))
  val L3a = IDS.forall(i => not(gh.get(i).broken) implies gh.get(i).invAll <= 1)
  val NoOverflow = not(bad.contains("overflow"))

  // ----------------------------------------------- fairness (E2-E5)
  action admitOkFor(i: int): bool = { nondet d = dels.filter(e => e.id == i).oneOf() admitOk(d) }
  action runFor(i: int, t: int): bool =
    { nondet d = dels.filter(e => e.id == i and e.tok == t).oneOf() runOf(d) }
  action startFor(i: int, t: int): bool =
    { nondet d = dels.filter(e => e.id == i and e.tok == t).oneOf() runStart(d) }
  action settleFor(i: int, t: int): bool =
    { nondet d = dels.filter(e => e.id == i and e.tok == t).oneOf() settleOf(d) }
  action retryOkFor(i: int, t: int): bool =
    { nondet d = dels.filter(e => e.id == i and e.tok == t).oneOf() tickSettleRetry(d, true) }

  // E1: from some point on every queue is declared, forever.
  temporal E1 = eventually(always(QS.forall(q => pol.decl.get(q))))
  // E2: the loop ticks (weak fairness) and the clock reaches every due time.
  temporal E2 = weakFair(tick, vars) and IDS.forall(i => weakFair(timePasses(i), vars))
  // E3: every admitted run ends; the task then settles it.
  temporal E3 = IDS.forall(i => TOKS.forall(t => weakFair(runFor(i, t), vars)))
            and IDS.forall(i => TOKS.forall(t => weakFair(settleFor(i, t), vars)))
  // E4: a message tried infinitely often is admitted infinitely often.
  temporal E4 = IDS.forall(i => strongFair(admitOkFor(i), vars))
  // E5: a settle tried again is eventually written.
  temporal E5 = IDS.forall(i => TOKS.forall(t => strongFair(retryOkFor(i, t), vars)))
  // Not in the doc: a run admitted infinitely often is started infinitely often.
  temporal E7 = IDS.forall(i => TOKS.forall(t => strongFair(startFor(i, t), vars)))
  temporal Fair = E2 and E3 and E4 and E5

  // ------------------------------------------------ liveness properties
  // L1: every message on `message` eventually leaves it.
  temporal L1 = IDS.forall(i => always(msg.get(i).on implies eventually(not(msg.get(i).on))))
  // L2: a due message is eventually handed out (or leaves).
  temporal L2 = IDS.forall(i => always((msg.get(i).on and msg.get(i).due)
                  implies eventually(not(msg.get(i).on) or not(msg.get(i).due))))
  // L3b: a message is eventually invoked at least once.
  temporal L3b = IDS.forall(i => always(msg.get(i).on implies eventually(gh.get(i).invAll >= 1)))
}
