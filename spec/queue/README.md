# A model of the queue

A Quint model of minicloud's queue — the store (`crates/minicloud/src/queue.rs`)
and the delivery loop (`queue_host.rs`) as they are at `e39f40e` — checked
with TLC against the contract in [`docs/queue-properties.md`](../../docs/queue-properties.md).

- `queue.qnt` — the model. One transition relation; the proposed fixes are
  flags (`FIX_*`), one per fix, so each can be checked on its own.
- `checks.qnt` — one module per check: the bounds, the environment and the
  flags it runs with.
- `tools/check.sh` — runs one check and summarises it; `tools/trace.py`
  turns TLC's counterexample into one line per step with what changed.
- `sanity/toy.qnt` — the backend's sanity checks.

## Running

```sh
cd spec/queue
npm ci
npm run setup        # once: makes Quint download Apalache, whose jar carries TLC
npm run s5:impl      # any script in package.json
```

Each check script writes TLC's whole output to `out/<name>.txt` and, for a
counterexample, the summary to `out/<name>.trace`.

Java 21 is needed. TLC is the one inside the Apalache 0.62.1 jar Quint
downloads to `~/.quint/apalache-dist-0.62.1/apalache/lib/apalache.jar`
(sha256 `880c0b2b72354816f12e9a9755829f2907071e9a181ffea1937954c23d54739d`
here). Quint runs it as `tlc2.TLC -deadlock -workers <n>` on a generated
`.cfg` with `INIT`, `NEXT`, `INVARIANT` and `PROPERTY` only.

- `tlc-bfs1.json` runs TLC with **one worker**. TLC's breadth-first search
  with one worker reports a shortest counterexample; with several it may not
  (with `auto` workers the first S5 run reported a trace one step longer
  than the shortest). Every counterexample length below comes from a
  one-worker run.
- `tlc.json` runs it with all cores, for the checks that pass.

Every check also checks `NoOverflow`: a bound of the model (attempts past
`MAXATT`, the token pool `NTOK` exhausted) was hit. A run that passes with
`NoOverflow` has not been cut short by those bounds.

## Backend sanity checks

`sanity/toy.qnt` is a counter that may climb from 0 to 3 or stand still.

| script | what | expected | got |
|---|---|---|---|
| `sanity:inv-false` | invariant `x <= 2` | violated | violated, 3-step trace to `x = 3` |
| `sanity:inv-true` | invariant `x <= 3` | holds | holds, 4 states |
| `sanity:live-nofair` | `eventually(x == 3)`, no fairness | violated | violated: stutters at `x = 2` |
| `sanity:live-fair` | `weakFair(inc, x) implies eventually(x == 3)` | holds | holds |
| `sanity:live-false` | `weakFair(inc, x) implies eventually(x == 4)` | violated | violated: stutters at `x = 3` |

So TLC through Quint reports a false invariant, a false liveness property
and a true one, and the fairness written into the property changes the
verdict (`live-nofair` against `live-fair`). Quint compiles `weakFair(A, v)`
to `WF_v(A)` and `strongFair` to `SF_v`; it cannot put fairness in a
`SPECIFICATION`, so every liveness check here is `fairness implies property`
in a `PROPERTY`, which TLC checks soundly (only more slowly).

## Modelling decisions

(Filled in as the checks go; see the end of this file.)

## S5 — at most `M` deliveries per life

Property, from the contract: a lease is issued only when the message's
`attempts` is below the current `M`. Checked as a monitor: `tickLease`
records `S5` in `bad` when it leases a message whose `attempts` (before the
lease) is at least the `M` of the version the tick is running.

Bounds (`s5_*`): one queue, one message id, M ∈ {1, 2} (starts at 2), D ∈
{1, 2}, batch 1, ≤ 1 crash, ≤ 1 redeploy (to any M, D, or removing the
queue), ≤ 1 revive, ≤ 1 clock set back, prune, sendAfter, sends from the
previous version's runs, admission refused, scheduler timeout, 2xx-4xx /
5xx / no answer, settle writes that fail. Token pool 5 per id, attempts ≤ 4.

| check | result | length | states | time |
|---|---|---|---|---|
| `s5:impl` | **violated** | 8 steps | — | 1 s |
| `s5:race` | **violated** (M = 1 from the start; no redeploy, no fault but time) | 9 steps | — | 1 s |
| `s5:f1` | **violated** (F1 alone: no time passes between a queue's `bury_spent` and its `lease`) | 10 steps | — | 1 s |
| `s5:fixed` | **holds**, complete (TLC, all workers) | — | 2,414,568 distinct, depth 35 | 81 s |
| `s5:strict-fixed` | **violated** (see below) | 8 steps | — | 1 s |

### The shortest S5 violation is not F1

The shortest trace TLC finds (`s5:impl`, 8 steps) is a second way to break
S5, not F1: **`bury_spent` and `lease` read the clock separately.** The
tick calls `now()` for `bury_spent`, and again for `lease`. A message whose
last lease runs out between the two is not buried (not yet due for
`bury_spent`) and then leased (due for `lease`): one delivery more than
`M`. It needs no redeploy: `s5:race` finds it with `M = 1` throughout and no
fault but time. With whole-second `now()`, the window is any tick whose two
calls straddle a second boundary.

`s5:race` (9 steps, M = 1, batch 1):

1. The tick begins; `bury_spent(a, M=1)` finds nothing.
2. The app sends message 1.
3. `lease` takes it: attempts 1, token t1.
4. It is admitted (in flight 1); the tick ends.
5. The next tick begins; `bury_spent(a, 1)` finds nothing: message 1's lease has not run out.
6. The app's entry starts.
7. The run answers 2xx (in flight 0); its `done` is not yet written.
8. Time passes: message 1's lease runs out.
9. `lease` (same tick as 5) takes message 1 again: attempts 1 → 2 with `M = 1`. **S5 broken.**

Steps 6–7 are there only because batch 1 leaves no room while the first
run is in flight; with a larger batch the run may still be going.

Replay on the store (one `Queues`, `in_memory()`):

```
send("a", "m", now_unix=1000, due_unix=1000, max_body_bytes=65536, max_depth=2)  -> Ok(1)
bury_spent("a", max_attempts=1, now_unix=1000, why)                               -> []
lease(Some("a"), now_unix=1000, limit=1, lease_secs=30)                          -> [id 1, attempts 1, lease_id t1]   (due 1030)
bury_spent("a", max_attempts=1, now_unix=1029, why)                               -> []   (not due yet)
lease(Some("a"), now_unix=1030, limit=1, lease_secs=30)                          -> [id 1, attempts 2]   <- S5: attempts was 1 >= M = 1
```

The contract wants the last call to answer `[]` (and the next tick's
`bury_spent` to bury it). The S5 fix (`lease` takes `M`) closes this too.

### F1: lowering `max_attempts` does not stop a waiting retry

`s5:f1` adds one restriction — no time passes between a queue's
`bury_spent` and its `lease` — which takes the race away, and TLC's shortest
trace is then F1, 10 steps:

1. The tick begins: it reads version 1 (M = 2); `bury_spent(a, 2)` finds nothing.
2. Redeploy: version 2 has M = 1 (and D = 1). The running tick keeps version 1.
3. The app sends message 1 (attempts 0, due).
4. `lease(a, room 1)` under version 1: attempts 1, token t1. The delivery's policy is M = 2.
5. It is admitted; in flight 1. The tick ends.
6. The scheduler answers 503: the run waited past its deadline, never started. In flight 0.
7. The delivery settles: not taken, attempts 1 < its M 2, so `retry`: lease cleared, due later.
8. Time passes: message 1 is due.
9. The next tick reads version 2 (M = 1); `bury_spent(a, 1)` does not bury message 1: attempts 1 ≥ 1, but its `lease_id` is NULL.
10. `lease` takes it: attempts 1 → 2, with M = 1. **S5 broken.**

Step 6 is a scheduler timeout only because it is one step shorter than
"the app starts, then answers 5xx" (two steps); the store sees the same
`retry` either way. The redeploy has to come after the first tick read
version 1 (otherwise the first delivery has M = 1 and buries instead of
retrying); anywhere between steps 1 and 9 will do.

Replay on the store:

```
send("a", "m", now_unix=1000, due_unix=1000, max_body_bytes=65536, max_depth=2)  -> Ok(1)
bury_spent("a", max_attempts=2, now_unix=1000, why)                               -> []
lease(Some("a"), now_unix=1000, limit=1, lease_secs=30)                          -> [id 1, attempts 1, lease_id t1]
retry(1, Some(t1), now_unix=1000, delay_secs=10)                                 -> Ok(true)
    -- redeploy: max_attempts is now 1 --
bury_spent("a", max_attempts=1, now_unix=1010, why)                               -> []   <- contract: message 1 buried
lease(Some("a"), now_unix=1010, limit=1, lease_secs=30)                          -> [id 1, attempts 2]   <- S5: attempts was 1 >= M = 1
```

`delay_secs` is the delivery's `retry · 2^(attempts−1)` = `retry` for
attempts 1; any value works as long as the `now_unix` of the next two calls
is at or past `1000 + delay_secs`. `why` is any string (the loop passes
`"no answer within the lease"`).

### The fix

`s5:fixed` sets `FIX_S5` alone: `lease` takes `M` and leases only
`attempts < M`; the tick buries every due message with `attempts >= M`,
whatever its lease, with the reason "lowered" when it held no lease. S5
holds under a complete search at the bounds above (2.4 M distinct states).

### A redeploy during a tick

The contract says "the **current** `M`". The tick reads the app's version
once, when it begins, and uses it for every queue; a redeploy that lands
during a tick is seen at the next one. `S5` is checked against the `M` of
the version the tick read. Checked against the `M` of the version current
at the instant of the lease (`S5strict`), even the fixed model fails
(`s5:strict-fixed`, 8 steps: the redeploy lands between the tick's
`bury_spent` and its `lease`). Closing that would need the redeploy to wait
for the tick; we take the tick's version as "current" and say so.
