# The queue's properties

What the queue (`crates/minicloud/src/queue.rs`, `queue_host.rs`) promises,
stated so that a model checker and tests can hold the code to it (issue
#168). Each property says three things apart:

- **Contract** — what the queue is meant to promise. Where the code does
  not keep it, the contract is not weakened to fit; the gap is a finding.
- **Implementation** — what the code at `e39f40e` does.
- **Proposed fix** — what would close the gap, where there is one.

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
  started under.
- **Process crash**: the host process dies; committed SQLite transactions
  survive, every delivery and every in-memory counter is gone.
  **Power loss**: the machine stops; with WAL and `synchronous = NORMAL`,
  the last committed transactions may be lost too.

## Safety

### S1. A message is not lost

**Contract.** Under process crashes: a message whose `send` answered `Ok`
stays on `message` or `dead` until one of (a) `done` by a delivery holding
its current lease, after the app answered below 500; (b) `prune_dead` of
its dead letter. Nothing else removes it.

Power loss is a separate, weaker promise, and the code's own docs make it:
a transaction committed shortly before power loss may be undone, so an
acknowledged `send` may be lost (and a `done` undone, which only repeats a
delivery). The model checks S1 under process crash only; power loss is
recorded, not checked.

**Implementation.** `done` deletes only on a matching token; `bury` and
`bury_spent` move to `dead` in one transaction; `revive` moves back in one.
A queue removed from `app.toml` keeps its messages on `message` — not
lost, but see L1.

**Proposed fix.** None for crashes. For power loss: decide whether
`synchronous = FULL` for the queue store is worth its cost (open question
for the owner).

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

### S5. At most `M` deliveries per life

**Contract.** A lease is issued only when the message's `attempts` is below
the **current** `M`. So lowering `M` limits what happens next: a message
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
the message. The same for a run cancelled before it starts (`499`).
**Violation by reading**, to be reproduced.

**Proposed fix.** The delivery learns that the run never started (a flag
on the reply, or the `ErrorKind`) and gives the message back instead.

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

## Liveness

Liveness is promised only under assumptions, which are part of the
contract:

- **E1** From some point on, the app is enabled and loaded, and the
  queue is declared by its current version, forever.
- **E2** The host's clock grows without bound; the delivery loop ticks
  (weak fairness).
- **E3** Every admitted run ends (it is bounded by its deadline): weak
  fairness on a run's end.
- **E4** Admission is not refused forever: if a delivery is tried
  infinitely often, it is admitted infinitely often (strong fairness).
- **E5** A settle that is retried is eventually written (no permanent
  store failure).
- **E6** Only finitely many crashes, revives and redeploys.

### L1. Every message is settled

**Contract.** Under E1–E6, every message on `message` eventually leaves
it (done, or a dead letter).

**Implementation.** Messages of a queue the current version no longer
declares are never looked at: E1 excludes them, so the contract does not
cover them, but nothing tells anyone — the admin page should list them.
Otherwise expected to hold; to be checked with a complete (TLC) search.

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

### L3. Exactly once when nothing goes wrong

**Contract.** Split in two:
- **L3a (safety)** If, for a message, every delivery is admitted, its run
  answers below 500, the answer's settle is written before the configured
  lease has run out, no crash happens, the clock does not jump, and nobody
  revives it or redeploys the queue — then the app is invoked for it at
  most once.
- **L3b (liveness)** Under E1–E6, it is invoked at least once (from L2).

A 5xx, or a crash after the app answered and before the settle was
written, is outside the hypothesis: the queue is at-least-once, and those
are where the second delivery comes from.

**Implementation.** The one-second truncation (S8) makes the real lease up
to a second shorter than configured, so a delivery that answers within
`lease` may still be handed out again. **Violation by reading.**

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
| F1 | S5 | lowering `max_attempts` does not stop a waiting retry | reading | |
| F2 | S6, S7 | a run that never started (scheduler timeout / cancel) spends an attempt and can bury | reading | |
| F3 | S7 | a failed settle write is ignored; the lease later expires as "no answer" | reading | |
| F4 | S8, L3a | whole-second truncation: early delivery, short leases, sub-second `lease`/`retry` become 0 | reading | |
| F5 | S10 | `revive` ignores `max_depth` | reading | |
| F6 | L2 | in-flight counted per app, room per queue: one queue starves another | reading | |
| F7 | L1 | messages of an undeclared queue are never delivered nor shown | reading | |
| F8 | S1 | power loss may lose an acknowledged send (`synchronous = NORMAL`) | docs | open question |
