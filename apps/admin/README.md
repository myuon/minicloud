# The admin app

What the host runs, and the forms that change it (issue #17): enable or
disable an app, change its grant, its fetch allowlist and its limits, reset
it to its `app.toml`, and read the change history. What each app holds and
what it said are there to read as well — a page of its `kv` store and the
last lines of its log — and what the version it is running *is*: its
`app.toml`, its `.cove` files and its `assets/` (issue #89) — and the tables
of its `sql` store, with one statement that reads them (issue #88). All of
that is only to read. Pages, form reading, validation and rendering are
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
| `GET /` | every app: state (`serving`, `disabled`, `refused`, `removed`) and why, version and tier, how it is reached, what its code requires and what it is granted (changes marked), its counters and store |
| `GET /apps/<app>` | one app in full, its limits and recent errors, and the forms below |
| `POST /apps/<app>/enable`, `/disable` | routes to the app again, or stops (503; its data stays) |
| `POST /apps/<app>/configure` | the grant (`cap.<name>` checkboxes), the allowlist (`allow`, one per line) and every limit (`maxHostCalls`, `deadlineMs`, `maxHeapWords`, `maxInFlight`, `maxQueued`, `maxRequestBytes`, `maxResponseBytes`) |
| `POST /apps/<app>/reset` | drops the changes made here, back to `app.toml` |
| `GET /apps/<app>/kv` | a page of the app's store: 50 keys at a time (`prefix` to narrow, `after` to go on), each value cut to a line; `?key=<key>` is one key, its value whole. **Reading only** — nothing here writes to another app's store |
| `GET /apps/<app>/code` | what the version it is running is made of: its `app.toml` first, then its `.cove` files at any depth, then its `assets/` — each with its size and how the file of the same name in the app's directory compares; `?file=<name>` is one file's text, cut past 128 KiB, which the page says. **Reading only** |
| `GET /apps/<app>/sql` | the app's SQL store: its tables and how many rows each has (`not counted` where counting ran out of the time); `?q=<statement>` runs that one statement and shows the answer as a table. At most 200 rows — more is an error naming the limit, not the first 200 shown as the whole — and 1000 ms. `NULL` and the empty string are written apart. **Reading only** — a statement that writes is refused, and nothing typed here is kept in the history |
| `GET /apps/<app>/logs` | the last lines the app logged and the host said of it, oldest first, with their level; `?n=` how many (1–1000, 200 by default). The ring is in memory, so a restart empties it |
| `GET /history` | every change, made here or by `minicloud` on the machine, refused ones included |
| `GET /secrets` | the host's secret store: each secret stored or used by an app — set or unset, when it was set, which apps use it — **never a value** |
| `POST /secrets/set` | `name` and `value` (a password field, never filled back in): stores it and reloads the apps that use it; the page says what each reload came to |
| `POST /secrets/delete` | `name`, `confirm` (a required checkbox: there is no script for a dialog), and `force` for a secret an app uses, which leaves those apps refused |

The secret store and what a change to it does are in
[docs/design.md, Secrets set at run time](../../docs/design.md#secrets-set-at-run-time);
the code window, and why it shows what is in memory rather than what is on
disk, is in
[docs/design.md, Seeing the code that is deployed](../../docs/design.md#seeing-the-code-that-is-deployed);
the SQL window, and why it reads through the store the app has open, is in
[docs/design.md, Reading an app's SQL store](../../docs/design.md#reading-an-apps-sql-store).

A change that went through redirects back to the app's page (`?done=`); one
that did not answers 422 with the reasons — the form's own (a field that is
not a whole number, below its least value, an allowlist line that is not
`http(s)://…`), or the host's (the reload's) — and the form as posted.

## On a screen of any width

The pages are one stylesheet, [`assets/style.css`](assets/style.css), and
no script at all: what they do on a phone, they do in CSS. Nothing scrolls
sideways under about 760 px — each row of a wide table (the apps, the
history, the secrets, an app's recent errors) becomes a card, its cells
labelled by the column headings they lose, and the forms' fields go one to a
line. The colours are custom properties with a `prefers-color-scheme: dark`
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
