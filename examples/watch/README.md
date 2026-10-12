# watch

A watch on the things you run: every minute it asks each target whether it
is there, keeps the answers, shows them, and tells somebody when one
changes.

It is the example for what the host gained after 0.5 — `[[triggers]]`,
`sql`, `[[queues]]`, `defer`, `stream`, `socket`, `[keyed]`, `[rate]` and
`assets/` — in the shape an app doing a real job uses them, rather than one
path per feature.

```console
$ WATCH_ADMIN_TOKEN=dev minicloud serve --apps examples
$ curl -u a:dev -X POST --data 'id=hello&url=http://127.0.0.1:8080/hello/' \
    http://127.0.0.1:8080/watch/targets
watching hello
$ open http://127.0.0.1:8080/watch/
```

## What it does, and what each thing is for

| | |
| --- | --- |
| **`[[triggers]] cron = "* * * * *"`** | `/cron/check` asks every target. Nothing outside the host runs this: no crontab, no systemd timer, no "did the deploy keep the cron job" |
| **`sql`** | two tables — the targets, and every check ever made. The dashboard's question is *"how much of the last day was good, by target"*, which is one query with an index and an aggregate, and which `kv` could not answer at all |
| **`[keyed] by = "query:room"`** | every path about one target carries `?target=`, so a check the clock started and one somebody asked for cannot both write that target's history. One run at a time for one key, and the rest wait |
| **`keyed.alarm`** | a target that is **down** is asked again in half a minute rather than at the next minute, and stops being asked that way the moment it comes back |
| **`[[queues]]`** | a target that *changed* is a message, not a `fetch` in the middle of a check: whoever is told may be down too. The host keeps trying — four times, twenty seconds apart — and what nobody could be told is a dead letter rather than a loss (`minicloud queues watch`) |
| **`defer`** | adding a target answers `201` at once and finds out whether it is up in a moment; the minute's run checks and lets the tidying of old rows happen after it has answered |
| **`stream`** | `GET /events` is the dashboard as server-sent events: the run writes as it goes and parks between lines, so a browser left open costs a parked run and no worker |
| **`socket`** | `GET /live` is the same over a WebSocket, which also **takes commands** — `status`, `check <target>`. One connection, both directions, no page to refresh |
| **`assets/`** | `style.css` and `favicon.svg`, answered by the host before the app runs. The pages carry no inline style, so their policy is `style-src 'self'` |
| **`[rate]`** | a dashboard anyone may look at is a dashboard somebody may hammer: 120 requests a minute from one address |
| **`[fetch] allow`** | **a monitor can only watch what its `app.toml` lets it reach.** Adding a target means adding its origin to the allowlist. That is the capability model doing its job, and it is the thing to know before using this |

## The paths

| path | |
| --- | --- |
| `GET /` | the dashboard |
| `GET /events` | the same, as server-sent events |
| `GET /live` | the same over a WebSocket; send `status` or `check <target>` |
| `GET /history?target=` | one target's last hundred checks |
| `POST /targets` | `id`, `url`, and an optional `notify` to POST a change to |
| `POST /targets/remove?target=` | drop one |
| `POST /check?target=` | check one now |

Reading is open — a dashboard is meant to be looked at. Changing what is
watched needs the admin: Cloudflare Access where it is on, and the
`WATCH_ADMIN_TOKEN` secret where it is not (`Authorization: Bearer …`, or a
browser's login prompt).

The host's own runs — the minute's, the alarm's, the queue's, a deferred
check — are none of those: they carry a header the host sets and strips
from anything that comes off a connection, and the app lets them through on
that.

## What it does not do

**It has no history beyond the last week.** Checks older than that are
dropped by the deferred `/prune`; what a week of minutes is worth keeping is
the question, and a watch that kept everything would answer it by filling
the disk.

**It tells one place per target.** Somewhere to POST a change to, not a
fan-out: more than one is a list, and a list is a table, and that is a
bigger app than an example should be.
