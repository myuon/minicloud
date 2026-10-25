# A model of the queue

A Quint model of minicloud's queue — the store (`crates/minicloud/src/queue.rs`)
and the delivery loop (`queue_host.rs`) as they are at `e39f40e` — checked
with TLC against the contract in [`docs/queue-properties.md`](../../docs/queue-properties.md).

- `queue.qnt` — the model. One transition relation; each proposed fix is a
  flag (`FIX_*`) so it can be checked on its own; `ENV_*` and `MAX_*` turn
  the environment's faults on and bound them.
- `configs.qnt` — one `Cfg` per check: bounds, environment, flags, and the
  ghost variables it tracks.
- `tools/check.sh <name> <CONFIG> <quint verify options>` — runs one check:
  writes `out/<name>.qnt` (the model instantiated with that config), runs
  TLC, keeps the whole output in `out/<name>.txt` and a step-by-step summary
  of a counterexample in `out/<name>.trace` (`tools/trace.py`).
- `tools/safety.sh`, `tools/liveness.sh` — every check of the table, in
  turn (`tools/safety.sh 's7_'` runs only the matching ones);
  `tools/show.sh <name>...` prints verdicts and traces.
- `tools/impl2.sh` — round 2: every check on the fixed code (`IMPL2`);
  `tools/reach.sh` — its reachability sanity; `tools/sanity.sh` — the
  backend sanity checks.
- `tools/traces.sh` — the traces the Rust replay drives the delivery loop
  with (below, "Traces for the implementation"); `tools/itf-pack.py`
  packs each one.
- `sanity/toy.qnt` — the backend's sanity checks.

## Running

```sh
cd spec/queue
npm ci
npm run setup        # once: makes Quint download Apalache, whose jar carries TLC
npm run sanity       # the backend's sanity checks
npm run s5           # S5's checks; npm run <property> for the others
npm run safety       # all safety checks (hours)
npm run liveness     # all liveness checks (about an hour)
npm run impl2        # round 2: every check on the fixed code (about two hours)
npm run reach        # round 2: reachability sanity
npm run traces       # the corpus the Rust replay uses (about a minute)
```

Java 21 is needed. TLC is the one inside the Apalache 0.62.1 jar Quint
downloads to `~/.quint/apalache-dist-0.62.1/apalache/lib/apalache.jar`
(sha256 `880c0b2b72354816f12e9a9755829f2907071e9a181ffea1937954c23d54739d`
here). Quint runs it as `tlc2.TLC -deadlock -workers <n>` on a generated
`.cfg` with `INIT`, `NEXT`, `INVARIANT` and `PROPERTY` only.

- `tlc-bfs1.json` runs TLC with **one worker**: breadth-first with one
  worker reports a shortest counterexample, with several it may not (with
  all workers the first S5 run reported a trace one step longer than the
  shortest). Every counterexample length below is from a one-worker run,
  except liveness lassos, which TLC does not minimise.
- `tlc-all.json` uses every core, for searches expected to pass.

Lengths are in **steps** (transitions; the trace has one more state).
Every check also checks `NoOverflow` — a bound of the model (attempts past
`MAXATT`, the token pool `NTOK` exhausted) was hit; a pass with it means
those bounds did not cut the search. A check that does not end within
20 minutes is stopped and reported as **not complete**.

All results below are TLC on the model; none is from Apalache or `quint
run`.

## Traces for the implementation (model-based testing)

`npm run traces` (`tools/traces.sh`) has the simulator draw traces from
the model of the code and writes them, gzipped, to
`crates/minicloud/tests/queue_traces/`, which is committed: the Rust test
`queue_host::mbt` replays each against the real delivery loop under the
loop harness and compares the states after every step, so CI needs
neither Node nor Quint. The seed is fixed and the simulator runs on one
thread, and `tools/itf-pack.py` drops the time of making from each trace,
so the same model gives the same bytes.

- **Configurations** (`configs.qnt`): `MBT_MIN`, IMPL2 with two queues (a
  with batch 2, b with batch 1; M = 2, D = 2 each), four message ids, at
  most two crashes, admission refused, failing settle writes, answers
  2xx–4xx / 5xx / none; `MBT_ONE`, the same with one queue and one id.
  `MBT_FULL` (and `MBT_FULL_ONE`) adds at most two redeploys (either
  queue removed or kept, its M and D 1 or 2), two revives, two clock set
  backs and one host stall, `sendAfter` and scheduler timeouts.
- **Actions** (`mbtStep`): send and `sendAfter`, a send the depth refuses,
  the tick (`bury_undeclared`, `bury_spent`, room and `lease`, the flush
  of held settles), admission accepted or refused, the run's start, its
  answer, the scheduler's 503, the settle's write (ok or failing), a
  crash, time passing (a lease running out under a live delivery is the
  stall), the clock set back, a redeploy, a revive and a revive the depth
  refuses. Not prune (the admin prunes by age, not one id), cancelled runs
  (unreachable for a delivery), nor sends from a previous version (not in
  the code any more).
- **Corpus**: 200 runs of 100 steps on `MBT_MIN` (`min_NNN`), 150 on
  `MBT_FULL` (`full_NNN`), and one witness for each reachability
  invariant (`witness_*`, below): 360 files, about 1.8 MB.

### What the model needed for it

All behind the `MBT` flag or a ghost a configuration tracks, so that
every other check sees the model unchanged, except the last item, which
corrects IMPL2.

1. **Time in delays** (`MBT`). The model's time was a flag per message
   (due or not, in any order), which real due times cannot always follow:
   two leases of the same length taken one after the other run out in
   that order. With `MBT`, every lease and retry is one unit of time
   (`DELAY_MS` in the replay: `lease` = `retry`, and M = 2 keeps the retry
   from doubling); `clk` is the time and a message not due has a `dueAt`;
   `timePasses(i)` moves `clk` to `i`'s `dueAt` and makes every message
   due at or before it due as well, which is what moving a clock does. A
   held settle remembers when it was decided (`Del.at`), because its write
   is as of then: a flushed retry may be due at once.
2. **The tick in one go** (`mbtStep`). The code's tick is one call: it
   flushes, buries and leases at one reading of the clock and admits as
   it goes. The model's tick is several steps; `mbtStep` lets nothing else
   happen between them once a tick has begun, and draws each nondet only
   from where its action is enabled (so a trace does not end early on a
   pick that leads nowhere). `step` and every TLC check are as before.
3. **Counters** (ghost `counts`, tracked as `"counts"`): delivered,
   retried and buried, moved only by a write that took and set back to
   nothing by a crash. The contract says "counters and the log move only
   when a write takes"; without them in the model a code that counted a
   failed write could not be told apart.
4. **Witness invariants** `NeverHeldPastLease` (F3's setting on the code:
   a held settle whose lease has run out) and `NeverAllQueuesBusy` (F6's:
   both queues with a delivery in flight).
5. **Clock set back, oldest first** (`MBT`). `clockBackTo(t)` sets `clk`
   back to `t`, and a message is due again only if its `dueAt` is at or
   before `t`; so a sent or revived message has `dueAt` too (when it was
   sent), and `sendAfter` is one unit. `lease` takes the oldest first by
   the time sent and then id (`older`), as the code's `ORDER BY sent, id`
   does: with the clock set back a later id can be older. Without `MBT`
   it is by id, as before.
6. **Refusals in the trace** (`sendRefused`, `reviveRefused`): a send or
   revive the queue's depth refuses changes nothing, so the model had no
   step for it and the replay never asked the code. Each is a step that
   changes nothing, so that the code is asked and must refuse too.
7. **IMPL2's in-flight count comes down at the settle** (a fix of the
   model, found by the replay). The model lowered `inflight` when the run
   ended; the code lowers it in `Deliveries::settle`, when the delivery's
   task takes the reply. Between the two a tick has less room in the code
   than in the model. With `IMPL2` the model now lowers it in `settle`.

`tools/check.sh` also takes `TLC_TMP`, a directory for TLC's states off
the checkout.

**The checks after these changes.** With 1–4 only, every IMPL2 safety
check that finished before ran to the same state count as in the table
below (`i2_safe` 34,412; `i2_s5` 34,330; `i2_s3` 85,283; `i2_s6` 48,738;
`i2_s7` 47,452; `i2_s9` 1,151,209; `i2_s10` 3,052,952; `i2_l3a` 49,162;
with a stall `i2_safe` 550,870, `i2_s5` 579,960, `i2_s3` 863,788) and the
same counterexamples (`i2_s10u` 9 steps, `i2_s6_stall` and `i2_s7_stall`
11). With 7 as well, `npm run sanity` and `npm run reach` end as before
(every check `ok`), and every IMPL2 check of `tools/impl2.sh` but S10 with
a stall on B10 (which did not finish before either) was run again; the
table below has the new counts. Every verdict is the one before, but one:
**S6.3 with a stall now holds on B1**, where it failed in 11 steps. Its
counterexample had the next tick lease the message again while the 503's
settle was still to be written, which batch 1 leaves no room for once the
in-flight count waits for the settle. With batch 2 (`I2_S6_STALL_B2`) it
fails as before, in 11 steps: the residual (N5 under a stall) stands. The
safety searches are smaller (fewer interleavings of a run's end and its
settle with a tick); the liveness ones are as before but L1 with a stall
(33,149 states, was 72,140). 5 and 6 are behind `MBT`; the IMPL2 safety
checks on B1/B2/B2q/B10 (with and without a stall), L1, L2 on two queues
and L3b were run once more after them, to the same counts.

### The replay

`crates/minicloud/src/queue_host/mbt.rs`. It parses the ITF itself
(`serde_json`, `#bigint`/`#set`/`#map`), drives each step through the
harness from `mbt::actionTaken` and `mbt::nondetPicks`, and compares, after
every step, a state read only from the implementation with one read only
from the trace; the comparison is of states, not of either side's rules.

- **Compared**: the clock; each message on the queue (queue, attempts,
  due or the time it is due at, the lease it holds by the model's token);
  each dead letter (queue, attempts, why: `fail`, `outlived`, `lowered`,
  `undeclared`); the in-flight count per queue; each delivery (queue,
  attempts, M, leased / queued / running / answered, started) and each
  held settle (queue, attempts, the op and when it was decided); the
  delivered / retried / buried counters; per message the invocations,
  leases less written give-backs and unstarted deliveries lost to a crash
  (the harness's ghost facts against the model's `invoc`, `net`, `gap`).
- **Names**: model ids and (id, token) are bound to the store's ids and
  lease ids as they appear. An id or lease id the store gives twice fails
  the replay (S3, S4). A delivery is driven by its lease, since after a
  stall a message has two.
- **Lives**: the model's ghosts start again at a revive; the harness's
  facts are per message, so the replay counts them from the revive on
  (by the leases issued since, and the runs and losses of those leases).
- **Time**: "message m becomes due" moves the clock to m's due time in the
  store; that the model has m not due before, and that every other
  message is due or not as the model says after, is checked.
- **The tick** is replayed as one call. The model's picks inside it are
  set up first (which held settle's write fails, by its lease, which
  admission is refused and whether its give-back is written); after it, the writes the
  model made must all have been tried, the admissions asked in the model's
  order, and the leases issued exactly the model's.
- Not compared: states inside a tick (the code has none to show), a
  delivery's answer before its settle (inside the run's channel), the
  `valid` ghost, the M of a held settle.

**What the replay found.** Besides the in-flight count (7 above, the
model's), the harness's ghost facts did not count a give-back held over a
failed write as an unstarted delivery lost when the process crashes,
which the model (and S6.3) does; `Harness::crash` now does.

**Traces from TLC.** Quint 0.33's TLC backend writes no ITF (`quint verify
--backend=tlc --out-itf` leaves no file), and its traces carry no action
names. The reachability invariants of `npm run reach` that the slice can
break were checked again on the MBT configurations with TLC, for their
shortest witnesses, and the simulator drew a witness of each, checked
only between ticks (TLC's length; the corpus's): on `MBT_ONE`
`NeverHeld` (5; 7), `NeverHeldPastLease` (6; 11), `NeverDone` (8; 13),
`NeverSecondAttempt` (10; 18), `NeverFail` (14; 24), `NeverOutlived` (15;
24); on `MBT_MIN` `NeverAllQueuesBusy` (9; 16); on `MBT_FULL_ONE`
`NeverLowered` (12; 25), `NeverUndeclared` (3; 5), `NeverStall` (6; 9).
The round-1 counterexamples are on the code before the fixes, and not
traces of this model; their settings on the fixed code are what the
witnesses are: F3 on `done` and `give_back` (`NeverHeld*`), F1 (`NeverLowered`),
F7 (`NeverUndeclared`), F6 (`NeverAllQueuesBusy`), N5 and N3 (`NeverStall`).

## Round 2: the code on `fix/queue-contract`

The fixes of `git log origin/qa/queue-properties..origin/fix/queue-contract`
are modelled by setting every `FIX_*` flag and `IMPL2` (configs:
`impl2(c, stalls)` in `configs.qnt`; checks: `tools/impl2.sh`, log in
`out/impl2.log`). Where the implementation differs from the fix proposed
in round 1, the model follows the implementation:

| implemented (commit) | in the model |
|---|---|
| `lease` takes M, leases only `attempts < M` (`818e6fc`) | `FIX_S5` |
| `bury_spent` buries due `attempts >= M` whatever its lease; reason by whether a lease is held (`818e6fc`) | `FIX_S5` ("outlived" / "lowered") |
| one clock reading per tick (`818e6fc`, N2) | `IMPL2`: no time passes from the tick's reading (after its flush) to its end; time that passes meanwhile is seen by the next tick. Sound: only the tick reads `due` |
| "current M" is the tick's version (D4) | as in round 1 (`S5`, not `S5strict`) |
| settles name a token (`1e0d0fd`, N1) | settles were already by token |
| replies carry an origin; never-started → give back; host reply after start → no answer (`b505fac`) | `FIX_S6`; outcome "none" |
| failed settles held in memory, flushed first at the next tick; `bury_spent`, `bury_undeclared` and **`lease`** skip a message whose current lease has a held settle; a crash loses them (`2d99362`) | `FIX_S7` + `FIX_S7_GUARD`; with `IMPL2` the guard is the *held* settle only (not the window between the run's reply and the lock), and `lease` skips held messages too |
| `revive` refuses at the current D (`15bbd3b`) | `FIX_S10`; with `IMPL2` **not for a queue the current version does not declare**, which the code lets through |
| in flight per queue (`db61025`) | `FIX_L2` |
| `send`/`sendAfter`/`depth` use the current version; undeclared → `Err` (`2537946`, N4) | no sends from a previous version (`ENV_OLD_SENDS` off) |
| D1: `lease ≥ deadline + 1 s` (`70c2dea`) | `IMPL2`: a lease runs out while the delivery holding it is alive (leased, queued, running, or answered and not yet holding its settle) **only as a host stall**, a bounded fault (`MAX_STALL`). A held settle's lease and a crashed delivery's lease run out freely |
| due times in ms (`09e0122`, F4) | out of the model's scope, as before |
| D3: `synchronous = FULL` (`eb238f8`, F8) | power loss not modelled, as before |
| D6: messages of a queue the tick's version does not declare are buried at the tick, unless their lease still runs or their settle is held (`e7b4d6a`) | `IMPL2`: `tickUndecl`, after the flush, before the first queue; reason "undeclared". E1 no longer asks for the queue to be declared (`L1_fixed`, `L2_fixed`, `L3b_fixed`) |
| shutdown writes nothing (`815f400`, N6) | a crash, as before (the queue cannot tell them apart) |
| E7 in the contract | `E7` in `L1_fixed` and `L3b_fixed` |

The bury..lease race (`MAX_RACE`) and sends from a previous version cannot
happen in the code any more and are off. Bounds are the first round's
(B1, B2, B2q, B10, L, L2q), each run with no host stall and with at most
one. Searches may take 60 minutes. Nothing was shrunk to make a search
finish: every search of round 1 that did not finish now finishes at the
same bounds, because the fixed code has far fewer reachable states (no
lease runs out under a live delivery without a stall, one clock reading
per tick). No symmetry or step merging was added.

**Sanity for round 2.** `tools/reach.sh`: each "never" invariant
(`NeverFail`, `NeverOutlived`, `NeverLowered`, `NeverUndeclared`,
`NeverHeld`, `NeverSecondAttempt`, `NeverDone`, `NeverStall`) fails on
`I2_SAFE_STALL`, so every kind of burial, a held settle, a second attempt,
a `done` and a stall are reachable and the passes below are not vacuous.
The fairness constructs are the round-1 ones (`weakFair`, `strongFair`);
`i2_l1_nofair` fails (stutters) where `i2_l1` holds, and `i2_l1_noE7`
fails where `i2_l1` holds, so E7 changes the verdict as well.

### Results on the implementation

"Excl.?" says whether a counterexample falls within the contract's stated
exclusions (crash/shutdown, host stall, power loss) or is a gap.

| prop. | assumptions | bounds | complete? | result | cex length | excl.? |
|---|---|---|---|---|---|---|
| S1, S2, S4, S8 | crash only (no power loss) | B1 | yes: 34,412 states, 10 s | holds | — | — |
| ″ | + host stall | B1 | yes: 251,778, 17 s | holds | — | — |
| ″ | | B1, 2 ids | yes: 2,994,112, 160 s | holds | — | — |
| S3 | | B2 | yes: 57,386 / with stall 291,637 | holds | — | — |
| S5 | M of the tick's version (D4) | B1 | yes: 34,330 / with stall 270,604; 2 ids 2,896,166 | holds | — | — |
| S6 (1)(2)(3) | | B1 | yes: 48,738; 2 ids 9,702,820 (292 s) | holds | — | — |
| ″ | + host stall | B1 | yes: 324,722 | holds (batch 1: no room to lease again while the 503's settle is pending) | — | — |
| ″ | + host stall, batch 2 | B1 | yes (cex) | (3) **violated** | 11 | **yes**: host stall (N5) |
| S7 | an answer lost in a crash is none | B1 | yes: 47,452; 2 ids 5,852,001 (149 s) | holds | — | — |
| ″ | + host stall | B1 | yes (cex) | **violated** ("outlived" though the app answered) | 11 | **yes**: host stall (N3 residual) |
| S9 | one app | B2q | yes: 968,149 / with stall 5,261,443 (560 s) | holds | — | — |
| S10 | declared queues | B10 | yes: 2,896,166, 90 s | holds | — | — |
| ″ | + host stall | B10 | **no**: 66.5 M states at 33 min, stopped (disk nearly full) | nothing found | — | — |
| ″ | ± host stall | B2 | yes: 57,386 / 291,637 | holds | — | — |
| S10, undeclared queue | `revive` into a queue the current version does not declare | B10 | yes (cex) | the queue grows past the D its old declaration had | 9 | **departure**: the code allows it; the contract has no D for an undeclared queue (and D6 buries it again at the next tick) |
| L3a | | B1 | yes: 49,162 / with stall 427,192; 2 ids 6,738,428 (183 s) | holds | — | — |
| L1 | E1 (app enabled), E2–E5, E7, E6 as bounds | L | yes: 8,835 states, 57 s | holds | — | — |
| ″ | + host stall | L | yes: 33,149, 216 s | holds | — | — |
| ″ | | L2q | yes: 2,038 / with stall 1,171 s | holds | — | — |
| ″ | every fault once (+ revive, clock back, sendAfter) | L | yes: 35,226 states, 249 s | holds | — | — |
| ″ | without E7 | L | yes (cex) | **violated**: 503 → give back → lease → 503 for ever | lasso | outside the assumptions (E7) |
| ″ | without fairness | L | yes (cex) | **violated** (stutter) | lasso | sanity |
| L2 | E1–E6 | L; L2q | yes: L ± stall (236 s), L2q ± stall (122 s) | holds | — | — |
| ″ | every fault once | L | yes: 35,226, 69 s | holds | — | — |
| L3b | E1–E7; redeploys that keep the queue; no crash | L | yes: 2,189 | holds | — | — |
| ″ | + host stall | L | yes (cex) | **violated**: lease runs out while the last attempt's run waits in the scheduler | lasso | **yes**: host stall |
| ″ | + a crash | L | yes (cex) | **violated**: crash after `lease` of the last attempt, before the start | lasso | **yes**: the contract's crash exclusion |
| ″ | + a redeploy that removes the queue, no crash | L | yes (cex) | **violated**: D6 buries the message "undeclared", never invoked | lasso | **gap in the statement**: L3b's exclusions do not name it (see below) |
| ″ | every fault once, keep, no crash | L | yes: 8,693, 16 s | holds | — | — |

Traces (all one-worker BFS, shortest; `tools/show.sh <name>`):

- **S6.3 with a stall** (`i2_s6_stall_b2_bfs1`, batch 2, 11 steps): lease (attempts 1),
  admit; the scheduler answers 503 (the run never started); **the host
  stalls** and the lease runs out before the delivery takes the lock;
  the next tick leases the message again (attempts 2); the 503's
  `give_back` names the old token and matches nothing. Attempts 2,
  invocations 0, no crash: N5, closed by D1 only without a stall.
- **S7 with a stall** (`i2_s7_stall_bfs1`, 11 steps): M lowered to 1;
  lease, admit, the app starts and answers 2xx; **the host stalls** before
  the delivery takes the lock, the lease runs out, the tick's `bury_spent`
  buries it "no answer within the lease". This is the N3 window the
  contract leaves open; it needs the stall.
- **S10, undeclared** (`i2_s10u_bfs1`, 9 steps): message 1 leased and
  admitted, message 2 sent (depth 2 under D = 2); a redeploy removes the
  queue (its last D was 1 in the version map); the tick buries message 2
  "undeclared" (message 1's lease still runs, so it stays); the admin
  revives message 2: the queue holds 2. The code lets a revive into an
  undeclared queue through; the next tick buries it again.
- **L3b, queue removed** (`i2_l3b_nocrash`): send; a redeploy removes the
  queue; the tick buries the message "undeclared"; it is never invoked.
  L3b's statement (E1–E7, no crash or shutdown on the last attempt
  before the start) does not exclude it now that E1 no longer asks for
  the queue to stay declared. A **gap in the contract's statement**, not
  in the code: L3b needs "the queue stays declared" (or a revive after
  it is declared again).

### The known residuals

- **N3 is closed except under a host stall: confirmed.** S7 holds on B1
  and on two ids without a stall; the only S7 counterexample with a stall
  is N3's window (11 steps).
- **L3b is not promised when a crash spends the last attempt before the
  run starts: confirmed** — that is the crash counterexample. Without a
  crash (and with the queue kept) L3b holds; with a stall it fails the
  same way as a crash would (lease runs out before the start), which the
  coordinator lists as an exclusion; the contract's L3b text names only
  crash and shutdown and should name the stall too.

### Bounded or unfinished in round 2

- S10 with a host stall on B10 (two ids, every fault): stopped at 66.5 M
  states after 33 minutes because the disk was nearly full; nothing found.
  The same property with a stall on B2 is complete and holds.
- Everything else in the table is complete at its bounds. The bounds
  themselves (one or two message ids, one or two queues, M ≤ 2, each fault
  at most once) are the limit of what is checked; the larger behaviours
  are not covered.

# Round 1: the code at `e39f40e`

Everything below is the first round, kept as it was found.

## Backend sanity checks

`npm run sanity` (`tools/sanity.sh`) runs all of these and says `ok` for
each that ends as expected; all eight do. `sanity/toy.qnt` is a counter
that may climb from 0 to 3 or stand still.

| script | property | expected | got |
|---|---|---|---|
| `sanity:inv-false` | invariant `x <= 2` | violated | violated, 3 steps |
| `sanity:inv-true` | invariant `x <= 3` | holds | holds, 4 states |
| `sanity:live-nofair` | `eventually(x == 3)` | violated | violated: stutters at `x = 2` |
| `sanity:live-fair` | `weakFair(inc, x) implies eventually(x == 3)` | holds | holds |
| `sanity:live-false` | `weakFair(inc, x) implies eventually(x == 4)` | violated | violated: stutters at `x = 3` |

On the queue model itself:

- **A planted false invariant is caught:** `PRE_167` makes `lease` take
  every queue (the loop before #167); S9 then fails in 3 steps
  (`s9_pre167`), where it holds without it.
- **Fairness changes the verdict on the real model:** L1 with E1 but no
  fairness fails (`l1_nofair`: stutters with a message on the queue); the
  same property under E2–E4 holds (`l1_impl`).

Quint compiles `weakFair(A, v)` to `WF_v(A)` and `strongFair` to `SF_v`;
the TLC backend cannot put fairness in a `SPECIFICATION`, so every liveness
check is a `PROPERTY` of the form `fairness and assumptions implies
property`, which TLC checks soundly. TLC reports these as "1 branch" (or 2
with E1), so the number of fairness conjuncts does not blow up; what costs
is strong fairness on large graphs (the slowest liveness checks below).
No translation to TLA+ by hand and no other TLC was needed.

## Modelling decisions

Places where the doc does not settle what to model, and what the model does.

1. **Time is abstract.** A message is due or not. `timePasses(i)` makes one
   message due, in any order: the model does not keep the order real due
   times would come in (a lease of 30 s taken after a retry of 10 s still
   may run out first). That over-approximates, so every counterexample
   below was read for whether real times can produce it. All can, with one
   caveat: TLC's F1 trace lets a whole retry delay pass between one tick's
   `bury_spent` and its `lease`, which only a tick stalled that long (on the
   store's mutex, say) would see; the F1 replay below does not need it. Retry
   backoff doubling, the lease length and their change on redeploy have no
   effect in this abstraction. `clockBack(s)` makes any set of due messages
   not due. F4 (whole-second truncation) is arithmetic and is left to Rust
   tests; S8 is checked only in the abstraction, where it holds trivially.
2. **Tokens** are the code's fresh `next_lease_id` up to renaming: a lease
   takes the least token no live reference to that message has (its own
   column, any delivery's). S4.3 is checked through a ghost `valid` per
   delivery, set and cleared by the events the contract names (new lease,
   `bury_spent`, retry/give_back, leaving), not by comparing tokens.
3. **The tick reads the app's version once**, when it begins, and uses it
   for every queue (the code's `slot.current()` before the queue loop). A
   delivery's policy is that version's. "The current `M`" in S5 is taken to
   be the tick's version; the strict reading is `S5strict`.
4. **Granularity of the tick.** Separate steps: `bury_spent(q)`; room +
   `lease(q)` (one step: `in_flight` only rises on the tick's own task, so
   reading it early only makes room smaller); per leased message, `in_flight
   += 1` + admission (one step) or admission refused + `give_back` (one
   step: only the tick leases or buries, so nothing else can move the
   message's token in between). Each `now()` is its own: time may pass
   between `bury_spent` and `lease` of one queue (that is the S5 race).
5. **The run's end and `in_flight -= 1` are one step**; the settle's write is
   the next. A crash between them is the same as before the answer.
6. **Cancelled runs (499) are not modelled.** A queue delivery's `Flight`
   gets `Cancel::new()`, and nothing ever cancels it (only `server.rs`'s
   `CancelOnDrop` does, for connections). Note that if one ever were
   cancelled, 499 < 500 counts as *taken*: `done` would delete a message
   the app never saw, against S1.
7. **Store failures** are modelled for the settles only (`done`, `retry`,
   `bury`, `give_back`): each may fail, and the code ignores the failure.
   A failing `send`, `lease`, `bury_spent` or `revive` is a rolled-back
   transaction that changes nothing.
8. **Sends from the previous version.** A run of the old version may still
   be running after a redeploy, and its `queue.send` checks the old
   version's `[[queues]]` (D, and whether the queue is declared:
   `QueueHost.queues` is per version). Modelled in S1 and S10 checks.
9. **Oldest first** is taken as id order (send order). A clock set back
   between two sends can make the later one older by `sent`; with batch 1
   and one message per queue in most checks this does not matter.
10. **S6 per life.** Invocations are counted for the life of the lease the
    delivery holds; a delivery leased before a burial that starts after a
    revive is an invocation of the old life.
11. **S7 and crashes.** An app's answer that a crash took away before the
    settle was written counts as no answer: the strict reading fails on a
    crash between the answer and the write, which nothing can avoid without
    persisting answers.
12. **S7 fix.** The doc's fix (keep a failed settle, try it again at the
    next tick: `FIX_S7`) is not enough on its own; `FIX_S7_GUARD` adds
    what closes it: `bury_spent` skips a message whose current lease holder
    has an answer in memory (answered, write pending or failed). The guard
    needs `FIX_S5` beside it: alone, `lease` still takes the message the
    guard kept from `bury_spent`, past `M`, and `attempts` grows without
    bound (`s7_fix_f3g_small` reports the bound overflow and S5).
13. **The bury/lease race is a bounded fault** (`MAX_RACE`, 1 in most
    checks): without a bound, `attempts` grows without limit in the code as
    it is, and the state space is infinite.
14. **Faults at most once.** Unbounded crashes, redeploys, revives and
    clock set back keep the state space finite and were tried first; they
    do not finish within 20 minutes, so every check bounds each fault to
    one (`MAX_* = 1`). The code supports `-1` (no bound) for anyone with
    more time.
15. **Liveness.** Sends are unbounded: an id that is on neither table and
    has no delivery left may be sent again (`RECYCLE`). With two queues,
    id q+1 belongs to queue q: with ids shared, a recycled smaller id would
    jump ahead of an older message in the id-ordered `lease`, a starvation
    the code cannot have (TLC found exactly that before the change).
    Fairness: E2 is weak fairness on the tick and **strong** fairness on
    each message's clock (the race bound disables time inside a window, so
    it is enabled infinitely often, not continuously); E3 weak fairness per
    delivery (id, token) on its run's end and its settle (per id was tried
    first and let a stale delivery wait for ever while another moved); E4
    strong fairness per message on admission; E5 strong per delivery on a
    retried settle being written (only with `FIX_S7`; vacuous otherwise).
    E6 is the bounds. E7 (not in the doc): strong fairness per message on
    the app's entry starting — a message admitted infinitely often is
    started infinitely often.
16. **Not modelled:** power loss (S1's second promise, F8); several apps
    (S9's "delivered only to that app" rests on one store file per app);
    an app disabled or not loaded (E1); `max_body_bytes`; the admin's
    reads.

## Bounds

| name | what |
|---|---|
| **B1** (`ONE`) | one queue, one message id; M ∈ {1, 2} (starts at 2), D ∈ {1, 2}, batch 1; at most one each of: crash, redeploy (to any of the 8 versions: M, D, or the queue removed), revive, clock set back, a lease running out inside the tick's `bury_spent`..`lease` window; admission refused, scheduler timeout (503), app answers 2xx–4xx / 5xx / none, failing settle writes, `sendAfter`, prune; 5 tokens per id, attempts ≤ 4 |
| **B1-S5** | B1 with the `bury_spent`..`lease` race unbounded |
| **B1s** (`small`) | B1 without clock set back, the race and `sendAfter` |
| **B2** (`TWO`) | two ids; at most one each of crash, redeploy, revive; prune; answers 2xx–4xx / 5xx; no refusal, timeout, failing write, clock set back, race, `sendAfter` |
| **B2q** | B2 with two queues a, b (declared a then b, batch 1 each); a redeploy may remove either or change either's M (16 versions) |
| **B2q-s** | B2q without revive; a redeploy only removes b or lowers a's M |
| **B10** | B1 with two ids and sends from the previous version's runs |
| **L** | liveness, one queue: one id, sent again whenever it is free (sends without bound); at most one crash and one redeploy; admission refused, scheduler timeout, all three answers, failing writes, prune; no revive, clock set back, race or `sendAfter`; 6 tokens |
| **L2q** | liveness, two queues, id q+1 for queue q, each sent again whenever free; admission refused, timeout, all answers, prune; no crash, redeploy or failing write; 3 tokens |

Larger configurations were tried and did not finish within 20 minutes:
two ids with every fault (30 M states at depth 25, growing); unbounded
faults with one id (S1: 15.6 M at 10 minutes, growing); B1 with sends from
the previous version (S1: 25 M at depth 37); liveness with every fault
once (6.8 M states at 20 minutes). They found nothing in the part they
explored, which is all they say.

## Results

"Fixed" names the fix flags; "all fixes" is every flag (`FIX_S5`,
`FIX_S6`, `FIX_S7`, `FIX_S7_GUARD`, `FIX_S10`, `FIX_L2`). Lengths are of
the shortest counterexample, in steps (one TLC worker); liveness lassos are
not minimal and have no length. Times are wall-clock on a 16-core machine.

| prop. | as checked | assumptions | backend | bounds | complete? | impl | fixed | cex length |
|---|---|---|---|---|---|---|---|---|
| S1 | inv: an id whose `send` answered Ok is on `message` or `dead`, or was removed by `done` from the valid lease holder after a < 500 answer, or by prune | process crash only (F8 not checked) | TLC | B1 (impl), B1s (fixed) | impl: yes, 17.0 M states, 813 s. All fixes on B1: **no** (34 M, 20 min); on B1s: yes, 13.4 M, 646 s | holds | all fixes: holds (B1s) | — |
| S2 | inv: no id on both tables | — | TLC | as S1 (same runs) | as S1 | holds | holds (B1s) | — |
| S3 | monitor: `send` never gives an id ever given | AUTOINCREMENT as a counter | TLC | B2 | yes: impl 21.2 M, 660 s; all fixes 19.2 M, 625 s | holds | holds | — |
| S4 | (1) inv: ≤ 1 valid delivery per message; (2) monitor: a written settle changes the store iff its delivery holds the valid lease; (3) inv: a delivery whose lease was replaced or removed never matches the message's token | — | TLC | as S1 | as S1 | holds | holds (B1s) | — |
| S5 | monitor: a lease is issued only when `attempts` < M of the tick's version | — | TLC | B1-S5 | impl: counterexample; `FIX_S5`: yes, 4.1 M, 147 s | **violated**: the clock race (new), F1 | `FIX_S5`: holds. Strict "current M": violated even fixed (8) | 8 (race; 9 with M = 1 and no fault), 10 (F1) |
| S6 | per life: (1) inv attempts = leases − written give_backs; (2) inv invocations ≤ attempts; (3) inv attempts − invocations = unstarted deliveries alive + unstarted deliveries lost to a crash | — | TLC | (1)(2): B1 / B1s; (3): B1 | (1)(2) impl on B1: **no** (22.8 M, 20 min); on B1s: yes, 1.7 M, 130 s; all fixes on B1s: yes, 18.7 M, 972 s | (1)(2) hold (B1s); (3) **violated**: F2, and F3 on `give_back` | (1)(2) hold (B1s); (3) `FIX_S6`: **violated** (F3 on `give_back`); all fixes: **violated** (lease ran out before the run started) | (3): 4 (`give_back` write fails), 6 (F2); `FIX_S6`: 4; all fixes: 9 |
| S7 | monitor at every burial: `fail` only for the valid holder whose run started and answered 5xx/none on its M-th delivery; `outlived` only if the current lease's holder had no answer from its app before the lease ran out (an answer lost in a crash counts as none); `lowered` only if M was once higher | — | TLC | B1; B1s | impl and doc fix: counterexamples; all fixes on B1s: yes, 16.2 M, 867 s | **violated**: F2, F3, and the answer/settle race (new) | doc's `FIX_S7`: **violated** (race 9; retried write fails again 13); + `FIX_S7_GUARD` without `FIX_S5`: attempts unbounded (bound hit); all fixes: holds (B1s) | 7 (F2), 9 (race), 10 (F3) |
| S8 | monitor: a `sendAfter` message is not leased before its time has passed | abstract time; F4 not modelled | TLC | as S1 | as S1 | holds (trivially in the abstraction) | holds (B1s) | — |
| S9 | monitor: `bury_spent` and `lease` for queue q change only q's messages | one app | TLC | B2q; B2q-s | B2q: **no** (45.7 M, depth 42, 20 min); B2q-s: yes, impl 1.7 M, 50 s; all fixes 1.8 M, 63 s | holds (B2q-s; nothing found on B2q) | holds (B2q-s) | sanity: 3 with `lease` taking every queue (before #167) |
| S10 | monitor: `send` and `revive` add a message only when the queue holds fewer than the current D | — | TLC | B10; B2 | impl: counterexamples; `FIX_S10` without old-version sends: yes, 18.7 M, 606 s (B2) | **violated**: F5, and old-version sends (new) | `FIX_S10`: **violated** by old-version sends (3); without them: holds | 3 (old-version send), 9 (F5) |
| L1 | `[](on(i) => <>!on(i))` | E1–E4 (+ E5 with `FIX_S7`), E6 as bounds | TLC | L; L2q | yes for L (impl 49,770 states, 22 s); L2q `FIX_L2`: yes, 516,596, 591 s; L2q all fixes + E7: **no** (20 min) | L: holds; L2q: **violated** (F6); without E1: **violated** (F7) | `FIX_S6`: **violated** (503 for ever); + E7: holds; all fixes + E7 on L: holds (136,564, 1157 s); `FIX_L2` on L2q: holds | lasso |
| L2 | `[](on(i) ∧ due(i) => <>(!on(i) ∨ !due(i)))` | E1–E4, E6 | TLC | L2q | impl: counterexample; `FIX_L2`: yes, 516,596, 741 s; all fixes: **no** (20 min) | **violated** (F6) | `FIX_L2`: holds | lasso |
| L3a | inv: if, for a message, no admission was refused, no run timed out or answered ≥ 500 or nothing, no lease ran out before its settle was written, and no crash, clock set back, revive or redeploy happened while it lived, then it was invoked at most once | — | TLC | B1; B1s | impl on B1: **no** (23.7 M, 20 min); impl on B1s: yes, 2.0 M, 74 s; all fixes on B1s: yes, 24.2 M, 820 s | holds (B1s; nothing found on B1) | holds (B1s) | — |
| L3b | `[](on(i) => <>invoked(i))` | E1–E4, E6; also with E7 | TLC | L | yes (counterexamples) | **violated** (crash before the app starts) | all fixes + E7: **violated** (crash); + no crash: **violated** (the lease runs out while the run waits in the scheduler) | lasso |

## The findings, one by one

| # | suspicion in the doc | verdict here |
|---|---|---|
| F1 | lowering `max_attempts` does not stop a waiting retry (S5) | **confirmed**, 10 steps, store replay below |
| — | *new*: `bury_spent` and `lease` read the clock separately (S5) | **found**, 8 steps (9 with M = 1 and no fault at all) |
| F2 | a run that never started spends an attempt and can bury (S6, S7) | **confirmed**: S6.3 in 6 steps, S7 in 7 |
| F3 | a failed settle write is ignored; the lease later runs out as "no answer" (S7) | **confirmed**, 10 steps; also for `give_back` (S6.3, 4 steps) |
| — | *new*: the same false "no answer" without any failure: the lease runs out between the app's answer and the settle's write (S7) | **found**, 9 steps |
| F5 | `revive` ignores `max_depth` (S10) | **confirmed**, 9 steps |
| — | *new*: a run of the previous version sends with that version's `max_depth` (S10), and to a queue the new version removed | **found**, 3 steps; not closed by the S10 fix |
| F6 | in flight counted per app: one queue starves another (L2, and so L1) | **confirmed** (lasso); either queue can starve the other, not only the first the second; closed by `FIX_L2` |
| F7 | messages of an undeclared queue are never delivered (L1) | **confirmed** for delivery (L1 without E1 fails); "nor shown" is the admin page, not modelled |
| F4 | whole-second truncation | not modelled (arithmetic; Rust tests) |
| F8 | power loss | not modelled |

### S5 — at most `M` deliveries per life

**The race (new, `s5_impl` 8 steps, `s5_race` 9 steps with M = 1 and no
fault).** The tick calls `now()` for `bury_spent`, then again for `lease`.
A message whose lease runs out between the two is not buried (not due yet
for `bury_spent`) and then leased (due for `lease`): one delivery past `M`.
With whole-second `now()`, any tick whose two calls straddle a second
boundary is the window. `s5_race`:

1. The tick begins; `bury_spent(a, M=1)` finds nothing.
2. The app sends message 1.
3. `lease` takes it: attempts 1, token t1. 4. Admitted (in flight 1); the tick ends.
5. The next tick: `bury_spent(a, 1)` finds nothing — message 1's lease has not run out.
6. The app's entry starts. 7. It answers 2xx (in flight 0); the `done` is not written yet.
8. Time passes: message 1's lease runs out.
9. `lease` (the tick of step 5) takes message 1 again: attempts 1 → 2 with M = 1.

(Steps 6–7 only make room under batch 1.) Store replay:

```
send("a", "m", now_unix=1000, due_unix=1000, max_body_bytes=65536, max_depth=2) -> Ok(1)
bury_spent("a", max_attempts=1, now_unix=1000, why)                              -> []
lease(Some("a"), now_unix=1000, limit=1, lease_secs=30)                         -> [id 1, attempts 1, lease_id t1]   (due 1030)
bury_spent("a", max_attempts=1, now_unix=1029, why)                              -> []   (not due yet)
lease(Some("a"), now_unix=1030, limit=1, lease_secs=30)                         -> [id 1, attempts 2]   <- S5: attempts was 1 >= M = 1
```

**F1 (`s5_f1`, 10 steps).** With no lease allowed to run out inside the
window (`MAX_RACE = 0`), the shortest trace is F1:

1. The tick begins, reading version 1 (M = 2); `bury_spent(a, 2)` finds nothing.
2. Redeploy: version 2 has M = 1. The running tick keeps version 1.
3. The app sends message 1.
4. `lease` under version 1: attempts 1, token t1; the delivery's M is 2.
5. Admitted; the tick ends.
6. The next tick reads version 2; `bury_spent(a, 1)` finds nothing (message 1 is on lease).
7. The scheduler answers 503: the run waited past its deadline, never started.
8. The delivery settles: not taken, attempts 1 < its M 2, so `retry`: lease cleared, due later.
9. Time passes: message 1 is due.
10. `lease` takes it: attempts 1 → 2 with M = 1. **S5 broken.**

TLC's pick puts the second tick's `bury_spent` (6) before the retry is
written; the essence — a message with no lease and `attempts >= M` is
neither buried nor refused — does not need that, as the replay shows.
Step 7 is a 503 only because it is one step shorter than "starts, then
answers 5xx"; the store sees the same `retry`. The redeploy must come
after the first tick read version 1, else the first delivery has M = 1 and
buries.

Replay on the store, for a Rust test (one `Queues`, `in_memory()`):

```
send("a", "m", now_unix=1000, due_unix=1000, max_body_bytes=65536, max_depth=2) -> Ok(1)
bury_spent("a", max_attempts=2, now_unix=1000, why)                              -> []
lease(Some("a"), now_unix=1000, limit=1, lease_secs=30)                         -> [id 1, attempts 1, lease_id t1]
retry(1, Some(t1), now_unix=1000, delay_secs=10)                                -> Ok(true)
    -- redeploy: max_attempts is now 1 --
bury_spent("a", max_attempts=1, now_unix=1010, why)                              -> []   <- contract: message 1 buried
lease(Some("a"), now_unix=1010, limit=1, lease_secs=30)                         -> [id 1, attempts 2]   <- S5: attempts was 1 >= M = 1
```

`delay_secs` is the delivery's `retry · 2^(attempts−1)`, i.e. `retry` for
attempts 1; any value works if the next two `now_unix` are at or past
`1000 + delay_secs`. `why` is any string (the loop passes `"no answer
within the lease"`). Under the contract the last call answers `[]` and the
message is on `dead`.

**Fix.** `FIX_S5` (`lease` takes M and leases only `attempts < M`; the tick
buries due messages with `attempts >= M` whatever their lease, "lowered"
when they held none) closes both, under a complete search on B1-S5.

**"Current M".** S5 is checked against the version the tick read. Against
the version current at the instant of the lease (`S5strict`) even the
fixed model fails (`s5strict_fixed`, 8 steps: a redeploy between the tick's
`bury_spent` and its `lease`). Closing that needs the redeploy to wait for
the tick.

### S6 — `attempts` counts deliveries handed out

(1) and (2) are checked as invariants; (3) as: attempts − invocations =
unstarted deliveries of this life still alive + unstarted deliveries of
this life a crash took.

- `s6_impl` (4 steps): admission refused, and the `give_back` write fails;
  the code ignores it. The attempt stays counted with no invocation and no
  crash: F3 on `give_back`.
- `s6_3_nowf` (6 steps, no failing writes): **F2**. Lease, admit, the
  scheduler answers 503, the delivery `retry`s: attempts 1, invocations 0.
- `s6_3_fix` (`FIX_S6` alone, 4 steps): the same failing `give_back`.
- `s6_3_fixed` (all fixes, 9 steps; `s6_3_fixed_norace` the same without
  the race): lease, admit, the run waits in the scheduler, **the lease runs
  out**, the next tick leases the message again (attempts 2), then the
  first run's 503 arrives and its `give_back` matches no token. The host
  saw the fault but cannot give the attempt back: its lease is gone. The
  contract's (3) needs a third cause ("the lease ran out before the run
  started") or the host must hold `lease` ≥ the scheduler's deadline.

### S7 — dead letters say why, truthfully

- `s7_impl` (7 steps): **F2**. M = 1; the scheduler answers 503; the
  delivery buries it as failed (`why = "503 …"`) though the app never ran.
- `s7_noto` (the code, no scheduler timeouts, 9 steps), and the same with
  the doc's `FIX_S7` (`s7_fix_f3`, 9 steps): **the app answers 2xx within the lease, the
  lease runs out before the `done` is written, and `bury_spent` buries it
  "no answer within the lease"**. No store failure needed: the delivery
  task and the tick take the same mutex, and the tick may get it first.
  The message is a dead letter although the app took it.
- `s7_f3` (no timeouts, no lease running out between answer and write, 10
  steps): **F3** proper: the `done` write fails, the code counts it as
  delivered, the lease runs out, `bury_spent` buries "no answer".
- `s7_fix_f3_atomic` (doc's fix, same restriction, 13 steps): the retried
  settle fails again at the next tick, the lease runs out, `bury_spent`
  buries "no answer". The doc's fix alone does not close F3.
- `FIX_S7_GUARD` (not in the doc): `bury_spent` skips a message whose
  current lease holder holds an answer in memory (answered, write not yet
  done or failed). With every fix on (`FIX_S6` closes the 503 gap, `FIX_S5`
  bounds attempts), S7 holds under a complete search on B1s. Without
  `FIX_S5` the guard lets `lease` take, past M, the message it kept from
  `bury_spent`, and attempts grow without bound (`s7_fix_f3g_small`).
- A crash between the answer and the write also buries "no answer"
  although the app answered; the model counts an answer a crash took as no
  answer (decision 11).

### S10 — depth is bounded

- `s10_impl` / `s10_keep` (3 steps, new): after a redeploy that lowers D
  to 1 (`s10_keep`) — or removes the queue (`s10_impl`) — a run of the
  previous version, still running, sends with the old `[[queues]]` (D = 2,
  declared): the queue holds 2 > 1. `QueueHost.queues` is per version.
- `s10_f5` (no old-version sends, 9 steps): **F5**. D lowered to 1; message
  1 is buried; message 2 is sent (depth 1); `revive(1)` makes it 2.
- `FIX_S10` (revive refuses when full) still fails on the old-version send
  (`s10_fix`, 3 steps); without old-version sends it holds (complete, B2).

### L1 / L2 — every message is settled / handed out

- One queue (L): L1 holds under E1–E4 (`l1_impl`). Without fairness it
  fails (`l1_nofair`); without E1 it fails (`l1_noE1`): the redeploy
  removes the queue and the message stays (F7).
- `FIX_S6` breaks L1 (`l1_fix_s6`): a scheduler that times the run out
  every time makes `give_back` → lease → 503 → `give_back` loop for ever;
  under the code as it is, the timeouts spend the attempts and the message
  is buried. E1–E6 do not exclude this; with E7 (a message admitted
  infinitely often is started infinitely often) L1 holds again
  (`l1_fix_s6_e7`, and all fixes + E7: `l1_fixed_e7`).
- Two queues (L2q): L2 and L1 fail (`l2_impl`, `l1_2q_impl`): F6. The lasso
  TLC gives has queue b's delivery in flight every time the tick reaches
  queue a, so a's due message never gets room — the later queue starving
  the earlier, the reverse of the doc's example; both directions are
  possible. `FIX_L2` makes L2 hold (complete, 516,596 states).

### L3 — exactly once when nothing goes wrong

- L3a holds, impl and fixed, on B1s (complete); on B1 the impl search did
  not finish (23.7 M states, nothing found). F4 (the lease up to a second
  short) is the doc's reason to expect a violation, and it is not modelled.
- L3b fails in every variant: with a crash before the app starts and M = 1
  the message is buried never invoked (`l3b_impl`, all fixes + E7:
  `l3b_fixed_e7`); with no crash at all, a lease that runs out while the
  run waits in the scheduler is buried by `bury_spent` on its last attempt
  (`l3b_fixed_e7_nocrash`). L3b as the doc states it ("from L2") does not
  follow: L2 gives a lease, not an invocation. It would need no crash, E7,
  and a lease longer than the scheduler's wait.

## What could not be checked, and why

- **Power loss (F8)** and **F4** (truncation): not modelled.
- **Complete searches that did not finish within 20 minutes** are listed
  as "not complete" in the table: the fixed model with every fault on B1
  (S1, S2, S4, S8), S6 (1)(2) on B1, S9 on B2q, L1 on two queues with all
  fixes, L2 with all fixes. Each was rerun on a smaller configuration
  (named in the table) where one exists.
- **Two message ids with every fault**, and unbounded faults: too large
  (see Bounds). S3, S9 and S10 have two ids with fewer faults.
- **Liveness with revive, clock set back, the race and `sendAfter`**: L
  leaves them out (with them, 6.8 M states at 20 minutes).
- **Cancelled runs (499)**: unreachable for queue deliveries; not modelled
  (decision 6).
- The admin page ("nor shown" in F7), several apps, `max_body_bytes`.
