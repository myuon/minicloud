# minicloud: what the tests hold it to

`cargo t` (an alias for `cargo test --workspace --profile checked`) runs the
whole suite; the [README](../README.md#development) has the full gate CI runs.

The tests run Cove programs, so they run optimised (`--profile checked`, as
in Cove's own repository); a bare `cargo test` works, more slowly. The
integration tests (`crates/minicloud/tests/host.rs`, `services.rs` and
`updates.rs`, `deploying.rs`, `admin.rs`, `access.rs`, `secrets.rs`,
`assets.rs`, `triggers.rs`, and one
per real example: `webhooks.rs`, `ledger.rs`, `algo.rs`) start the host
in-process on a free port and ask it over TCP. None asserts a duration: where
a test needs the host in some state it waits for the host's own stats to say
so, and what it asserts is counted.

- two or more apps answer concurrently;
- `hello` answers while `crunch` holds every worker and `slow` is parked,
  and the crunches yielded and the slow runs parked at every sleep without
  blocking a worker;
- an app past its `max_queued` gets 429 with `Retry-After` while another app
  answers; the server past `--max-in-flight` gets 503; past
  `--max-connections`, 503;
- deadline (while running and while parked) and host-call overruns end only
  that request;
- an over-reaching app and a spawning app are refused at load, the others
  serve, and `check` agrees;
- request (declared and chunked) and response size limits;
- with one app flooding its queue on a single worker, another app's five
  requests are all served while the flood still has requests queued;
- on x86-64 Unix, the apps run on the native tier and a compiled run still
  yields;
- two apps' stores are separate (and separate files); a store survives
  stopping the host and starting another over the same data directory;
  quotas answer the app's `Err`; listing pages both ways;
- the SQL store (`sql.rs`, and `sql.rs`'s unit tests): a table, an index and
  an aggregate answer what `kv` could not; a parameter holding `'; DROP
  TABLE …` is stored and found as that string; `NULL` is not an empty
  string; a write through `query` is refused pointing at `exec`; a syntax
  error is the app's `Err` and names no file; `ATTACH` and `BEGIN` are
  refused by the authorizer and the app still works after them; two
  statements in one call are refused rather than half-run; `max_rows`,
  `max_result_bytes`, `max_bytes`, `max_statement_bytes` and
  `max_statement_ms` are each the app's `Err` naming the limit; a blob says
  what to do instead; the store survives a restart and is a file of its own
  beside where `kv`'s would be; an app not granted `sql` is refused at load
  and no file is made for it;
- `fetch` reaches an allowed local upstream with GET and with POST carrying
  a header and a body, exactly as the upstream records them; a target off
  the allowlist is refused with the upstream seeing no connection at all;
- a `[fetch.headers]` secret reaches its origin, with its prefix, replacing
  the app's header of the same name; another allowed origin gets the app's
  header and not the secret; and the value is in none of the app's answers,
  a failed fetch's `Err`, its logs, its admin view or the stats;
- a fetch is abandoned — the upstream sees its connection closed — at the
  run's deadline, and when the client goes away;
- a client going away cancels a spinning run that is running or yielded, one
  still queued (without running it), and a parked one, each counted;
- an update while requests are parked, running, yielded and queued: each of
  those answers with the old version's body and `x-cove-app-version`, a
  request after the switch with the new one's, and once they have drained
  the old version and its `PreparedProgram` are gone;
- an update refused for a parse error, an ungranted capability, a `spawn` or
  a config error keeps the current version serving, answers 422 with the
  reason, and another app answers throughout;
- the admin listener answers 401 without the token (missing, wrong, empty)
  and changes nothing, and the public listener has no update route;
- an update adds an app, `remove` removes one, and an update brings it back;
- deploying (`deploying.rs`, and `deploy.rs`'s unit tests): an archive is
  refused for a symbolic link, a path that leaves the directory (`..`, an
  absolute path), a hard link, a pipe, a duplicate or its size, and packs
  `app.toml` and `.cove` files only; a deploy adds an app, and replaces one
  while a request parked on the old version finishes on it; a deploy that
  does not parse, that needs an ungranted capability or that needs a secret
  the env lacks is refused with its reason and changes no file; rollback
  restores the kept version and a second undoes it; both need the token;
  `minicloud deploy` from a directory and from stdin, and `--into`;
  `deploy/smoke.sh` (CI, Linux) checks that a release carrying a newer copy
  of an installed app says so and does not deploy it, and says nothing once
  it has been asked to (#37); that the release packages `admin` alone,
  `install.sh` deploys it by default and refuses an example asked for as a
  bundled app, pointing at `examples/`, leaves an app that is not the
  release's alone (the examples, deployed from `examples/`) and refuses to
  switch while one does not check;
- through the `host` module: the list shows every app's state, grant and
  limits; a disabled app answers 503 while one of its requests in flight
  finishes, and enabled again has its store; taking a needed capability
  away refuses that app alone, and granting it back restores it; eight kinds
  of wrong change are refused with their reasons and change nothing; `admin`
  granted to another app is refused at load and by `check`; the admin app
  cannot disable itself or drop `admin`, and the admin listener can; the
  changes survive a restart and a release that replaces `apps/`; the history
  records who, when and what; an app with a hostname is reached by it alone,
  as that hostname's origin, and a hostname reaches one app;
- the secret store (`secrets.rs`, and `secrets.rs`'s and `config.rs`'s unit
  tests): the file survives reopening, is mode 0600, leaves no temporary
  file, and a write that fails leaves the old file and value; names and
  values are held to their rules and a broken file is refused, none of it
  quoting a value; an app whose stored secret is missing is refused naming
  it; setting it reloads the app, and `auth.check` and a `[fetch.headers]`
  header sent to a test upstream use the new value, replacing it the old
  value no longer works, and a restart finds it; a reload that fails keeps
  the value stored and the serving version; deleting a secret an app uses is
  409 unless forced, and forced refuses the app; the admin app sets,
  replaces and deletes from its page (a cross-site POST refused, a delete
  needing *confirm*, and *force* when used); `minicloud secret set|list|delete`;
  `check --data` and `test` without the secret (on a placeholder, said); and
  the value is in none of the listener's answers, the admin pages, the stats,
  the operations views, the logs, the change history or `overrides.json`;
- schedules (`triggers.rs`, and `cron.rs`'s and `config.rs`'s unit tests):
  a schedule runs the app in the minute it names and in no other, a minute
  fires once however often the clock is asked, and the run is counted in
  `served` and `ok` like any other; a client that sends
  `x-minicloud-trigger` itself is still refused by an app that only answers
  the clock; a tick whose last run has not finished is let go by and
  counted, and the schedule runs again once it has; a failing tick is
  counted and named in the app's log; a `cron` that does not parse refuses
  that app alone; the schedules and when they are next due are in the
  stats. `cron.rs` holds the parser and the calendar to 32 cases of their
  own — the epoch, leap days, negative seconds, the day-or-weekday rule,
  the aliases, and every way a field can be wrong;
- a WebSocket a run holds (`socket.rs`, and `socket.rs`'s unit tests): the
  run speaks first and answers what it hears more than once, a ping is the
  host's to answer, and the run ends the socket when it decides to; a
  socket being held costs no worker — another app answers throughout on a
  single-worker host; a client that goes away is told to the run; an app
  may refuse the upgrade like any other request; a call on a run with no
  socket is the app's `Err`; an app not granted `socket` is refused at
  load. The accept key is held to RFC 6455's own example and the hash to
  the digests everyone checks a SHA-1 against;
- an answer a run writes as it goes (`stream.rs`): the head is readable
  before the run ends and each chunk as it is sent, the response the
  handler returns afterwards is nowhere on the connection, and there are no
  run headers; a stream that waits between its chunks holds no worker —
  another app answers throughout on a single-worker host; a chunk without
  an open, a second open and a chunk past `max_response_bytes` are each the
  app's `Err` while the rest of the run goes on; a client that goes away is
  told to the run, which ends; an app not granted `stream` is refused at
  load;
- one run at a time for one key (`keyed.rs`, and `keyed.rs`'s unit tests):
  five requests about one key that each read, wait and write back end with
  the count at five, where without a turn they would all write one; another
  key is not waiting for this one and a request with no key is not keyed;
  more waiting for one key than `max_waiting` is 429 with `Retry-After`
  while another app answers; an alarm wakes the app once at its key, is one
  per key, can be taken back, survives a restart, and is the host's to ring;
  four ways of writing `[keyed]` wrong refuse that app alone. The store's
  own tests hold the upsert, the taking in time order and the limit, and
  the lock's hold that one turn waits and another key does not;
- queues (`queue.rs`, and `queue.rs`'s 11 unit tests): a message outlives
  the request that sent it, is delivered as a run, is on disk and is off
  the queue once taken; a client cannot deliver one itself; a delivery that
  is refused is tried again and then buried with its reason in the log,
  while the app and its neighbour go on; a queue the app does not declare
  is an `Err` that says which it has; a queue past `max_depth` refuses the
  send; the messages survive a restart; five ways of writing `[[queues]]`
  wrong refuse that app alone. The store's own tests hold the lease, the
  retry, the burying and reviving, the depths and the pruning;
- `defer` (`defer.rs`): the client is answered at once and the work is done
  after it, as a run of its own counted in `served`; a client that sends
  `x-minicloud-deferred` itself is still refused by an app that only
  answers the host; more outstanding work than `max_in_flight` is the app's
  `Err` and there is room again once a run ends; a deferred run that fails
  is counted and named in the log while the app and its neighbour go on; a
  path that is not a path is refused without a run; an app not granted
  `defer` is refused at load;
- metrics (`metrics.rs`, and `histogram.rs`'s and `series.rs`'s unit
  tests): the time every request took is a distribution with its quantiles
  in order, and an app that answered nothing has none; `/_host/metrics` is
  the exposition format, with the counters, the per-kind errors, the
  per-reason rejections and the histogram, and every line is a comment or a
  name and a value; a minute is kept as what changed *over* it and not as a
  running total, the first sample keeps nothing, and a quiet minute is kept
  as a quiet minute; the operations page draws the last hour. The
  histogram's own tests hold the bucketing, the interpolation and the
  difference; the series' hold the upsert, the roll-up (summed counts,
  worst percentiles), the pruning and that sweeping twice does not double;
- `[rate]` (`rate.rs`, and `rate.rs`'s unit tests): a caller past its
  allowance is 429 with a `Retry-After` and the app does not run, while its
  neighbour answers; a burst larger than the rate is allowed and then is
  not; a header may be the caller instead of the address, and two keys have
  two allowances; `app` is one bucket for everybody; an app's files are
  behind it too; five ways of writing it wrong refuse that app alone; and
  the buckets refill, are swept when there are too many, and are forgotten
  with the app;
- ACME (`acme.rs`, and `acme.rs`'s unit tests): the plain listener answers
  a challenge the host is offering and redirects everything else to
  `https://` keeping the path and the query; a token that is not being
  offered is not an answer, and one that was withdrawn stops being one; a
  host with no certificate yet starts; one with a certificate already on
  disk serves it and makes no account. **The exchange with an authority is
  not covered**: it needs an authority, a public name and port 80.
  `--acme-staging` is what a first deployment finds out with;
- the front door (`ingress.rs`, and `tls.rs`'s unit tests): the host
  terminates TLS itself and the answer comes back over HTTP/2, a plain
  request to that port is not served and does not take the host down; a
  certificate renewed on disk is read again without a restart and a half
  renewal keeps the pair that is serving; a host whose key does not read
  does not start; a body over a kilobyte is gzipped for a client that takes
  it and not for one that says `gzip;q=0`, a small one is left alone; a
  `*.name` reaches its app one label below and never two, and the name
  itself is not the wildcard;
- following a log (`logs.rs`): the answer is server-sent events, it opens
  with the lines the ring already has and says how many, a line written
  while the connection is open reaches it, an app that is not there is 404,
  and the admin listener's own route answers the lines and follows them to
  the token and to nobody else;
- static files (`assets.rs`, and `assets.rs`'s and `config.rs`'s unit
  tests): a file is answered with the type its extension says and the
  host's headers while the app's `served` stays at zero; **bytes that are
  not UTF-8 come back byte for byte**, which is the whole point of the
  feature; an `etag` answers `304` with no body and a changed file answers
  `200`; a path naming no file and a `POST` to a file's path reach the app;
  a changed file is a new version and an update switches to it; an
  `assets/` over `max_bytes`, or with a symbolic link in it, refuses that
  app alone; the admin app's own `favicon.ico`, `favicon.svg` and
  `style.css` are answered as they ship, its pages link the sheet rather
  than inlining one, and its policy no longer allows an inline style;
- the operations page escapes markup an app logs, and shows errors and KV
  usage against the quota; an app's log reaches `<data>/<app>/log.txt`;
- Cloudflare Access (`access.rs`, against a JWKS the test serves with RSA
  keys it generates): a valid token gets into the admin UI (header or
  cookie) and the history records its verified email, not the unverified
  `Cf-Access-Authenticated-User-Email`; no token, a malformed one, a forged
  signature, another application's `aud`, another team's `iss`, an expired
  one, one not valid yet, one with no `exp`, one with no email (a service
  token) and an email not allowed are each refused with 403 and no prompt,
  and change nothing; the admin secret alone is refused while Access is on;
  a token signed by a rotated-in key fetches the keys again and gets in, the
  keys are then held, and an unknown key is refused with its fetches rate
  limited; with the JWKS down every token is refused and the failure logged,
  and back up the next one gets in; `fallback = "token"` lets the secret in,
  recorded as `token`; with Access off the secret and its Basic prompt are
  as before; the webhook lab's pages need the lab's own application's token
  while its receive URLs need nothing and fetch no keys.

`COVE_HOST_TEST_BACKEND=vm cargo t` runs the same suite on the encoded VM,
as CI does on its second pass.

## Issue #1's completion criteria

| criterion | where it is shown |
| --- | --- |
| two or more independent Cove apps run at once | `host.rs::two_or_more_apps_are_served_concurrently`; the README's [quick start](../README.md#quick-start) serves six |
| per-app KV isolation, and persistence across a restart | `services.rs::an_apps_keys_are_its_own`, `::the_store_survives_a_restart`; manual: store a note in `notes`, restart, `GET /notes/todo` |
| a light app keeps answering beside a CPU-heavy and an I/O app | `host.rs::hello_answers_while_crunch_saturates_the_workers_and_slow_is_parked`; measured: [measurements.md](measurements.md), `hello`'s p99 in the mix |
| budget overrun, overload and an invalid update stop no other app | `host.rs::a_budget_overrun_ends_that_request_only`, `::overload_is_rejected_explicitly_and_other_apps_still_answer`, `updates.rs::a_failed_update_keeps_the_current_version_and_says_why` (another app answering throughout five kinds of refused update, `limits.fuel` among them) |
| requests on the old version complete during an update | `updates.rs::in_flight_requests_finish_on_the_old_version_and_new_ones_get_the_new` (parked, running, yielded and queued, each answered by v1; the next by v2; v1 and its program dropped after) |
| tests, and procedures for starting, updating and checking performance | this file; the README's [Quick start](../README.md#quick-start) and [Deploying an app](../README.md#deploying-an-app); [measurements.md](measurements.md) |

## Issue #17's completion criteria

| criterion | where it is shown |
| --- | --- |
| an app enabled and disabled from the browser; a disabled app does not answer; enabled again, its KV data is there | `admin.rs::the_pages_enable_disable_configure_and_reset`, `::a_disabled_app_answers_503_finishes_what_it_had_and_keeps_its_data` (a parked request finishes; the store survives) |
| a change of grant or budget is applied only when it checks; a wrong one keeps the current config and says why | `admin.rs::a_change_that_is_wrong_is_refused_with_the_reason_and_changes_nothing` (seven kinds), `::the_pages_enable_disable_configure_and_reset` (the form's problems and the host's reason, on the page) |
| an app whose needed capability is taken away is refused, and the others answer | `admin.rs::taking_away_a_needed_capability_refuses_that_app_and_no_other` |
| `admin` cannot be granted to any app but the admin app | `admin.rs::only_the_admin_app_may_be_granted_admin` (at load, by `check`, and by a change); the admin app cannot disable itself or drop it: `::the_admin_app_cannot_disable_itself_or_drop_admin_but_the_listener_can` |
| changes survive a restart; unauthenticated and cross-site operations are refused | `admin.rs::changes_survive_a_restart_and_a_release_that_replaces_the_apps`, `::the_admin_app_needs_its_secret`, `::a_cross_site_form_changes_nothing`; the history: `::the_history_says_who_when_and_what`; escaping: `::the_pages_escape_what_they_show`; the hostname: `::the_admin_app_is_reached_by_its_hostname_only`, `::an_app_with_a_hostname_is_reached_by_it_and_by_nothing_else` |
| tests, and the Cloudflare Access steps | this file; [deploy/cloudflare.md](../deploy/cloudflare.md) §4, [apps/admin/README.md](../apps/admin/README.md) |
