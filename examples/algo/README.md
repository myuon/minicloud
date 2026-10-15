# The algorithm playground

A Cove app on minicloud (issue #4). Each page takes an input — written, or
one of the examples, some generated from a seed — runs an algorithm on it in
the request's own isolate, and shows the answer, an independent check of
it, what the run cost and, when a limit stopped it, which limit. The
algorithms are Cove; the host supplies the clock the app times itself by,
the limits, and nothing else. It is also the CPU-heavy real app the host's
time slice is tested against: while it computes, the other apps on the host
keep answering.

| path (below `/algo`) | what |
| --- | --- |
| `GET /` | the algorithms |
| `GET /matching` | maximum bipartite matching: the form, with the first example |
| `GET /matching?example=<name>[&algorithm=…]` | the form filled with an example; the page's script runs it at once |
| `POST /matching` (`graph`, `algorithm`) | runs a graph; the page with the result (works without scripts) |
| `GET /sat`, `GET /sat?example=<name>`, `POST /sat` (`formula`, `budget`) | satisfiability, the same way |
| `GET /anneal`, `GET /anneal?example=<name>&…`, `POST /anneal` | simulated annealing, two runs compared (fields below) |
| `…&part=result` (`GET` or `POST`) | the result alone, as HTML — what the page's script asks for |
| `GET /app.js`, `GET /style.css` | the page's script and stylesheet: files in `assets/`, answered by the host before the app runs |

`algorithm` is `hopcroft-karp` (the default), `augmenting` or `both`.

## Running it

```console
$ cargo build --profile checked
$ ./target/checked/minicloud serve --apps examples
$ open http://127.0.0.1:8080/algo/matching?example=seminars
$ curl -s 'http://127.0.0.1:8080/algo/matching?example=large&algorithm=both&part=result' \
    | grep -o 'Maximum: [0-9,]* pairs'
Maximum: 1,996 pairs
```

Every answer of a run carries the host's meter for that run in its headers
(`x-cove-run-instructions`, `-yields`, `-yields-declined`, `-parks`,
`-worker-us`, `-wall-us`), and a run a limit stopped carries `x-cove-stop`
(`deadline`, `cancelled`, `heap`, …):

```console
$ curl -s -o /dev/null -D - 'http://127.0.0.1:8080/algo/matching?example=heavy&algorithm=augmenting&part=result' \
    | grep -i -e '^HTTP' -e x-cove-stop -e x-cove-run-worker-us
HTTP/1.1 504 Gateway Timeout
x-cove-run-worker-us: 10000061
x-cove-stop: deadline
```

## The page

The form posts, so the page works without scripts. With them, `app.js`
runs the form with `fetch` instead, and that is what gives the page its
three run-time features:

- **Cancel.** The button aborts the fetch; the browser closes the
  connection, and the host cancels the run at its next safepoint (it is
  counted as `errors.cancelled`). A heavy run can always be withdrawn.
- **Why it stopped.** A run the host stopped is answered with its status, the
  runtime's diagnostic and `x-cove-stop`; the page says which limit it was
  — *Stopped: deadline. The run passed its wall-clock deadline (app.toml:
  limits.deadline)…*, or a cancellation, the heap, the queue — and shows the
  diagnostic under it. A stop the app makes itself (an input it cannot read,
  or over its bounds) is answered in the result, with the line at fault.
- **What it cost.** The "host meter" line is filled from the `x-cove-run-*`
  headers: the instructions the runtime counted, how often the run
  yielded and declined to, its time on a worker and from admission to
  answer. An app cannot read its own meter, which is why it comes from the
  headers rather than the page. The algorithms' own measures — their time on
  the app's clock and their work in edge scans — are in the page itself.

Example links fill the form and let the script run it (`data-autorun`), so a
heavy example's stop reason is shown in the page rather than instead of it.
The pages carry a CSP that allows only the app's own script and stylesheet
(`script-src 'self'; style-src 'self'` — both are files in `assets/`, so
nothing inline is allowed), only same-origin fetches and nothing else; every
name from the input is escaped wherever it lands.

## Maximum bipartite matching

### Input

```text
# a comment
a x              an edge from left vertex a to right vertex x
b: x y z         edges from b
left: a b c      the left vertices, in order (optional)
right: x y z     the right vertices, in order (optional)
random 40 40 120 7
                 a generated graph: left and right vertex counts, edges, seed
```

A vertex an edge names joins its side if it is not there yet, in order of
first appearance, so `left:`/`right:` are only needed for an isolated vertex
or an order. The two sides are separate namespaces (`left:` and `right:` are
keywords). A repeated edge counts once. `random` must be the only line that
is not a comment; it draws exactly that many distinct edges uniformly from
the `left × right` possible ones (`l1…`, `r1…`), with the playground's own
PRNG (below) — the same four numbers are the same graph on every machine
and both backends.

Bounds: at most **30,000 vertices a side and 300,000 edges** (the app's own,
answered in the page); a graph is drawn up to 40 vertices a side, and the
table view lists the first 500 pairs.

### What runs

- **Hopcroft–Karp**, O(E √V): rounds ("phases") of one breadth-first search
  that layers the graph from the free left vertices, then depth-first
  searches that augment along vertex-disjoint shortest paths in the layers.
- **Simple augmenting paths** (Kuhn's algorithm), O(V E): one search per left
  vertex, no greedy start. It is there to compare against, and it is the
  heavy one: on `heavy` it is still searching when the run's ten-second
  deadline passes (about 32 s to finish, native tier, measured on a 2026
  x86-64 Mac), where Hopcroft–Karp needs about a second.
- **`both`** runs the two on the same graph and checks that they agree.

Both searches are iterative with an explicit stack, so a long augmenting
path is not a deep Cove call stack. The work is counted the same way in
both — adjacency entries examined, "edge scans" — beside the time on the
app's clock.

**The check does not trust the algorithm.** `matching.certify` takes the
answer and verifies every pair is an edge and no vertex is used twice; then
it builds **König's vertex cover** — from the free left vertices, follow
unmatched edges right and matched edges back, and take the left vertices
*not* reached and the right vertices reached — and checks, edge by edge, that
every edge has an end in it and that it has as many vertices as the
matching has pairs. A cover of size *k* means no matching is larger than
*k*, so a matching and a cover of equal size prove each other optimal. The
page says "Maximum: N pairs" only when that check passes; the drawing marks
the cover with squares.

### The picture

The ledger's chart rules (`examples/ledger/chart`): fixed colours (the first
categorical colour for the matching, the second for the cover), text in
neutral ink, a legend, and nothing said by colour alone — matched edges are
thick and blue against thin grey edges, matched vertices are filled and
unmatched ones hollow, cover vertices are squares. Every edge and vertex has
a tooltip. The table view under it lists the pairs, the unmatched vertices
and the cover.

### Examples

| example | graph | maximum |
| --- | --- | ---: |
| `jobs` | five people, five jobs | 5 (perfect) |
| `seminars` | three students who all want the same two seminars, and a fourth: Hall's condition fails for the three | 3 of 4; the cover is {logic, algebra, s4} |
| `greedy` | `a x`, `a y`, `b x`: the greedy first choice has to be undone | 2 |
| `random` | `random 14 14 34 7` (drawn) | 12 |
| `large` | `random 2000 2000 12000 42` | 1,996 |
| `heavy` | `random 30000 30000 300000 1`, with `both`: the simple algorithm is stopped by the deadline | — (stopped: deadline; Hopcroft–Karp alone finds 30,000) |

## Satisfiability (SAT)

### Input

DIMACS CNF, or one generator line:

```text
c a comment
p cnf 3 2            variables and clauses (optional)
1 -3 0               a clause: literals, ended by 0
2 3 -1 0
random 3 60 256 7    random k-SAT: k, variables, clauses, seed
pigeonhole 6 5       pigeons, holes
sudoku 53..7....6..195....98....6.8...6...34..8.3..17...2...6.6....28....419..5....8..79
```

A clause may span lines and the last may omit its `0`; `%` ends the input
(as some benchmark files do). A repeated literal counts once and a clause
holding a literal and its negation is dropped (the page notes it when the
`p` line's count then differs). An empty clause is refused, as is a
variable above the `p` line's count. The reader scans bytes — no string is
compared — so on the native tier it has machine code (see below).

Generators:

- **`random k n m seed`**: `m` clauses of `k` distinct variables, each
  negated with probability ½, from the playground's PRNG. Near 4.26 clauses
  a variable, random 3-SAT is about as likely satisfiable as not: of
  `random 3 60 256 s`, seed 1 is satisfiable and seeds 2 and 3 are not.
- **`pigeonhole p h`**: every pigeon in a hole, no two pigeons in one
  (`p · h` variables, `p + h·p(p−1)/2` clauses). Unsatisfiable exactly when
  `p > h`, and exponentially hard for DPLL (it is resolution, and the
  principle has no short resolution proof).
- **`sudoku <81 cells>`**: variable `81r + 9c + d` is "cell (r, c) holds
  d + 1"; each cell exactly one digit, each row, column and box each digit
  exactly once (11,988 clauses), each given a unit clause.

Bounds: **5,000 variables, 50,000 clauses, 200,000 literals.**

### What runs

DPLL (`sat.solve`): **unit propagation over two watched literals** per
clause (a linked list of watch slots per literal, so a watch moves in
O(1)); **pure literals** assigned once, at the root; a **static branching
order** — the variable in the most clauses first, its more frequent sign
first, by a counting sort; **chronological backtracking** and no clause
learning. The page reports decisions, conflicts, propagations, the
deepest decision level and the pure literals.

A **decision budget** (the form's field, default 1,000,000) is the app's
own stop: past it the answer is *Unknown: the search gave up*, said in the
page. `heavy` (10 pigeons, 9 holes) fills in a budget of 50,000 decisions
and gives up at it — 8 pigeons took about 33,000, and 10 take far more — so
the page says *Unknown: the search gave up after its budget of 50,000
decisions*. A budget large enough to finish runs into the host's deadline
instead, and the page says *Stopped: deadline*.

**The check**: a satisfying assignment is evaluated against every clause
(`sat.unsatisfied`) before the page says "Satisfiable". An unsatisfiable
answer has no certificate here (DPLL without learning keeps no proof); it
is checked in the tests instead, on formulas whose answer is known.

### The picture

A satisfying assignment is drawn as a grid of squares, variable 1 top left,
forty a row: filled in the first categorical colour for true, hollow for
false (so the two differ by more than colour), each with a tooltip; up to
1,000 variables. A sudoku is drawn as its grid instead: the givens in bold
ink, the digits the solver found in the secondary ink, a legend for the
two. The table view is the DIMACS `v` line, and the formula as read (the
first 200 clauses).

### Examples

| example | formula | answer | search (native) |
| --- | --- | --- | --- |
| `tiny` | 3 variables, 2 clauses | satisfiable | 2 decisions |
| `contradiction` | all four clauses over two variables | unsatisfiable | 1 decision, 2 conflicts |
| `pigeonhole` | 6 pigeons, 5 holes | unsatisfiable | 374 decisions, 1.6 ms |
| `random` | `random 3 60 256 1` | satisfiable | 17 decisions |
| `sudoku` | the classic puzzle (30 givens) | satisfiable, one solution | 0 decisions: propagation alone solves it |
| `hard` | 8 pigeons, 7 holes | unsatisfiable | 32,780 decisions, ~170 ms |
| `heavy` | 10 pigeons, 9 holes, budget 50,000 | unknown: gave up | 50,000 decisions, ~300 ms |

## Simulated annealing

### The problem and the runs

A travelling-salesman tour through `n` points (4–500): **random points**
in the unit square from the problem seed (`pseed`), or **points on a
circle**, shuffled by the seed, whose optimum is known — around the circle,
`n · 0.9 · sin(π/n)`. Two runs, **A** and **B**, anneal the same problem
from the same tour `0, 1, …, n−1`, each with its own:

| field | what |
| --- | --- |
| `a_seed`, `b_seed` | the run's seed: every random move and acceptance comes from it |
| `a_iterations`, `b_iterations` | 1 to 50,000,000 (more than a deadline's work, on purpose: see `heavy`) |
| `a_start`, `a_end` (and `b_…`) | the temperature at the first and the last iteration, above 0, at most 1000 |
| `a_schedule`, `b_schedule` | `geometric` (the same factor each iteration) or `linear` (the same amount) |

and the problem's `problem` (`random` or `circle`), `n` and `pseed`. Every
field may be given in the query, over an example's: `?example=cooling&a_seed=9`.

Each iteration proposes a **2-opt move** — reverse the tour between two
random positions, changing its length by `d(a,c) + d(b,e) − d(a,b) − d(c,e)`
— and takes it if it is shorter, or with probability `e^(−Δ/T)` if it is
longer by Δ (a move worse by more than 40 T is not even drawn for). The
temperature is stepped, not recomputed. The best tour seen is kept, and the
page's lengths are **recomputed from the tours**, not from the running sum
of the moves; both answers are checked to be tours through every point.

The standard library has no `exp` (or `ln`, `sin`), so `anneal.exp` is
range reduction by ln 2 and the series to 1e-16, `anneal.ln` the atanh
series after scaling into [0.75, 1.5), checked against the library's values
in the tests.

**Reproducible**: the points and both runs come from the playground's
xorshift64, never the host's `random`, so `?example=cooling` gives the same
two tours, the same lengths and the same trajectories every time, on both
backends (`aSeedIsAlwaysTheSameRun`, and `algo.rs` asks twice).

### The picture

**One chart, one y-axis** — the tour length — against the **share of each
run done**, so runs of different lengths share the x-axis: each run's
current length (2 px; A solid in the first colour, B dashed in the second,
so the two differ by more than colour) and, thinner, its best so far; the
greedy nearest-neighbour tour, and on the circle the optimum, as dotted
reference lines labelled at the right in ink. A legend names each. Beside it,
both best tours drawn over the points; under it, the table view: every tenth
of the 200 recorded points of both trajectories, with the temperature.

### Examples

| example | A | B | result (native) |
| --- | --- | --- | --- |
| `cooling` | 60 points, T 0.5 → 0.001 | T 0.002 → 0.001 (quenched) | A 6.2157, B 7.2554: A shorter by 14.33%, 11.1% below greedy; ≈ 160 ms |
| `schedules` | geometric | linear, the same temperatures | A shorter by 8.84% |
| `seeds` | seed 1 | seed 2, the same settings | A shorter by 3.13% |
| `circle` | 40 points on a circle | 20,000 iterations only | both find the known optimum |
| `long` | 200 points, 200,000 iterations | seed 2 | ≈ 240 ms (1.5 s on the VM) |
| `heavy` | 500 points, 50,000,000 iterations | the same | stopped: deadline (5,000,000 each finish in 7.5 s; ten times that cannot) |

## Limits

`app.toml`, per request:

| limit | value | why |
| --- | --- | --- |
| `deadline` | 10 s | what bounds a run's work. Every example but the heavy ones takes at most a quarter of a second on the native tier and 1.5 s on the VM (annealing's `long`; SAT's `hard` 0.2 s and 0.9 s); matching's and annealing's `heavy` ask for several times the deadline's work on purpose, and are stopped by it. A run parked or queued past it is stopped too |
| `max_heap_words` | 4 Mi words (32 MiB, the runtime's default) | a 30,000 × 30,000 graph with 300,000 edges and its working arrays need more than 2 Mi words; a formula at its bounds is well inside it |
| `max_in_flight` | 4 | at most four runs of this app at once, on any number of workers: a burst of heavy runs cannot take every worker of a larger host |
| `max_queued` | 32 | past it, 429 for this app only |
| `max_request_bytes` | 256 KiB | a written graph at the edge bound fits; a DIMACS file larger than that is over the literal bound anyway |
| `max_response_bytes` | 4 MiB | the largest page (500 table rows) is far inside it |
| `max_host_calls` | 64 | the clock is read four times a run |

The app's own bounds (vertices, edges; variables, clauses, literals; the
decision budget) are checked before anything runs and answered in the page.

## Reproducibility: the PRNG

`rng` is xorshift64 (Marsaglia 2003): one 64-bit word of state, three
shifts and xors, no multiplication — Cove's `*` stops the run on overflow,
and shifts and `bitXor` read an `Int` as a word. A seed is mixed with the
golden-ratio word and the generator is turned eight times before use. Its
first outputs from Marsaglia's state are checked against the published
sequence (`rng_test.cove`). The host's `random` is not used anywhere: it
cannot be replayed.

## Tests

```console
$ ./target/checked/minicloud test --apps examples algo     # the Cove test fns
$ cargo test --profile checked --test algo              # the app on a host
```

- `matching_test.cove`: the examples' known maxima, each by both algorithms
  and proved by the certificate; `seminars`' cover is exactly the one its
  comment names; on 60 small random graphs both agree with an exhaustive
  search; on larger ones they agree and are proved; a non-maximum matching
  and a matching with a non-edge are *not* proved; a seed is always the same
  graph with exactly the edges asked for; the input format and every refusal.
- `sat_test.cove`: small formulas with known answers (and the unique model
  of one); the pigeonhole principle for 1–5 holes, both ways; on 80 random
  3-SAT formulas (3–10 variables, 35 of them satisfiable) the solver agrees
  with trying all 2^n assignments, and every model it gives satisfies every
  clause; a seed is always the same formula, with distinct variables per
  clause; the sudoku's known first row, and a sudoku with two fives in a row
  unsatisfiable; the decision budget; every refusal, with the line.
- `anneal_test.cove`: `exp`, `ln`, `sin`, `cos` against the library's
  values; the schedules' ends and midpoints; a run's answer is a tour of the
  length it reports, its trajectory starts at the start and ends at the end
  and its best never rises; a seed is always the same run and the same
  points; on 24 shuffled points on a circle it finds the known optimum; on
  four random 7-point problems it finds the optimum an exhaustive search
  finds; four corners of a square, the perimeter; the greedy tour is a tour.
- `rng_test.cove`: Marsaglia's sequence, seeds, ranges.
- `crates/minicloud/tests/algo.rs`: the known answers through the page,
  for all three algorithm choices, with the proof; the meter headers; CSP,
  the script, escaping of hostile vertex names, refusals; matching's and
  annealing's `heavy` answered `x-cove-stop: deadline` (under a deadline
  of a second, so the test does not wait ten); SAT's
  known answers through the page, its models re-checked in Rust against the
  formula as the page prints it, the sudoku's first row, the budget's
  *Unknown*, SAT's `heavy` giving up at its own budget of 50,000
  decisions; annealing's comparison (the
  hot start beats the cold one), reproduced exactly on a second request, a
  reseeded run changed and the other not, the chart's two series and
  references, the circle's optimum found, and every refusal; a client
  that goes away cancels its heavy run (`errors.cancelled`, nothing in
  flight after); and **the responsiveness test** below, on the VM and on the
  native tier; and that every function of the app has machine code on the
  native tier.

## Responsiveness

**Tested** (`algo.rs::the_other_apps_answer_while_algo_computes_on_*`, no
duration asserted): a host with **two workers** and **four clients**, two
asking for the simple matching algorithm on `large`, one for SAT's `hard`
(DPLL refuting 8 pigeons in 7 holes) and one for annealing's `cooling`, each
again as soon as it is answered;
while at least one heavy run is in flight, ten `hello` requests and ten
webhooks posted to the webhook lab all complete, the heavy answers all agree,
and the heavy runs yielded (`yields > 0`). On the VM and on the native tier.

**Measured** (`sh bench/algo.sh 3 <backend>`, four workers, `hello` open
loop at 500 req/s, latency from the intended start; K heavy clients closed
loop on `example=large&algorithm=augmenting`):

2026-10-05, i7-10700K (8 cores, 16 threads), macOS 26.6.2, `--profile
checked`, Cove at 9272282, load average 3–7 with other work on the machine.
Medians of three repetitions, 5,000 `hello` requests each; raw output in
`bench/results/algo-2026-10-05-{native,vm}.txt`.

| tier | heavy clients | `hello` p50 | `hello` p99 | heavy runs answered in the 10 s | yields per heavy run | declined per run | overdue |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| native | 0 | 1.18 ms | 2.04 ms | — | — | — | — |
| native | 4 | 2.84 ms | **4.68 ms** | ~410 | 32 | ~290 | 0 |
| native | 8 | 3.18 ms | **4.95 ms** | ~410 | 34 | ~290 | 0 |
| VM | 0 | 1.21 ms | 2.00 ms | — | — | — | — |
| VM | 4 | 2.84 ms | **4.65 ms** | ~64 | 227 | 0 | 0 |
| VM | 8 | 2.66 ms | **4.40 ms** | ~66 | 220 | 0 | 0 |

The same with SAT's `hard` as the heavy run (`MIX=algo-sat HEAVY="0 4" sh
bench/algo.sh 3 <backend>`; `bench/results/algo-sat-2026-10-05-*.txt`):

| tier | heavy clients | `hello` p50 | `hello` p99 | heavy runs answered in the 10 s | yields per heavy run | declined per run | overdue |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| native | 0 | 1.24 ms | 2.06 ms | — | — | — | — |
| native | 4 | 2.70 ms | **4.52 ms** | ~250 | 57 | 0 | 0 |
| VM | 0 | 1.15 ms | 2.03 ms | — | — | — | — |
| VM | 4 | 2.65 ms | **4.45 ms** | ~48 | 302 | 0 | 0 |

And with annealing's `cooling` as the heavy run (`MIX=algo-anneal`;
`bench/results/algo-anneal-2026-10-05-*.txt`):

| tier | heavy clients | `hello` p50 | `hello` p99 | heavy runs answered in the 10 s | yields per heavy run | declined per run | overdue |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| native | 0 | 1.67 ms | 2.49 ms | — | — | — | — |
| native | 4 | 2.57 ms | **4.43 ms** | ~266 | 52 | ~6 | 0 |
| VM | 0 | 1.76 ms | 2.58 ms | — | — | — | — |
| VM | 4 | 2.54 ms | **4.29 ms** | ~48 | 302 | 0 | 0 |

(The few declined yields per annealing run were polls while the clock's
encoded leaf, `now()`, ran; that leaf is gone since Cove 2ca1c94, below.)

**Re-measured at Cove 2ca1c94 with the workarounds removed** (the same
commands, native, three repetitions; `bench/results/*-2026-10-05-native-2ca1c94.txt`,
and the control — 2ca1c94 with the workarounds still in —
`algo-2026-10-05-native-2ca1c94-with-workarounds.txt`; load average 3–6):

| heavy run | heavy clients | `hello` p99, control | `hello` p99, workarounds removed | heavy runs in the 10 s | yields per run | declined per run | overdue |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| matching | 0 | 2.56 ms | 2.47 ms | — | — | — | — |
| matching | 4 | 4.71 ms | **4.90 ms** | ~424 (control ~452) | 33 | 0 | 0 |
| matching | 8 | 4.34 ms | **4.81 ms** | ~426 | 33 | 0 | 0 |
| SAT `hard` | 4 | — | **5.00 ms** | ~266 | 53 | 0 | 0 |
| annealing `cooling` | 4 | — | **4.11 ms** | ~280 | 51 | 0 | 0 |

`hello`'s p99 stays under 5 ms beside every heavy mix, and no yield is
declined or overdue: with nothing on the encoded tier there is no frame left
to decline one. The matching run is about 6% slower than with the
hand-written merge sort (~424 runs in the 10 s against ~452, the same worker
time): `sorted(by:)` calls its closure for every comparison.

With every worker held by a heavy run, a `hello` waits for the next yield —
at most one 2 ms slice plus a tick of the monitor — and its p99 goes from 2
to under 5 ms. Eight heavy clients are no worse than four: the app's
`max_in_flight = 4` keeps the other four queued in *its own* queue, where
they cost `hello` nothing. The heavy matching run is about 6.5 times faster on
the native tier (≈ 100 ms against ≈ 620 ms on a worker), the DPLL run about
5 times (≈ 180 ms against ≈ 940 ms). On the native tier the SAT runs decline
no yield at all: everything from the entry to the solver's loop has machine
code (the request is a `GET`, so no form is decoded).

```console
$ cargo build --profile checked
$ sh bench/algo.sh 3 native > bench/results/algo-$(date +%F)-native.txt
$ sh bench/algo.sh 3 vm > bench/results/algo-$(date +%F)-vm.txt
$ MIX=algo-sat HEAVY="0 4" sh bench/algo.sh 3 native > bench/results/algo-sat-$(date +%F)-native.txt
$ MIX=algo-anneal HEAVY="0 4" sh bench/algo.sh 3 native > bench/results/algo-anneal-$(date +%F)-native.txt
```

## Yields on the native tier

**Fixed upstream; the workarounds are gone.** Writing this app found three
shapes of Cove code that stopped a compiled run from yielding, one that
made a sliced compiled run answer wrongly, and a loop that never yielded.
All were reported to Cove (cove#604, cove#605) and fixed there, and since
cove-tools pins Cove 2ca1c94 the app is written the plain way again. What was found, briefly — the
measurements are in this file's history:

- **A host call above a heavy loop.** The template code generator did not
  lower `CallHost`, so `timed`, which read `time.nowMicros()` around an
  algorithm, stayed on the encoded tier with compiled code above and below
  it; compiled code below an encoded frame cannot yield (ADR 0085), and the
  heavy run held its worker to the end. Measured with four heavy clients on
  four workers: `hello`'s p99 **10.2 s** (against 2 ms), 624 of 625 yield
  requests overdue, 72 million declined
  (`bench/results/algo-2026-10-05-native-before-leaf-clock.txt`). The
  workaround was a leaf `now()` in `pages.cove`. **Fixed** by Cove #610 (ADR
  0087): compiled code calls the host, and may park there. The pages call
  `time.nowMicros()` in place.
- **A lambda** (`FuncRef`) did the same, so the graph generator's sort was a
  hand-written merge sort (`matching.distinctSorted`). **Fixed** by Cove
  #609: closures are made and called in machine code. It is
  `sorted(by:)` again.
- **A `String` `!=` or `<`** (only `==` was lowered) did the same, so the
  annealing page wrote `!(a == b)` and the SAT reader compared
  `byteLength() > 0` and used `startsWith` for the generator's name.
  **Fixed** by Cove #608: every `String` comparison runs as machine code. They
  are `!=`, `!= ""` and `==` again.
- **A wrong answer: a copy that was sliced.** A native run sliced while it
  copied an array into a vector (`Array.toVector`, `Vector.snapshot`) could
  resume with a store of the wrong size — `runCopy writes 12000 element(s)
  to 0 of a destination of 0`, or `this run has no memory left` — in 2 of 400
  heavy matching runs under load, and 10 to 28 of 160 in `bench/repro/`'s
  70-line reproduction (0 with `--slice 0`, 0 on the VM). The workaround
  copied element by element (`matching.copyOf`, the `drawn` loop). **Fixed**
  by Cove #607 (ADR 0086): resumed allocation reads its frame, and a yield
  request makes compiled code poll. The generator uses `toVector()` and
  `snapshot()` again. The same ADR covers the fifth finding, a compiled loop
  of one `toVector()` and a length check that was asked to yield 22 times
  and neither yielded nor declined: a yield request now makes compiled code's
  next poll due.

**Held now**: `algo.rs::the_heavy_path_has_machine_code_on_the_native_tier`
asserts that `/_host/apps/algo` reports **no** native refusal at all, so a
function that falls back to the encoded tier above a heavy loop — a
regression upstream, or a new shape — fails a test rather than a latency
percentile. `bench/repro/run.sh native` is kept as the copy fault's
regression check: at 2ca1c94 it answers **160 of 160** right (three runs,
about 3,150 yields each), and it exits non-zero otherwise; CI runs it on the
native tier.

**What remains, upstream:**

- **ADR 0085's limit itself**: compiled code below an encoded frame that
  compiled code called — a nested encoded segment — still cannot yield. Nothing
  in this app is encoded any more, so it does not arise here, but a function
  the code generator refuses for any other reason would bring it back, which
  is what the test above is for.
- **cove#606**: starvation on the VM tier. Not met by this app's
  measurements (VM rows above), but it is open.

## Cove gaps met while writing it

For upstream (myuon/cove), besides the native-tier shapes above (now fixed):

- **No `Float.exp`/`ln`/`sin`/`cos`** in the standard library (`sqrt` is
  there): the annealing's are written in Cove (`anneal.exp`, `anneal.ln`,
  `anneal.sine`), and cost a loop of 20–30 multiplications each.
- **An `if` in statement position must still have matching branch types**:
  `if a { x += 1 } else { v.set(i, 0) }` is refused because `set` answers an
  `Option`. Reordering the branch so it ends in a `Unit` statement is the
  workaround everywhere in this app.
- **An element type inferred only from later use can pass `check` and fail
  lowering** with an empty diagnostic: `var levels = Vector.of()` used by
  `pop()` before any `push` checked, then `minicloud check` reported "does
  not lower:" and nothing else (the test runner said "the type of this
  expression was never settled `_`" without a location). Writing
  `Vector<Int>` on the binding fixes it.
- **`freeze()` cannot see through a helper that returns a fresh vector**
  (`var a = filled(n, -1)` … `a.freeze()` is refused), so the algorithms
  answer with `toArray()`, an O(n) copy; and there is no
  `Vector.filled(n, value)` to make one.

## Issue #4's completion criteria

| criterion | where it is shown |
| --- | --- |
| results verified on small known problems | matching: `matching_test.cove` (known maxima, 60 graphs against exhaustive search, König's certificate checked edge by edge, non-maximum matchings refused) and `algo.rs::the_examples_have_their_known_maximum_and_a_proof`; SAT: `sat_test.cove` (80 formulas against all 2^n assignments, pigeonhole 1–5 holes, the sudoku's known solution, every model checked) and `algo.rs::sat_answers_known_formulas_and_checks_its_models`; annealing: `anneal_test.cove` (the circle's known optimum, 7-point problems against exhaustive search, the square) and `algo.rs::annealing_compares_two_runs_reproducibly` |
| execution limits work, and say which stopped a run | `algo.rs::a_run_a_limit_stops_says_which_limit` (the deadline, on matching's and annealing's `heavy`), `::a_sat_example_too_hard_for_its_budget_gives_up_and_says_so` (the app's own decision budget); the app's own bounds answered in the page; [The page](#the-page) (`x-cove-stop` shown as *Stopped: deadline* and so on) |
| cancellation works | `algo.rs::a_client_that_goes_away_cancels_its_run` (the connection closed mid-run: `errors.cancelled`, nothing in flight after); the page's Cancel button aborts the fetch, which is that |
| the webhook lab and a light app answer while it computes | `algo.rs::the_other_apps_answer_while_algo_computes_on_the_vm` / `_on_the_native_tier` (two workers, four heavy clients of all three algorithms, twenty requests to `hello` and `webhooks` all answered, the heavy runs yielded); [Responsiveness](#responsiveness): `hello`'s p99 4.3–5.0 ms beside them on both tiers |
| input examples and reproduction steps | every page's examples (seeded generators, so each is the same input everywhere); [Running it](#running-it), the input formats above, `bench/algo.sh`, `bench/repro/run.sh` |
| shapes that stop park/yield on the native tier recorded and returned as runtime issues | [Yields on the native tier](#yields-on-the-native-tier): a host call (`CallHost`), a lambda (`FuncRef`) or a `String` `!=`/`<` in a function above a heavy loop kept that loop from yielding (measured: `hello` p99 10.2 s, 624 overdue yields), and a sliced native run's `toVector`/`snapshot` could resume with a wrong-sized store (`bench/repro/`); reported as cove#604/#605 and fixed in Cove 2ca1c94, so the app's workarounds are removed. `/_host/apps/<app>`'s `native.refusals` names such functions; `algo.rs::the_heavy_path_has_machine_code_on_the_native_tier` asserts there are none; `bench/repro/run.sh` runs in CI |
