# The queue's properties

What the queue (`crates/minicloud/src/queue.rs`, `queue_host.rs`) promises,
stated so that a model checker and tests can hold the code to it (issue
#168). Each property says three things apart:

- **Contract** — what the queue is meant to promise. Where the code does
  not keep it, the contract is not weakened to fit; the gap is a finding.
- **Implementation** — what the code at `e39f40e` does.
- **Proposed fix** — what would close the gap, where there is one.
- **Fixed** — what the fixes on `fix/queue-contract` changed, with the
  commit; the decisions the owner took on the way (D1–D6) are written
  into the contract where they belong.

How each is checked, how far, and what was found is in the table at the
end, kept up to date as the work goes.

## Vocabulary

- A message's **life** runs from the `send` (or `revive`) that puts it on
  the `message` table to the operation that takes it off: `done`, a burial,
  or nothing ever.
- A **lease** is one handing out of a message by `lease`. It writes a fresh
  **token** (`lease_id`) beside the message. A **delivery** is the host's
  work for one lease: admit a run, let the app answer, **settle** (`done`,
  `retry`, `bury`, `give_back`).
- A delivery's steps are separate events, and a fault may fall between any
  two: lease → admit → the app's entry starts → the app answers → the
  settle is written.
- An **invocation** is the app's entry actually starting for a delivery.
- `M`, `L`, `D`: a queue's `max_attempts`, `lease`, `max_depth` as the
  **current version** of the app says them. A redeploy is a new version;
  deliveries already running keep the version (and policy) they were
  started under. For the delivery loop (S5, D4), the current version is
  **the one the tick read when it started**: a tick buries and leases
  every queue under that version's policy, even if a redeploy lands while
  it runs. For a send (S10, N4) it is the version installed at the call.
- **Process crash**: the host process dies; committed SQLite transactions
  survive, every delivery and every in-memory counter is gone, and so are
  the settles kept in memory (S7). A **shutdown** is a crash as far as the
  queue is concerned: a run the closing engine drops writes nothing (N6).
  **Power loss**: the machine stops; the queue store syncs every commit
  (`synchronous = FULL`, D3), so committed transactions survive it too.
- **Times** are host time in milliseconds since the epoch (D2): `due`,
  `sent` and a dead letter's `buried`. `sendAfter`'s delay is still given
  in seconds, and the window's reads still say seconds.

## Safety

### S1. A message is not lost

**Contract.** Under process crashes and power loss: a message whose
`send` answered `Ok` stays on `message` or `dead` until one of (a) `done`
by a delivery holding its current lease, after the app itself answered
below 500; (b) `prune_dead` of its dead letter. Nothing else removes it.

Power loss was a separate, weaker promise until D3: under
`synchronous = NORMAL` a transaction committed shortly before power loss
may be undone, so an acknowledged `send` could be lost. The store now
syncs every commit (`synchronous = FULL`), so the promise is the same as
for a crash. The model checks S1 under process crash only; power loss is
recorded, not checked.

**Implementation.** `done` deletes only on a matching token; `bury` and
`bury_spent` move to `dead` in one transaction; `revive` moves back in one.
A queue removed from `app.toml` keeps its messages on `message` — not
lost, but see L1.

**Proposed fix.** None for crashes. For power loss: decide whether
`synchronous = FULL` for the queue store is worth its cost (open question
for the owner).

**Fixed.** D3, the owner's answer: the queue store opens with
`synchronous = FULL` (`eb238f8`, F8). A `done` is taken only on the app's
own answer, never on a reply the host made in its place (`b505fac`, F2:
a 499 for a run cancelled before it started deleted the message). A
queue removed from `app.toml` has its messages buried, not left (`e7b4d6a`,
F7).

### S2. One place

**Contract.** No id is on `message` and `dead` at once.

**Implementation.** Every move is an insert and a delete in one
transaction.

### S3. Ids are not reused

**Contract.** A new message never gets an id that any message (live, dead
or pruned) has had.

**Implementation.** `AUTOINCREMENT`; `revive` reinserts the dead letter's
own id.

### S4. Only the current lease holder settles

**Contract.** A lease does not stop being valid when its time runs out; it
stops being valid when the message's token is replaced or removed — by a
new lease (which only an expired lease allows), by `bury_spent`, or by the
message leaving the queue. So:

1. A message has at most one valid token.
2. A settle changes the store if and only if the delivery's token equals
   the message's token at the moment it is written (atomically).
3. A token, once replaced or removed, never becomes valid again.

A delivery whose lease has run out but has not been replaced still owns
the message: its late answer is accepted. That is deliberate — nobody else
has it.

**Implementation.** Holds by construction in the store (`... AND lease_id
IS ?`). (3) rests on `next_lease_id` being unique within a process and on
no delivery surviving the process; a clock set back across a restart
cannot matter, because the old process's deliveries are gone.

**Fixed.** Every settle names a token, an `i64` (`1e0d0fd`, N1): with
`IS ?` a settle naming none matched a message whose lease a `retry` or
`give_back` had ended, not only one never leased. The comparison is
`lease_id = ?`.

### S5. At most `M` deliveries per life

**Contract.** A lease is issued only when the message's `attempts` is below
the **current** `M` — the `max_attempts` of the version the tick read when
it started (D4; a redeploy landing in the middle of a tick takes effect at
the next one). So lowering `M` limits what happens next: a message
that has already had `M` or more deliveries is not handed out again — it
becomes a dead letter at the next tick. Raising `M` does not raise the
dead. Deliveries already running when `M` changes finish under the policy
they were started with, but no new lease breaks the new bound.

**Implementation.** `bury_spent` only buries a message that still holds a
lease (`lease_id IS NOT NULL`), and `lease` does not look at `M` at all.
A message waiting for its retry (`retry` cleared its lease) when `M` is
lowered to or below its `attempts` is leased again — **suspected
violation**, to be shown by the model and reproduced.

**Proposed fix.** `lease` takes `M` and leases only `attempts < M`; the
tick buries due messages with `attempts >= M` whatever their lease, with
a reason that says which happened (lease ran out / `max_attempts`
lowered).

**Fixed** (`818e6fc`, F1 and N2). As proposed: `lease` takes `M`;
`bury_spent` buries what is due at `attempts >= M` and says "no answer
within the lease" when it still holds a lease and "max_attempts lowered
to M" when it does not. And the tick reads the clock once, burying and
leasing at the same instant: read twice, a lease that ran out between
the readings was too early for `bury_spent` and in time for `lease` (N2).

### S6. `attempts` counts deliveries handed out

**Contract.** Equality with invocations cannot be promised: a crash after
`lease` commits and before the app's entry starts leaves an attempt
counted and no invocation, and nothing written before the start can be
undone by a process that is gone. What can be promised:

1. `attempts` = leases issued in this life − `give_back`s.
2. invocations ≤ `attempts`.
3. `attempts` − invocations grows only by a delivery that did not reach
   the app's entry because of a **crash** or a **fault the host can
   see** (the run turned away, cancelled, or timed out in the scheduler's
   queue). Of these, every one the host can see is given back; only crashes
   leave the gap.

**Implementation.** (1) and (2) hold. (3) does not: a run admitted to the
scheduler that waits past its deadline is answered `503` by the scheduler
(`sched.rs`, `QueueTimeout`) without the app ever starting, and the
delivery counts it as a failed attempt — and on the last attempt buries
the message. A run cancelled before it starts is answered `499`, which
is below 500: the delivery takes it for the app's answer and `done`s
the message, so the message is lost (S1) though the app never ran. No
path cancels a queue delivery's run today (its `Cancel` is its own and
nothing fires it), so this waits for one.
**Violation by reading**, to be reproduced.

**Proposed fix.** The delivery learns that the run never started (a flag
on the reply, or the `ErrorKind`) and gives the message back instead.

**Fixed** (`b505fac`, F2). A `Reply` says who made it
(`convert::Origin`): the app's answer, the host's word on a run that never
started, or the host's word on a run that started and did not answer. A
never-started run is given back with its attempt; a host-made reply after
the start (a cancel mid-run, a deadline) is no answer, whatever its
status. A failed `give_back` is kept and written at the next tick
(`2d99362`, F3). The third cause the model found (N5: the lease running
out while the run still waits for a worker, so its give-back matches
nothing) is closed by D1 (`70c2dea`): the wait for a worker counts against
the run's deadline and the lease outlasts the deadline by a second, so a
run is answered 503 by the scheduler before its lease can run out.

### S7. Dead letters say why, truthfully

**Contract.** A message becomes a dead letter only when:
(a) the delivery holding its current lease, on its `M`-th delivery, got
an answer of 500 or more or no answer; or (b) its lease ran out on its
`M`-th delivery (`no answer within the lease`); or (c) `M` was lowered to
or below its `attempts` (the S5 fix). The reason recorded is the one that
happened.

**Implementation.** Two gaps:
- A settle that fails to write (SQLite error) is ignored: `done` failing
  is counted as delivered, and the message keeps its lease; when the lease
  runs out on the last attempt, `bury_spent` buries it as "no answer within
  the lease" although the app answered. **Violation by reading.**
- S6's never-started runs are buried as if the app had failed.

**Proposed fix.** On a failed settle, keep the settle and try it again
(in memory, next tick) rather than dropping it; count only what was
written. With S6's fix, the second gap closes.

**Fixed** (`2d99362`, F3 and N3; with `b505fac` for F2 and `818e6fc` for
(c)). A delivery's settle is kept in memory beside the store, by message
and lease, then written; a failed write leaves it kept, and each tick
writes the kept settles before it does anything else. Counters and the
log move only when a write takes. While a settle is kept for the lease a
message holds, `bury_spent` does not bury it (the guard the model showed
the proposed fix needs: N3, the lease running out between the answer and
the write) and `lease` does not hand it out. A crash loses the kept
settles, which is a crash: an answer a crash took counts as none
(model decision 11).

### S8. Not before its time

**Contract.** A message sent with `sendAfter(d)` at host time `t` is not
handed out before host time `t + d`. "Host time" is the wall clock; if the
clock is set back, deliveries wait longer; if set forward, they come
earlier in real time. Persisted due times cannot use a monotonic clock
across restarts, so this is the contract, and it is written down as such.

**Implementation.** `now()` truncates to whole seconds, so `t` is rounded
down: `sendAfter(1)` at 10.9 s is due at 11 and may be delivered 0.1 s
later. **Violation by reading** (up to one second early). The same
truncation shortens every lease by up to a second, and a `lease` or
`retry` under a second (allowed by the config, which refuses only 0)
becomes 0.

**Proposed fix.** Keep due times in milliseconds, or round the due time
up; refuse `lease`/`retry` under one second if seconds stay.

**Fixed** (`09e0122`, F4; D2). Times are milliseconds: the host's clock
is read in milliseconds and the store keeps `due`, `sent` and `buried` in
them, so `t + d` is the sending millisecond plus `d` seconds, and a lease
or retry that is not whole seconds is as long as it says. A store from
before is converted once when it is opened (`PRAGMA user_version` 1).

### S9. Nothing mixes

**Contract.** Leasing, settling or burying in queue X never changes a
message of queue Y; an app's messages are delivered only to that app.

**Implementation.** One store file per app; `lease` and `bury_spent` take
a queue name (#167). Settles are by id and token.

### S10. Depth is bounded

**Contract.** No operation makes a queue hold more than the current `D`
waiting messages. Lowering `D` does not drop messages already waiting
(that would break S1), so the bound is on growth: an operation that adds a
waiting message to a queue happens only when the queue holds fewer than
`D`.

**Implementation.** `send` checks inside its write transaction. `revive`
(admin) does not check. **Violation by reading.**

**Proposed fix.** `revive` refuses when the queue is full (the admin gets
a 409 saying so).

**Fixed** (`15bbd3b`, F5). As proposed, against the current version's
`D`. And a send is held to the current version's `[[queues]]`, not the
calling run's version's (`2537946`, N4): a run that outlived a redeploy
could otherwise send past the new `D`, or to a queue the new version no
longer declares (now an `Err`).

## Liveness

Liveness is promised only under assumptions, which are part of the
contract:

- **E1** From some point on, the app is enabled and loaded, forever.
  (Until D6 E1 also asked that the queue be declared by the current
  version. Now a message of a queue no longer declared is buried at the
  next tick, which settles it.)
- **E2** The host's clock grows without bound; the delivery loop ticks
  (weak fairness).
- **E3** Every admitted run ends (it is bounded by its deadline): weak
  fairness on a run's end.
- **E4** Admission is not refused forever: if a delivery is tried
  infinitely often, it is admitted infinitely often (strong fairness).
- **E5** A settle that is retried is eventually written (no permanent
  store failure).
- **E6** Only finitely many crashes, revives and redeploys.
- **E7** A message admitted infinitely often is eventually started:
  fairness of the scheduler towards a queue's runs (verified under #165).
  L1 and L3b assume it. Without it, once a run that never starts is given
  back (S6), a scheduler that times the run out every time loops lease →
  503 → give-back for ever.

### L1. Every message is settled

**Contract.** Under E1–E7, every message on `message` eventually leaves
it (done, or a dead letter).

**Implementation.** Messages of a queue the current version no longer
declares are never looked at: E1 excludes them, so the contract does not
cover them, but nothing tells anyone — the admin page should list them.
Otherwise expected to hold; to be checked with a complete (TLC) search.

**Fixed** (`e7b4d6a`, F7; D6). Each tick, when the app is enabled and
loaded, buries the messages of every queue the current version does not
declare, with the reason "queue `x` is no longer declared"; a message
whose lease still runs, or whose settle is kept, waits for its delivery.
As dead letters they are counted, listed and revivable. With F6's fix
(`db61025`) and E7, L1 holds as the model checked it.

### L2. Every due message is handed out

**Contract.** Under E1–E6, a due message is eventually leased. This is per
queue: a busy queue does not starve another of the same app.

**Implementation.** `room` is a queue's `batch` minus the in-flight count
of **the whole app**. Queues are leased in declaration order each tick, so
a first queue that always has at least its `batch` due fills the app's
in-flight count, and a later queue with `batch` no larger is never leased.
**Suspected violation** (environment: sends to the first queue forever).
A leaked in-flight count (a run whose answer never arrives) would stop
every queue of the app; none is known.

**Proposed fix.** Count in flight per queue.

**Fixed** (`db61025`, F6). The app's counters keep in-flight deliveries
per queue beside the total, and a queue's room is its own `batch` less its
own deliveries. `queue_in_flight` stays the app's total.

### L3. Exactly once when nothing goes wrong

**Contract.** Split in two:
- **L3a (safety)** If, for a message, every delivery is admitted, its run
  answers below 500, the answer's settle is written before the configured
  lease has run out, no crash happens, the clock does not jump, and nobody
  revives it or redeploys the queue — then the app is invoked for it at
  most once.
- **L3b (liveness)** Under E1–E7, if no crash or shutdown takes a
  delivery of its last attempt before the run starts, no host stall of a
  second or more lets that attempt's lease run out while its run waits for
  a worker, and the queue stays declared (a redeploy that removes it
  buries the message, D6), it is invoked at least once. This does not follow from L2 alone: L2 promises a lease,
  not an invocation. E7 turns admissions into a start; D1 keeps the lease
  from running out while the run waits for a worker (the scheduler
  answers 503 first, and the attempt is given back); and a crash before
  the start spends an attempt that nothing can give back (S6.3), so on
  the last attempt the message is buried never invoked.

A 5xx, or a crash after the app answered and before the settle was
written, is outside the hypothesis: the queue is at-least-once, and those
are where the second delivery comes from.

**Implementation.** The one-second truncation (S8) makes the real lease up
to a second shorter than configured, so a delivery that answers within
`lease` may still be handed out again. **Violation by reading.**

**Fixed** (`09e0122`, F4: the lease is as long as configured, to the
millisecond). And D1 (`70c2dea`): a queue's `lease` must be at least a
second longer than the app's `limits.deadline`, and an app with queues
must have one, or the app is refused. A run that answers at all answers
within its deadline, so within its lease.

## Not promised

- Exactly-once in general.
- FIFO completion. `lease` takes the oldest first; deliveries run in
  parallel.

## What the model contains

Store (`message`, `dead`, tokens, `attempts`, due), `queue_in_flight`,
delivery tasks with their own policy and version, the delivery loop's
tick (`bury_spent`, then `lease`, queue by queue), and the environment:
send, `sendAfter`, admission refused, the run timing out in the
scheduler, app start, app answer (2xx–4xx / 5xx / none), settle write
(ok / fail), clock (forward, back), process crash, revive, prune,
redeploy (changing `M`, `L`, `D`, removing a queue). Each step is its own
action, so a fault can fall between any two.

## Findings and status

| # | Property | Finding | Shown by | Fixed |
|---|---|---|---|---|
| F1 | S5 | lowering `max_attempts` does not stop a waiting retry; `lease` alone ignores `M` | model (TLC, 10 steps); store PBT, shrunk; loop harness | `818e6fc` |
| F2 | S6, S7, S1 | a run that never started spends an attempt and can bury (scheduler timeout); a cancel's `499` is taken as done and deletes it | model (6 steps); loop harness; timeout also end to end (`tests/queue.rs`) | `b505fac` |
| F3 | S7, S6 | a failed settle write is ignored; the lease later expires as "no answer"; a failed `give_back` leaves the attempt spent | model (10 / 4 steps); loop harness | `2d99362` |
| F4 | S8, L3a | whole-second truncation: early delivery, short leases, sub-second `lease`/`retry` become 0 | loop harness | `09e0122` (D2) |
| F5 | S10 | `revive` ignores `max_depth` | model (9 steps); store PBT, shrunk | `15bbd3b` |
| F6 | L2, L1 | in-flight counted per app, room per queue: either queue can starve the other | model (TLC lasso); loop harness (a repeating cycle) | `db61025` |
| F7 | L1 | messages of an undeclared queue are never delivered nor shown | model (delivery only) | `e7b4d6a` (D6) |
| F8 | S1 | power loss may lose an acknowledged send (`synchronous = NORMAL`) | docs | `eb238f8` (D3) |
| N1 | S1, S4, S6 | a settle with no token (`None`) matches a message whose lease was *ended* (`retry`/`give_back`), not only one never leased | store PBT, shrunk; not reachable from the delivery loop | `1e0d0fd` |
| N2 | S5 | the tick reads the clock once for `bury_spent` and again for `lease`; a lease running out between them is leased past `M` with no redeploy | model (TLC, 8 steps, shortest S5 violation) | `818e6fc` |
| N3 | S7 | the lease can run out between the app's 2xx and the `done` write; `bury_spent` then buries it as "no answer within the lease" | model (9 steps) | `2d99362` (guard), `70c2dea` (D1) |
| N4 | S10, L1 | a run of the previous version still sends with that version's `max_depth`, and to queues the new version removed | model (3 steps) | `2537946` |
| N5 | S6.3, L3b | a lease that runs out while its run is still queued in the scheduler: the run's give-back can no longer match | model (with all fixes, 9 steps) | `70c2dea` (D1) |
| N6 | S6, S7 | at shutdown the engine drops queued runs before the runtime stops; a settle in between spends the attempt | reading: confirmed (below); loop harness | `815f400` |

N6, as read: `Host`'s drop calls `Engine::close`, which clears the queued
starts and the parked runs (dropping each flight and its answer's sender),
and only then `shutdown_background` on the runtime. A delivery's task
waiting on that answer can be run in between, on a runtime worker that is
still going, and it settled the missing answer as "no answer": an attempt
spent, a retry written, or the message buried on its last attempt. The
fix makes the task ask `Engine::is_closed` and write nothing, as in a
crash; the loop also starts no delivery while the host drains. The harness
shows the settle side; the race itself (between the close and the
runtime's stop) has no deterministic test.

What is left open:

- N3's guard covers a settle once it is kept; between the run's reply
  being sent and its task taking the store's lock there is still a window
  in which a tick could find the lease run out. D1 makes that window at
  least a second past the run's end, so only a host stalled that long
  meets it.
- L3b with a crash before the start of the last attempt's run (above): a
  property of crashes, not a gap the host can close.
- L3b under a host stall, and when a redeploy removes the queue: both
  exclusions are now in L3b's statement (the model found them, round 2).
- `revive` into a queue the current version does not declare is allowed
  and not bounded by any `max_depth`; the next tick buries it again. S10
  is stated for declared queues.
- S10 with a host stall on two message ids is the one search that did not
  finish (66.5M states, nothing found); the same check on B2 is complete.

Contract notes the model forced (see `spec/queue/README.md`), now part of
the contract above:

- **E7** — L1 and L3b need it once never-started runs are given back: a
  message admitted infinitely often is eventually started. Without it a
  scheduler that times the run out every time loops forever. (D5.)
- L3b does not follow from L2: L2 promises a lease, not an invocation.
  (D5; L3b's statement now says what else it needs.)
- "Current `M`" read strictly (the version at the moment of the lease) fails
  even with the S5 fix, because a redeploy can land in the middle of a tick;
  the contract's `M` is the version the tick read when it started. (D4.)
