// -*- mode: Bluespec; -*-
//
// The configurations the checks run `queue.qnt` with: bounds, environment
// and fix flags. `tools/check.sh <name> <CONFIG> ...` instantiates `queue`
// with one of them in a module of its own (one per file: Quint hands
// Apalache the whole file, and several instances of `queue` in one file
// exceed its 20 MB limit) and runs TLC on it.
module configs {
  import queue.* from "./queue"

  // One queue: M = 2, D = 2, batch 1. A redeploy may change M and D or
  // remove the queue.
  pure val POL1: Pol = { decl: Map(0 -> true), m: Map(0 -> 2), d: Map(0 -> 2), batch: Map(0 -> 1) }
  pure val POLS1: Set[Pol] =
    Set(true, false).map(dc => Set(1, 2).map(m => Set(1, 2).map(d =>
      { decl: Map(0 -> dc), m: Map(0 -> m), d: Map(0 -> d), batch: Map(0 -> 1) }))).flatten().flatten()

  // Two queues a (0) and b (1), declared in that order.
  pure val POL2: Pol = { decl: Map(0 -> true, 1 -> true), m: Map(0 -> 2, 1 -> 2),
                         d: Map(0 -> 2, 1 -> 2), batch: Map(0 -> 1, 1 -> 1) }
  // A redeploy of two queues may remove either or change either's M.
  pure val POLS2: Set[Pol] =
    Set(true, false).map(da => Set(true, false).map(db => Set(1, 2).map(ma => Set(1, 2).map(mb =>
      { decl: Map(0 -> da, 1 -> db), m: Map(0 -> ma, 1 -> mb), d: Map(0 -> 2, 1 -> 2),
        batch: Map(0 -> 1, 1 -> 1) })))).flatten().flatten().flatten()

  pure val ALLGHOSTS: Set[str] = Set("life", "net", "invoc", "gap", "invAll", "broken", "ranOut",
                                     "answered", "early", "acked", "legit", "used", "mmax", "valid")
  pure val NOGHOST: Set[str] = Set()

  // Every fault on, the code as it is at e39f40e.
  pure val IMPL: Cfg = {
    NQ: 1, NID: 2, NTOK: 5, MAXATT: 4, RECYCLE: false,
    INIT_POL: POL1, REDEPLOY_POLS: POLS1,
    MAX_CRASH: 1, MAX_REDEPLOY: 1, MAX_REVIVE: 1, MAX_CLOCKBACK: 1,
    ENV_SEND_AFTER: true, ENV_OLD_SENDS: true, ENV_REFUSE: true, ENV_TIMEOUT: true,
    ENV_WRITE_FAIL: true, ENV_PRUNE: true, ENV_OUTCOMES: Set("ok", "fail", "none"),
    FIX_S5: false, FIX_S6: false, FIX_S7: false, FIX_S7_GUARD: false,
    FIX_S10: false, FIX_L2: false, PRE_167: false, MAX_RACE: 1, ATOMIC_ANSWER: false,
    TRACK: ALLGHOSTS,
  }
  pure def allFixes(c: Cfg): Cfg = { ...c, FIX_S5: true, FIX_S6: true, FIX_S7: true,
                                     FIX_S7_GUARD: true, FIX_S10: true, FIX_L2: true }

  // ------------------------------------------------------------- S5
  // One message is enough (the property is per message; a second one only
  // takes room). Every fault on, crash/redeploy/revive/clock back at most
  // once each, the bury/lease race without bound.
  pure val S5_IMPL: Cfg = { ...IMPL, NID: 1, TRACK: NOGHOST, MAX_RACE: -1 }
  pure val S5_FIXED: Cfg = { ...S5_IMPL, FIX_S5: true }
  // F1 apart from the bury/lease clock race: no lease runs out between a
  // queue's bury_spent and its lease.
  pure val S5_F1: Cfg = { ...S5_IMPL, MAX_RACE: 0 }
  // The race needs no redeploy: M = 1 from the start, no fault but time.
  pure val S5_RACE: Cfg = { ...S5_IMPL,
    INIT_POL: { ...POL1, m: Map(0 -> 1) }, MAX_REDEPLOY: 0, MAX_CRASH: 0,
    MAX_REVIVE: 0, MAX_CLOCKBACK: 0, ENV_REFUSE: false, ENV_TIMEOUT: false,
    ENV_WRITE_FAIL: false, ENV_SEND_AFTER: false, ENV_OLD_SENDS: false }

  // ------------------------------------------------------ the rest
  // Crashes, redeploys, revives and clock set back without bound: for
  // safety that is more behaviour, and fewer states (no counters). Leases
  // running out inside the tick's bury..lease window stay bounded (1):
  // without a bound `attempts` grows for ever. One message id: two do not
  // finish within 20 minutes with every fault on (see README); the checks
  // that need two (S3, S9, S10) have them.
  pure val UNB: Cfg = { ...IMPL, NID: 1, MAX_CRASH: -1, MAX_REDEPLOY: -1, MAX_REVIVE: -1,
                        MAX_CLOCKBACK: -1, ENV_OLD_SENDS: false }
  // S2 and S4: S4 reads the deliveries' `valid` ghost.
  pure val STORE_IMPL: Cfg = { ...UNB, TRACK: Set("valid") }
  pure val STORE_FIXED: Cfg = allFixes(STORE_IMPL)
  // Two messages, every fault, for as far as TLC gets in 20 minutes.
  pure val STORE2_IMPL: Cfg = { ...STORE_IMPL, NID: 2 }

  // S1: acknowledged sends and allowed removals; sends from the previous
  // version's runs too.
  pure val S1_IMPL: Cfg = { ...UNB, ENV_OLD_SENDS: true, TRACK: Set("acked", "legit", "valid") }
  pure val S1_FIXED: Cfg = allFixes(S1_IMPL)
  // S3: the ids ever given; two ids, so that one may be sent after the
  // other is revived.
  pure val S3_IMPL: Cfg = { ...UNB, NID: 2, TRACK: Set("used") }
  pure val S3_FIXED: Cfg = allFixes(S3_IMPL)
  // S6: lives are counted, so revives are bounded.
  pure val S6_IMPL: Cfg = { ...UNB, MAX_REVIVE: 1, TRACK: Set("life", "net", "invoc", "gap") }
  pure val S6_NOWF: Cfg = { ...S6_IMPL, ENV_WRITE_FAIL: false }
  pure val S6_FIX: Cfg = { ...S6_IMPL, FIX_S6: true }
  pure val S6_FIXED: Cfg = allFixes(S6_IMPL)
  pure val S6_FIXED_NORACE: Cfg = { ...S6_FIXED, MAX_RACE: 0 }
  // S7
  pure val S7_IMPL: Cfg = { ...UNB, TRACK: Set("answered", "ranOut", "mmax", "valid") }
  pure val S7_NOTO: Cfg = { ...S7_IMPL, ENV_TIMEOUT: false }
  pure val S7_F3: Cfg = { ...S7_NOTO, ATOMIC_ANSWER: true }
  pure val S7_FIX_F3: Cfg = { ...S7_NOTO, FIX_S7: true }
  pure val S7_FIX_F3_ATOMIC: Cfg = { ...S7_FIX_F3, ATOMIC_ANSWER: true }
  pure val S7_FIX_F3G: Cfg = { ...S7_NOTO, FIX_S7: true, FIX_S7_GUARD: true }
  pure val S7_FIXED: Cfg = allFixes(S7_IMPL)
  // S8
  pure val S8_IMPL: Cfg = { ...UNB, TRACK: Set("early") }
  pure val S8_FIXED: Cfg = allFixes(S8_IMPL)
  // S10: two messages, D in {1, 2}; sends from the previous version's runs.
  pure val S10_IMPL: Cfg = { ...UNB, NID: 2, ENV_OLD_SENDS: true, TRACK: NOGHOST }
  pure val S10_NOREVIVE: Cfg = { ...S10_IMPL, MAX_REVIVE: 0 }
  pure val S10_FIX: Cfg = { ...S10_IMPL, FIX_S10: true }
  pure val S10_FIX_NOOLD: Cfg = { ...S10_FIX, ENV_OLD_SENDS: false }
  // L3a
  pure val L3A_IMPL: Cfg = { ...UNB, TRACK: Set("broken", "invAll") }
  pure val L3A_FIXED: Cfg = allFixes(L3A_IMPL)
  // S9: two queues, two messages.
  pure val S9_IMPL: Cfg = { ...UNB, NQ: 2, NID: 2, INIT_POL: POL2, REDEPLOY_POLS: POLS2, TRACK: NOGHOST }
  pure val S9_FIXED: Cfg = allFixes(S9_IMPL)
  // Sanity: the delivery loop before #167 (lease took every queue).
  pure val S9_PRE167: Cfg = { ...S9_IMPL, PRE_167: true }

  // ------------------------------------------------------ liveness
  // E6: every fault at most once. Sends without bound: a message id that
  // has left both tables may be sent again (RECYCLE). One id per queue, so
  // that `lease`'s oldest-first order, which the model takes from the id,
  // is never wrong for a recycled id.
  // Every fault at most once does not finish within 20 minutes (6.8 M states
  // and growing after 20 minutes), so liveness keeps crash and redeploy (at most once each),
  // admission refused, scheduler timeouts, failing writes and prune, and
  // leaves out revive, clock set back, the bury..lease race and sendAfter.
  pure val LIVE: Cfg = { ...IMPL, NID: 1, NTOK: 6, RECYCLE: true, TRACK: NOGHOST,
                         MAX_CRASH: 1, MAX_REDEPLOY: 1, MAX_REVIVE: 0, MAX_CLOCKBACK: 0,
                         MAX_RACE: 0, ENV_SEND_AFTER: false, ENV_OLD_SENDS: false }
  pure val LIVE_FIXED: Cfg = allFixes(LIVE)
  pure val LIVE_FIX_S6: Cfg = { ...LIVE, FIX_S6: true }
  pure val LIVE_L3B: Cfg = { ...LIVE, TRACK: Set("invAll") }
  pure val LIVE_L3B_FIXED: Cfg = allFixes(LIVE_L3B)
  pure val LIVE_L3B_FIXED_NOCRASH: Cfg = { ...LIVE_L3B_FIXED, MAX_CRASH: 0 }
  // Two queues, id q+1 for queue q. With LIVE's faults the fixed model's
  // check does not finish in 20 minutes, so two queues have no crash, no
  // redeploy and no failing write; tokens 3 per id.
  pure val LIVE2: Cfg = { ...LIVE, NQ: 2, NID: 2, NTOK: 3, INIT_POL: POL2, REDEPLOY_POLS: POLS2,
                          MAX_CRASH: 0, MAX_REDEPLOY: 0, ENV_WRITE_FAIL: false }
  pure val LIVE2_FIX_L2: Cfg = { ...LIVE2, FIX_L2: true }
  pure val LIVE2_FIXED: Cfg = allFixes(LIVE2)
}
