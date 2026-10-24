// -*- mode: Bluespec; -*-
//
// The checks: one module per configuration of `queue.qnt`. Each is run
// with `quint verify --backend=tlc --main=<module> --invariant=... checks.qnt`
// (the npm scripts in package.json).
module base {
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

  // Every fault on, the code as it is at e39f40e.
  pure val IMPL: Cfg = {
    NQ: 1, NID: 2, NTOK: 3, MAXATT: 4, RECYCLE: false,
    INIT_POL: POL1, REDEPLOY_POLS: POLS1,
    MAX_CRASH: 1, MAX_REDEPLOY: 1, MAX_REVIVE: 1, MAX_CLOCKBACK: 1,
    ENV_SEND_AFTER: true, ENV_OLD_SENDS: true, ENV_REFUSE: true, ENV_TIMEOUT: true,
    ENV_WRITE_FAIL: true, ENV_PRUNE: true, ENV_OUTCOMES: Set("ok", "fail", "none"),
    FIX_S5: false, FIX_S6: false, FIX_S7: false, FIX_S7_GUARD: false,
    FIX_S10: false, FIX_L2: false, PRE_167: false, ONE_NOW: false,
    TRACK: Set("life", "net", "invoc", "gap", "invAll", "broken", "ranOut", "answered",
               "early", "acked", "legit", "used", "mmax"),
  }
  pure val NOGHOST: Set[str] = Set()
  pure val FIXED: Cfg = { ...IMPL, FIX_S5: true, FIX_S6: true, FIX_S7: true,
                          FIX_S7_GUARD: true, FIX_S10: true, FIX_L2: true }
}

// S5: one message is enough (the property is per message; a second one
// only takes room). Every fault on.
module s5_impl {
  import base.* from "./checks"
  import queue(C = { ...IMPL, NID: 1, NTOK: 5, TRACK: NOGHOST }).* from "./queue"
}
module s5_fixed {
  import base.* from "./checks"
  import queue(C = { ...IMPL, NID: 1, NTOK: 5, TRACK: NOGHOST, FIX_S5: true }).* from "./queue"
}
// F1 apart from the bury/lease clock race: no time passes between a
// queue's bury_spent and its lease.
// The bury/lease clock race needs no redeploy: M = 1 from the start, no
// redeploy, no other fault but time.
module s5_race_impl {
  import base.* from "./checks"
  import queue(C = { ...IMPL, NID: 1, NTOK: 5, TRACK: NOGHOST,
                     INIT_POL: { ...POL1, m: Map(0 -> 1) }, MAX_REDEPLOY: 0, MAX_CRASH: 0,
                     MAX_REVIVE: 0, MAX_CLOCKBACK: 0, ENV_REFUSE: false, ENV_TIMEOUT: false,
                     ENV_WRITE_FAIL: false, ENV_SEND_AFTER: false, ENV_OLD_SENDS: false }).* from "./queue"
}
module s5_f1_impl {
  import base.* from "./checks"
  import queue(C = { ...IMPL, NID: 1, NTOK: 5, TRACK: NOGHOST, ONE_NOW: true }).* from "./queue"
}
