# The webhook lab

A Cove app on minicloud (issue #2). It hands out **receive URLs** that keep
every request sent to them, and **admin pages** to read what was received.
Handling and HTML are Cove. Persistence, the clock, randomness and who is
asking are the host's typed modules.

| path (below `/webhooks`) | what | auth |
| --- | --- | --- |
| `ANY /in/<endpoint>[/...]` | stores the request: method, the path below the receive URL, query, headers, body, and the time it arrived. Answers `200 {"ok":true,"endpoint":…,"event":…}` | none; the endpoint id (16 random hex digits) is the secret part of the URL |
| `GET /admin` | lists the endpoints, with a form to create one | admin |
| `POST /admin/endpoints` | creates an endpoint from `name`, `retention`, `maxBody` and `mask` fields. Answers 303 to its page, or 201 JSON with `Accept: application/json` | admin |
| `GET /admin/e/<endpoint>` | the endpoint's history, newest first, 50 per page (`?before=`). `?format=json` gives the same as JSON | admin |
| `GET /admin/e/<endpoint>/events/<event>` | one request in full: summary, query, headers, the body (also pretty-printed when it is JSON), and a copy button. `?view=json` gives the stored event as JSON; `?view=raw` gives the body alone, as plain text | admin |
| `POST /admin/e/<endpoint>/events/<event>/resend` | resends the stored request to the form's `url` (one `[fetch] allow` admits) and keeps what came back; `?view=resends` on the request lists the attempts as JSON | admin |
| `POST /admin/e/<endpoint>/settings` | sets what the endpoint answers: `status`, `headers` (`Name: value` lines), `body`, `delayMs`, `errorEvery`, `errorStatus`, `errorBody` | admin |
| `POST /admin/e/<endpoint>/clear`, `…/delete`, `…/events/<event>/delete` | deletes the endpoint's history, the endpoint with its history, or one request | admin |

## Running it

```console
$ export WEBHOOKS_ADMIN_TOKEN=change-me      # the admin secret; the app is refused without it
$ ./target/checked/minicloud serve --apps examples
$ curl -s -u admin:change-me -H 'accept: application/json' -d 'name=github' \
    http://127.0.0.1:8080/webhooks/admin/endpoints
{"id":"fd9f7a5017d31e7e","name":"github","url":"http://127.0.0.1:8080/webhooks/in/fd9f7a5017d31e7e"}
$ curl -s -H 'content-type: application/json' -H 'x-github-event: push' \
    -H 'authorization: Bearer ghs_secret' -d '{"ref":"refs/heads/main"}' \
    'http://127.0.0.1:8080/webhooks/in/fd9f7a5017d31e7e/push?delivery=1'
{"endpoint":"fd9f7a5017d31e7e","event":"1791171122814503-09b5","ok":true}
$ curl -s -u admin:change-me 'http://127.0.0.1:8080/webhooks/admin/e/fd9f7a5017d31e7e?format=json'
{
  "endpoint": {
    "created": 1791171122675,
    "id": "fd9f7a5017d31e7e",
    "mask": true,
    "maxBody": 65536,
    "name": "github",
    "retention": 100
  },
  "events": [
    {
      "bytes": 25,
      "contentType": "application/json",
      "id": "1791171122814503-09b5",
      "method": "POST",
      "path": "/push",
      "receivedAt": "2026-10-05T03:32:02.814Z"
    }
  ]
}
```

Stop the host (Ctrl-C) and start it again: the history is still there, because
it lives in the app's store, `data/webhooks/kv.sqlite3`. Then resend it to a
second endpoint:

```console
$ curl -s -u admin:change-me -H 'accept: application/json' -d 'name=staging' \
    http://127.0.0.1:8080/webhooks/admin/endpoints
{"id":"22e210190123a8df","name":"staging","url":"http://127.0.0.1:8080/webhooks/in/22e210190123a8df"}
$ curl -s -u admin:change-me -H 'accept: application/json' \
    --data-urlencode 'url=http://127.0.0.1:8080/webhooks/in/22e210190123a8df/replayed' \
    http://127.0.0.1:8080/webhooks/admin/e/fd9f7a5017d31e7e/events/1791171122814503-09b5/resend
{
  "at": 1791171532766,
  "atIso": "2026-10-05T03:38:52.766Z",
  "body": "{\"endpoint\":\"22e210190123a8df\",\"event\":\"1791171532766856-a40e8c42\",\"ok\":true}\n",
  "bytes": 78,
  "error": "",
  "headers": { … },
  "id": "1791171532766257",
  "method": "POST",
  "millis": 1,
  "ok": true,
  "status": 200,
  "truncated": false,
  "url": "http://127.0.0.1:8080/webhooks/in/22e210190123a8df/replayed"
}
$ curl -s -u admin:change-me -H 'accept: application/json' -d 'url=http://example.org/x' \
    http://127.0.0.1:8080/webhooks/admin/e/fd9f7a5017d31e7e/events/1791171122814503-09b5/resend | grep -e '"ok"' -e error
  "error": "`http://example.org:80` is not on app `webhooks`'s fetch allowlist (http://127.0.0.1:8080, http://localhost:8080)",
  "ok": false,
```

`staging` now holds the request at `/replayed`: same method, body and
headers. In the browser, the request's page has a **Resend** form and the
table of earlier attempts. In a browser, open
`http://127.0.0.1:8080/webhooks/admin` and log in with any user name and the
secret as the password.

## Admin access

The admin pages ask the host who is asking: `auth.identity(request.headers)`
(main README, *Cloudflare Access*).

**Deployed, Cloudflare Access** (issue #23). `covtools.ramda.io` is behind
the Access application `covtools`, and the host verifies the token Access
adds to each request (`Cf-Access-Jwt-Assertion`, or the `CF_Authorization`
cookie): signed by the team's keys, for that application's AUD tag, not
expired, with an email (and one of `ACCESS_ALLOWED_EMAILS`, if set). The
settings are `[access]` in `app.toml`, the values from `ACCESS_TEAM_DOMAIN`,
`COVTOOLS_ACCESS_AUD` and `ACCESS_ALLOWED_EMAILS` in the environment
([deploy/cloudflare.md](../../deploy/cloudflare.md) §6). Without a valid
token the pages answer 403 with no login prompt; with Access on, the
`WEBHOOKS_ADMIN_TOKEN` secret is not accepted (`fallback = "none"`).

**Run without Access** (`team` or `aud` unset, as in [Running
it](#running-it)): the secret named under `[secrets]`, `admin = { env =
"WEBHOOKS_ADMIN_TOKEN" }` here (`file = "..."` and, for tests, `value = "..."`
also work), is the way in. The host accepts `Bearer <secret>` and `Basic
<base64(any-user:secret)>`, so a browser's login prompt (401 with
`WWW-Authenticate: Basic`) works. The comparison is constant-time, and the
secret never enters the app's run: the code can check a credential but cannot
read one, so it can't log it or leak it in an error.

Receive URLs and admin pages are separate paths. Only `/admin/` asks who is
asking — the receive URLs never call `auth.identity`, so they need no token
and fetch no keys, and in the deployment `/webhooks/in` is an Access Bypass
application — and nothing under `/in/` can read or change what is stored.

**Cross-site forms.** A browser re-sends a stored Basic login with any request
to the site, including a form posted from another site. So a request that
changes something is refused (403) when `Sec-Fetch-Site` says it came from
another site, or when `Origin` names another origin. curl sends neither header
and is allowed, because it holds the secret itself.

**Page headers.** Every admin page is sent with a `Content-Security-Policy`
(no script but the page's own copy buttons, no framing, forms to the lab
only), `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, no referrer
and `Cache-Control: no-store`.

## What is stored, and how much

- **Retention:** each endpoint keeps its last `retention` requests (1–1000,
  default 100). The oldest are deleted as new ones arrive.
- **Body size:** each endpoint stores at most `maxBody` bytes of a body (up to
  1 MiB, default 64 KiB), cut at a character boundary. The event records the
  full size and that it was truncated.
- **Host limits:** the host refuses a body over the app's `max_request_bytes`
  (1 MiB) with 413 before the app runs. The app's KV quotas (`[kv]` in
  `app.toml`) bound the whole store. A write past a quota answers the sender
  507 and stores nothing.
- **Masking:** on an endpoint that masks (the default), the values of
  `authorization`, `proxy-authorization`, `cookie`, `set-cookie`,
  `x-api-key`, `x-auth-token` and `x-access-token` are replaced by
  `[masked: N characters]` **before anything is stored**, and the event lists
  which headers were masked. An endpoint created with `mask=off` (or with the
  checkbox unticked) stores every header as sent.
- **Body encoding:** bodies must be UTF-8 (the host answers 400 otherwise).
  Binary webhooks are out of scope for now.

## Resending

From a request's page (or `POST …/events/<event>/resend` with `url=`), the
stored request goes out again with `fetch.request`:

- **What is sent:** the same method and body, and the headers less the ones
  that belong to the connection it arrived on (`host`, `content-length`,
  `connection`, `transfer-encoding`, …), the host's own `x-forwarded-*`,
  `accept-encoding` (the app could not decode a compressed answer), and any
  header that was **masked**. Only a placeholder of a masked header was
  stored, and sending that as though it were the value would be wrong.
- **Where it may go:** only a URL that `[fetch] allow` in `app.toml` admits.
  The shipped config allows this host on port 8080, which is what the
  walkthrough uses; add the services you replay to. A URL off the list is
  refused by the host before anything is sent.
- **While it is out**, the run is parked: it holds no worker. `[fetch]
  timeout = "8s"` keeps a slow target below the run's 10 s deadline, so it is
  stored as a failure rather than answered 504.
- **What is kept:** every attempt, as one record under the request:
  - when it was sent, the URL, and how long it took;
  - for a response (any status): its status, headers and body (truncated to
    the endpoint's `maxBody`);
  - for no response: why (refused by the allowlist, could not connect, timed
    out, too large).

  The newest 20 per request are kept. Deleting the request, its endpoint's
  history or the endpoint deletes its attempts too. A response is shown as
  text, escaped, like everything else.
- Resending is manual, one request at a time. Automatic retries or forwarding
  on receipt are not part of this version.

## Simulating responses

Each endpoint answers what its settings say. The form is on the endpoint's
page, or `POST …/settings`:

| setting | default | what |
| --- | --- | --- |
| `status` | 200 | the status of an ordinary answer (100–599) |
| `headers` | none | `Name: value` lines, added to the answer. A name is letters, digits and `-`; the host keeps the framing headers its own |
| `body` | the receipt | an ordinary answer's body; empty answers the receipt `{"ok":true,"endpoint":…,"event":…}` |
| `delayMs` | 0 | how long to wait before answering, 0–8000 ms (below the app's 10 s deadline) |
| `errorEvery` | 0 | fail 1 in every N requests (the Nth, 2Nth, …); 0 never fails |
| `errorStatus`, `errorBody` | 500, a line saying so | what a failing request is answered with |

```console
$ curl -s -o /dev/null -w '%{http_code}\n' -u admin:change-me --data-urlencode 'status=202' \
    --data-urlencode $'headers=Content-Type: text/plain\nX-Lab: yes' --data-urlencode 'body=accepted' \
    -d 'delayMs=300&errorEvery=3&errorStatus=503' \
    http://127.0.0.1:8080/webhooks/admin/e/02f9dcf99bc6668a/settings
303
$ for i in 1 2 3 4; do curl -s -w ' %{http_code} %{time_total}s\n' -d x http://127.0.0.1:8080/webhooks/in/02f9dcf99bc6668a; done
accepted 202 0.302484s
accepted 202 0.302283s
simulated error: request 3 of endpoint 02f9dcf99bc6668a fails (every 3)
 503 0.302161s
accepted 202 0.302223s
```

- **The delay holds no worker.** It is `timer.sleep`, which the host answers
  pending, so the run parks and the worker serves others in the meantime.
  The test runs the host on a single worker: `hello` and the lab's own pages
  answer while a 3 s delay is waiting. The request is stored **before** the
  delay, so it is on the history page while the sender waits.
- **The schedule counts exactly**, with concurrent senders too. It runs on
  `kv.increment`, an atomic add in the host's store (a `get` then a `put`
  would lose counts under concurrency). 20 concurrent requests with
  `errorEvery=2` fail exactly 10 times. Deleting the endpoint's history
  starts the count again.
- The history shows the status each request was answered with. Every
  answer carries `x-webhook-event: <event id>`, so a sender's log can be
  matched to the stored request.
- Bad settings (a status outside 100–599, a delay over 8000, a header line
  that is not `Name: value`, a number that is not one) are refused with 400
  and change nothing. The settings are shown escaped like everything else.

## Issue #2's completion criteria

| criterion | test (`crates/minicloud/tests/webhooks.rs`) | README |
| --- | --- | --- |
| curl receive → restart → history still visible → resend to another endpoint | `receive_restart_and_resend_to_another_endpoint`; also `a_request_is_stored_whole_and_survives_a_restart` | [Running it](#running-it) |
| a delayed endpoint waiting doesn't stop other apps answering | `a_delayed_endpoint_parks_while_other_apps_answer` (one worker; `hello` and the lab's pages answer during a 3 s delay; one park, no blocking call) | [Simulating responses](#simulating-responses) |
| admin access control | `the_admin_pages_need_the_secret_and_the_receive_urls_do_not` (401 without, with a wrong Bearer token, and with a wrong Basic password; the Basic login works; receive URLs need nothing; cross-site form posts are 403); behind Access, `access.rs::the_webhook_labs_pages_need_a_token_and_its_receive_urls_do_not` (403 without a token, with the admin UI's token and with the secret; the lab's token gets in; receive URLs need nothing and fetch no keys) | [Admin access](#admin-access) |
| storage limits | `retention_and_the_body_limit_bound_what_is_kept`, `a_body_past_the_apps_request_limit_is_refused_by_the_host`; masking in `secret_headers_are_masked_unless_the_endpoint_says_not` | [What is stored, and how much](#what-is-stored-and-how-much) |
| display escaping | `everything_a_sender_controls_is_escaped_on_the_pages`, plus resend responses in `a_resend_that_gets_no_response_is_stored_as_a_failure` and settings in `bad_settings_are_refused_and_good_ones_are_shown_escaped` | [Display and escaping](#display-and-escaping) |
| resend stores the response or the failure | `receive_restart_and_resend_to_another_endpoint`, `a_resend_that_gets_no_response_is_stored_as_a_failure` | [Resending](#resending) |
| per-endpoint status, headers, body and error every N | `an_endpoint_answers_as_configured_and_fails_on_schedule`, `the_error_schedule_counts_every_request_even_at_once` | [Simulating responses](#simulating-responses) |

## Display and escaping

Every value a sender controls goes through `text.escapeHtml` before it becomes
part of a page: the endpoint name, the path, the query, the header names and
values, and the body. The page's own markup is the only markup on it. The raw
view is `text/plain` with `nosniff`, so a body that is HTML is shown as text,
not rendered. The tests send `<script>` and `<img onerror>` in every one of
those places.

## Layout

| file | what |
| --- | --- |
| `webhooks.cove` | routing, receiving, the admin actions |
| `store.cove` | the records in `kv`, as JSON: `endpoint:<id>`; `index:<id>:<event>` (a summary for listing and trimming); `event:<id>:<event>` (the request in full). An event id is its arrival time in microseconds, zero-padded to 16 digits and strictly increasing (`time.nowMicros()`), plus a random suffix, so keys sort by arrival |
| `resend.cove` | resending, and the attempts, `resend:<id>:<event>:<n>` |
| `simulate.cove` | what an endpoint answers (`reply:<id>`), its delay, and its error schedule (`count:<id>`, by `kv.increment`) |
| `pages.cove` | the HTML |
| `fields/` | reading a named field of a stored object as text, an `Int`, a flag or a map of text, with a fallback, over `std.json`'s accessors. The parser and the renderers themselves are `std.json`'s |
| `text/` | HTML escaping, form decoding (`+`, `%XX`, UTF-8), ISO 8601 times, byte sizes, truncation at a character boundary |

`minicloud test --apps examples webhooks` runs the Cove tests in `fields/` and `text/`. The Rust
tests in `crates/minicloud/tests/webhooks.rs` drive the app over HTTP.
