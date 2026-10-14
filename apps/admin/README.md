# The admin app

What the host runs, and the forms that change it (issue #17): enable or
disable an app, change its grant, its fetch allowlist and its limits, reset
it to its `app.toml`, and read the change history. What each app holds and
what it said are there to read as well — a page of its `kv` store and the
last lines of its log, and every app's lines in one order (issue #108) — and
what the version it is running *is*: its
`app.toml`, its `.cove` files and its `assets/` (issue #89) — and the tables
of its `sql` store, with one statement that reads them (issue #88), and what
is on its queues (issue #90). All of that is only to read. Pages, form reading, validation and rendering are
Cove; everything it learns of the host and every change it makes go through
one host module, `host`, whose capability is
`admin` — and **only this app may be granted `admin`**. The startup banner
and `/_host/stats` say so like any other grant:

```text
  admin      v1-…  requires [admin, auth]  granted [admin, auth]  ok: …
```

The host side — what a change means, what is refused, where it is kept — is
in [docs/design.md, Administering apps at run time](../../docs/design.md#administering-apps-at-run-time).

| request | does |
| --- | --- |
| `GET /` | every app, one card each: state (`serving`, `disabled`, `refused`, `removed`) and why, version and tier, how it is reached, what it has served — and, last and hardest to miss, what is worth a look about it, which is also the link to the page that shows it. The apps with something to say are first; the rest say `quiet`. What an app may do and what its code asks for are not here: they are what Settings is for |
| `GET /apps/<app>` | the app's **Overview** (issue #108): the numbers (served, errors, in flight, queued), then **worth a look** — the host saying what is wrong before being asked — then a card for each store it has, the last lines of its log, and one line about how it is set up with the way to Settings |
| `GET /apps/<app>/settings` | everything that *changes* the app: what the deploy says (read only), then the grant, the fetch allowlist and the limits as one form, then routing, then reset |
| `POST /apps/<app>/enable`, `/disable` | routes to the app again, or stops (503; its data stays) |
| `POST /apps/<app>/configure` | the grant (`cap.<name>` checkboxes), the allowlist (`allow`, one per line) and every limit (`maxHostCalls`, `deadlineMs`, `maxHeapWords`, `maxInFlight`, `maxQueued`, `maxRequestBytes`, `maxResponseBytes`) |
| `POST /apps/<app>/reset` | drops the changes made here, back to `app.toml` |
| `GET /apps/<app>/speed` | how fast the app is and how much it served, over time: the window's totals (requests, errors, the worst p99), then two charts drawn as inline SVG with no script — **traffic** (served and errors) and **speed** (p50 and p99, in milliseconds) — each line its own dash pattern as well as its own colour. `?over=` is `hour` (the default), `day` or `month` (30 days); anything else is an hour. The rows are the host's own once-a-minute samples of the app's counters (`host.series`), minutes kept for about two days and hours for about three months, and **p50 and p99 are estimates** read off a histogram, which the page says. A host that keeps no history and one that has not sampled yet both answer nothing, so the page's one sentence for that is true of both. **Reading only** |
| `GET /apps/<app>/kv` | a page of the app's store: 50 keys at a time (`prefix` to narrow, `after` to go on), each value cut to a line; `?key=<key>` is one key, its value whole. **Reading only** — nothing here writes to another app's store |
| `GET /apps/<app>/code` | what the version it is running is made of, as a tree: its `app.toml` first, then its `.cove` files at any depth, then its `assets/` — each row with its size and how the file of the same name in the app's directory compares, nested — a list inside the item that holds it — by the depth the host gave it; `?file=<name>` is one file's text, syntax-coloured, cut past 128 KiB, which the page says. **Reading only** |
| `GET /apps/<app>/sql` | the app's SQL store: its tables and how many rows each has (`not counted` where counting ran out of the time); `?q=<statement>` runs that one statement and shows the answer as a table. At most 200 rows — more is an error naming the limit, not the first 200 shown as the whole — and 1000 ms. `NULL` and the empty string are written apart. **Reading only** — a statement that writes is refused, and nothing typed here is kept in the history |
| `GET /apps/<app>/queues` | the app's queues: each one its `[[queues]]` declares, with what is **due** (the next delivery tick may take it), what is **held** (its due time is ahead — a delivery's lease, or a failed delivery waiting for its retry; the store keeps one due time for both) and how many dead letters it has, beside the settings the host delivers it by; then the messages waiting, what is due first, with their tries and when each is next due; then the dead letters, newest first, with why each was buried. `?message=<id>` is one message, its body whole (cut past 64 KiB, which the page says) — a listing cuts a body to the column, and a queue's body is a payload. At most 200 of each are listed and the counts above them are whole, so a page that is showing the first 200 says so. **Reading only** — the window leases nothing and counts no attempt, so a queue somebody looks at is delivered exactly as it would have been, and putting a dead letter back is `minicloud queues <app> --revive <id>` on the machine |
| `GET /apps/<app>/logs` | the last lines the app logged and the host said of it, oldest first, with their level; `?n=` how many (1–1000, 200 by default). The ring is in memory, so a restart empties it |
| `GET /logs` | **every app's log in one order** (issue #108), oldest first, each line saying which app wrote it; `?n=` how many, as above. Above it, a chip for every app the host has a line of, which is the way to that app's own `Logs` |
| `GET /history` | every change, made here or by `minicloud` on the machine, refused ones included |
| `GET /secrets` | the host's secret store: each secret stored or used by an app — set or unset, when it was set, which apps use it — **never a value** |
| `POST /secrets/set` | `name` and `value` (a password field, never filled back in): stores it and reloads the apps that use it; the page says what each reload came to |
| `POST /secrets/delete` | `name`, `confirm` (a required checkbox: there is no script for a dialog), and `force` for a secret an app uses, which leaves those apps refused |

The secret store and what a change to it does are in
[docs/design.md, Secrets set at run time](../../docs/design.md#secrets-set-at-run-time);
the code window — why it shows what is in memory rather than what is on
disk, why the host nests the tree and colours the text, and why it answers
pieces rather than HTML — is in
[docs/design.md, Seeing the code that is deployed](../../docs/design.md#seeing-the-code-that-is-deployed);
the SQL window, and why it reads through the store the app has open, is in
[docs/design.md, Reading an app's SQL store](../../docs/design.md#reading-an-apps-sql-store);
the queue window, and why it cannot tell a lease from a retry's wait, is in
[docs/design.md, Seeing what is on an app's queues](../../docs/design.md#seeing-what-is-on-an-apps-queues).

A change that went through redirects back to the app's page (`?done=`); one
that did not answers 422 with the reasons — the form's own (a field that is
not a whole number, below its least value, an allowlist line that is not
`http(s)://…`), or the host's (the reload's) — and the form as posted.

## An app is a workspace

Every page about one app opens the same way (issue #108): where it is, its
name and state, one line of what it is serving and where, and a row of tabs
— `Overview`, `Logs`, `Data`, `SQL`, `Queues`, `Code`, `Settings`.

**A tab is there when the app has the thing.** An app not granted `sql` has
no `SQL` tab, and the same for `kv` and `queue`: the capabilities an app was
granted are the shape of its pages, which is this app saying what the
capability model already says. The windows still answer for an app without
the grant — a URL somebody kept is not a 404 — and say what is missing; they
are simply not offered. `Overview`, `Code` and `Settings` are on every app,
and so is `Logs`: the ring that page reads is the host's, so it holds the
host's own lines about an app granted nothing at all.

Every page's explanation is folded into a `<details>` whose `<summary>` is
the one line worth reading. `<details>` is HTML and runs nothing, so the CSP
is unchanged.

### Worth a look

The Overview's second heading is the thing these pages never had: the host
saying what is wrong before being asked. A refusal and why; the requests
that failed; requests turned away at a limit; what is held and what is dead
on each queue; a store near what it may hold; a file of the deploy the app's
directory holds differently; Cloudflare Access asked for in `app.toml` and
not on. The worst first, each a line with the way to the page that shows it.

**Nothing wrong is one green line saying so**, naming what was looked at.
"Is anything wrong" is the question somebody opened the page to answer, and
a page that answers it by being empty has not answered it.

The one read there that is not free is the comparison with the app's
directory, which reads every file of the deploy. The host does it under a
budget ([`code::DISK_CHECK_BYTES`](../../crates/minicloud/src/code.rs), a
megabyte) and says whether it looked: past that the page says it did not and
sends the reader to the `Code` tab, which reads them all because somebody
asked for exactly that. A cheaper comparison — size, modification time —
would miss the hand edit that keeps the length, which is the case the
comparison is for, so the page says "I did not look" rather than guessing.
The list of apps does not compare at all, and says nothing about the files:
it has every app on it.

## Logs across every app

`Logs` is in the top navigation beside `Apps`, and the page there is every
app's lines in one order (issue #108). A queue delivery failing in one app
and the request that caused it in another are one sequence; on two pages
they are two pages and a clock.

**The host does the mixing** (`host.allLogs`): it holds every app's ring,
and merging sorted arrays is not something to write in a page. It takes the
asked-for number of lines from each ring, merges them by time and cuts the
merge back — which is exactly the last *n* lines of the host's whole log,
because every line of it is a line of some app's.

**So one app with a lot to say fills the page**, and the quiet ones fall off
the end. That is what "the last *n* lines" is, and it is why the number of
lines (50 / 200 / 1000) is offered above the table and below it. The chips
above it are the other way out: one per app the host has a line of, and each
goes to **that app's own `Logs`**, which is the same window with the app
chosen. They are every app with a log and not the apps on the page — a way
in that comes and goes with `?n=` is not a way in — and `all apps` is the
one marked, because that is where the reader is. They carry no dot of
health: that is what the list of apps works out, from an app's counters and
a read of its queues, and a second rule for the same word is a second
answer.

## On a screen of any width

The pages are one stylesheet, [`assets/style.css`](assets/style.css), and
no script at all: what they do on a phone, they do in CSS. Nothing scrolls
sideways under about 760 px — each row of a wide table (the history, the
secrets) becomes a card, its cells labelled by the column headings they
lose, an app's card stacks its facts under its name, and the forms' fields
go one to a line. Under about 560 px the tabs are pills that wrap: six
folder tabs do not fit a phone in one row, and a row that scrolls sideways
hides the ones that matter behind a gesture. The colours are custom properties with a `prefers-color-scheme: dark`
set beside them, and `color-scheme` tells the browser to draw its own parts —
the checkboxes, the scrollbars, the login prompt — to match.

## `assets/`

Static files the host serves itself, before a request reaches the app (issue
#57). The directory has no `.cove` in it, so it is not a module.

| file | is |
| --- | --- |
| `assets/style.css` | the one stylesheet every page links |
| `assets/favicon.svg` | the icon, which scales to any size; linked by the pages |
| `assets/favicon.ico` | the same icon at 16x16 and 32x32, as PNGs; browsers ask for `/favicon.ico` on their own |

The request path is the file name: this app is reached by hostname, so the
path arrives as it was sent, and `GET /style.css` is `assets/style.css`,
`GET /favicon.ico` is `assets/favicon.ico`. Only `GET` and `HEAD` are
answered from the files; a path with no file goes to the app's `handle`, as
before. The host adds `content-type` (from the extension), `etag` and
`cache-control: no-cache`.

**The pages link them by version**: `/style.css?v=<the version serving>`,
from `host.selfVersion` — the same string every answer carries as
`x-cove-app-version`. Without it the URL is the same URL after every deploy,
and a browser or a proxy holding the old `style.css` draws the new HTML with
it; that happened on a phone, where the rules 0.24.0 added were the only
ones missing. With it the new pages ask for a URL nothing has. The host
reads no query, so the file answered is the one the path names; the version
is there to change the URL. A host that cannot say the version gets the
plain URL back — a page without its stylesheet is worse than one a browser
revalidates. `[assets] cache_control` is left at `no-cache` all the same,
and `app.toml` says why: one header covers every file, and `/favicon.ico` is
a path browsers ask for without a version.

To draw the icon again, run `python3 apps/admin/favicon.py` from the
repository root (standard library only). It writes both icon files.

Because the stylesheet is a file, the pages no longer carry inline styles,
and the CSP has no `'unsafe-inline'`: it is `default-src 'none'; style-src
'self'; img-src 'self'; ...`.

## Getting in

- **Only by hostname**: `[route] hosts = ["covtools-admin.ramda.io",
  "admin.localhost"]`. `https://covtools.ramda.io/admin/` is a 404, so the
  main hostname's Access policy and its bypass paths are never a way in. On
  the machine, a browser reaches `http://admin.localhost:8790/` (`*.localhost`
  is loopback).
- **Cloudflare Access** in front of `covtools-admin.ramda.io`, owner only, no
  bypass ([deploy/cloudflare.md](../../deploy/cloudflare.md)).
- **Verified by the host** (issue #23): every request needs an identity,
  `auth.identity(request.headers)`. Deployed, that is the Access token the
  request came with (`Cf-Access-Jwt-Assertion`, or the `CF_Authorization`
  cookie), which the host verifies against the team's keys and the
  `covtools-admin` Access application's AUD tag — `[access]` in `app.toml`,
  its values from `ACCESS_TEAM_DOMAIN`, `COVTOOLS_ADMIN_ACCESS_AUD` and
  `ACCESS_ALLOWED_EMAILS` in the environment. Without a valid one the answer
  is 403, with no login prompt, so logging in to Access is all a browser
  does. `fallback = "none"`: `ADMIN_UI_TOKEN` is not accepted while Access
  is on, so a request straight to the host's port gets nowhere.
- **Run without Access** (`team` or `aud` unset, as locally): the secret
  `[secrets] admin = { env = "ADMIN_UI_TOKEN" }` is the way in —
  `Authorization: Bearer <token>`, or the browser's login prompt (401 with
  `WWW-Authenticate: Basic`) with the token as the password.
- **A change from another site is refused** (403): a POST whose
  `Sec-Fetch-Site` is not `same-origin`, or whose `Origin` is not the app's
  own. The host tells the app its origin is the hostname it was routed by
  (`https://covtools-admin.ramda.io` under `--public-origin
  https://covtools.ramda.io`), so the main hostname's pages cannot post here
  either.
- Every value shown is HTML-escaped; the pages carry a CSP of `default-src
  'none'` with this site's own stylesheet and icon only — **no script at
  all** — `form-action 'self'`, `frame-ancestors 'none'`, and `no-store`.
- The files in `assets/` are answered before the app runs, so the login is
  not in front of them: a stylesheet and an icon are public here, and
  nothing else belongs there. On the server Cloudflare Access is in front of
  the hostname, so they are behind it like everything else.
- Who made a change is the verified email from the Access token (`admin
  app: you@example.com` in the history), or `token` when the secret got in.
  The unsigned `Cf-Access-Authenticated-User-Email` header is not read: any
  client can send it.

Isolation between apps in one process is a fault and resource boundary, not a
security boundary against malicious code (main README, *Security*): `admin`
is a capability to give to code you trust, which is why one app holds it.

## If it breaks

The admin app is an app like the others: if it is refused or broken, the host
and every other app run on. It cannot disable itself, take `admin` away from
itself, or make any change that would leave it refused. On the machine, the
admin listener still can — `minicloud enable|disable|reset <app>` — and the
changes are plain JSON in `<data>/_host/overrides.json`, which can be edited
or deleted before a restart.

```console
$ ADMIN_UI_TOKEN=secret ./target/checked/minicloud serve --apps apps
$ curl -s -u x:secret http://admin.localhost:8080/ | head
$ open http://admin.localhost:8080/
```
