# minicloud: measurements

## Throughput and latency against the edge sample and Go

`bench/perf.sh` runs `minicloud-load` (a small load generator in this crate)
against a host with four workers — the configuration of the Cove repository's
edge/Go comparison (`examples/edge/compare/README.md`, cove #589/#593) —
using the same method: open loop with latency measured from each request's
**intended** start (no coordinated omission), and closed loop for capacity.
[`bench/README.md`](../bench/README.md) has the conditions, the commands, the
full tables and the comparison; in short, on the same machine
(i7-10700K, macOS, native tier):

| | minicloud | edge (native where measured) | Go |
| --- | ---: | ---: | ---: |
| `hello` capacity, 64 in flight | 110,671 req/s | 84,235 (VM) | 122,725 |
| `crunch n=20000` capacity, 16 in flight | 4,809 req/s | 2,783 | 3,720 |
| `cpu-io` mix capacity, 256 in flight | 1,831 req/s | 1,044 | 1,364 |
| `hello` p99 inside the mix at 990 req/s | 2.3 ms | 13.5 | — |

The HTTP stacks differ (hyper on tokio here; the edge sample's own std
HTTP/1.1 with a `poll(2)` idle thread; Go's `net/http`), the load generators
differ, and the edge `crunch` row predates the native tier's 32-bit division
(cove #596), which this host's Cove revision has. So these say *no
regression* rather than *faster than Go*. The check that matters for the
scheduler: **`hello`'s p99 inside the CPU+I/O mix stays at 2.3–3.0 ms from
330 to 1,434 req/s** (78% of the mix's capacity), against 2.6 ms for `hello`
alone; past saturation, the 2 ms slice keeps it at 95 ms against 139 ms
without one.

Beside a real CPU-heavy app — the algorithm playground's matching runs
holding all four workers — `hello`'s p99 is 4.7 ms on the native tier and
4.7 ms on the VM, against 2.0 ms alone (`bench/algo.sh`,
[`examples/algo/README.md`](../examples/algo/README.md#responsiveness)).

```console
$ cargo build --profile checked
$ sh bench/perf.sh 3 > bench/results/perf-$(date +%F).txt
$ SLICE=0 ONLY=mix sh bench/perf.sh 3 > bench/results/perf-$(date +%F)-noslice.txt
$ python3 bench/summarize.py bench/results/perf-*.txt
```

## The cost of a `kv` call against a park

**Why every call answers at once, on the worker**: a call into a local
SQLite is cheaper than parking. Measured on the macOS x86-64 development
machine, `--profile checked`: a `put` 34 µs, a `get` 5 µs, a list of 50 entries
250 µs (`cargo t --lib -- --ignored kv_call_cost --nocapture`), against **15
µs** for a park and resume end to end (`cargo t --test host -- --ignored
park_cost --nocapture`: a request of sixteen zero-length sleeps against one).
Parking a `get` would triple its cost, and parking a `put` would only move
its write to another thread. The call runs on the worker, never on the
async runtime's threads, so HTTP is not blocked behind a write; a worker held
34 µs is far inside a 2 ms slice.

## What the request timeline costs

**Nothing measurable: under 1% of `hello`'s capacity, the worst case, and
within the spread between rounds.** The same macOS x86-64 development
machine (i7-10700K, load average about 1.6), `--profile checked`, the host
as `bench/perf.sh` starts it (four workers, a 2 ms slice, the samples'
limits raised), `minicloud-load` closed loop with keep-alive. Five rounds,
each alternating a host built from `main` (v0.31.0, `de01067`) and one with
the timeline (`cc3b8ec`), each started afresh and warmed with 20,000
requests first:

| | `main` req/s | timeline req/s | difference (per round) |
| --- | --- | --- | ---: |
| `hello`, 64 in flight, 200,000 requests | 98,330 / 100,657 / 98,760 / 97,945 / 100,745 (mean 99,287) | 99,547 / 98,608 / 98,275 / 97,629 / 98,898 (98,591) | −0.7% (+1.2, −2.0, −0.5, −0.3, −1.8) |
| `hello`, 256 in flight, 200,000 requests | 97,829 / 99,719 / 96,226 / 96,023 / 98,009 (97,561) | 98,409 / 98,404 / 97,113 / 96,191 / 97,640 (97,551) | 0.0% (+0.6, −1.3, +0.9, +0.2, −0.4) |
| `--mix cpu-io`, 256 in flight, 3,000 requests | 1,909 / 1,910 / 1,895 / 1,891 / 1,774 (1,876) | 1,909 / 1,909 / 1,896 / 1,880 / 1,872 (1,893) | +0.9% (0.0, −0.1, +0.1, −0.6, +5.5) |

`hello`'s p99 was 0.99–1.12 ms on `main` and 1.01–1.04 ms with the
timeline at 64 in flight, 3.96–4.27 ms and 4.02–4.13 ms at 256, and inside
the mix 52–56 ms and 50–68 ms. `hello` is the worst case: nothing to wait
on, a run's start and end its only events, and a host whose two HTTP
threads are the bottleneck — `sample(1)` of the host under that load has
them 95% busy and the workers half idle. In that profile the recorder is
0.4–0.8% of the HTTP threads' busy time (admitting a request: one latch, a
record refilled) and 0.6–0.9% of the workers' (a run's start and end, and
trimming the ring to its bounds).

The first version cost 4–5% on `hello` (95,700 against 100,700 req/s), and
it took four changes to get here, each measured the same way: the record's
lock became a spin latch (`std::sync::Mutex` on macOS is a pthread mutex
boxed on first use, an allocation and a destroy per request); forgetting
old records moved from the HTTP side, where it freed them, to the worker
that ends a request, after its answer; a forgotten record that nobody holds
is refilled by the next request instead of freed, and the app is recorded
as its slot's index rather than a shared `Arc<str>`; and a request's
tracker holds its record and not the recorder, whose count every request
would have taken on the HTTP side and given back on a worker (1.5% before
that last change, 5 rounds). [`bench/ab.sh`](../bench/ab.sh) is the
interleaving, a fresh host for each build in each round:

```console
$ git switch main && cargo build --profile checked && cp target/checked/minicloud /tmp/main/
$ git switch - && cargo build --profile checked && cp target/checked/minicloud /tmp/timeline/
$ sh bench/ab.sh /tmp/main /tmp/timeline 5
```
