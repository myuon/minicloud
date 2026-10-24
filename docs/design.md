# minicloud: design notes

How the host works and why, for a reader changing it. The
[README](../README.md) is how to use it; [testing.md](testing.md) is what the
tests hold it to, and [measurements.md](measurements.md) the numbers behind
the choices here.

## How requests are scheduled

```text
  HTTP (hyper on tokio) ──admit──▶ ┌ app A: starts ░░░ runs ░ ┐
           ▲                       │ app B: starts ░   runs   │ ──take, round robin──▶ workers × N
           │ oneshot               └ app C: …                 ┘                          │
           │                               ▲   ▲                                         │
           │                               │   └── continue ◀── yielded at a safepoint ──┤
           │                               └────── resume ◀── host future on tokio ◀─────┤ parked
           └────────────────────────────────────────────────── answered ─────────────────┘
```

- **The HTTP front is hyper 1.x on tokio.** It owns the protocol —
  keep-alive, pipelining, chunked bodies, header size and read timeouts — and
  the sockets, so an idle connection costs a task and no thread, and waiting
  on thousands of them is epoll/kqueue rather than the edge sample's `poll(2)`
  over every socket (cove#590). It is the established choice for a Rust HTTP
  server, and it is a library rather than a framework: routing, limits and
  the stats are this crate's, a few hundred lines.
- **No Cove code runs on tokio's threads.** A request is admitted into its
  app's queue and the connection's task awaits a oneshot. A dedicated pool of
  worker threads (`--workers`) takes jobs and, for each request, builds a
  fresh `OwnedVm` over the app's shared `PreparedProgram` (a few
  microseconds; nothing of one request is visible to the next).
- **A run that waits holds no worker.** A host call that answers pending
  (`timer.sleep`, a `fetch`) hands the scheduler a future; the run becomes a `ParkedVm`
  and the future runs on tokio, raced against the run's deadline. Its answer
  puts a resume job on the app's queue for whichever worker is free; a
  deadline that comes first cancels the run (504) and drops the future, which
  is how a pending `fetch` is withdrawn.
- **A long run is sliced.** Each worker publishes the run it is running; a
  monitor raises the run's `YieldRequest` once it has held its worker for a
  slice (`--slice`, 2 ms) *and* something is waiting for a worker. The run
  gives its worker up at its next safepoint — inside compiled code too (Cove
  ADR 0084, 0085) — and goes to the back of its app's queue as a
  `YieldedVm`, which any worker continues.
- **Apps are served round robin.** Each app has its own queue: runs to resume
  or continue, and requests waiting to start. A worker takes one job from the
  next app in turn that has work it may run; within an app, turns alternate
  between its started runs and its new requests, so a long run of an app
  does not hold back that app's own new requests either. A job is at most one slice of a run while others
  wait, so an app with hundreds of requests queued gets one turn in N like
  every other busy app: its backlog costs itself, not its neighbours. An app
  at `max_in_flight` has its starts held back (its started runs still
  continue); at `max_queued` new requests are rejected.

Turns are equal, not CPU: a turn that answers in 50 µs and one that runs its
full slice count the same. That is enough to stop starvation, which is what
this base needs; weighting turns by worker time (deficit round robin over
`worker_ms`) fits the same queues if measurements show it is wanted.

## Runtime constraints, made explicit

What the runtime cannot do yet is handled by refusing it or by counting it,
never by trusting it:

- **`spawn` is refused at load.** A spawned task is a thread outside the
  worker pool and its accounting; while one is alive its parent can neither
  park nor yield (ADR 0080 §2, 0084 §3), and it has no native tier (ADR
  0085). An app whose entry the checker says can reach a task `scope`
  (`FnEntry::can_spawn`, ADR 0088) is refused before it is prepared, with a
  diagnostic at the `spawn` (`minicloud::spawn`; the README's
  [Checking and testing an app](../README.md#checking-and-testing-an-app) shows one) — or at the entry
  when there is none to point at. That fact is a lower bound for a
  capability-open entry, so such an entry in a package where some function
  opens a scope is decided by its lowered program instead. Every run also
  carries `max_tasks = 0` as the runtime's backstop.
- **A run that cannot yield is surfaced.** Below an encoded function that
  compiled code called, inside a host call, or beside a task, a run declines a
  yield request and keeps its worker (ADR 0085's open items). The monitor
  counts a run still holding its worker well after it was asked (four slices,
  at least 20 ms) as `overdue_yields` and logs it; the runtime's own
  `yields_declined` is summed per app; and the run's **deadline still ends
  it**, since the deadline is checked at every safepoint whether or not the
  run can yield. `/_host/apps/<app>`'s `native.refusals` names the functions
  left on the encoded tier, with the instruction and the source line: a
  function that makes a host call, or writes a lambda, is one — and an
  algorithm called from such a function, by compiled code, cannot yield at
  all. The algorithm playground met exactly that ([its README](../examples/algo/README.md#yields-on-the-native-tier)).
- **A host call that cannot park blocks, and is counted.** In the same
  places a call cannot park; `timer.sleep` then sleeps on the worker
  (`blocking_host_calls`), bounded by its 60 s maximum. None of the small
  examples reaches this.
- **`max_heap_words` is the run's heap capacity.** Each isolate is built
  with `OwnedVm::with_heap_words` (ADR 0088), so a run that needs more fails
  the allocation where it makes it ("this run has no memory left", 500,
  `errors.heap`) rather than after it answers. Without it a run gets the
  runtime's default, 4 Mi words (32 MiB). It bounds Cove's heap, not what a
  host module holds outside it; above `u32::MAX` words, the most the runtime
  addresses, it is refused. A run's heap and declined yields are read at its
  every park and yield as well as its answer (`heap_peak_words`,
  `yields_declined`). Its garbage collections and the words it allocated
  are read once, when it ends, from the `OwnedVm` that answered
  (`OwnedVm::collections`, `allocated_words`): a parked or yielded run does
  not expose them, and as each run has a heap of its own the machine's
  totals at the end are the run's (`gc_collections`, `gc_runs`,
  `allocated_words`). The heap collects when what was allocated since the
  last collection reaches twice what was live, so most light runs never
  collect; how often runs do, and how much they churn, is what the admin's
  Garbage collection charts show.
- **The native tier is used where it exists.** `--backend auto` compiles each
  app with `PreparedProgram::with_native` once at load on Unix x86-64 and
  says once, on stderr, that it falls back to the encoded VM elsewhere;
  `--backend vm` forces the VM, `--backend native` refuses apps where there is
  no native tier. `tier` in the stats says which each app got.
- **A client that disconnects cancels its run.** hyper drops the request's
  future when the connection closes, and a guard on it raises the request's
  cancellation: the runtime's `Cancellation` in the run's budget stops a
  running run at its next safepoint and a yielded one when it is continued;
  `Cancellation::on_cancel`, registered on the flag the parked run's meter
  hands back, wakes a parked run's wait, which cancels the run and drops its
  pending work (a fetch's connection with it); a request still queued is
  dropped when a worker reaches it. A run that cannot yield stops at its next
  safepoint all the same, since cancellation is checked there.

## Updates and versions

`minicloud update <app>` asks the running host to load `<apps>/<app>` again.
The new version is parsed, checked, admitted (capabilities, `spawn`, config),
lowered, prepared and compiled on a blocking thread — not on a worker, not on
the I/O runtime — while the current version keeps serving. **Only if all of
that succeeds** is the app's route switched, in one step; otherwise the
current version stays and the command prints the diagnostics and exits 1
(`updates_refused` in the stats, a line in the app's log).

- **Requests in flight finish on the version they were admitted to** —
  queued, running, yielded or parked: a request holds its version (and so
  its prepared program and registry) until it answers. Requests admitted
  after the switch get the new one.
- **The old version is dropped when its last request ends.** Its
  `PreparedProgram` goes with it; `/_host/apps/<app>` lists each version with
  `alive` and `program_alive`, and `versions_alive` / `programs_alive` count
  them.
- **Every response says which version answered**: `x-cove-app-version:
  v<n>-<hash>`, the hash being of the app's files, so two loads of the same
  code have the same hash. The id is in the stats, the logs and every
  recent error too.
- The app's counters, log, queue and KV store belong to the app, not the
  version: they carry over. During an update both versions share the one
  store (and its quota accounting); the new version's quotas apply from the
  switch.
- A name the host does not serve yet is **added** by `update`; `minicloud
  remove <app>` stops routing to an app (in-flight requests finish; its data
  stays, and `update` brings it back as its next version).
- One update runs at a time.

**The admin listener.** Updates go to a second listener, `--admin`
(default `127.0.0.1:8081`), never to the public one, so a reverse proxy that
forwards the public port cannot reach them, and the admin port can stay on
localhost whatever the public one is. Every admin request needs
`Authorization: Bearer <token>`; the host writes a random 256-bit token to
`<data>/admin.token` (mode 0600) the first time it starts without one, and
`minicloud update` / `remove` read it from there (`--token-file`, `--admin`
to point them elsewhere). Anything without the token is 401 and changes
nothing. The endpoints are `POST /apps/<app>/update`,
`POST /apps/<app>/deploy` and `/rollback` (see [Deploys](#deploys)), `DELETE /apps/<app>`,
`POST /apps/<app>/enable`, `/disable` and `/reset` (see [Administering apps
at run time](#administering-apps-at-run-time)), `GET /secrets`,
`PUT /secrets/<name>` and `DELETE /secrets/<name>` (see [Secrets set at run
time](#secrets-set-at-run-time)), `GET /apps` (the stats) and
`GET /changes?n=50` (the change history). There is no file watcher: an update is an
explicit act, which is what makes a refused one a report rather than a
silent non-event.

## Deploys

`minicloud deploy` packs an app and sends it to the admin listener, which
loads it and only then writes it into the apps directory and updates to it.

- **What is sent** is the app's `app.toml` and its `.cove` files, at any
  depth — nothing else (a README, samples, a `.git`; anything hidden is
  skipped). A symbolic link, a path outside the directory, and more than
  16 MiB or 4096 files are refused before anything is sent, and again by
  the host. The name is the directory's (`--name` to choose another); it is
  the app's route and its main module's name, so `aidaily/aidaily.cove`.
- **The check is the running host's**: the app is loaded as its next
  version with the host's env (its `[secrets]`, `[access]`), the admin's
  changes to its grant, and the hostnames other apps claim — exactly as an
  update loads it. **If it does not load, nothing changes**: no file is
  written, the current version keeps serving, and the diagnostics are
  printed (exit 1). An app that needs a secret the host does not have is
  refused here, saying which: set it first — in the admin app's Secrets
  page or with `minicloud secret set` for a `{ store = ... }` secret
  ([Secrets set at run time](#secrets-set-at-run-time)), or in
  `~/cove-tools/env` and a restart for an `{ env = ... }` one. A `[secrets]`
  value `{ file = ... }` is not sent: keep secrets in the store or the env.
- **If it loads**, it is written to `<apps>/.deploy/<app>`, the current
  `<apps>/<app>` is moved to `<apps>/.previous/<app>` (replacing the one kept
  there), the new copy is renamed into place, and the host updates to it:
  requests in flight finish on the old version, as with `update`.
- `minicloud rollback <app>` swaps `<apps>/<app>` with the kept copy — after
  loading it, so a kept version that no longer checks is refused and nothing
  moves — and updates to it. A second rollback undoes the first.
- Both are admin requests (`POST /apps/<app>/deploy` with the archive as the
  body, `POST /apps/<app>/rollback`), behind the same token as `update`, and
  both go in the change history.

`deploy -` reads a tar archive (any `tar`'s, macOS's included: its `._`
files are hidden and skipped) and needs `--name`. With a tunnel
(`ssh -L 8791:127.0.0.1:8791 whisky`) and a copy of the token,
`minicloud deploy apps/aidaily --admin 127.0.0.1:8791 --token-file <copy>`
works from the laptop too.

`minicloud deploy <dir> --into <apps>` does the same with no running host,
checking as `minicloud check` does with the current environment (and, with
`--data <data>`, that data directory's secret store); the host loads it at
its next start. `deploy/install.sh` installs the bundled admin app this way, and
checks every installed app with `--data` before it switches.

## Administering apps at run time

Beyond updating code, an app's **configuration** can be changed on the
running host — enabled or disabled, its grant, its fetch allowlist, its
limits — by one app, the admin app (`apps/admin`, issue #17), through a host
module of its own, and on the machine through the admin listener. No other
app can: the capability is `admin`, and **only the app named `admin` may be
granted it**. An `app.toml` of any other app that grants it — or a change
that would — refuses that app at load, and `minicloud check` says so. The
grant is a line in the startup banner and in `/_host/stats` like every other
(`admin  requires [admin, auth, …]  granted [admin, auth, …]`): the one app
that can change the others is visible as such.

The module is `host` (the admin app's own main module is `admin`, and Cove
refuses a package module that shadows a host module):

| operation | answers |
| --- | --- |
| `host.apps()` | `Array<host.App>`: every app — `state` (`serving`, `disabled`, `refused`, `removed`) and `reason`, `version`, `tier`, `entry`, `hosts`, `required` and `granted` (and `grantAdded`/`grantRemoved`, what the admin changed), `fetchAllow`, `limits` (`host.Limits`: `maxHostCalls`, `deadlineMs`, `maxHeapWords`, `maxInFlight`, `maxQueued`, `maxRequestBytes`, `maxResponseBytes`) and `limitsChanged`, `isAdmin`, its counters (`served`, `ok`, `errors`, `rejected`, `inFlight`, `queued`, `kvKeys`, `kvBytes`) and its ten newest `recentErrors` |
| `host.capabilities()` | `Array<String>`: what a grant may name |
| `host.selfVersion()` | `String`: the version of the admin app itself, the string its own answers carry as `x-cove-app-version`; `""` when the host cannot say. No app name: only `admin` is granted `admin`. It is a slot lookup and a `String` clone, which is what lets every page ask — `host.apps()` answers the same question only by reading every app's counters and sizing its store |
| `host.history(limit)` | `Array<host.Change>`: `atMs`, `who`, `app`, `action`, `detail`, `outcome`, newest first |
| `host.setEnabled(app, enabled, who)` | `Result<String, Error>` |
| `host.configure(app, settings, who)` | `Result<String, Error>`: `settings` is `host.Settings { grant, fetchAllow, limits }`, the whole of what they are to be |
| `host.reset(app, who)` | `Result<String, Error>`: back to `app.toml` |
| `host.secrets()` | `Array<host.Secret>`: every secret stored or used — `name`, `set`, `updatedMs` (0 when unset), `apps` that use it — never a value |
| `host.setSecret(name, value, who)` | `Result<String, Error>`: stores it and reloads the apps that use it; the message is what each reload came to |
| `host.deleteSecret(name, force, who)` | `Result<String, Error>`: refused while an app uses it unless `force` |
| `host.kv(app, prefix, after, limit)` | `Array<host.KvEntry>`: a page of another app's store — `key`, `value` cut to 512 characters, `bytes` (the whole value's), `cut` — ascending, at most 500 |
| `host.kvValue(app, key)` | `Result<host.KvEntry, Error>`: one key, its value whole up to 64 KiB; `Err` when the store has no such key |
| `host.logs(app, limit)` | `Array<host.LogLine>`: the last `limit` lines of the app's ring — `app`, `atMs`, `level`, `text` — oldest first |
| `host.allLogs(limit)` | `host.AllLogs { apps, lines }`: every app's ring as one sequence — the last `limit` lines of it, oldest first — and `apps`, every app whose ring holds a line |

The five that change something park the run while the host works (an app
is reloaded on a blocking thread, as an update is), so they hold no worker.

**A change is a re-check, not a revocation.** Cove decides capabilities
before a program runs, so there is nothing to take away from a run in
progress: a change loads the app again with the change applied — parsed,
checked, admitted, lowered, prepared, compiled — and routes to that version
the way `minicloud update` does. Requests already admitted finish on the
version they were admitted to.

### Reading another app's store and log

The four reads above are how the admin app shows what an app holds and
what it said, and they are **reads only**: there is no operation that
writes to another app's store, so the one app granted `admin` cannot change
another app's data, only its configuration.

- **The store** is read from `<data>/<app>/kv.sqlite3` without opening it as
  the app does. A store a running version has open is read through that
  `Store`, so the page sees what the app has just written; one no version
  has open is opened read-only, and only if the file is there — the admin
  app looking at an app that has never written must not be what creates its
  store. The value is cut in SQL (`substr`), not after reading: a page of
  500 keys of up to 1 MiB each would otherwise be half a gigabyte to throw
  away. `bytes` is the whole value's length, the same measure the quota
  uses, so a cut value still says how large it is.
- **The log** is the ring of [Logs](#logs) — in memory, the last 1,000 lines,
  emptied by a restart — parsed back into `atMs`, `level` and `text` by the
  module that writes it. The file beside it (`<data>/<app>/log.txt`) is not
  read: a page that tails a rotating file is a different thing, and the ring
  is what a browser wants.
- **Every app's log is one sequence** (`host.allLogs`, issue #108). The host
  mixes them, not the app: it holds every slot, and merging sorted arrays in
  Cove is a loop nobody should write twice. It takes `limit` lines from each
  ring, merges them by time and cuts the merge back to `limit` — which is
  **exactly the last `limit` lines of the host's whole log**, because every
  line of it is a line of some app's and so the newest `limit` across the
  host are among the newest `limit` of the apps they came from.

  Which means one app that is talking fills the answer and the quiet ones
  fall off the end. That is what "the last `limit` lines" is; the page's
  `?n=` is how to see past it, and an app's own log is how to read it alone.
  So `apps` is answered beside the lines and is **every app whose ring holds
  a line**, not the apps in the window: the way to a quiet app's log must
  not come and go with `?n=`.

  A line the log module could not read has no time of its own (`atMs` 0). It
  is never dropped — a log that loses lines is the worst this could do — and
  it is not sorted to the epoch either, which would bury it under everything
  and cut it first: it is ordered by the line before it in its own ring,
  which is where the ring put it, and it still says 0, because that is all
  that is known about it.
- **The app is named, not the path.** Both take an app's name and resolve it
  through the host's slots; a name the host does not serve answers empty
  rather than reaching into the data directory, so `..` in a name reads
  nothing.

| the reload | the change | the app |
| --- | --- | --- |
| loads | kept | serves the new version |
| is refused because its entry requires a capability the change took away | kept | **refused**: answers 503 with the reason; every other app serves |
| anything else: a limit out of range (deadline or in-flight below 1, a negative number, a heap above the runtime's), an allowlist entry that does not parse, a capability the host does not have, `admin` for another app | **not kept** | as before; the answer is the reason |

Taking a capability away from an app that needs it is how an app is stopped
from using it, so that is applied; a change that is merely wrong is not.
Granting it back loads the app again, with its store as it was.

**Disabling** stops routing to an app and nothing else: its version stays
loaded, and its queue, counters, log and store stay. A request to it is
answered **503** `app … is disabled by the administrator` (no `Retry-After`;
counted as `rejected.disabled`) — 503 rather than 404 so that an app that is
switched off is not mistaken for a mistyped URL; what was already admitted
finishes. Enabling routes to it again.

**The admin app cannot disable itself, take `admin` away from itself, or
make any change that would leave it refused** — those are refused, through
it. The admin listener can do all three, which is the point of it:

```console
$ ./target/checked/minicloud disable notes    # or enable, reset
`notes` disabled: not routed to; in-flight requests finish
$ ./target/checked/minicloud reset admin      # drop the admin's changes to the admin app
```

**Where the changes are kept.** Not in `app.toml`: a deploy replaces an
app's directory with what was sent (`install.sh --with-bundled-apps` too).
They are kept in the data directory, which no release and no deploy
touches:

- `<data>/_host/overrides.json`: per app, `enabled`, the capabilities and
  allowlist entries added and removed, and the limits set — each relative to
  `app.toml`, so a later release whose `app.toml` grants something new still
  grants it unless that very capability was removed. A change back to what
  the file says leaves no entry. It is applied every time the app loads (at
  start, on `update`, on a change), so it survives a restart and a release.
  It is plain JSON and the last way out: delete an app's entry, or the file,
  and restart. A file that does not read stops the host from starting —
  running without it would quietly re-enable and re-grant. A `limits.fuel`
  written by a host from before Cove ADR 0091 is dropped when the file is
  read, with a warning naming the app (see
  [Migrating from fuel](../README.md#migrating-from-fuel-cove-adr-0091)).
- `<data>/_host/changes.jsonl`: the history — who (what the admin app says
  of its user, or `admin listener`), when, which app, what was asked and
  what came of it, refused attempts included — one JSON line each, appended.
- `<data>/_host/secrets`: the secret store, below.

### Seeing the code that is deployed

The pages say what version an app is (`v<n>-<hash>`) and a hash is only
useful if you can get at what it is a hash of. `GET /apps/<app>/code` is
that: the app's `app.toml`, its `.cove` files at any depth and its
`assets/` — the whole of what a deploy carries ([Deploys](#deploys)) — with
`?file=` for one file's text.

**Nothing is read for this.** Every part was already held, each for its own
reason, and `code.rs` only collects them:

- the `.cove` text is the `SourceMap` the app was compiled with, kept
  because a runtime error is rendered against it;
- the asset bytes are the `Assets` read at load, kept so that a request for
  one costs no disk access;
- `app.toml` is the exception: it is parsed at load and the `Settings` are
  what survive, so that one is a read of the app's directory.

**What is in memory is what is running**, and that is the honest default: it
cannot disagree with the version the page shows. The directory can — a
deploy that was staged and not switched to, an edit made on the machine —
and that disagreement is sometimes exactly the thing worth seeing, so every
file says how the one of the same name beside it compares: `same`,
`differs`, `missing`, or `unread` for one that cannot be read. The whole
file is compared, not its size or its time, because an edit that keeps the
length is the case this is for. `app.toml` has nothing to disagree with, as
it is the directory's copy either way — which is also why the app's own
page can show a grant or a limit that the file does not: a change made here
is in `overrides.json`, not in `app.toml`, and seeing both is how that is
noticed.

**A name is never a path.** A listing is built from what the host holds, and
one file's read finds its entry in that listing by name, so `..`, an
absolute path or another app's file matches nothing rather than being
reached for. The app is named and resolved through the host's slots, as it
is for the store above. The only path joined from a name is the one that is
read to compare with disk, and every component of that has to be an
ordinary one.

**Both limits are said rather than quietly applied.** One file's text is cut
past 128 KiB, with the whole file's size beside it; a listing carries at
most 500 files, and the page, which asks for 200, says when there are more.
A file that is not UTF-8 — a PNG, a font — has no text at all, and the page
says where the host answers it instead: a browser shows the file as it is,
which is more than a page pretending to render it would.

A refused app was never compiled and so has no source map: what is
deployed, for it, is its `app.toml` and whatever the host read of its
assets. That is the file somebody looking at a refusal came for anyway.

**The listing is a tree, and the host is what nests it** (issue 100). The
names carry the structure already — `text/text.cove`, `assets/img/icon.svg`
— and a flat list of paths makes the reader do the nesting in their head. So
`code.rs` puts a row where a directory begins and says, for every row, how
deep it is; the page opens and closes lists to match and decides nothing.
Grouping a flat list of paths is a sort and a walk, which Cove can do and
awkwardly, and the host sorted them in the first place. One pass over the
names is enough because the order they are already in puts every file of a
directory next to the others in it, and that order is the one there is a
reason to read in: `app.toml` first because it decides what the app may do,
then the code, then the assets.

**The tree is a list in a list, not a table.** It was a table first, with a
left padding a level on the first cell, and that was wrong twice: on a wide
screen a padded cell is a row that starts further right and not a branch of
anything, and on a narrow one `table.cards` makes every row a card of its
own, which left eleven cards in a pile and no nesting at all. Nesting a
`<ul>` inside the `<li>` that holds it puts the shape in the markup, where
no width can take it away, and `assets/style.css` draws the branches with a
`border-left` on each list and a tick to each item on it — two rules, and no
rule a level to run out of. What the columns said is beside each name
instead: what the file is, what it comes to, and how the app's directory
compares, the last of them the state badge where it is bad news. There is
**no collapsing**, because the CSP is `default-src 'none'` and that is not
what to spend it on; a deploy is bounded and small, and all of it is the
whole feature. A directory row says what the files under it come to, so "how
large is `assets/`" needs no column added up.

**A file's text is coloured by the host, and `.cove` by the real lexer.**
The same CSP means no highlighter runs in the browser, so the colouring is
server-side: `highlight.rs` cuts a file into pieces, the page wraps each in
a `<span>` with a class and `assets/style.css` colours it. For `.cove` that
is `cove_syntax::lexer::lex_recovered` — **the lexer the host compiled the
file with** — so a colour cannot disagree with what the host runs, and the
first time the language gains a keyword or a literal this gains it too. A
hand-written approximation would have been wrong from that moment and gone
on looking right. Its diagnostics are ignored and its tokens are used: a
file that does not lex is exactly the file somebody is looking at.

**The gaps between the token spans are walked**, and that is not a detail.
The lexer keeps a `///` as a token and **drops every other comment**,
recording only that a line break happened — so an ordinary `//` is not a
token at all and lives between two spans. Walking the gaps is how it gets
coloured, and it has a second effect worth more than the first: because the
text between two spans is emitted whatever is in it, **no byte of the file
is dropped**, including whatever a lexical error made the lexer skip. The
test that matters most is that the pieces concatenated are the file, byte
for byte, and it is run over the files this repository ships as well as over
written-out cases. The languages with no lexer to borrow have scanners
written the same way — TOML for `app.toml`, which is the file people come to
read, and CSS and XML for the assets that are text — each either recognising
the run in front of it or stepping one byte into the plain run between, so
each keeps the same property for free. **A file no scanner claims is shown
plain**, and the page says so: a colouring guessed from a name is a claim
about the file, and none is no claim.

**The host answers pieces and never HTML.** Every other value these pages
show is a `String` in a hole of an `html"…"` literal, which escapes it, and a
file's own text is the value likeliest to *be* markup — a comment holding
`<img src=y>`, a string literal holding `<script>`. A host that returned
`<span>`s would split the escaping between the two sides of the boundary and
leave the page unable to check its own. So `host.CodePiece` is a kind and a
run of the file's bytes, the page puts every one of them in a hole as text,
and the class names live in the page beside the stylesheet that colours
them. The vocabulary is nine kinds for four languages — `plain`, `keyword`, `type`, `ident`, `string`,
`number`, `comment`, `doc`, `punct` — because a kind is a *role*: a Cove
type, a CSS selector and an XML tag all want the colour of "the name the
language gives meaning to". A kind the page has no colour for is its text
and no element at all, which is how the host may grow one.

**The colouring has a limit, and it is said.** The limit is on *pieces*
rather than bytes, because a piece is what the page carries — a `<span>`
with a class, for each of them — and bytes are a poor guess at how many
there will be: runs of one kind are joined, so this repository's longest
file is 58 KiB and 6,500 pieces while the same length of nothing but
alternating operators and names would be twenty times that. Past 20,000 a
file is shown plain. A file whose text was **cut** is not coloured either,
whatever it is: the pieces of half a file would say the rest of it is
whatever the cut fell inside — an unclosed string literal colours everything
after it — which is a page making a claim about a part of the file it is not
showing. Each of the three reasons is said on the page rather than left to
look like a file with nothing in it worth colouring.

### Reading an app's SQL store

The state of an app like `watch` or `aidaily` is in its `sql` store, and
until now "why does the dashboard say that" meant ssh and `sqlite3`.
`GET /apps/<app>/sql` is the answer from the browser: the store's tables
with each one's row count, and `?q=` for one statement whose answer is shown
as a table.

**It reads, and it rests on a refusal that was already there.**
`Store::query` refuses a statement that writes (`Statement::readonly()`,
asked after the statement is prepared and before it is stepped). The window
and the app's own `sql.query` are the same function, so what "reads" means
is decided in one place and there is no check here to drift from it. An
admin who has to repair data still does it over ssh, where it is deliberate.

**The reader goes through the store the app has open**, and opens the file
`SQLITE_OPEN_READ_ONLY` only where nothing has — the shape `kv::peek` has
for the same job, which `sql` had no equivalent of: the only thing outside
`sql.rs` that touched another app's store was `sql::usage`, which measures
the file and does not read it. Two reasons. A second connection on a WAL
database reads the snapshot its transaction started with, so a row the app
wrote a moment ago would be missing from a window whose whole job is to show
what is there; and two connections on one file take locks against each
other, so a read would wait on a write or be refused as busy. The cost is the
other way round — the app's next statement waits for the window's — which is
why the window's deadline is short. A file that is not there is **no store**
rather than an error: an app not granted `sql` has no file, and looking at
it must not create one, as `Store::open` would. The app is resolved through
the host's slots, as for the `kv` window, so no name is joined to a path.

**The authorizer holds on both paths.** The authorizer that refuses
`ATTACH`, `DETACH`, `PRAGMA`, transactions, savepoints and virtual tables
belongs to a `Store`, and the connection the window opens itself is not
one, so it is installed on that connection too. Without it that path would
allow what the app's is refused — `ATTACH` naming any file the host can
read, which a read-only connection may still do, `PRAGMA`, a transaction
left open — none of which is a write that `readonly()` would catch. There is
a test for exactly that.

**The limits are the window's own, and they are said.** An app's `[sql]` is
for the app — what one of its requests may cost — and reading the store from
outside should not mean raising it. So: at most 200 rows, and more is an
error naming the limit rather than the first 200 shown as though they were
all of it; 256 KiB of values; one statement at a time, at most 4 KiB of
text, stopped after 1000 ms where it is (SQLite's progress handler).
1000 ms is fifty times an app's 20 ms default for the opposite reason: an
app serves requests at a rate, this is one person asking one question. It
stays small because the statement holds a worker, and the app's store lock
with it. An error now says *whose* limit was reached, `this app's [sql]` or
`the admin window's`, because a message that said `[sql]` for both would
send somebody to edit the wrong number. A row count is a statement per table
under that same deadline, so a table too large to count inside it is listed
as "not counted" rather than left out or shown as 0.

**A value is text**, as it is for an app. `NULL` is shown as the word `NULL`
and the empty string as `empty`, so the two are never both an empty cell —
the one thing a nullable column is about. A cell longer than the column is
cut to 140 bytes, as the `kv` window cuts a value; `substr` in the statement
is how to ask for the rest.

**The form is a GET.** A read changes nothing, so the statement belongs in
the URL — the page can be linked, reloaded and gone back to — and there is
nothing here for a form on another site to make happen, which is what the
origin check on the app's POSTs is for. The statement goes back into the
form's `value=` inside quotes, and the hole it goes in writes `"` and `'` as
entities as well as the angle brackets, so it stays the attribute's value
whatever it holds. **Nothing typed here is kept**: the change history
records who changed what, and a read is not a change.

The admin app reaches this through `host.sqlTables` and `host.sqlQuery`,
declared in the `host` namespace and not reused from `sql.`, because the
admin app is granted `admin` and not `sql`. It has no `fetch`, so the `host`
module is the only way anything reaches it.

### Seeing what is on an app's queues

`minicloud queues <app>` says how deep each queue is, and a depth is a
number: a queue with 400 messages on it says `400`, and *which* 400 was not
a question the host could answer. `GET /apps/<app>/queues` is that question
answered — each declared queue with what it holds, the messages waiting, and
the dead letters with why each was buried.

**Listing what is waiting had to be built.** `Queues::depths`,
`Queues::dead` and `Queues::revive` were already there; the only way to see
a *waiting* message was `Queues::lease`, which is a side effect — it takes
the message, counts the try and starts the lease. A read that changes what
it reads is no good for a page, and a page that leased what it showed would
take the message the next delivery was about to take. So `queue.rs` has
three reads of its own (`peek_depths`, `peek_waiting`, `peek_dead`) that
lease nothing, count nothing and move no due time. There is a test that
reads a message three times and then watches the delivery happen exactly as
it would have.

**A message is `due` or it is `held`, and the store says no more than
that.** `due` is what the next tick may take, oldest sent first, which is
the order the listing is in. `held` is a message whose `due` is ahead — and
that is either a delivery holding it on lease (`lease` pushes `due` out by
the queue's `lease`) or a failed delivery waiting for its retry (`retry`
pushes it out by the delay, doubling). **One column carries both**, so
nothing can tell them apart, and the window says `held` rather than calling
a backoff a lease. The honest version of a number is worth more here than
the number somebody expected: `attempts` beside it is the real difference —
a message with two tries behind it has had two whatever it is waiting on.
Giving the store a second column to say which would be a write on the
delivery path to make an admin page prettier, which is the wrong way round.

**The queues are the app's `[[queues]]`, not the store's rows.** A queue
declared and never sent to has no rows, and leaving it out would read as
"there is no such queue" when the answer is "nothing is waiting on it". The
other way round matters too: a queue the store holds messages for that the
app *no longer declares* is listed after the declared ones, saying so,
because nothing will ever deliver it and that is exactly the thing a window
like this is for. The settings beside each queue are the host's own values,
after `[[queues]]`'s defaults — so the page says `retry 30s` where the file
says nothing at all.

**The reader goes through the store the app has open**, and opens the file
`SQLITE_OPEN_READ_ONLY` only where nothing has, as the `kv` and `sql`
windows do and for the same reasons: a second connection on a WAL database
reads the snapshot its transaction started with, so a message sent a moment
ago would be missing from a window whose whole job is to say what is
waiting, and two connections on one file take locks against each other. A
file that is not there is **no queue** rather than an error: an app not
granted `queue` has no file, and looking at it must not create one. The app
is resolved through the host's slots, so no name is joined to a path.

**The limits are the window's own.** At most 200 messages in a listing,
each body cut to 512 characters and again to 140 by the page. The counts are
whole — they are `count(*)`, not the length of the listing — so a page
showing the first 200 of 4000 says which it is doing. A queue's `max_depth`
is the app's business and may be thousands; a page is one person reading.

**A body is opened by its id**, as a value is by its key in the `kv` window
(`?message=<id>`, 64 KiB of text), because a queue's body is a payload: a
webhook's JSON is routinely longer than the 512 characters a listing
carries, so a window that could only cut would be answering the question
with half an answer. The id is enough to find it in either table — ids are
never reused, a dead letter keeps the one it had, and reviving moves it back
— and an id no queue of the app holds is the page's `Err` rather than the
host's error, because a link to a message that has since been delivered is
an ordinary thing to click.

**Reviving is not here.** `minicloud queues <app> --revive <id>` exists and
the listener behind it does too, so a button would be small — but it is a
*change*, and a change on these pages has a confirmation and a line in the
change history. Half of that is worse than none, so the page says where
reviving is done instead. **Nor is pruning**: `Queues::prune_dead` still has
no caller in the host, so a dead letter is kept until somebody deals with
it. That is the decision, and this page is what makes it visible — a number
under "dead" that grows is work nobody has looked at, which is the state a
retention rule would have hidden.

The admin app reaches this through `host.queues`, `host.queueMessages` and
`host.queueDead`. A waiting message and a dead letter are **one type**: a
`TypeSchema`'s cases carry no payload, so "waiting, or buried with this
reason" cannot be declared as one type whichever way it is cut, and two
types would be eleven fields written twice for a difference of two. `state`
says which a message is, so nothing is inferred from an empty `why`. Times
are carried in milliseconds, as every other time the module carries, though
the store keeps seconds.

## Secrets set at run time

An API key used to mean ssh, an edit of `~/cove-tools/env` and a restart.
A secret can instead live in the host's **secret store** and be set,
replaced and deleted on the running host — from the admin app's Secrets
page, the admin listener, or `minicloud secret`. An `app.toml` takes a
secret from it with `store`, beside `env`, `file` and `value`:

```toml
[secrets]
gemini = { store = "gemini" }

[fetch]
allow = ["https://generativelanguage.googleapis.com"]

[fetch.headers."https://generativelanguage.googleapis.com"]
x-goog-api-key = { secret = "gemini" }
```

- **Where it is kept**: `<data>/_host/secrets`, one JSON file (`{ "secrets":
  { "<name>": { "value", "updated_ms" } } }`), mode 0600, written whole
  through a temporary file that is synced and renamed (and the directory
  synced), so a crash leaves the old file or the new one. The data directory
  is the one thing the service may write besides the apps
  (`ReadWritePaths`), and no release or deploy touches it. A file that does
  not read stops the host from starting, saying so without quoting it.
- **A secret that is not set refuses the app** that takes it, at load, with
  the usual diagnostic naming it — `secret `gemini`: the host's secret
  store has no `gemini` (set it on the admin app's Secrets page, or with
  `minicloud secret set gemini`)` — which is the reason the admin app shows
  for the app, and the Secrets page lists the name as unset and used by it.
  A name is a letter or digit, then letters, digits, `_`, `-` or `.`, at
  most 64; a value is not empty and at most 16 KiB.
- **Setting or replacing one reloads every app that uses it** — those
  routed to whose `app.toml` names it in a `store` — through the same
  versioned update as `minicloud update`: requests in flight finish on the
  old value, the new version has the new one. The answer says what each
  reload came to. **The value is stored first and kept whatever the reloads
  do**: an app that does not load (its code broken on disk, say) keeps
  serving what it served, and picks the value up at its next load.
- **Deleting one an app uses is refused** (409) unless forced; forced, the
  apps that use it are reloaded without it and are refused (503), so the
  value is no longer used anywhere.
- **Write-only.** No endpoint, page or command returns a value: the listings
  are names, whether each is set, when, and which apps use it. Nothing
  logs one, and the stats, the diagnostics and the change history (which
  records who, the secret's name and the action — app `–`) do not hold
  one. The admin app handles the value once, in the request that sets it,
  in a password field that is never filled back in.

| through | list | set or replace | delete |
| --- | --- | --- | --- |
| the admin app (`/secrets`) | the table | the row's password field, or *Set a secret* | the row's *Delete*, with *confirm* ticked (and *force* for one in use) |
| the admin listener | `GET /secrets` | `PUT /secrets/<name>`, the value as the body | `DELETE /secrets/<name>[?force=1]` |
| `minicloud secret` | `list` | `set <name>`, the value on stdin | `delete <name> [--force]` |

The admin app's forms are POSTs under its usual protections: an identity
(`auth.identity`: Cloudflare Access, or its token where Access is off), and
a POST from another site refused (`Sec-Fetch-Site`, `Origin`). There is no
script on its pages, so the confirmation is a required checkbox, checked
again by the app. `minicloud secret` takes `deploy`'s `--admin` and
`--token-file`; `set` reads one line without echo from a terminal, or all of
stdin otherwise, dropping one trailing newline:

```console
$ ssh whisky '~/cove-tools/current/minicloud secret list --admin 127.0.0.1:8791 --token-file ~/cove-tools/data/admin.token'
$ ssh -t whisky '~/cove-tools/current/minicloud secret set gemini --admin 127.0.0.1:8791 --token-file ~/cove-tools/data/admin.token'
value for secret `gemini` (not echoed):
```

## Routing by hostname

An app may be reached by a hostname of its own instead of by its prefix:

```toml
[route]
hosts = ["covtools-admin.ramda.io", "admin.localhost"]
```

A request whose `Host` (without its port) is one of them reaches the app with
its whole path — `/`, `/apps/notes`, even `/_host/...` — and the app is **not**
reachable as `/<app>/` on any other hostname: `https://covtools.ramda.io/admin/`
is a 404. That is what lets a hostname carry an access policy of its own
(Cloudflare Access, in the deployment) without another hostname's policy
being a way round it. A hostname reaches one app; a second app claiming it
is refused. `x-forwarded-prefix` is empty for such an app.

**Each routed hostname is its own origin.** Under `--public-origin
https://covtools.ramda.io`, a request routed by `covtools-admin.ramda.io` is
told `x-forwarded-proto: https` and `host: covtools-admin.ramda.io` (the
public origin's scheme, and its port if it has one) — the name the route
matched, never one the client chose — so an app's same-origin check compares
against the origin its own pages were served from.

## State that outlives a request is safe one statement at a time

An app that keeps state — a count, a latch, a seat — has to keep it
somewhere a run's heap is not, because the heap ends with the run. That
place is the app's own `kv` or `sql`, and **what makes it safe is that one
statement is atomic by itself.** It is why the SQL authorizer refuses
`BEGIN`, `COMMIT` and `SAVEPOINT` (the table at the top of `sql.rs`): a
transaction held open across host calls would keep the connection locked if
the run hit its deadline in between. There is no transaction to open, so
there is none to leave open.

So **a read-modify-write is written as one conditional `UPDATE` whose row
count is the answer**, which is a compare-and-swap. Read the state, decide,
and write it back is three calls, and two runs can interleave them and both
act on the same reading. `examples/watch` tells somebody when a target goes
from up to down and must tell exactly once, however many checks saw it
(`changed()` in `store.cove`):

```sql
INSERT OR IGNORE INTO state (target, ok) VALUES (?1, ?2)  -- first check: not a change
UPDATE state SET ok = ?1 WHERE target = ?2 AND ok != ?1   -- rows > 0: this run flipped it
```

Of any number of runs that saw the target flip, one makes the row say so
and the rest find nothing to change. When the critical section holds
something that is not SQL — a call out, say — the same idea is a claim:
`UPDATE … SET busy_until = ? WHERE id = ? AND busy_until < ?`. The run that
gets a row owns the work until `busy_until`; the run that gets none steps
aside instead of waiting, which is what a run should do when waiting would
spend its own deadline.

**This holds on every path a run can come by**: a client's request, the
clock's `[[triggers]]`, a queue delivery, a deferred run. It survives a
restart, because the state is on disk. And it does not depend on there being
one process, because the database is what decides.

**A lock in the host was tried, and removed.** Issue #55 asked for
Cloudflare's Durable Objects' single writer per key, and `[keyed]` was
built: the host took a key's turn before admitting a request and held it
until the answer. It only ever covered the HTTP path. A run the host starts
itself — a trigger, a delivery, a deferred run — never took a turn, so the
guarantee it named was not the one it gave, and the example that
leaned on it said a check by the clock and one by hand could not both write
a target's history when they could. The lock was also in memory and
per-process, so a restart or a second process dropped it. What was given up
is the convenience: an app now writes its own compare-and-swap rather than
being handed a turn. What was gained is a guarantee that is the one it says.

**The other half of #55, an alarm per key, is a queue message with a
delay.** `queue.sendAfter` is below, under
[queues](#queues-work-that-is-written-down-first): it leases rather than
deletes, it retries, and what it cannot deliver is a dead letter somebody
can read — none of which the alarm had.

## Queues: work that is written down first

`defer` hands the host work and forgets it: if the host stops before the
run, the work is gone. That is the right trade for warming a cache and the
wrong one for anything that must happen. A queue is the other trade.

**The message is on disk when `send` answers.** The store is the app's own
SQLite file (`queue.rs`), beside its `kv` and its `sql`, and sending is one
insert — no delivery, no waiting, nothing that can fail after the app has
been told the message is taken.

**Delivery is a run of the app** ([a run the host asks for](#a-run-the-host-asks-for)),
which is what makes a consumer an ordinary path with ordinary limits rather
than a second kind of program. The host leases a message before it delivers
— `attempts` goes up and the message is hidden for `lease` — so a delivery
that never answers is tried again rather than lost, and a host that is
killed mid-delivery picks the message up when it starts.

**That means a delivery may happen twice**, and nothing here can prevent
it: the alternative is losing a message whenever the host stops in the
wrong moment. A consumer must be able to be asked twice. Saying so is
better than a guarantee that would be a lie.

**A lease outlasts the run.** A queue's `lease` must be at least a second
longer than the app's `limits.deadline`, and an app that declares
`[[queues]]` must have a deadline; otherwise the app is refused, with a
message naming both values. The deadline bounds a run, its wait for a
worker included, so a delivery whose app answers at all answers before its
lease runs out, and the second on top covers the settle being written. The
defaults (a lease of 60 s, a deadline of 10 s) keep it. **This is a
breaking change**: an `app.toml` whose lease was not past its deadline by
a second — a `deadline = "60s"` with the default lease, say — loaded
before and is refused now; give its queues a longer `lease` (the `watch`
example sets `lease = "61s"` beside its 60 s deadline).

**Only the delivery holding the current lease settles the message.**
A run can still be going when its lease runs out — a host stalled past the
second, a clock set forward — and then the message is handed to another. If
the first run's answer still counted, it would settle the second's message:
take it off the queue while the second is working on it, move the second's
lease so a third delivery could take it, or bury it while it is being taken.
So each lease writes a fresh lease id beside the message, and `done`,
`retry` and `bury` act only while it still matches. A run that outlived its
lease changes nothing; the app's log says so, and the counters do not count
it. `attempts` could not be the token: reviving a dead letter counts it
afresh from 0.

That took away what used to end a consumer that is always too slow: its own
late `done`. So **a lease that runs out on the last attempt makes a dead
letter** ("no answer within the lease"). Before it leases a queue, the
delivery loop buries what is due, has used `max_attempts` and still holds a
lease — which can only be a last delivery that did not answer in time,
because every settle either ends the lease (`retry`) or takes the message
away (`done`, `bury`). A delivery the host turns away before the app sees
it (the app's queue full, the host busy) is not an attempt: it gives the
message back with the attempt its lease counted taken back, so a busy host
cannot spend a message's attempts. And a lease takes from one queue at a
time, so one queue's loop never counts an attempt on another's message.

**A failure is retried with a doubling delay** until `max_attempts`, then
the message is buried as a dead letter with the reason. Burying rather than
dropping is the point of the feature: a queue that silently loses what
nobody could take is a queue that cannot be debugged. `minicloud queues`
reads them and puts one back, and the admin app has a page of what is on
each queue ([Seeing what is on an app's queues](#seeing-what-is-on-an-apps-queues)).

`batch` bounds how many of a queue are in flight at once, counted on the
app's counters, so a slow consumer cannot be given the whole queue at once;
the host looks for due messages once a second, which is one indexed
`SELECT` per app that has a queue.

**Work that should happen later is `queue.sendAfter`**: a send with a delay,
which is delivered no sooner than that many seconds from now. It is the same
message — on disk when the call answers, leased rather than deleted, retried
with backoff and buried if it cannot be delivered — so a reminder survives a
restart and a failed one is a dead letter someone can read, which a timer
held in memory is not. What it does not give is a moment: delivery is
looked for once a second, so "later" is "about then".

## A run the host asks for

A schedule and deferred work want the same thing, so they are the same
thing: `internal.rs` builds an `AppRequest` and admits it to the app's
queue exactly as the HTTP front does, and everything downstream — round
robin, the slice, parking, the deadline, the host-call budget, the
counters, the version a run is pinned to — is the same code. What the host
adds is a header saying who asked (`x-minicloud-trigger`,
`x-minicloud-deferred`), which it **strips from every request that comes
off a connection**, so an app may trust it. The answer comes back on a
`oneshot` that the caller may wait on to count and log, or drop; dropping
it does not cancel the run, which is the point.

**`defer` is this host's `waitUntil`, and it is a different shape.**
Cloudflare hands the runtime a promise and keeps the isolate alive until it
settles. Here the answer *is* the end of the isolate: a run's heap, its
budget and its deadline end together, and keeping one alive past its answer
would mean a request's budget outliving the request, which is the one thing
the whole design is built to prevent. So deferred work is another run, at a
path the app names, and the app routes it like anything else.

What it costs is that the work cannot see the run that asked for it —
there is no closure, only a path and a body. What it buys is that nothing
about the scheduler, the budgets or the counters needs a second case.

`[defer] max_in_flight` bounds what is outstanding: the count is taken
before the work is sent and given back when the run ends, so the bound
holds while the host is still starting it. A deferred run may defer again,
and an app that loops costs itself its own turns — the scheduler serves
apps round robin, so the loop is that app's problem.

## Running an app on a schedule

A trigger could have been a second entry — Cloudflare's `scheduled()` — and
is not. An app's entry is the one function the checker derives its required
capabilities from (`apps::admit`), the scheduler's `Start` carries a
`web.Request` and expects a `web.Response` back, and both would have had to
learn a second shape for a run that differs from a request in nothing but
who asked. **So the host makes the request itself**: `triggers.rs` builds an
`AppRequest` and admits it to the app's queue exactly as the HTTP front
does. Everything downstream — round robin, the slice, parking, the deadline,
the host-call budget, the counters, the version a run is pinned to — is the
same code, and the app routes a trigger with the routing it already has.

What a second entry would have given for free is **knowing who called**,
and that is bought with one header: the host sets `x-minicloud-trigger` on
a trigger's request and **removes it from every request that comes off a
connection** (`server.rs`, beside the forwarding headers). A client cannot
send it, so an app may trust it. The path stays an ordinary path of the
app's, which is deliberate: it is how a scheduled job is run by hand while
it is being written, and an app that does not want that refuses the path
for anyone but the clock, in one line.

**Minutes, UTC, and no catching up.** `cron.rs` is a five-field cron over
`std` alone — no date library is in the tree, and `civil_from_days` is
thirty lines. A tick looks at each minute once (a timer that wakes twice in
a minute fires nothing twice), and a minute the host was not running for is
gone rather than run late: a host that was down for an hour should not
answer by running an hour of jobs at once. A schedule whose last run has
not finished lets the tick go by and counts it, because a job that takes
longer than its period would otherwise fill its app's queue with copies of
itself.

**A trigger belongs to the app's version**, like its limits and its files:
an update changes a schedule with the code, and a run that is in flight when
the version changes finishes on the version it started on.

## What happened an hour ago

A counter says what an app has done, never when: a host that has served
four million requests and one that served them all yesterday read the same.
Once a minute the host takes every app's counters, takes the **difference**
from the last sample, and writes that minute as a row (`series.rs`, SQLite
beside the apps' own stores). A row is what happened *in* that minute,
which is the only shape a rate can be read off later, and a minute in which
nothing happened is written as a minute of nothing — which is what tells a
gap in the traffic from a gap in the host.

The first sample after a start writes nothing. There is nothing to take a
difference from, and counting the running totals as one minute's work would
put a spike at every restart.

**Two granularities, because retention and resolution pull apart.** Minutes
for 48 hours answer "what happened this morning"; at the top of each hour
the hour that ended is rolled into an hour table kept for 90 days, which
answers "is this worse than last month" at a thousandth of the rows. The
counts are summed and the percentiles are kept at their **worst** rather
than averaged: an hour that held one terrible minute should say so.

**The request times are a histogram** (`histogram.rs`): sixteen bounds from
100 µs to 10 s, cumulative, as Prometheus means one. A quantile off it is an
estimate — interpolated inside the bucket it falls in — and that is the
trade for being able to add two of them together, which is what the minute
rows and `/_host/metrics` both need. Keeping every duration to answer an
exact p99 would cost memory without bound, and a p99 that is right to the
microsecond is not a question anybody asks.

**`/_host/metrics` is the exposition format**, not a format of this host's,
so a scraper already knows how to read it. Every series is labelled by app,
because the totals are a sum anyone can do and the per-app numbers are not
recoverable from them.

**The host is sampled too, because the limits that matter are its.** Every
app runs in one process on one worker pool, so no app has a memory or CPU
limit of its own to run into: the systemd unit's `MemoryMax=` and
`CPUQuota=` are on the process, and when one is reached every app slows or
dies together. The same tick reads the process's cgroup v2, `/proc` and
`statvfs` of the data directory (`platform.rs`) and keeps a row beside the
apps' (`host_minute`, `host_hour`, the same retention). The CPU is a counter
and is kept as a difference, the first tick keeping nothing, as for the
apps; the rest are gauges. An hour keeps the *worst* minute of each gauge —
the most memory, the least free disk — because the question is how close
the host came to its edge, and an average hides the minute it nearly fell
off; only the load average, already a mean, is averaged. Every figure from
a file is optional: macOS, cgroup v1 and a sandbox that hides a file give
`null`, and the sampler never fails for one. A test replaces the source
(`Host::set_platform_source`), so nothing it asserts is the machine's.

## Where a request's time went

The counters and the minutes say how much and when; they cannot say *why*
one request took 300 ms — whether it waited for a worker, ran, or sat
parked at a fetch, and whether it came back on the worker it left. The
request timeline (`timeline.rs`) is the Cove repository's edge sample's
recorder, made always on and bounded.

**A request carries its own record.** A `Tracker` rides in the request's
`Flight`, which already goes from the HTTP side to a worker, to the I/O
runtime while parked, and back; whoever holds the flight writes the
request's events into its record. So an event costs a latch nobody else is
taking, and nothing is added under the run queue's mutex, which every
worker's every take goes through. The shared state — 32 shards by request
id, each a ring in arrival order — is touched when a request is admitted
and when it ends.

**Where each event is taken:** arrival at the top of the HTTP side's
`reply` (so reading a slow body shows as time before it is queued); queued
where the `Flight` is made; a run's start in `Engine::sliced`, with its
worker and whether it is a start, a resume or a continue; its end in
`settle`, as a park (with `PendingWork::op`), a yield, or the answer; the
park's end on the I/O runtime, where the host work, the deadline and the
client's going away race; a rejection where the request is turned away.
The end is recorded before the answer is sent, so a client that has its
answer finds it recorded. A flight dropped without an end — a panic, a
queue closed under it — ends its record as the HTTP side answers it, 500.

**Bounded, and cheap where it is hot.** The last hour and at most
100,000 requests — about 40 MB at the ~400 bytes a record is — the oldest
first; a request still in flight is set aside
rather than forgotten. Nothing is written down: it is a picture of now, and
the minutes are the history. The HTTP threads are what a host answering
small requests runs out of first, so what they do for the timeline is one
latch and a refilled record — no allocation in a steady state, records
being reused, and forgetting is done by the worker that ends a request. It
costs under 1% of `hello`'s capacity, within the noise
([measurements](measurements.md#what-the-request-timeline-costs)).

**What it does not see.** Not the query string, which may hold what a
caller did not mean to keep; not headers or bodies. A `kv` or `sql` call
answers inside the run, so it is running time — accepted for now, because
splitting a run at every synchronous call would put an event on the
hottest path an app has. An asset never runs.

**A view is drawn from phases.** The events of a request are made, when
asked, into phases — waiting for a worker (after arrival, a park or a
yield), running on a worker (and whether it moved), parked at a host call
(and what ended it) — and a window of them into the workers' run segments,
three strips of mean counts (running, parked, waiting) and p50/p99 per app.
A window reads only what overlaps it: a binary search over each shard's
ring, which is in arrival order, skips what arrived after it, and of what
arrived before only the end time — an atomic — is read; only the newest
2,000 are assembled whole. Segments of one worker closer than a pixel are
merged for a page, keeping how long the worker ran in them and how many
runs each app's were, so a merged bar is drawn by how busy the worker was
and never as one request that held it. A page asks for a window of the past
in Unix milliseconds (`host.timelineRange`), which the host turns into its
own time; panning and zooming are links, and the axis is the clock in JST.
A test hands the views a recorder filled with times of its own
(`Host::set_timeline_source`, `Recorder::fixed`), so a page of the timeline
is the same every run.

## The front door

Until `--tls-cert`, the host could not be reached by a browser without
something else in front of it. Now it can: rustls on the public listener,
ALPN offering `h2` then `http/1.1`, and `hyper_util`'s auto builder serving
whichever the client chose. A plain listener stays HTTP/1.1 — a browser
will not speak h2 without TLS, and h2c would be a second protocol for
nobody.

**The pair is read at start and on SIGHUP, never per connection.** Reading
per connection would make a half-written renewal a half-broken host, and
would put two file reads in front of every handshake. A reload that does
not read keeps what is serving, because an old certificate beats none. The
handshake itself is on the connection's own task, under a timeout, so one
that never finishes costs that task and nothing else.

**ACME is the other half**, and is `--acme-domain`. The protocol itself is
`instant-acme`'s; what this host owns is where the account and the pair are
kept, when to order, and the door the authority knocks on.

- **The account is kept** (`<data>/_host/acme/account.json`, mode 0600). A
  new account on every start is a new rate-limit allowance on every start,
  which an authority is right to refuse.
- **A certificate with more than thirty days left is served, not
  reordered.** The certificate on disk is the one thing that has to survive
  a restart, and renewing with a third of its life left turns a month of
  failures into a month of warnings rather than an outage.
- **The challenge is HTTP-01 on a listener of its own**, because it has to
  be answered in the clear on port 80 by a host that does not have a
  certificate yet. That listener redirects everything else to `https://`,
  which is the thing every deployment was going to want from it anyway.
- A host whose certificate has not been ordered yet **starts**, with a
  resolver that offers nothing: refusing a handshake is the honest answer
  when the host has nothing to prove who it is with, and the first order is
  usually seconds away.

The exchange with an authority is the one part of this host no test covers:
a test has no authority, no public name and no port 80. What is tested is
what this host owns — the challenge listener, what is written and read
back, the renewal clock, and that a certificate already on disk is served
without an order.

**Compression is one pass over one buffer**, because the bodies are already
whole: an app's answer is a `String` and a file is bytes the host holds. It
is done where it pays — over a kilobyte, text-shaped, asked for, and only
kept if it came out smaller — and nowhere else. A streamed answer
([an answer written as it goes](#an-answer-written-as-it-goes)) is not
compressed, which is right for server-sent events anyway: a buffered
encoder would hold the lines it is meant to deliver.

**A `*.name` is one label**, and the lookup is the exact name first and the
pattern second, so a name that is both is its own. A wildcard that crossed
a dot would claim `a.b.example` from whoever holds `*.b.example`.

## A socket a run holds

A WebSocket is the one thing here that is not a request and an answer, and
it needed less new machinery than that suggests: the run already knows how
to be found by a host call ([a run that writes its own
answer](#a-run-that-writes-its-own-answer)), and already knows how to park.

`socket.accept()` answers `101` through the same `Answering` that answers
everything — so the run's returned response is not the answer afterwards,
exactly as it is for a stream — takes hyper's `OnUpgrade` (which the HTTP
front put beside the answer when it saw the request asking) and starts a
task that holds the connection and speaks frames. The run and that task
have a channel each way, and **both ends of both channels park**: a socket
nobody is speaking on is a parked run and no worker, which is the whole
reason a host with two workers can hold a hundred sockets.

**The request arrives as a request.** The app sees the headers and may
refuse the upgrade with an ordinary response, which is both the simplest
thing to implement and the right place for the decision: whether this
client may open a socket is the app's question, not the host's.

`serve_connection` had to become `with_upgrades` on both listeners, which
is the whole of the change to the HTTP front.

**The handshake's SHA-1 is written out** rather than taken from a crate:
twenty lines, the only place this host hashes anything for a protocol, and
a hash whose weakness does not matter — the handshake uses it to prove the
server read the key, not to keep a secret.

What bounds a socket is what bounds a run, which means the **deadline is
how long it may be held**. That is a strange sentence until you notice it
is the same sentence as everywhere else: a run has a budget, a socket is a
run, so a socket has a budget. An app that wants an hour says an hour.

## A run that writes its own answer

`stream.open` and `stream.send` let a run answer while it is still going,
and two things had to be true for them.

**A host call has to know which run made it.** A module instance is the
app's, shared by every run of it, and `HostApi::call` carries no run: for
`defer` that was fine (the work is the app's, not the run's), and for a
chunk it is not — the chunk belongs to one connection. A worker runs
**one** run at a time, so the scheduler puts the run's answer in a
thread-local around every slice of it (`sched::Engine::sliced`) and a host
call reads it there. Outside a run there is none, and the call is the app's
`Err` rather than a panic.

**The answer has to be able to go out early.** The one-shot the HTTP side
waits on is behind an `Answering` now, which answers once: whoever is first
— the run returning, or `stream.open` — is the answer, and the other is a
no-op rather than a second one. `open` sends a `Reply` whose body is the
receiving end of a channel, which the HTTP front turns into the same
written-as-it-goes body a followed log uses; `send` puts a chunk on it, and
the run ending drops the sender, which ends the body.

**`send` parks.** It answers pending with a future that awaits room on the
channel, so a run that is faster than its reader gives up its worker rather
than holding one, and backpressure is the channel's. A client that went
away is a closed channel, which is an `Err` the run is told about — the one
way a run finds out, since nothing else about a run knows there is a
connection.

What this costs is that a streamed answer has no run headers (the head goes
out before the run has cost anything) and cannot be gzipped (a buffered
encoder would hold the lines it is meant to deliver). What it does not cost
is any change to the budgets: the deadline, the host calls and the heap
bound a streaming run exactly as they bound any other, and `max_response_bytes`
becomes the bound on one chunk, which is the only thing it can still mean.

## An answer written as it goes

Every answer used to be a `Full<Bytes>`: the host had all of it before it
wrote any of it, which is true of a run's answer — an app's response is a
Cove `String` — and false of a log being followed, which has no end.

The HTTP front's answers are a `BoxBody` now (`server.rs`), and one of them
is fed from a channel: `to_streamed` writes the headers at once and each
`Bytes` sent afterwards as a chunk. The connection ends the answer by being
dropped, which the send on the channel is what notices, so a client that
goes away costs one failed send and a task that returns. Nothing is
cancelled and nothing is cleaned up, because there is nothing but the task.

`GET /_host/apps/<app>/logs?follow=1` is the first user: the ring's last
lines, then every line as `LogRing::push` writes it, over a
`tokio::sync::broadcast` the ring hands out. A broadcast with no receiver
costs a failed send, so nothing has to know whether anybody is watching; a
reader that falls more than 1,000 lines behind is **told** (`: N line(s)
missed`) rather than quietly skipping them, because a log with an unmarked
hole in it is worse than one that says it has a hole. A `: ping` every 15
seconds keeps a proxy from closing the connection as idle.

The same answer is on the admin listener at `GET /apps/<app>/logs?follow=1`
with the token, which is what `minicloud logs --follow` asks for: the CLI
should not depend on where `--ops-listener` put the views.

**This is not an app streaming.** An app still answers one `String`, and
making a run able to write as it goes is the rest of #51.

## `[rate]`: how often one caller may ask

`max_queued` bounds how many requests may wait for an app and
`--max-in-flight` how many the host holds at once, and neither stops one
caller filling an app's queue by itself: the app's neighbours are fine, and
the app is not.

A token bucket per caller, checked in the HTTP front before anything else
is done for the request — before the body is read, before a file is looked
for, before the queue is touched — so a caller over its allowance costs a
map lookup and a 429. Tokens are a `f64`: "sixty a minute" is one token a
second, and an integer count could not say that.

**Who a caller is, is configured, because the host cannot know.** The
connection's address is the honest default and is the proxy's address
behind a proxy; `header:<name>` names a header instead, for a host behind
something that sets one, and the host does not verify it because a header
is only as good as whoever is allowed to set it. `app` is one bucket for
everyone.

**The buckets are the host's.** They live in the `Front`, not on an `App`,
so an update does not reset an allowance — a deploy loop would otherwise be
a way around the limit — and the host keeps at most ten thousand of them,
sweeping those that are back to full first, because a full bucket is the
same as no bucket at all.

## Static files: the app's `assets/` directory

A run answers a `web.Response` whose `body` is a `String`, and a Cove
`String` is UTF-8 — `core.bytesFinish` refuses a `ByteBuffer` whose bytes
are not. An app therefore **cannot answer an icon**, and until this the
admin app inlined its whole stylesheet into every page with
`style-src 'unsafe-inline'` in its policy, because it could not serve a
`.css` file either.

Serving a file does not need those bytes to cross the Cove boundary. The
app's `assets/` directory is read by the host and answered by the host
(`assets.rs`, and [`server::Front::reply`](../crates/minicloud/src/server.rs)
before the request is admitted), and the app never sees the request.

**The path below the app's prefix names the file**: `assets/style.css` is
`/<app>/style.css`, or `/style.css` on an app reached by a hostname of its
own. The directory is the app's root rather than a `/assets/` prefix of its
own because `/favicon.ico` is a path a browser chooses, not one a page does.
A file wins over the app for `GET` and `HEAD` alone, so a form may still be
posted to a path a file has; a path that names no file reaches `handle`
exactly as before, which is what keeps this from changing any app that has
no `assets/`.

**Read once, at load, into memory, and part of the version.** The files are
in the version hash (`apps::content_hash`), so changing a stylesheet is a
new version with everything that follows from it: an update switches the
code and the files together, a request in flight keeps the files it started
on, and a rollback takes both back. Reading them from disk per request would
break that — the bytes an `ETag` names would stop being the bytes that are
served — and would put a `read(2)` on the HTTP path. The cost is memory:
the files of every live version, which is why `[assets] max_bytes` is 8 MiB
by default and an app over it is refused at load, naming the file that
crossed it.

**The headers are the host's.** The content type comes from the extension,
with `nosniff` beside it, since a browser guessing another type for a file
the app did not choose the type of is the one way this could surprise. The
`etag` is FNV-1a over the bytes — the hash the version id already uses —
and `If-None-Match` is answered `304`. `cache-control` is `no-cache` by
default: the files change with a deploy, and a stale stylesheet is worse
than a conditional request that an `etag` makes cheap. An app whose file
names carry a hash of their contents sets `[assets] cache_control` to
something long instead.

**A version in the URL is the app's job, not the host's.** The path of a
file does not change between deploys, so a browser or a proxy that holds one
holds it against the new pages too — and `no-cache` is a request to
revalidate, not a guarantee that something in the middle will. The admin app
therefore links its own files by version, `/style.css?v=<the version
serving>`, from `host.selfVersion`: the new pages ask for a URL no old cache
has, so there is nothing of an old deploy left to be served. The host reads
no query — the path names the file, and `/style.css?v=anything` is the same
file — because the query is there to change the URL and not to be checked.
Which also means `[assets] cache_control` stays `no-cache` on the admin app:
one header covers every file of an app, and the paths a browser chooses for
itself (`/favicon.ico`) carry no version, so a long `max-age` there would be
the staleness this is about with no way left to clear it. Saying it properly
— long where the `?v=` names the version serving, `no-cache` where it does
not — is a rule per request and so the host's to grow, not an app setting.

**They are public.** The answer happens before the app runs, so an app's
own `auth.check` or `auth.identity` is not in front of its files. That is
the point — a login page needs its stylesheet — but it means `assets/` is
not a place for anything private. On the server the admin app's files are
behind Cloudflare Access all the same, because Access is in front of the
hostname rather than inside the app.

What an app may **compute** as bytes — an uploaded image, a generated PNG —
is a different question, and needs a byte type that can cross the host
boundary; issue #57 says so and does not do it.

## `kv`: the app's persistent store

Each app has its own store, `<data>/<app>/kv.sqlite3`, opened only for an app
granted `kv`. One database per app is the namespace: an app's `kv` holds its
own connection and nothing else, so no key it can write reaches another
app's data.

- `list(prefix, after, limit)` answers the entries (`kv.Entry { key, value
  }`) whose key starts with `prefix` and is greater than `after` (`""` for the
  first page), ascending, at most `limit` (1–1000); `listDesc(prefix, before,
  limit)` the same descending, keys less than `before`. The last key of a page
  is where the next one starts, so an app that writes `event:<zero-padded
  time>` lists its newest events with `kv.listDesc("event:", "", 50)` — the
  shape the webhook lab's history (#2) and the benchmark ledger's runs by
  prefix (#3) need.
- A `put` past a quota is the app's `Err`, naming the quota, and the store is
  unchanged: `max_key_bytes`, `max_value_bytes`, `max_keys`, `max_bytes` (keys
  and values summed) under `[kv]`.

**Why SQLite**: it is the boring choice — one file per app, a format that
outlives this program, crash-safe by design — and `rusqlite`'s `bundled`
feature builds it in, with no system library. It runs in WAL mode with
`synchronous = NORMAL`: a committed write survives the host process
crashing; a power loss may lose the last transactions, never the file.

**Every call answers at once, on the worker**, never on the async runtime's
threads: a call into a local SQLite is cheaper than parking, and a worker
held for a `put` is far inside a 2 ms slice
([measurements.md](measurements.md#the-cost-of-a-kv-call-against-a-park)).

## `sql`: the app's relational store

`kv` answers "what is under this key" and "what keys start with this", and
an app that wants "the ten longest, by author, since Tuesday" has to read
everything and sort it in Cove. `sql` is a second store for that question —
`<data>/<app>/sql.sqlite3`, its own file beside `kv.sqlite3`, in WAL mode,
for the reasons [`kv`](#kv-the-apps-persistent-store) gives — and it does
not replace the first: a counter or a blob of state is still `kv`'s.

**Values are text, because the schema cannot say otherwise.** A host type is
a struct, an array or a named type whose `TypeSchema` carries the *names* of
an enum's cases and not their payloads (`cove-schema`), so a value that is
sometimes an `Int` and sometimes a `String` cannot be declared at all. Every
value an app reads is therefore `Option<String>` — `NULL` is `None`,
`INTEGER` and `REAL` are decimal text, a `BLOB` is the app's `Err` rather
than a mangled string — and every parameter is a `String`, **bound** and
never pasted, which SQLite's type affinity turns back into a number where
the column is one. The one thing text cannot say is `NULL`: `nullif(?1, '')`
says it.

**The app writes the SQL, so the connection is what is locked down.** An
authorizer, which SQLite consults while a statement is prepared, refuses
`ATTACH` and `DETACH` (they name other files on the disk), `PRAGMA` (the
host set the ones it needs: `foreign_keys`, and the `max_page_count` that is
`[sql] max_bytes`), transactions and savepoints, and virtual tables.
Everything else, `CREATE TABLE` and `ALTER TABLE` included, is the app's.
**Transactions are the interesting refusal**: one held open across host
calls would keep the connection locked if the run hit its deadline in
between, and nothing in the host could tell that from a slow app. A single
statement is atomic by itself, which is what an ordinary write needs;
several that must be atomic together are an operation this module does not
have yet rather than one it fakes.

**One statement a call.** `sqlite3_prepare_v2` compiles the first statement
and hands back the rest, and rusqlite drops the rest silently: an app that
wrote two would see the first run and the second vanish. The host asks
SQLite whether a second statement follows and refuses the call if one does.

**On the worker, with a bound.** A call answers at once on the worker, as
`kv`'s does, rather than parking — but a query, unlike a `get`, can be
arbitrarily slow, and a worker held is a worker not yielded. So every
statement runs under SQLite's progress handler with `[sql]
max_statement_ms` (20 ms by default, ten slices) as its bound, and one that
runs past it is stopped where it is and is the app's `Err`. The number is
deliberately small: a query that needs longer wants an index.

## `fetch`: outbound HTTP to an allowlist

`[fetch] allow` in `app.toml` lists where the app may call:
`scheme://host` (the scheme's port), `scheme://host:port`, or
`scheme://host:*` (any port), `http` or `https`. The `fetch` capability says
an app may call out at all; the allowlist says where, and the host holds
every call to it at the boundary:

- a URL off the list is answered `Err` **at once, before anything is
  sent**, without parking (counted in `fetch.refused`);
- redirects are not followed — a 3xx is answered as the response it is — so
  an allowed host cannot send the request elsewhere;
- hosts match by name as written; what a name resolves to is the system
  resolver's, so this is no defence against DNS rebinding.

**An API key the app never sees.** An app cannot read a secret (`auth.check`
only compares against one), so a key for an authenticated API is given to the
host instead, which adds it as a header when the request is sent:

```toml
[secrets]
openai = { env = "OPENAI_API_KEY" }
gemini = { env = "GEMINI_API_KEY" }

[fetch]
allow = ["https://api.openai.com", "https://generativelanguage.googleapis.com"]

[fetch.headers."https://api.openai.com"]
authorization = { secret = "openai", prefix = "Bearer " }

[fetch.headers."https://generativelanguage.googleapis.com"]
x-goog-api-key = { secret = "gemini" }
```

- Each `[fetch.headers."<origin>"]` key is a header name; its value names a
  `[secrets]` entry, with an optional literal `prefix` sent before it
  (empty by default).
- The app is refused at load, saying why and never quoting the value, if
  the origin is not a bare origin (`scheme://host[:port]`: no path, no
  `:*`), is not an entry of `[fetch] allow` (compared as parsed, so
  `https://api.openai.com` and `https://api.openai.com:443` are the same), if
  the secret is not in `[secrets]`, the header name is not one, or the prefix
  and secret cannot be a header value.
- The header is added to a request whose URL has **exactly that origin** —
  scheme, host and effective port — and to no other. It is added after the
  app's headers and **replaces** an app-supplied header of the same name
  (names compare case-insensitively).
- The value never reaches the app: not in a `fetch.Response`, an `Err`, a
  log line, the stats or the admin app's view of the app. Redirects are not
  followed, so a redirect cannot carry it to another origin.
- The admin app's allowlist changes cannot point it at a new origin: the
  origin is checked against `[fetch] allow` as `app.toml` writes it, and only
  `app.toml` can bind a header. Removing the origin from the allowlist at run
  time leaves the app loaded, and requests there are refused like any other
  off the list.

What the host does not do is defend the value against the app's own code:
apps share one process (see the README's [Security](../README.md#security)). This keeps the key out of
the app's hands as a string, in its logs and in its answers.

A fetch parks the run. It is a future on the I/O runtime (reqwest over
hyper) raced against the run's deadline and against the client going away;
whichever ends first drops the future, which closes the outbound
connection, so the upstream sees the request abandoned. Each fetch is also
bounded by `[fetch] timeout` (default 10 s), `max_request_bytes` (its body,
default 1 MiB) and `max_response_bytes` (default 4 MiB). Any response is
`Ok`, whatever its status; an `Err` is a fetch that got none.

**Compressed responses are decoded.** A request goes out with
`accept-encoding: gzip, deflate, br` unless the app set its own
`accept-encoding`, which is sent as written. A response with
`content-encoding: gzip`, `deflate` or `br` is decoded whatever the request
asked for — some servers compress regardless — and the app sees the decoded
body, without the `content-encoding` and `content-length` headers, which
describe the encoded one. Any other encoding is handed over as it came,
header included.

- `max_response_bytes` bounds the **decoded** body. Decoding streams and
  stops as soon as the limit is passed, so a compression bomb (a megabyte
  that inflates to a gigabyte) is the usual too-large `Err`, refused without
  being inflated.
- A body that does not decode — corrupt, truncated, or not in the encoding
  its header names — is an `Err` (`fetch could not decode the response: …`),
  never garbage text.

**https** is supported, with rustls and the Mozilla root set
(`webpki-roots`), so there is no dependency on the system's OpenSSL. Inbound
TLS stays the reverse proxy's job, but outbound calls have no proxy to do it
for them.

## Cloudflare Access: who is asking

An app behind [Cloudflare Access](../deploy/cloudflare.md) asks the host who
its user is with `auth.identity(request.headers)`, instead of a login of its
own (issue #23). Access adds the user's token — a JWT signed by the team's
keys — to every request it lets through, as `Cf-Access-Jwt-Assertion` (and
the `CF_Authorization` cookie, read when the header is missing). The host
verifies it: the RS256 signature by a key of the team's JWKS
(`https://<team>/cdn-cgi/access/certs`, chosen by `kid`), `iss` the team,
`aud` one of the app's `[access] aud`, `exp` and `nbf` with a minute's
leeway, an `email` claim (so an Access service token is not an identity),
and `[access] emails` if set. The answer is `Ok(auth.Identity { email, via:
"access" })`, or `Err` saying what was wrong.

The keys are fetched with the first token and kept; a token naming a key not
held fetches them again (Cloudflare rotates them), at most once a second, and
keys an hour old are refreshed when next needed. If the keys cannot be
fetched, every token they would verify is refused and the failure is logged:
it fails closed. A fetch parks the run; a token whose key is held is answered
at once.

`[access] token` names a `[secrets]` entry presented as `Authorization:
Bearer <secret>` or a Basic password (`via: "token"`, no email). With Access
off for the app — a local run, where `team` or `aud` is unset — it is the way
in, and `auth.usesAccess()` is `false`, so the app can answer 401 with a
login prompt. With Access on it is accepted only under `fallback = "token"`;
the admin app and the webhook lab say `"none"`, so a request that did not come through Access
— straight to `127.0.0.1:8790`, say — is refused whatever it presents, and
no browser prompt is shown (403). A script that needs in goes through Access
too, which takes an Access service token anyway, and a service token has no
email: the admin listener is the way for a script on the machine.

The apps only call `auth.identity`; nothing else of a request is affected,
so a path an app does not guard — the webhook lab's receive URLs — needs no
token and fetches no keys.

## What the deployment relies on from the host

- **`--ops-listener admin`**: `/_host/` is 404 on the public listener; the
  operations views are on the admin listener, which is never in the tunnel.
- **`[route] hosts` in the admin app's `app.toml`**: the admin UI is
  `https://covtools-admin.ramda.io`, a second public hostname on the same
  tunnel and port with an Access application of its own; it is not
  reachable under `covtools.ramda.io` ([Routing by hostname](#routing-by-hostname)).
  Its changes live in `data/_host/`, which a release does not touch. No flag
  and no unit change was needed for it.
- **Cloudflare Access, verified by the host** (`[access]` in the admin
  app's and the webhook lab's `app.toml`, the values in `~/cove-tools/env`):
  the admin UI and the webhook lab's admin pages know their user from the
  Access token, and refuse a request that did not come through Access
  ([Cloudflare Access](#cloudflare-access-who-is-asking),
  [deploy/cloudflare.md](../deploy/cloudflare.md) §6). No flag and no unit
  change: `EnvironmentFile=` already reads the env.
- **`--public-origin https://covtools.ramda.io`**: cloudflared reaches the
  host as plain HTTP on localhost, but the browser's `Origin` is the public
  one. The host tells every app `x-forwarded-proto: https` and
  `host: covtools.ramda.io`, so the webhook lab's and the ledger's
  cross-site refusals compare against the right origin and the receive URLs
  the lab shows are the public ones. Without it a client cannot claim
  `https` (the host sets `x-forwarded-proto: http` itself); `--trust-proxy`
  believes a proxy's `X-Forwarded-Proto` and `X-Forwarded-Host` instead, for
  a proxy that serves several names. Nothing in the host or the apps uses the
  client's address; cloudflared's `cf-connecting-ip` reaches the apps as a
  header (the webhook lab stores it with each request).
