# The bench ledger

A Cove app on minicloud (issue #3). Benchmark runs are **posted as JSON**,
checked, kept in the app's key-value store, and shown as pages: the run list
and each run in full. Validation, unit conversion, the stored layout and the
HTML are Cove; the host supplies the store, the log and the credential check.

| path (below `/ledger`) | what | auth |
| --- | --- | --- |
| `POST /api/runs` | stores a run (`content-type: application/json`). `201` stored, `200` the same run was already stored, `400` with every problem, `409` another run has this ID | `post` secret |
| `DELETE /api/runs/<id>` | deletes a run and everything kept for it | `post` secret |
| `GET /api/runs`, `GET /api/runs/<id>` | the run list's summaries, and a run as stored, as JSON | none |
| `GET /` | the runs, newest measured first, 50 a page (`?before=`; `?format=json`) | none |
| `GET /runs/<id>` | one run: where and how it was measured, and every result's median and spread (`?format=json`: as stored) | none |
| `GET /cases`, `GET /cases/trend?case=…&input=…` | every case measured; one case's trend over runs, a chart per metric with a table view (`metric=`, `format=json`) | none |
| `GET /compare?a=…&b=…` | two runs, or the runs of two commits (`commit:<sha>`), case by case: absolute and relative differences, the spread on each side, and whether the two may be compared at all (`metric=`, `all=1`, `format=json`) | none |

Reading is open — a ledger of benchmark numbers is meant to be looked at;
put the host behind something that asks for a login if yours is not.

## Posting a run

```console
$ export LEDGER_TOKEN=change-me            # the post secret; the app is refused without it
$ ./target/checked/minicloud serve --apps examples
$ curl -s -H "authorization: Bearer $LEDGER_TOKEN" -H 'content-type: application/json' \
    --data @examples/ledger/samples/cove-593-capacity.json http://127.0.0.1:8080/ledger/api/runs
{"ok":true,"results":12,"run":"cove-593-capacity","stored":true,"url":"/ledger/runs/cove-593-capacity"}
$ curl -s -H "authorization: Bearer $LEDGER_TOKEN" -H 'content-type: application/json' \
    --data @examples/ledger/samples/cove-593-capacity.json http://127.0.0.1:8080/ledger/api/runs
{"duplicate":true,"ok":true,"run":"cove-593-capacity","stored":false}
```

Then open `http://127.0.0.1:8080/ledger/`. Stop the host and start it again:
the runs are still there, in `data/ledger/kv.sqlite3`.

### The sample data: the existing Cove results

`examples/ledger/samples/` holds the existing measurements converted to runs —
22 of them — and `importer/ledger_import.py` is what converted them and what
posts them:

```console
$ python3 examples/ledger/importer/ledger_import.py post --url http://127.0.0.1:8080/ledger examples/ledger/samples/*.json
201 examples/ledger/samples/cove-589-capacity.json: {"ok":true,"results":20,…}
…
$ python3 examples/ledger/importer/ledger_import.py convert --cove ../cove --tools . --out examples/ledger/samples   # to convert again
```

| runs | from | commit |
| --- | --- | --- |
| `cove-589-capacity`, `-sweep`, `-connections`, `-waiting` | cove `examples/edge/compare/results/*.jsonl`: #589, the edge server against the same service in Go (VM and Go) | `fcf64a9` |
| `cove-593-capacity`, `cove-593-sweep` | `results/native/*.jsonl`: #593, the native tier re-measured against the VM and Go | `11ce12f` |
| `cove-595-ab-<experiment>-<build>` (13), `cove-595-idle-cost` | `results/ab-*.jsonl`, `idle-cost.jsonl`: #595's A/B rounds of scratch builds, one run per build (`base` is `fcf64a9`; `sock`, `wake` and `kqueue2` are the branches later merged as #597, #598 and #599) | per build |
| `cove-host-perf-2026-10-05`, `-noslice` | this repository's `bench/results/perf-*.txt` (`bench/perf.sh`, minicloud under the same kinds of load) | `00b397e` |

The converter reads exactly those formats; it is not a general importer. A
new kind of result gets a converter there, or its script writes the schema
below itself. The edge rows carry no time of day, so #589's runs carry the
time of the commit that added them (each run's `note` says so).

## The schema

```json
{
  "schema": 1,
  "id": "cove-593-capacity",
  "repository": "myuon/cove",
  "commit": "11ce12f",
  "measuredAt": "2026-10-05T02:09:26+09:00",
  "source": "examples/edge/compare/results/native/capacity.jsonl",
  "note": "anything worth knowing about the run",
  "environment": {"cpu": "Intel i7-10700K, 8 cores / 16 threads", "memory": "32 GB", "os": "macOS 26.6.2", "arch": "x86_64"},
  "toolchain": {"rustc": "1.98.1", "go": "1.23.2", "profile": "release"},
  "conditions": {"server": "cove-edge", "workers": "4", "generator": "cove-edge-load"},
  "results": [
    {
      "case": "crunch",
      "input": "c=16",
      "inputSize": 16,
      "backend": "native",
      "conditions": {"latency": "from the send"},
      "load": [2.6, 2.6, 2.5],
      "metrics": {
        "throughput": {"unit": "req/s", "values": [2769.67, 2783.08, 2784.37]},
        "p99": {"unit": "ms", "values": [8.86, 8.8, 8.7]},
        "memory": {"unit": "KiB", "values": [33212, 32412, 32161]}
      }
    }
  ]
}
```

- **Required**: `id` (letters, digits, `.`, `_`, `-`; at most 100 — it is
  part of a URL), `repository`, `commit`, `measuredAt` (ISO 8601 with a zone),
  `environment` (at least one entry) and `results` (1–5000). Each result
  needs `case`, `backend` and `metrics`; each metric `unit` and `values`.
- **Optional**: `schema` (1), `source`, `note`, `toolchain`, `conditions`
  (run-wide; a result's own are laid over them), and per result `input` (a
  label), `inputSize` (a number) and `load` (the load average before each
  repetition). A metric may say `"better": "higher"` or `"lower"`; by default
  a rate is better higher and everything else lower.
- **`values` are the raw repetitions**, one number each, and are stored as
  sent. The page shows their median with the least and the most.
- `environment`, `toolchain` and `conditions` are text (`"4"`, not `4`):
  they are compared as written.
- A case is measured once per run on each backend at each input: the same
  `case`, `input` and `backend` twice is an error (the repetitions go in
  `values`).

### Validation

A post that breaks any rule stores nothing and is answered `400` with
**every** problem, each with the path of the field it is about:

```console
$ curl -s -H "authorization: Bearer $LEDGER_TOKEN" -H 'content-type: application/json' \
    -d '{"id":"x y","repository":"r","commit":"c","measured_at":"now","environment":{"cores":8},
         "results":[{"case":"a","backend":"vm","metrics":{"p99":{"unit":"mss","values":[1,null]}}}]}' \
    http://127.0.0.1:8080/ledger/api/runs
{
  "error": "the run was not stored: 6 problem(s)",
  "ok": false,
  "problems": [
    { "message": "unknown field; the fields here are `schema`, `id`, …", "path": "measured_at" },
    { "message": "a run ID is letters, digits, `.`, `_` and `-` (it is part of a URL)", "path": "id" },
    { "message": "missing: `measuredAt` is required", "path": "measuredAt" },
    { "message": "write it as text (`\"4\"`): these are compared as written", "path": "environment.cores" },
    { "message": "`mss` is not a unit this ledger knows; it knows `ns`, `us`, …", "path": "results[0].metrics.p99.unit" },
    { "message": "a repetition that was not measured is left out, not written `null` (and not 0)", "path": "results[0].metrics.p99.values[1]" }
  ]
}
```

Unknown fields are refused (a misspelt `measured_at` would otherwise be a run
with no time), and so is a JSON object that names a field twice.

### Units

Every value is kept **as sent** and, beside it, **in the canonical unit of
its dimension**, which is what pages show and what is compared:

| dimension | canonical | accepted |
| --- | --- | --- |
| duration | `ms` | `ns`, `us`, `µs`, `ms`, `s`, `min` |
| rate | `/s` | `/s`, `req/s`, `ops/s`, `req/min` |
| memory | `MiB` | `B`, `KiB`, `MiB`, `GiB`, `kB`, `MB`, `GB` |
| count | `count` | `count` |
| percent | `%` | `%` |

A metric keeps the dimension it was first posted with: once `p99` is a
duration, a `p99` in `KiB` is refused (`400`, naming the field), so that a
chart of `p99` never mixes times and sizes. The page's tooltip on a value
shows the unit and numbers that were sent.

### Unmeasured is absent, never zero

A value that was not measured is **left out**: the metric is absent from the
result, or the repetition from `values`. `null` and an empty `values` are
refused, because either would have to become something, and a zero is a
measurement. On the pages an absent metric is a dash (`–`, "not measured"),
and a measured zero is `0`.

### Duplicates

The run ID is what makes posting safe to repeat. A post whose ID is already
stored is compared with what is stored, after normalisation (so formatting
and field order do not matter):

- **the same run** is answered `200` with `"duplicate": true` and changes
  nothing — a CI job that retries, or posting a whole directory again, is
  harmless;
- **a different run** is answered `409`, saying how it differs (`` `commit`
  is `abc1234`, not `def5678` ``, `results[3] (…) differs`). Post it under a
  new ID, or `DELETE /api/runs/<id>` first.

Two posts of different content racing under one new ID are not detected:
the store has no compare-and-set, and the last one wins.

## Comparing

`/compare` takes two sides, **A** (the baseline) and **B**, from the form or
the query: a run ID, or `commit:<sha>` (a prefix of seven or more works) for
every run of a commit — where two runs of the commit measured the same case,
the one measured later counts. A run's page links to `/compare?a=<id>`.

```console
$ open 'http://127.0.0.1:8080/ledger/compare?a=cove-589-capacity&b=cove-593-capacity'
$ curl -s 'http://127.0.0.1:8080/ledger/compare?a=commit:fcf64a9&b=commit:bb13e6f&format=json'
```

Cases are matched by `case`, `input` and `backend`. For each metric both
sides measured the page shows:

- **A and B**: the median in the canonical unit, with the least–most and the
  number of repetitions under it;
- **B − A** and **relative** (B − A over A). ▲ is a change for the better and
  ▼ for the worse (by the metric's `better`), but only when the two ranges
  do not overlap. A difference inside the repetitions' own spread is grey
  and marked `~`.

A metric only one side measured shows "not measured in A/B", with no
difference: nothing is compared against a zero. A measured zero is compared
(0 → 3 is +3), but it has no relative difference. Cases only one side
measured are listed by name under the table.

**Largest differences**, at the top: the ten metrics with the largest
relative difference among the comparable measurements.

### What may be compared

Before any number, each pair of measurements gets a verdict:

| verdict | when | shown |
| --- | --- | --- |
| **not comparable** | the `environment` differs (another machine, OS, CPU), or the conditions do — the run's, with the result's own over them (another server, worker count, slice, latency taken from the send against from the intended start) | hidden, counted as "N not comparable (hidden: show them)"; with `all=1` shown in red with the reason. Never among the largest differences |
| **warning** | the `toolchain` differs, or the load average does — medians more than 1.5× and at least 1.0 apart — or a side did not record it | compared, in yellow with the reason |
| comparable | neither | compared |

The form lists, under the chosen A, every run measured on the same machine
under the same run-wide conditions, linked to compare against it, and says
how many others are not comparable with it. In the sample data,
`cove-589-capacity` against `cove-593-capacity` is comparable (the same
machine, server and generator). `cove-host-perf-2026-10-05` against
`…-noslice` is not: its `slice` condition is `2 ms` against `0 ms`, and the
page says so.

## Trends

`/cases` lists every case and input measured; each links to its trend,
`/cases/trend?case=hello&input=c%3D64` (a case's name on a run page links
there too). The trend is one chart per metric, as inline SVG generated in
Cove (`chart/`), read from the `point:` records without opening a run:

- **x**: the runs that measured the case, oldest measured first, labelled
  with the commit and the date. **y**: the metric in its canonical unit,
  named on the axis (`p99 (ms)`), with round-number ticks. **One y-axis per
  chart**: a second metric is the next chart down, never a second scale.
- **A series per backend**, and, where they vary between the points, the
  conditions and environment that tell them apart (`native · server=cove-host
  · slice=2 ms`). The same case on another server is a line of its own, not
  a step in this one.
- Each point is the **median**, with a **whisker from the least to the most**
  repetition. A run that did not measure a series is a **gap**: no marker,
  and the line is broken there rather than drawn across.
- Series colours in a fixed order (`#2a78d6`, `#eb6834`, `#1baf7a`,
  `#eda100`, `#e87ba4`, …) by series name, so a backend has the same colour on
  every chart of a page. Each series also has its own marker shape (circle,
  square, diamond, triangle), so colour is never the only identity. There is a
  legend when there are two or more series. Lines are 2px, markers 9px with
  a ring in the surface colour, and the grid is a hairline. Text is in
  neutral ink (`#0b0b0b`, `#52514e`), never in a series colour.
- **Hover** a point for its numbers (an SVG `<title>`: series, median, range,
  repetitions, run, commit, time). **Click** it for the run. Under each chart,
  **table** gives the same numbers as a table, with a dash for a gap.

The palette was checked with the dataviz validator: lightness, chroma, and
colour-blind separation of adjacent pairs all pass. Three colours are below
3:1 contrast on the light surface, which is why there are shapes, a legend
and the table. The pages are light only.

## What is stored

| key | what |
| --- | --- |
| `run:<id>` | the run as normalised: every value as sent and in its canonical unit |
| `index:<time>:<id>` | its summary, for the run list (newest first by `listDesc`) |
| `commit:<repository>\|<commit>:<time>:<id>` | the runs of a commit |
| `metric:<name>` | the dimension a metric has |
| `case:<case>\|<input>`, `point:<case>\|<input>:<time>:<id>:<backend>` | a case, and each measurement of it with every metric's median and spread: what a case's trend reads without opening every run |

`<time>` is `measuredAt` in milliseconds, zero-padded to 16 digits. Text from
a poster that is part of a key has `%`, `:` and `|` escaped. `run:<id>` is
written last, so a post that fails halfway (a quota) leaves no run, and what
it wrote is deleted again.

Limits (`app.toml`): a body up to 4 MiB, a stored run up to 8 MiB, a million
keys and 1 GiB in all. The converted samples are 1.5–70 KiB each.

## Access and escaping

`POST` and `DELETE` need `Authorization: Bearer <secret>` (or a Basic login
with the secret as the password), the `post` secret of `app.toml`'s
`[secrets]`, here `LEDGER_TOKEN`. The secret never enters the app's run:
`auth.check` compares it in the host. A post must be
`content-type: application/json`, and a request whose `Origin` or
`Sec-Fetch-Site` says it came from another site is refused (403), so a form
on another page cannot use a browser's stored login.

Every value a poster controls — the run ID, repository, commit, note,
source, environment, toolchain, conditions, case, input and backend — is a
`String` in a hole of an `html"…"` literal, where it is escaped because it is
not `Html`; every page and the trend chart's SVG are such literals, and every
page is sent with a `Content-Security-Policy` that allows no script.

## Layout

| file | what |
| --- | --- |
| `ledger.cove` | routing, the API, access |
| `comparing.cove` | the comparison page and its JSON |
| `trends.cove` | the case list and the trend pages |
| `store.cove` | the records in `kv` |
| `pages.cove` | the HTML |
| `schema/` | reading and checking a posted run; the stored form |
| `units/` | the units, their dimensions and the canonical unit of each |
| `stats/` | medians and spreads; numbers written for people |
| `compare/` | matching two sides' measurements, the comparability verdict, differences, the largest ones |
| `chart/` | the trend chart as SVG: scale, ticks, series, gaps, whiskers, legend |
| `fields/`, `text/` | reading a named field of a stored object with a fallback, over `std.json`'s accessors; the webhook lab's text helpers less HTML escaping, plus reading ISO 8601 times |
| `importer/ledger_import.py` | converts the existing results, and posts runs |
| `samples/` | the converted runs |

`minicloud test --apps examples ledger` runs the Cove tests in `schema/`, `stats/`,
`compare/`, `chart/`, `fields/` and `text/`. The Rust tests in `crates/minicloud/tests/ledger.rs` drive the
app over HTTP.

## Issue #3's completion criteria

| criterion | test (`crates/minicloud/tests/ledger.rs`) | README |
| --- | --- | --- |
| at least two real Cove measurement results registered | `real_runs_survive_a_restart` posts three converted real runs (`cove-589-capacity`, `cove-593-capacity`, `cove-host-perf-2026-10-05`); `samples/` holds 22 | [The sample data](#the-sample-data-the-existing-cove-results) |
| after a restart, comparisons and graphs still work | `real_runs_survive_a_restart`: a new host over the same data lists the runs, compares #589 with #593 and draws `crunch c=16`'s trend | [Posting a run](#posting-a-run) |
| schema validation | `an_invalid_run_is_refused_with_every_reason` (one case per rule, each with its path; nothing stored), plus `schema/schema_test.cove` | [Validation](#validation) |
| units | `a_valid_run_is_stored_as_sent_and_in_canonical_units` (us, s, KiB, ns, req/min; a metric keeps its dimension); go's µs on the ms axis in `a_trend_draws_every_run_and_leaves_gaps` | [Units](#units) |
| duplicates | `reposting_a_run_is_idempotent_and_a_conflict_is_refused` (same content → no duplicate, conflicting → 409 saying how) | [Duplicates](#duplicates) |
| comparison conditions | `a_comparison_marks_measurements_that_are_not_comparable` (environment, conditions → not comparable; toolchain, load → warning; hidden unless asked; the comparable runs offered), `a_commit_is_compared_by_all_its_runs` | [What may be compared](#what-may-be-compared) |
| unmeasured is never zero | `an_unmeasured_metric_is_absent_not_zero` (pages), `a_metric_one_side_did_not_measure_is_not_compared_with_zero` (comparison), the gap in `a_trend_draws_every_run_and_leaves_gaps` (graphs) | [Unmeasured is absent](#unmeasured-is-absent-never-zero) |
| graphs: points, spread, gaps, units, legend, table | `a_trend_draws_every_run_and_leaves_gaps`, plus `chart/chart_test.cove` | [Trends](#trends) |
| access and escaping | `posting_needs_the_secret_and_reading_does_not`, `everything_a_poster_controls_is_escaped` (run list, run, compare, cases, trend) | [Access and escaping](#access-and-escaping) |
| browsable sample data and a posting procedure | `samples/` and `importer/ledger_import.py post` | [Posting a run](#posting-a-run) |

Notifications and automatic PR comments are out of scope for this version.
