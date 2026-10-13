# minicloud

A small self-hosted cloud for web apps written in
[Cove](https://github.com/myuon/cove), on one machine.

An app is a directory of Cove code and an `app.toml`. The host loads each app
once — checks it, prepares it, compiles it to machine code where the machine
has a native tier — and then runs **every request in a fresh isolate** on a
shared pool of worker threads. A run that waits on I/O parks and holds no
worker; a long run yields its worker at a safepoint when others are waiting;
apps are served round robin, so one busy app cannot starve the rest.

Files an app serves as they are — a stylesheet, an icon — go in an
**`assets/` directory** beside its code and are answered by the host, bytes
and all, before the app runs. An app that must run on its own asks for a
**schedule** (`[[triggers]]`), and the host makes the request itself.

Each app gets only the **capabilities** its `app.toml` grants (logging,
timers, a persistent key-value store, outbound HTTP to an allowlist, the
clock, randomness, authentication), and runs under **limits** — a deadline,
a host-call budget, a heap, a queue — that end that app's request and nobody
else's. Apps are **deployed, updated and rolled back** on the running host:
a new version is switched to only if it loads, and requests in flight finish
on the version they started on. An **admin app** changes apps' grants and
limits at run time and keeps their secrets, and the host shows what it is
doing as **stats**, an operations page and per-app logs.

Same-process isolation is a fault and resource boundary for code you run
yourself, **not a security boundary against untrusted or malicious code**:
see [Security](#security).

The repository:

- **[`crates/minicloud`](crates/minicloud)** — the host, in Rust: the
  `minicloud` binary.
- **[`apps/admin`](apps/admin/README.md)** — the admin app, the one app a
  release bundles.
- **[`deploy/`](deploy)** — the install script, systemd unit and backups that
  run the host on a server.
- **[`examples/`](examples)** — apps deployed like any other: not part of a
  release, checked and tested by CI.
- **[`docs/`](docs)** — [design notes](docs/design.md), [what the tests
  cover](docs/testing.md) and [measurements](docs/measurements.md).

minicloud was called cove-tools, and its binary `cove-host`, until 0.5.0
([issue #43](https://github.com/myuon/minicloud/issues/43)); a server keeps
some of those names ([Names kept from cove-tools](#names-kept-from-cove-tools)).

## Quick start

```console
$ cargo build --profile checked
$ ./target/checked/minicloud serve --apps examples
minicloud: loading apps from examples
minicloud 0.19.0 (cove 2af2d11c5fe02ea3f8f4a057e24cb4f2cc297dc2)
  algo       v1-cb6c97f3  requires [time] (lower bound)  granted [time]  ok: 456 fn on native, checked in 37.1 ms, prepared in 83.5 ms
  crunch     v1-c4b9d04b  requires [-]  granted [-]  ok: 227 fn on native, checked in 16.9 ms, prepared in 1.6 ms
  hello      v1-e421b416  requires [-]  granted [-]  ok: 234 fn on native, checked in 10.7 ms, prepared in 2.3 ms
  ledger     v1-f6ca8c2f  requires [-]  granted [-]  REFUSED: `examples/ledger/app.toml`: secret `post`: the environment variable `LEDGER_TOKEN` is not set
  notes      v1-cf6980a0  requires [kv, log]  granted [kv, log]  ok: 226 fn on native, checked in 4.2 ms, prepared in 2.6 ms
  proxy      v1-f26c6f57  requires [fetch, log]  granted [fetch, log]  ok: 227 fn on native, checked in 17.7 ms, prepared in 2.0 ms
  slow       v1-2914903a  requires [log, timer]  granted [log, timer]  ok: 220 fn on native, checked in 10.9 ms, prepared in 1.6 ms
  webhooks   v1-3f69757e  requires [-]  granted [-]  REFUSED: `examples/webhooks/app.toml`: secret `admin`: the environment variable `WEBHOOKS_ADMIN_TOKEN` is not set

listening on http://127.0.0.1:8080 — 16 worker thread(s), a run yields after 2.0 ms while others wait, a fresh isolate per request, apps served round robin
  stats: curl -s http://127.0.0.1:8080/_host/stats   ops page: http://127.0.0.1:8080/_host/ui
  admin: http://127.0.0.1:8081 (token in data/admin.token); update with `minicloud update <app>`
```

Two apps were refused because they take a secret from an environment
variable that is not set; every other app serves, and a refused one answers
503 with that reason. Set `LEDGER_TOKEN` and `WEBHOOKS_ADMIN_TOKEN` to any
value to serve them too. (`--profile checked` is release with debug
assertions and overflow checks on; `--release` works as well.)

An app is reached under `/<app>/`:

```console
$ curl -i 'http://127.0.0.1:8080/hello/?name=Cove'
HTTP/1.1 200 OK
content-type: text/plain; charset=utf-8
x-cove-run-instructions: 82
x-cove-run-yields: 0
x-cove-run-yields-declined: 0
x-cove-run-parks: 0
x-cove-run-worker-us: 152
x-cove-run-wall-us: 211
x-cove-app-version: v1-e421b416
content-length: 21

Hello, Cove! (GET /)
$ curl -s 'http://127.0.0.1:8080/crunch/?n=100000'
9592 primes up to 100000, the largest 99991
$ curl -s 'http://127.0.0.1:8080/slow/?ms=200&times=2'
waited 2 x 200 ms
$ curl -s -X PUT --data 'buy milk' http://127.0.0.1:8080/notes/todo
stored todo
$ curl -s http://127.0.0.1:8080/notes/todo
buy milk
$ curl -s 'http://127.0.0.1:8080/proxy/?url=http://127.0.0.1:8080/hello/?name=proxy'
Hello, proxy! (GET /)
$ curl -s 'http://127.0.0.1:8080/proxy/?url=http://example.org/'
`http://example.org:80` is not on app `proxy`'s fetch allowlist (http://127.0.0.1:8080, http://localhost:8080, https://example.com:443)
```

`slow`'s answer says it parked twice (`x-cove-run-parks: 2`) and spent
0.2 ms of its 400 ms on a worker (`x-cove-run-worker-us: 204`). A bug in an app is one failed request,
answered with the runtime's diagnostic and the limit that stopped it:

```console
$ curl -i http://127.0.0.1:8080/hello/spin
HTTP/1.1 504 Gateway Timeout
content-type: text/plain; charset=utf-8
...
x-cove-stop: deadline
x-cove-app-version: v1-e421b416

error[cove::runtime]: execution stopped: wall-clock deadline of 2s exceeded
  --> hello/hello.cove:46:9
   |
46 |   while turns >= 0 {
   |         ^^^^^^^^^^
...
```

`GET /` lists the apps; `/_host/ui` is the operations page
([Operating](#operating)). Stop the host with Ctrl-C.

`serve` takes:

| flag | default | |
| --- | --- | --- |
| `--apps DIR` | `apps` | one directory per app |
| `--data DIR` | `data` | the apps' state (`<data>/<app>/`) and the host's (`<data>/_host/`, `<data>/admin.token`) |
| `--addr ADDR` | `127.0.0.1:8080` | the public listener |
| `--admin ADDR` / `--no-admin` | `127.0.0.1:8081` | the admin listener: deploys, updates, configuration; keep it on localhost |
| `--workers N` | one per hardware thread | threads that run Cove |
| `--io-threads N` | `2` | threads for HTTP and pending host work |
| `--slice MS` | `2` | how long a run may hold a worker while others wait; `0` never asks a run to yield |
| `--max-connections N` | `10000` | connections open at once |
| `--max-in-flight N` | `10000` | requests admitted at once, over every app |
| `--backend auto\|vm\|native` | `auto` | the native tier where the machine has one (Unix x86-64), else the VM |
| `--ops-listener public\|admin` | `public` | which listener serves `/_host/` |
| `--tls-cert FILE` / `--tls-key FILE` | — | the certificate chain and key the public listener serves; with them the host terminates TLS itself and speaks HTTP/2 |
| `--acme-domain NAME` | — | get a certificate for this name from an ACME authority and renew it, instead of being given one. Repeatable; needs `--acme-http` reachable on port 80 |
| `--acme-email ADDRESS` / `--acme-staging` | — | what the authority is told, and whether it is Let's Encrypt's staging one |
| `--acme-http ADDR` | `0.0.0.0:80` | the plain listener that answers challenges and redirects everything else to `https://` |
| `--public-origin URL` | — | the origin clients reach the host at, behind a TLS-terminating proxy |
| `--trust-proxy` | off | believe `X-Forwarded-Proto` and `X-Forwarded-Host` instead |
| `--shutdown-grace SECONDS` | `10` | on SIGTERM or Ctrl-C, how long to wait for requests in flight |
| `--quiet` | off | no `log` lines on stdout |

`minicloud --version` names the Cove commit the binary was built against.

## Writing an app

An app is a directory holding an `app.toml`. The `.cove` files directly in
it are the module named after the directory; each subdirectory holding
`.cove` files is a module of that name, which the app may `use`. Nothing
outside the directory is visible: every app is a package of its own with the
standard library.

The entry is `handle(request: web.Request) -> web.Response` in the main
module (`<app>.handle`, unless `entry` in `app.toml` says otherwise):

```cove
use web

export fn handle(request: web.Request) -> web.Response {
  // request.method, request.path, request.query, request.headers, request.body
  web.Response(status: 200, headers: Map.of(), body: "hello\n")
}
```

- `path` is the path below the app's prefix: `/hello/a/b` reaches `hello`
  with `/a/b`.
- `query` and `headers` are `Map<String, String>`; header names are
  lowercased, and `x-forwarded-prefix` says where the app is mounted
  (`/hello`) so it can link to itself.
- `body` is a `String`; a request body that is not UTF-8 is answered 400
  before the app runs.
- A response without a `content-type` gets `text/plain; charset=utf-8`;
  `content-length`, `transfer-encoding`, `connection`, `keep-alive` and
  `upgrade` are the HTTP layer's and are dropped.

### `app.toml`

Every key is optional, and an unknown one refuses the app, so a misspelt
limit is never silently not applied. The defaults are shown:

```toml
entry = "hello.handle"          # `<app>.handle` by default
grant = ["log", "timer"]        # the capabilities granted; none by default

[limits]
deadline = "10s"                # per request, parked time included ("ms", "s", "m", "h")
max_host_calls = 1000           # per request
# max_call_depth = 512          # per request; no default but the runtime's own bound
# max_heap_words = 4194304      # per request; the runtime's default, 32 MiB
max_in_flight = 64              # this app's runs started and not answered
max_queued = 256                # this app's requests waiting to start
max_request_bytes = 1048576     # request body
max_response_bytes = 4194304    # response body

[kv]                            # the app's store, with `kv` granted
max_key_bytes = 1024
max_value_bytes = 1048576
max_keys = 100000
max_bytes = 67108864            # keys and values, summed

[keyed]                         # one run at a time for one key
by = "query:room"               # or "header:x-room"; without it there is no key
max_waiting = 64                # requests waiting for one key; past it, 429
alarm_path = "/alarm"           # where a due alarm runs, with `keyed` granted

[[queues]]                      # a durable queue, with `queue` granted
name = "mail"                   # what `queue.send` names
path = "/queue/mail"            # where a message is delivered
max_attempts = 5                # deliveries before it becomes a dead letter
retry = "30s"                   # after a failure; doubles each attempt
lease = "60s"                   # how long one delivery may take
max_depth = 10000               # messages waiting
max_body_bytes = 65536
batch = 8                       # deliveries of this queue at once

[defer]                         # work that outlives a request, with `defer` granted
max_in_flight = 8               # deferred runs of this app that have not finished

[rate]                          # how often one caller may reach the app
requests = 60                   # over `per`; without it there is no limit at all
per = "1m"                      # "300ms", "5s", "1m", "2h"
burst = 60                      # the most it may hold at once; `requests` by default
key = "address"                 # or "app", or "header:cf-connecting-ip"

[sql]                           # the app's SQL store, with `sql` granted
max_bytes = 67108864            # the database
max_rows = 1000                 # rows one query may answer
max_result_bytes = 4194304      # the values of one answer, summed
max_statement_ms = 20           # how long one statement may run
max_statement_bytes = 65536     # how long its text may be

[fetch]                         # outbound HTTP, with `fetch` granted
allow = []                      # e.g. ["https://api.github.com", "http://127.0.0.1:*"]
timeout = "10s"                 # one fetch, connect to last byte
max_request_bytes = 1048576
max_response_bytes = 4194304    # the decoded body

# [fetch.headers."https://api.openai.com"]   # a secret the host adds as a header
# authorization = { secret = "openai", prefix = "Bearer " }

[[triggers]]                    # the host runs the app itself, on a schedule
cron = "0 * * * *"              # UTC, to the minute; or @hourly, @daily, @weekly, …
path = "/cron/hourly"           # the path the run is given; `/` by default
method = "POST"                 # the method it is given

[assets]                        # the app's `assets/` directory, answered by the host
max_bytes = 8388608             # its files together; a larger one refuses the app
cache_control = "no-cache"      # what every one of them is answered with

[route]
hosts = []                      # e.g. ["admin.example"]: reached by these hostnames only

[secrets]                       # what `auth.check` and `[fetch.headers]` use; one of:
# admin = { env = "APP_ADMIN_TOKEN" }  # an environment variable of the host
# admin = { store = "admin" }          # the host's secret store, set at run time
# admin = { file = "admin.secret" }    # a file, relative to the app's directory
# admin = { value = "..." }            # literal, for tests

# [access]                      # Cloudflare Access, for `auth.identity`
# team = { env = "ACCESS_TEAM_DOMAIN" }
# aud = { env = "APP_ACCESS_AUD" }
# emails = { env = "ACCESS_ALLOWED_EMAILS" }  # optional: only these users
# token = "admin"               # a `[secrets]` name: the way in without Access
# fallback = "none"             # or "token": the token as well with Access on
```

An app is refused at load — answered 503 with the reason, while every other
app serves — if its config does not read, its code does not check (warnings
included), its entry requires a capability it is not granted, its code can
`spawn` a task, or a secret it names cannot be resolved (the value is never
printed). An `[access]` value is a setting, not a secret: an unset `team` or
`aud` turns Access off for the app. The rules for `[fetch]`, `[secrets]`,
`[route]` and `[access]` are in [docs/design.md](docs/design.md).

### Static files

An app's answer is a Cove `String`, which is UTF-8, so an icon cannot be
one. What an app serves as it is — a stylesheet, an image, a font — goes in
an **`assets/` directory** beside its code, and the host answers it itself:

| on disk | reached at, by prefix | by hostname |
| --- | --- | --- |
| `assets/style.css` | `/<app>/style.css` | `/style.css` |
| `assets/img/logo.png` | `/<app>/img/logo.png` | `/img/logo.png` |

- The directory is the app's root, so `/favicon.ico` — which a browser asks
  for by itself, whatever a page says — is `assets/favicon.ico`.
- A file wins over the app for `GET` and `HEAD` and for nothing else: a
  `POST` to a file's path, and any path naming no file, reaches `handle` as
  before. There is no directory index and no `index.html`.
- The answer carries the content type the extension says, a strong `etag`
  (the next ask with `If-None-Match` is `304`), `x-content-type-options:
  nosniff`, the `cache-control` `[assets]` asks for, and
  `x-cove-app-version`. Nothing ran, so there are no `x-cove-run-*` headers
  and the app's `served` does not count it — `assets_served` does.
- The files are read once, at load, and **belong to that version**: they are
  in the version's hash, a deploy or an update switches them with the code,
  and a request in flight keeps the ones it started on. Nothing is read from
  disk to answer one.
- They are answered **before the app runs**, so whatever the app asks of its
  users is not in front of them: `assets/` is public. Put nothing there that
  should not be. (The admin app's are public on the machine; on the server
  Cloudflare Access is in front of the hostname.)
- `assets/` holds no `.cove` file, so it is not a module, and everything
  under it travels with `minicloud deploy` — a hidden name (`.DS_Store`)
  aside.

An app whose `assets/` holds a symbolic link, or is larger than `[assets]
max_bytes`, is refused at load with the reason, as any config error refuses
it; every other app serves. The load line says what an app has:

```console
  admin      v1-17e8cb08  requires [admin, auth]  granted [admin, auth]  ok: 329 fn on native, checked in 24.1 ms, prepared in 19.9 ms, 3 file(s) in assets/ (10182 bytes)
```

### How often one caller may ask

An app's `max_queued` says how many requests may wait for it, and
`--max-in-flight` how many the host holds at once; neither stops **one**
caller filling an app's queue by itself. `[rate]` does, at the door:

```toml
[rate]
requests = 60      # a minute, by default
key = "address"
```

- A caller past its allowance is **429** with `Retry-After`, counted in
  `rejected.rate_limited`, and the app does not run. The request costs the
  host a map lookup.
- It is a **token bucket**: `requests` tokens are added over `per`, a bucket
  holds at most `burst` of them (`requests` by default), so a quiet caller
  may spend a burst at once and a busy one settles at the rate.
- `key` says who a caller is. `address` is the connection's, which behind a
  proxy is the proxy — there use `header:cf-connecting-ip` (or whatever
  your proxy sets), remembering that a header is only as good as whoever is
  allowed to set it. `app` is one bucket for everybody, which is how an app
  says "no more than this, from anyone".
- The app's static files are behind it too: a file is cheap, a flood of
  them is not.

Buckets are the host's, not the app's: they are kept across an update and
dropped when the app is removed, and the host keeps at most ten thousand of
them, sweeping the ones that are back to full first.

### The SQL store

`kv` holds a value under a key and lists keys by prefix, which is the wrong
shape for anything an app wants to *ask questions of*. With `sql` granted an
app gets a second store of its own — `<data>/<app>/sql.sqlite3`, beside its
`kv` — with a schema, indexes and aggregates:

```cove
use sql

sql.exec("CREATE TABLE IF NOT EXISTS books (id INTEGER PRIMARY KEY, title TEXT NOT NULL, pages INTEGER)", [])
sql.exec("INSERT INTO books (title, pages) VALUES (?, ?)", [title, pages])
match sql.query("SELECT title FROM books WHERE pages > ? ORDER BY pages DESC", ["300"]) {
  Ok(got) => …   // got.columns: Array<String>, got.rows: Array<sql.Row>
  Err(error) => …
}
```

- **Two operations.** `query` runs a statement that only reads — a statement
  that writes is the app's `Err`, pointing at `exec` — and `exec` runs one
  that writes or changes the schema, answering `{ rows, lastInsertId }`.
  **One statement a call**: two separated by `;` are refused rather than
  half-run.
- **Values are text.** A row is `Array<Option<String>>`: `NULL` is nothing,
  `TEXT` is itself, `INTEGER` and `REAL` are their decimal text. A `BLOB` is
  the app's `Err`. Parameters are `Array<String>`, bound as text — SQLite's
  type affinity makes `"42"` a number against an integer column — and
  **bound, never pasted**, so a parameter can never be SQL. For `NULL`,
  write `nullif(?1, '')`.
- **The app owns its schema**: `CREATE TABLE`, `CREATE INDEX` and `ALTER
  TABLE` are the app's to run, and migrating is the app's job. `ATTACH`,
  `PRAGMA` and transactions of its own (`BEGIN`, `SAVEPOINT`) are refused by
  SQLite's authorizer — a transaction held open across host calls would keep
  the connection locked if the run hit its deadline in between, and a single
  statement is atomic by itself.
- **The limits are `[sql]`**, and each is the app's `Err` rather than the
  host's error: a database past `max_bytes`, an answer past `max_rows` or
  `max_result_bytes` (an error, not a truncated answer), a statement past
  `max_statement_ms` (stopped where it is) or `max_statement_bytes`. SQLite
  answers on the worker, as `kv` does, so `max_statement_ms` is a worker
  held and not yielded: it is 20 ms by default for that reason, and a query
  that needs longer wants an index.

### A socket a run holds

Everything else here answers: a request comes in, a run answers it, the run
ends. A socket is the one thing that does not — it is open for as long as
both ends want it, and both may speak. With `socket` granted, the run holds
it:

```cove
use socket

export fn handle(request: web.Request) -> web.Response {
  match socket.accept() {
    Ok(_) => {}
    Err(error) => return web.Response(status: 400, headers: Map.of(), body: error.message)
  }
  while true {
    match socket.receive() {
      Ok(said) => { socket.send("you said {said}") }
      Err(_) => break        // the client has gone
    }
  }
  web.Response(status: 200, headers: Map.of(), body: "")   // not the answer
}
```

- The request **arrives as a request**: the app sees its headers and may
  refuse it like any other. `socket.accept()` is what turns it into a
  socket — the host answers `101` with the key the protocol asks for and
  takes the connection over.
- **Both ends park.** `send` gives the worker up until the client has taken
  what is queued, `receive` until there is a message. A socket nobody is
  speaking on costs a parked run and no worker at all, which is what makes
  holding one for minutes reasonable on a host with two of them.
- The host answers `ping` with `pong` itself: keeping a connection alive is
  not the app's business.
- The run's **`deadline` is how long the socket may be held** — an app that
  means to hold one for an hour says `deadline = "1h"` — `max_host_calls`
  bounds how many messages there may be, and `max_response_bytes` bounds
  one message. A run that ends closes the socket, and a client that goes is
  an `Err` from `receive`, which is how the run finds out.
- A message that is not text is given to the run as text where it is text
  and dropped where it is not: a run reads a `String`, and nothing of this
  host can carry bytes to an app yet.

### One run at a time for one key

An app can hold state and cannot hold it *safely* when two requests about
the same thing run at once: `kv.increment` is atomic and nothing else is,
and read-decide-write is three calls that two runs can interleave. `[keyed]`
is the guarantee that makes it safe:

```toml
[keyed]
by = "query:room"       # or "header:x-room"
max_waiting = 64
alarm_path = "/alarm"
```

- **At most one run per key.** The host takes the key's turn before the
  request is admitted and holds it until the answer, so everything about one
  key happens in order and an app may read, decide and write with nobody in
  the middle. The turn is fair: a key's requests are served in the order
  they arrived.
- The key comes from the request — a query parameter or a header the config
  names — because the waiting has to happen **before** the run starts. A
  request with no key is not keyed and runs as any request does.
- **Another key is not waiting**: only requests about the same key queue
  behind each other. `max_waiting` bounds that queue; past it the answer is
  429 with `Retry-After`, so one hot key cannot fill the host by itself.
- **An alarm per key**, with `keyed` granted: `keyed.alarm(key, atMillis)`
  asks the host to run `alarm_path` with that key at that time,
  `clearAlarm` takes it back and `alarmAt` says when (`-1` for none). One
  alarm a key — setting it again moves it — kept in the app's own SQLite
  file, so it survives a restart. A due alarm **takes the key's turn like a
  request**, so an alarm and a request about one key never run together.

This is the single-writer half of Cloudflare's Durable Objects and not the
object: a run's heap ends with the run here, so the state lives in the app's
own `kv` or `sql` and what the host adds is the guarantee that makes it
safe. An app that calls itself with the same key from inside a keyed run
waits for itself, and its deadline is what ends it.

### An answer written as it goes

A run answered once, at its end, with the whole body in a `web.Response`.
That is right for a page and impossible for the things that are not pages.
With `stream` granted a run can answer **while it is still going**:

```cove
use stream

export fn handle(request: web.Request) -> web.Response {
  stream.open(200, Map.of(MapEntry(key: "content-type", value: "text/event-stream")))
  stream.send("data: one\n\n")
  stream.send("data: two\n\n")
  web.Response(status: 200, headers: Map.of(), body: "")   // not the answer
}
```

- `open` sends the status and the headers at once; every `send` is a chunk
  on the connection. The `web.Response` the handler returns afterwards is
  **not** the answer — the answer has been going out since `open` — but
  returning one is still how a handler ends. The stream ends when the run
  does.
- **`send` parks.** The run gives up its worker until the client has taken
  the chunks before it, so a slow reader costs a parked run and no worker,
  and a run that is faster than its reader waits instead of filling memory.
- A client that goes away is an `Err` from `send`, so the run is told and
  can stop.
- Everything that bounds a run still bounds it: the `deadline` ends the
  stream (an app that means to hold one open sets a deadline that says so),
  `max_host_calls` bounds how many chunks there may be, and each chunk is
  held to `max_response_bytes` — the one limit that cannot mean the whole
  body any more.
- A streamed answer carries no `x-cove-run-*` headers: they say what a run
  cost, and the head goes out before the run has cost it. It is not
  gzipped either, which is what server-sent events want.

WebSocket is not this: it needs the connection upgraded, and is
[#51](https://github.com/myuon/minicloud/issues/51)'s remainder.

### Queues

`defer` is work the host will do in a moment and forget if it fails. A
**queue** is work that is written down first:

```toml
[[queues]]
name = "mail"
max_attempts = 5
retry = "30s"
```

```cove
use queue

queue.send("mail", "{id}")      // on disk when this answers
```

- A queue must be **declared in `app.toml`** before it can be sent to, so a
  misspelt name is an `Err` at the call and not a queue nobody reads.
- Sending is a write to the app's own SQLite file (`<data>/<app>/queue.sqlite3`)
  and nothing else: the message is **on disk when `send` answers**, so a
  host that stops between the send and the delivery still has it.
- **Delivery is a run of the app** at the queue's `path`, with the message
  as the body and `x-minicloud-queue` saying which queue, beside
  `x-minicloud-attempt` and `x-minicloud-attempts` — which try this is and
  how many there will be, so a consumer can tell the failure that is about
  to become a dead letter from the ones before it. The host sets all three
  and strips them from anything off a connection. A run that answers 2xx–4xx
  has taken the message; one that answers 5xx or does not answer has not.
- A delivery that is not taken is **tried again after `retry`, doubling**
  each time, until `max_attempts` — then the message is a **dead letter**,
  kept with the reason rather than dropped. `minicloud queues <app>` shows
  what each queue holds and the last 50 dead letters; `--revive <id>` puts
  one back.
- **A delivery may happen twice.** A run that takes longer than `lease` is
  taken to be lost and the message is tried again; if the first run then
  finishes, the work has been done twice. The alternative — never retrying
  — loses a message whenever the host stops mid-delivery, which is worse.
  A consumer must be able to be asked twice.

### Work that outlives a request

An app answers and is done: its isolate ends with its answer, so anything
it has not finished by then it has not done. Work that should not make the
client wait — writing an audit line, warming a cache, telling something else
that this happened — goes to `defer`:

```cove
use defer

// answer now; the host runs `/work` with this body in a moment
defer.run("/work", "{id}")
```

- **It is a run of its own, not the rest of this one.** Cloudflare's
  `waitUntil` hands the runtime a promise and keeps the isolate alive until
  it settles; here the answer *is* the end of the isolate, and keeping one
  alive past its answer would mean a request's budget outliving the
  request. So the host starts another run of the same app at that path,
  with its own deadline and its own budget, which the app routes as it
  routes anything else.
- `x-minicloud-deferred` says the host is the caller, and the host strips
  it from every request that comes off a connection — so a path meant for
  deferred work can refuse anyone else, the way a trigger's can.
- The call answers as soon as the work is **accepted**, not when it is
  done; the request that asked goes on to answer its client.
- `[defer] max_in_flight` (8) is how many deferred runs of one app may be
  outstanding; a call past it is the app's `Err`, because a queue of work
  that grows without end is a leak with a slower name. A deferred run may
  defer again, and an app that loops spends its own turns on it.

### Running on a schedule

An app had one way in — a request from outside — so anything periodic was a
crontab elsewhere calling the host. `[[triggers]]` is the host making that
call itself:

```toml
[[triggers]]
cron = "*/10 * * * *"
path = "/cron/poll"

[[triggers]]
cron = "@daily"
path = "/cron/rollup"
```

- **A trigger's run is an ordinary run**: admitted to the app's queue,
  served round robin, sliced, parked where it waits, and stopped by the
  app's own `deadline`, `max_host_calls` and heap. It is counted in
  `served`, `ok` and the errors like any other, and in `trigger_runs`
  beside them. It runs on the version current when it was admitted.
- **The app knows who called**: the host sets `x-minicloud-trigger` to the
  path the schedule asked for, and strips that header from every request
  that comes off a connection, so a path meant for the clock can refuse
  anyone else:

  ```cove
  if request.headers.get("x-minicloud-trigger").isNone() {
    return web.Response(status: 403, headers: Map.of(), body: "not the clock\n")
  }
  ```

  The path is otherwise an ordinary path of the app's — reachable unless the
  app refuses it, which is also how a job is run by hand.
- **Schedules are to the minute, in UTC.** Five fields (`minute hour day
  month weekday`) with `*`, `*/n`, `a-b`, `a-b/n` and lists of them, or
  `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`. A day and a weekday
  that are both given run on **either**, as cron has always done. UTC, not
  the machine's zone: midnight in Tokyo is `0 15 * * *`.
- **A tick is let go by rather than doubled**: a schedule whose last run has
  not finished counts `trigger_skipped` and does not start a second. A
  minute the host was not running for does not happen later.
- At most 16 schedules an app, and `method` is `POST` unless it says
  otherwise. A `cron` that does not parse refuses the app at load, naming
  the key, and every other app serves.

### Host modules

| module | capability | what |
| --- | --- | --- |
| `web` | — | the `Request` and `Response` types; building a response needs no capability |
| `log` | `log` | `info`, `warn`, `error`: a line on stdout (`[app] level: line`) and in the app's log |
| `timer` | `timer` | `sleep(millis)`: parks the run, at most 60 s |
| `kv` | `kv` | the app's own SQLite store: `get`, `put`, `delete`, `increment`, `list`, `listDesc`; a `put` past a quota is the app's `Err` |
| `socket` | `socket` | `accept`, `send`, `receive`, `close`: a WebSocket the run holds ([A socket a run holds](#a-socket-a-run-holds)) |
| `keyed` | `keyed` | `alarm(key, atMillis)`, `clearAlarm(key)`, `alarmAt(key)`: wake this app at a key ([One run at a time for one key](#one-run-at-a-time-for-one-key)) |
| `stream` | `stream` | `open(status, headers)`, `send(chunk)`: an answer written as the run goes ([An answer written as it goes](#an-answer-written-as-it-goes)) |
| `queue` | `queue` | `send(name, body)`, `depth(name)`: a durable queue the host delivers from ([Queues](#queues)) |
| `defer` | `defer` | `run(path, body)`: the host runs this app at `path` after this answer ([Work that outlives a request](#work-that-outlives-a-request)) |
| `sql` | `sql` | the app's own SQL store, beside its `kv`: `query`, `exec`, one statement a call, parameters bound as text ([The SQL store](#the-sql-store)) |
| `fetch` | `fetch` | `get(url)`, `request(method, url, headers, body)` to the `[fetch] allow` list only; parks the run; redirects are not followed |
| `time` | `time` | `nowMillis()`, `nowMicros()` |
| `random` | `random` | `hex(bytes)`: 1–64 bytes from the operating system |
| `auth` | `auth` | `check(secret, authorization)` against a `[secrets]` entry the app never reads; `identity(headers)`, the Cloudflare Access user the host verified; `usesAccess()` |
| `host` | `admin` | the admin app's view of the host and its changes, and what the version of an app it is running is made of ([`code.rs`](crates/minicloud/src/code.rs)); only the app named `admin` may be granted it |

The exact signatures are the schemas in the source:
[`hosts.rs`](crates/minicloud/src/hosts.rs) (`web`, `log`, `timer`),
[`kv.rs`](crates/minicloud/src/kv.rs), [`sql.rs`](crates/minicloud/src/sql.rs),
[`fetch.rs`](crates/minicloud/src/fetch.rs),
[`sys.rs`](crates/minicloud/src/sys.rs) (`time`, `random`, `auth`) and
[`admin_module.rs`](crates/minicloud/src/admin_module.rs) (`host`). How
`kv`, `fetch` and `auth` behave is in [docs/design.md](docs/design.md).

### Checking and testing an app

`cove check` and `cove test` do not know the host's modules, so `minicloud`
has its own. `check` runs the admission `serve` runs at load and exits
non-zero if any app would be refused:

```console
$ ./target/checked/minicloud check --apps crates/minicloud/tests/apps greedy spawner
error[minicloud::spawn]: `spawner.handle` can spawn a task, which minicloud does not run
 --> spawner/spawner.cove:6:19
  |
6 |     let doubled = tasks.spawn { 21 * 2 }
  |                   ^^^^^^^^^^^^^^^^^^^^^^
  rule: A spawned task runs outside the host's worker pool and keeps its parent from parking or yielding; minicloud refuses an app that can spawn.
greedy     requires [log]  granted [-]  REFUSED: `greedy.handle` requires `log`, which app.toml does not grant
spawner    requires [-]  granted [-]  REFUSED: `spawner.handle` can spawn a task, which minicloud does not run
checked 2 app(s) against the host's schemas (web, log, timer, kv, sql, defer, queue, stream, keyed, socket, fetch, time, random, auth, host); 0 warning(s), 2 refused
```

`test` runs every `test fn` the way `cove test` does, with the app's grant,
its limits and the host's modules:

```console
$ ./target/checked/minicloud test --apps examples hello crunch
ok    crunch     crunch.aSizeOutOfRangeIsRefused
ok    crunch     crunch.countsThePrimesUpToN
ok    hello      hello.answersABodyOfTheSizeAsked
ok    hello      hello.greetsTheWorldWhenNobodyIsNamed
ok    hello      hello.greetsWhoeverTheQueryNames
ran 5 test(s), 5 passed
```

Both take app names to narrow them, and `--data DIR` to look `{ store = … }`
secrets up in that data directory's store; `test` takes `--filter`. `check`
refuses an app whose secret cannot be resolved, as `serve` would; `test`
gives such a secret the value `minicloud-test-placeholder-<name>` and says so
on stderr.

## Deploying an app

The host's apps change on the running host, through its **admin listener**
(`--admin`, `127.0.0.1:8081` by default) — never the public one. Every admin
request needs the token the host writes to `<data>/admin.token` (mode 0600)
the first time it starts without one; the `minicloud` subcommands read it from
`data/admin.token` and reach `127.0.0.1:8081` unless given `--token-file`
and `--admin`.

`minicloud deploy <dir>` sends an app from anywhere — its own repository,
say — to the running host. The host loads it as the app's next version
**and only if it loads** writes it into the apps directory and switches to
it; otherwise nothing changes and the diagnostics are printed. Against a
host started with `--apps apps`, from a copy of `examples/hello` in
`src/hello` with a README beside its code:

```console
$ ./target/checked/minicloud deploy src/hello
minicloud: not part of the app, left out: README.md
minicloud: deploying `hello`: 3 file(s), 2433 bytes
{
  "app": "hello",
  "previous": null,
  "version": "v1-e421b416"
}
$ $EDITOR src/hello/hello.cove                   # Hello → Hi
$ ./target/checked/minicloud deploy src/hello 2>/dev/null | grep '"version"'
  "version": "v2-0a514f4f"
$ ./target/checked/minicloud rollback hello | grep '"version"'
  "version": "v3-e421b416"
$ echo 'fn broken( {' >> src/hello/hello.cove
$ ./target/checked/minicloud deploy src/hello; echo "exit $?"
minicloud: not part of the app, left out: README.md
minicloud: deploying `hello`: 3 file(s), 2443 bytes
minicloud: 422 Unprocessable Entity
deploy of `hello` refused; still serving v3-e421b416:
does not parse:
error[cove::parse::unexpected_token]: expected identifier, found `{`
  --> hello/hello.cove:51:12
   |
51 | fn broken( {
   |            ^

exit 1
```

- Only `app.toml`, the `.cove` files and everything under `assets/` are
  sent; symbolic links, paths outside the directory, and more than 16 MiB or
  4096 files are refused. The
  app's name is the directory's (`--name` for another).
- The deployed copy replaces `<apps>/<app>`; the one it replaced is kept in
  `<apps>/.previous/<app>`. `minicloud rollback <app>` swaps them back (a
  second rollback undoes the first).
- `deploy -` reads a tar archive of the app on stdin, and needs `--name`.
- `deploy <dir> --into <apps>` writes into an apps directory with no running
  host, checking as `minicloud check` does; the host loads it at its next
  start. `install.sh` installs the admin app this way.
- An app that needs a secret the host lacks is refused, saying which: set it
  first, in the host's environment or its secret store
  ([Secrets set at run time](#secrets-set-at-run-time)).

`minicloud update <app>` reloads `<apps>/<app>` as it is on disk, through the
same load-then-switch. Each version is `v<n>-<hash of the app's files>`, and
every response names the one that answered (`x-cove-app-version`). Requests
in flight — queued, running, yielded or parked — finish on the version they
were admitted to; the old version is dropped with its last request. The
app's store, log and counters carry over.

The other admin subcommands are `remove`, `enable`, `disable`, `reset`,
`logs`, `queues` and `secret` ([Operating](#operating)); `minicloud <command> --help` describes
each. The listener's HTTP endpoints are listed in
[docs/design.md](docs/design.md#updates-and-versions).

**To a server, over ssh.** The admin listener stays on the server's
localhost and the token never leaves it: send the archive over ssh's stdin to
the `minicloud` installed there. This is how an app in another repository
(such as [myuon/ai-daily](https://github.com/myuon/ai-daily)) or an example
is deployed:

```console
$ tar -C examples/algo -c . | ssh whisky \
    '~/cove-tools/current/minicloud deploy - --name algo --admin 127.0.0.1:8791 --token-file ~/cove-tools/data/admin.token'
$ ssh whisky '~/cove-tools/current/minicloud rollback algo --admin 127.0.0.1:8791 --token-file ~/cove-tools/data/admin.token'
```

## Deploying

[`deploy/`](deploy) runs the host as a systemd service on one Linux x86-64
machine behind a TLS-terminating proxy. It is written for the owner's server
(Ubuntu 24.04, a Cloudflare Tunnel to `https://covtools.ramda.io`, Cloudflare
Access in front), and its paths say so; adapt them for another.

| file | what |
| --- | --- |
| [`install.sh`](deploy/install.sh) | installs a release as the service's user, without sudo, and prints the one sudo command |
| [`cove-tools.service`](deploy/cove-tools.service) | the systemd unit: public listener `127.0.0.1:8790`, admin `127.0.0.1:8791`, `--ops-listener admin`, `--public-origin https://covtools.ramda.io`, two workers, resource caps and hardening |
| [`env.example`](deploy/env.example) | the env file's template: the apps' secrets and the Cloudflare Access settings |
| [`backup.sh`](deploy/backup.sh) | online SQLite backups of every app's store |
| [`cloudflare.md`](deploy/cloudflare.md) | the tunnel, the hostnames and the Access applications |

A release is a tag `v<version>`: [`release.yml`](.github/workflows/release.yml)
builds `minicloud` on Ubuntu 24.04 with the native tier and publishes
`minicloud-<version>-x86_64-linux.tar.gz` (the binary, a `cove-host` link to
it, `apps/admin`, `deploy/` and this README — not `examples/`), its
`.sha256`, and `install.sh`.

### Installing

On the server, as the user the service runs as:

```console
$ curl -fsSLO https://github.com/myuon/minicloud/releases/download/v0.19.0/install.sh
$ bash install.sh v0.19.0
```

`install.sh` downloads the release and checks its sha256, unpacks it into
`~/cove-tools/releases/<version>/`, creates `~/cove-tools/env` from
`env.example` the first time (every `change-me` a fresh random secret, mode
0600), checks every installed app with the new binary, deploys the bundled
admin app (`--with-bundled-apps ""` for none), and only then points
`~/cove-tools/current` at the release. The first time, it prints the sudo
command that installs the unit and starts `cove-tools.service`.

**A release is the platform; the apps are deployed separately.** Installing
one never removes or replaces an app other than `admin`. When a release
carries a **newer copy** of an app that is installed, it says so and leaves
the installed one serving, with the command that would deploy the
release's:

```console
this release carries a newer copy of: admin
the installed one is still what serves; deploy the release's with:

  bash install.sh --with-bundled-apps "admin" v0.19.0
``` The apps are
whatever was deployed into `~/cove-tools/apps` ([Deploying an
app](#deploying-an-app)), and their state is in `~/cove-tools/data`, which no
release or deploy touches.

### The env file and secrets

`~/cove-tools/env` is the unit's `EnvironmentFile=`: the secrets apps take
with `{ env = … }` (`WEBHOOKS_ADMIN_TOKEN`, `LEDGER_TOKEN`, `ADMIN_UI_TOKEN`)
and the Cloudflare Access settings (`ACCESS_TEAM_DOMAIN`,
`COVTOOLS_ACCESS_AUD`, `COVTOOLS_ADMIN_ACCESS_AUD`, `ACCESS_ALLOWED_EMAILS`).
A change takes a restart. A later release that adds a key gets it appended by
`install.sh`; existing keys are left alone. A secret that should change
without a restart belongs in the host's secret store instead ([Secrets set at
run time](#secrets-set-at-run-time)).

### Backups

`deploy/backup.sh` copies every app's `kv.sqlite3` with SQLite's online
backup while the host runs, into `~/cove-tools/backups/<stamp>/`, and keeps
14 days (`KEEP_DAYS`). It needs `sqlite3`; the crontab line is in the file.
The backups are on the same disk: copy them elsewhere to survive the machine.

### Cloudflare

[deploy/cloudflare.md](deploy/cloudflare.md) sets up the tunnel to the
public listener, the admin app's own hostname (`covtools-admin.ramda.io`,
through `[route] hosts`), and the Access applications whose tokens the host
verifies ([Cloudflare Access](#cloudflare-access)). The admin listener is
never in the tunnel.

### Upgrading and rolling back the platform

```console
$ bash install.sh v<new>
$ sudo systemctl restart cove-tools
```

`install.sh` says when the unit changed and needs installing again. Before it
switches, it checks every installed app with the new binary against the env
file and the secret store; if one would be refused, it stops and switches
nothing. On SIGTERM the host answers new requests 503 and waits up to
`--shutdown-grace` for those in flight.

To go back, run `install.sh` with the older version (releases before 0.5.0,
named `cove-host-<version>`, install too), or point `current` at a kept
release — the three newest are kept:

```console
$ ln -sfn releases/<old> ~/cove-tools/current && sudo systemctl restart cove-tools
```

### Names kept from cove-tools

The rename to minicloud (0.5.0) renamed the repository, the crate, the
binary and the release asset. What an installed server depends on kept its
name, so upgrading from cove-tools is `install.sh` and a restart:

- the release tarball's **`cove-host`** is a symbolic link to `minicloud`,
  because the unit runs `ExecStart=…/current/cove-host serve`;
- the unit **`cove-tools.service`**, the server root **`~/cove-tools`**
  (`releases/`, `current`, `apps/`, `data/`, `env`, `backups/`) and the
  hostnames `covtools.ramda.io` and `covtools-admin.ramda.io`; renaming them
  needs sudo and a Cloudflare change, and is later work;
- the environment variables **`COVE_TOOLS_ROOT`** and **`COVE_TOOLS_BACKUPS`**
  (`install.sh` and `backup.sh` accept `MINICLOUD_ROOT` and
  `MINICLOUD_BACKUPS` as well) and **`COVE_HOST_TEST_BACKEND`** for the
  tests.

## Operating

### The admin app

[`apps/admin`](apps/admin/README.md) is the browser's way to run the host:
every app's state, version, grant, limits, counters and recent errors;
enable and disable; change an app's grant, fetch allowlist and limits; the
change history; and the secret store. It also reads what an app holds and
what it said — a page of its `kv` store, one key at a time, and the last
lines of its log — which it can only read: the host gives it no way to
write to another app's store. It also shows **what the version an app is
running is** — its `app.toml`, its `.cove` files at any depth and its
`assets/`, and one file's text — out of the host's own copies, so the code
shown is the code that runs, with a note when the file of the same name in
the app's directory is not the one that is running. It is reached only by its own
hostnames (`[route] hosts`): on the server `https://covtools-admin.ramda.io`
behind Cloudflare Access, and locally `http://admin.localhost:8080/` with
`ADMIN_UI_TOKEN` as the password:

```console
$ ADMIN_UI_TOKEN=dev ./target/checked/minicloud serve --apps apps
```

### Administering apps at run time

An app's configuration — enabled, its grant, its fetch allowlist, its limits
— can be changed on the running host by the admin app (through the `host`
module, capability `admin`, which only the app named `admin` may hold) or on
the machine through the admin listener:

```console
$ ./target/checked/minicloud disable hello
`hello` disabled: not routed to; in-flight requests finish
$ ./target/checked/minicloud enable hello
`hello` enabled: routed to again
$ ./target/checked/minicloud reset hello        # drop the admin app's changes
```

A change reloads the app with the change applied and switches to it only if
it loads — except that taking away a capability the app needs is kept, and
leaves that app refused. A disabled app answers 503 and keeps its data. The
admin app cannot disable itself or take `admin` away from itself; the admin
listener can. Changes are kept in `<data>/_host/overrides.json`, relative to
`app.toml`, and every change, refused ones included, in
`<data>/_host/changes.jsonl`. The rules are in
[docs/design.md](docs/design.md#administering-apps-at-run-time).

### Secrets set at run time

A secret an app takes with `{ store = "<name>" }` lives in the host's secret
store, `<data>/_host/secrets` (mode 0600), and is set, replaced and deleted on
the running host — from the admin app's Secrets page or with `minicloud
secret list|set|delete`. Setting one reloads every app that uses it.
Nothing returns a value: listings show names, whether each is set, when, and
which apps use it. Details:
[docs/design.md](docs/design.md#secrets-set-at-run-time).

```console
$ ssh -t whisky '~/cove-tools/current/minicloud secret set gemini --admin 127.0.0.1:8791 --token-file ~/cove-tools/data/admin.token'
value for secret `gemini` (not echoed):
```

### Cloudflare Access

An app behind Cloudflare Access calls `auth.identity(request.headers)` to
learn its user. The host verifies the Access token (`Cf-Access-Jwt-Assertion`)
itself against the team's keys and the app's `[access] aud`, so a request
that reached the host's port without going through Access has no identity.
With Access off (`team` or `aud` unset, as in a local run) the `[access]
token` secret is the way in. See
[docs/design.md](docs/design.md#cloudflare-access-who-is-asking) and
[deploy/cloudflare.md](deploy/cloudflare.md).

### The operations views

| path | what |
| --- | --- |
| `GET /_host/ui` | one HTML page, refreshed every 5 s: every app's version, state, tier, counters, queues, errors, KV usage and fetch counts, and its recent errors and log lines |
| `GET /_host/stats` | every app's counters as JSON, with the server's totals |
| `GET /_host/metrics` | the same, in the text format Prometheus scrapes (`version=0.0.4`), with the request times as a histogram |
| `GET /_host/series?app=&over=hour\|day\|month` | what each app did minute by minute (or, for a month, hour by hour) |
| `GET /_host/apps/<app>` | one app as JSON: its limits, every version it has had, its last 50 errors, and on the native tier which functions stayed on the VM and why |
| `GET /_host/apps/<app>/logs?n=200` | its recent log lines, as text |
| `GET /_host/apps/<app>/logs?follow=1` | those, and then every line as it is written: server-sent events, until the client goes away |

They are read-only and unauthenticated, on the public listener by default.
With `--ops-listener admin` (as deployed) the public listener answers 404
under `/_host/` and the views are on the admin listener, to a loopback `Host`
only; reach them with `ssh -L 8791:127.0.0.1:8791 <server>` and
`http://localhost:8791/_host/ui`. The admin listener also answers `GET /apps`
(the stats) and `GET /changes?n=50` (the change history), with the token.

Per app, `/_host/stats` has `state` (`ready`, `refused`, `disabled` or
`removed`), `version`, `tier`, `required` and `granted`, `served`, `ok`,
`errors` by kind, `rejected` by reason, `in_flight`, `queued`, `parked`,
`parks`, `yields`, `yields_declined`, `overdue_yields`,
`blocking_host_calls`, `instructions`, `worker_ms`, `heap_peak_words`,
`alarms_run` and `alarms_failed`,
`queues` (each queue's name, path, waiting and dead) with `queue_sent`,
`queue_refused`, `queue_in_flight`, `queue_delivered`, `queue_retried` and
`queue_buried`,
`deferred_runs`, `deferred_in_flight`, `deferred_failed` and
`deferred_refused`,
`latency_us` (how long its answered requests took: the count, the mean and
p50, p90 and p99, estimated from a histogram),
`fetch`, `kv` and `sql` usage against their quotas, `assets` (how many files and how many
bytes) with `assets_served` and `assets_not_modified`, `triggers` (each
schedule and when it is next due) with `trigger_runs`, `trigger_skipped` and
`trigger_failed`, `updates` and `versions_alive`; the full list is in
[`ops.rs`](crates/minicloud/src/ops.rs).

### Response headers

Every answer from a run carries what it cost:

| header | |
| --- | --- |
| `x-cove-app-version` | the version that answered, `v<n>-<hash>` |
| `x-cove-run-instructions` | the runtime's instruction count (compiled code dispatches none, so it undercounts on the native tier) |
| `x-cove-run-worker-us` | time on a worker, every slice summed |
| `x-cove-run-wall-us` | admission to answer |
| `x-cove-run-parks`, `x-cove-run-yields`, `x-cove-run-yields-declined` | how often the run parked, yielded, and declined to yield |
| `x-cove-stop` | on a run a limit stopped: the error kind, below |

A static file ([Static files](#static-files)) is not a run: it carries
`x-cove-app-version` and none of the others. An answer a run wrote as it
went ([An answer written as it goes](#an-answer-written-as-it-goes))
carries none of them either: its head was sent before the run had cost
anything.

### Limits and what they answer

| condition | status | `x-cove-stop` / counter |
| --- | --- | --- |
| no app by that name | 404 | |
| the app was refused at load | 503 with the reason | |
| the app is disabled | 503 | `rejected.disabled` |
| request body over `max_request_bytes` | 413 | `rejected.too_large` |
| request body not UTF-8 | 400 | `rejected.bad_request` |
| the caller is past the app's `[rate]` | **429**, `Retry-After: <n>` | `rejected.rate_limited` |
| the app already has `max_queued` requests waiting | **429**, `Retry-After: 1` | `rejected.queue_full` |
| the server has `--max-in-flight` requests admitted | **503**, `Retry-After: 1` | `rejected.server_busy` |
| `--max-connections` open | 503, `Retry-After: 1`, connection closed | |
| waited in the queue longer than the deadline | 503, `Retry-After: 1` | `queue_timeout` |
| deadline passed, running or parked | 504 | `deadline` |
| too many host calls / calls nested too deep | 500 | `host_calls` / `call_depth` |
| the heap needs more than `max_heap_words` | 500 | `heap` |
| a `spawn` past the task limit (the backstop for a refused `spawn`) | 500 | `concurrency` |
| any other runtime error | 500 | `runtime` |
| response over `max_response_bytes`, or not a valid response | 500 | `response_too_large` / `bad_response` |
| the client went away | the run is cancelled; 499 in the app's log | `cancelled` |
| a `kv.put` past a quota, a `fetch` off the allowlist | not a status: the app's `Err` | `fetch.refused` |

429 is one app at its own limit — its neighbours are unaffected; 503 with
`Retry-After` is the whole host. A run's 500 or 504 body is the runtime's
diagnostic, with the source line.

### What happened an hour ago

The counters are running totals, which say what an app has done and not
when. Once a minute the host writes **what changed over that minute** to
`<data>/_host/series.sqlite3` — requests, errors, rejections, worker time,
and p50 and p99 from the request-time histogram — so a rate can be read off
afterwards. The minutes are kept for 48 hours; at the top of each hour the
hour that ended is rolled up (counts summed, the percentiles kept at their
worst) into a second table kept for 90 days.

- `GET /_host/series?app=hello&over=hour` is the last hour minute by minute;
  `over=day` is 24 hours of minutes and `over=month` 30 days of hours.
- The operations page draws each app's last hour beside its counters.
- A host with no data directory keeps nothing and says so (`"kept": false`);
  every current value is still answered.

The first sample after a start writes nothing — there is nothing to take a
difference from, and a host that counted its running totals as one minute's
work would show a spike at every restart.

**Prometheus** scrapes `GET /_host/metrics`: `minicloud_requests_total`,
`_requests_ok_total`, `_errors_total` by kind, `_rejected_total` by reason,
`_worker_seconds_total`, `_in_flight`, `_queued`, `_assets_served_total`,
`_trigger_runs_total` and `minicloud_request_duration_seconds` as a
histogram, each labelled by app. With `--ops-listener admin` (as deployed)
that is on the admin listener, where a scraper on the machine reaches it.

### Logs

An app's `log` lines and the host's lines about it (a failed request, an
update, a run that would not yield) are kept in memory (the last 1,000, at
`/_host/apps/<app>/logs`) and written to `<data>/<app>/log.txt`, rotated to
`log.1.txt` past 1 MiB. **`minicloud logs <app>`** prints the last of them
through the admin listener, and `--follow` keeps the connection open and
prints every line as it happens:

```console
$ minicloud logs notes -n 20
$ minicloud logs notes --follow
1791803880.369 info: stored todo
```

It is the admin listener's `GET /apps/<app>/logs?follow=1` and the
operations view's `?follow=1`, both server-sent events; the host writes a
`: ping` comment every 15 s so that a proxy does not close an idle
connection, and says so if a reader falls more than 1,000 lines behind
rather than leaving a hole. Without `--quiet`, `log` lines also go to stdout; the
host's own messages go to stderr — on the server, `journalctl -u cove-tools`.
The admin app shows the same ring, a line at a time with its level
(`/apps/<app>/logs`), so the lines are there in a browser as well.

## The front door

The public listener is plain HTTP unless it is given a certificate, for a
host behind a proxy that holds one. With **`--tls-cert` and `--tls-key`**
it terminates TLS itself — one process, one port, nothing in front:

```console
$ minicloud serve --apps apps --addr 0.0.0.0:443 \
    --tls-cert /etc/letsencrypt/live/example/fullchain.pem \
    --tls-key /etc/letsencrypt/live/example/privkey.pem
```

- **HTTP/2 comes with it.** ALPN offers `h2` and `http/1.1` and the
  connection is served by whichever the client chose. A plain connection
  stays HTTP/1.1: a browser will not speak h2 without TLS, and h2c would be
  a second protocol for nobody.
- **The files are read at start and on `SIGHUP`**, never per connection, so
  a renewal reaches the host when its hook says so and a half-written pair
  is never served. A reload that does not read keeps the pair that is
  serving and says why; a host whose pair does not read at start does not
  start.
- **Or the host gets its own.** `--acme-domain tools.example` makes it an
  account with Let's Encrypt, orders a certificate, answers the HTTP-01
  challenge on `--acme-http` (port 80, in the clear, which is the one thing
  a host with no certificate can do), writes the pair under
  `<data>/_host/acme/` and renews it 30 days before it runs out. A start
  that finds a certificate with more than 30 days left serves it and orders
  nothing, because a host that ordered on every start would be rate-limited
  by any authority worth using.

  The plain listener redirects everything that is not a challenge to
  `https://`, so it is the http-to-https every deployment wants anyway.

  **Use `--acme-staging` first.** Its certificates are not trusted and its
  rate limits are kind, which is what a first run wants; the exchange with
  a real authority is the one part of this host that no test covers, for
  want of an authority, a public name and port 80.

**Answers are gzipped** where it is worth it: a body over 1 KiB, of a text
or text-shaped type, for a client that said `Accept-Encoding: gzip` — with
`Vary: accept-encoding`, and only if it actually came out smaller. An
image, a font or a `.wasm` is left alone, being compressed already.

**A hostname may be a `*.name`**: `[route] hosts = ["*.apps.example"]`
reaches that app for `a.apps.example` and `b.apps.example`. One label and
never more — `a.b.apps.example` is not it — because a wildcard that crossed
a dot would claim a name whose own `*.b.apps.example` is somebody else's.

## Security

Same-process isolation between apps is a fault and resource boundary for
code you run yourself: each request is its own isolate with its own heap and
budget, an app can reach only the host modules it is granted, and one app's
bug, overload or budget overrun ends that app's requests and nobody else's.
**It is not a security guarantee against untrusted or malicious code**: the
apps share one process, one address space and one native code generator, and
nothing here has been reviewed as a sandbox. Deploy only apps you wrote or
reviewed; `admin` is a capability for code you trust.

The host terminates TLS with `--tls-cert` and `--tls-key` ([The front
door](#the-front-door)); without them it is plain HTTP, for a reverse proxy
(Caddy, nginx, cloudflared) that terminates it and forwards to `--addr`,
which defaults to localhost. Behind a proxy, pass `--public-origin` so the
apps' same-origin checks and the URLs they write use the public origin.
Keep the admin listener on localhost, and on loopback the admin listener
has no TLS of its own.

## Examples

[`examples/`](examples) holds apps written for minicloud. They are not in a
release: each is deployed with `minicloud deploy` like an app from any other
repository ([Deploying an app](#deploying-an-app)). They live here rather
than in Cove's repository because they are checked against minicloud's host
modules, and CI checks and tests them.

| app | grant | what |
| --- | --- | --- |
| [`hello`](examples/hello) | — | greets `?name=`; `/echo`, `/big?bytes=`, and `/spin`, which runs into its deadline |
| [`crunch`](examples/crunch) | — | counts primes up to `?n=`: CPU-heavy, the reason for the slice |
| [`slow`](examples/slow) | `log`, `timer` | sleeps `?ms=` `?times=`: a run that parks |
| [`notes`](examples/notes) | `kv`, `log` | a note per path, in the app's own store |
| [`proxy`](examples/proxy) | `fetch`, `log` | forwards a request to `?url=` on its allowlist |
| [`webhooks`](examples/webhooks/README.md) | `kv`, `fetch`, `auth`, … | the webhook lab: receive URLs that keep every request, and pages to read and resend them |
| [`ledger`](examples/ledger/README.md) | `kv`, `auth`, `log` | the bench ledger: benchmark results posted as JSON, and pages to compare them |
| [`algo`](examples/algo/README.md) | `time` | the algorithm playground: bipartite matching, SAT and simulated annealing, run on request |
| [`watch`](examples/watch/README.md) | `sql`, `queue`, `defer`, `stream`, `socket`, `keyed`, … | a watch on the things you run: a minute's `[[triggers]]` check, the answers in `sql`, a change on a `[[queues]]`, a dashboard with `assets/` and `stream`, and a console over `socket` |

`webhooks`, `ledger` and `watch` take a secret from the environment
(`WEBHOOKS_ADMIN_TOKEN`, `LEDGER_TOKEN`, `WATCH_ADMIN_TOKEN`); on the server
those are in `~/cove-tools/env`. Deploy one from a checkout over ssh, as in [Deploying an
app](#deploying-an-app):

```console
$ tar -C examples/ledger -c . | ssh whisky \
    '~/cove-tools/current/minicloud deploy - --name ledger --admin 127.0.0.1:8791 --token-file ~/cove-tools/data/admin.token'
```

## Development

The host is Rust; the apps are Cove. Cove is a git dependency pinned to one
commit (`rev` in the workspace [`Cargo.toml`](Cargo.toml)): a change the
compiler or the runtime needs goes to [myuon/cove](https://github.com/myuon/cove)
first and is picked up here by moving `rev`.

Build and test under `--profile checked` — release with debug assertions and
overflow checks on. The tests run Cove programs, which unoptimised is several
times slower; `cargo t` is an alias for `cargo test --workspace --profile
checked`. What the tests cover is in [docs/testing.md](docs/testing.md).

CI ([`ci.yml`](.github/workflows/ci.yml)) runs, on x86-64 Linux so that the
native tier is exercised:

```console
$ cargo fmt --all --check
$ cargo clippy --workspace --all-targets --profile checked -- -D warnings
$ RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --profile checked
$ cargo test --workspace --profile checked
$ COVE_HOST_TEST_BACKEND=vm cargo test --profile checked --tests
$ export WEBHOOKS_ADMIN_TOKEN=ci LEDGER_TOKEN=ci ADMIN_UI_TOKEN=ci
$ ./target/checked/minicloud check --apps apps && ./target/checked/minicloud test --apps apps
$ ./target/checked/minicloud check --apps examples && ./target/checked/minicloud test --apps examples
$ sh bench/repro/run.sh native
$ shellcheck deploy/*.sh
```

and then packages the release tarball from that binary
([`deploy/package.sh`](deploy/package.sh)) and installs and runs it as
deployed ([`deploy/smoke.sh`](deploy/smoke.sh); Linux, ports 8790 and 8791).

[`bench/`](bench/README.md) is the load test (`bench/perf.sh`, with its own
load generator, `minicloud-load`) and its results;
[docs/measurements.md](docs/measurements.md) summarises them.

## Migrating from fuel (Cove ADR 0091)

Cove's [ADR 0091](https://github.com/myuon/cove/blob/main/docs/adr/0091-a-run-is-stopped-by-its-host-not-a-fuel-allowance.md)
removed the fuel allowance from the runtime, and minicloud 0.4.0 followed: a
request is bounded by its `deadline` (10 s by default), its host calls and
its heap. There is no `limits.fuel`, no `fuel` error kind or `x-cove-stop:
fuel`, no `fuel` in the stats, and no `x-cove-run-fuel` header.

- **`app.toml`**: delete `fuel` from `[limits]`, and set a `deadline` if the
  app relied on fuel to stop runaway work sooner. `minicloud check`, `test`,
  `deploy` and `update` refuse a file that still says `fuel`, by name.
- **Apps already deployed** keep running: at start (and on a rollback, an
  admin change or a secret reload) the key is ignored with a warning naming
  the app, and `install.sh` checks with `minicloud check --deployed`, which
  warns rather than refuses.
- **Admin overrides** in `<data>/_host/overrides.json` that set `fuel` lose
  it when the host reads the file, with a warning; nothing needs editing.
- **Scripts and dashboards** that read `x-cove-run-fuel` or `errors.fuel`
  should read `x-cove-run-worker-us`, `x-cove-run-instructions` or
  `worker_ms`, and expect `x-cove-stop: deadline` (504) where they expected
  `fuel` (500).

Upgrade the host first, then redeploy each app without `fuel`.
