//! The `host` module, capability `admin`: what the admin app (`apps/admin`)
//! sees of the host, and the only way it changes anything.
//!
//! The module is `host` rather than `admin` because the admin app's own main
//! module is `admin` — an app's main module is named after its directory —
//! and Cove refuses a package module that shadows a host module
//! (`cove::resolve::module_shadows_host`). What is granted, and what every
//! listing shows, is the capability: `admin`.
//!
//! | operation | answers |
//! | --- | --- |
//! | `host.apps()` | `Array<host.App>`: every app the host has, in load order — its state (`serving`, `disabled`, `refused`, `removed`) and why, version, tier, hostnames, what its entry requires and what it is granted (and which of those grants the admin changed), its fetch allowlist, its limits, its counters and its recent errors |
//! | `host.capabilities()` | `Array<String>`: every capability the host's modules declare, the ones a grant may name |
//! | `host.selfVersion()` | `String`: the version of the admin app itself, the same string its answers carry as `x-cove-app-version`; `""` when the host cannot say |
//! | `host.code(app, limit)` | `Array<host.CodeFile>`: the files of the version the app is running ([`crate::code`]) — `app.toml`, then its `.cove` files, then its `assets/` — at most `limit` files (0–[`crate::code::CODE_FILES`]) as a tree, each row saying its depth, its size and how the one on disk compares, and no text |
//! | `host.codeFile(app, file)` | `Result<host.CodeFile, Error>`: one of those files with its text, cut past [`crate::code::READ_TEXT_BYTES`], and the pieces its text is coloured by ([`crate::highlight`]); an `Err` for a name the deploy has no file under |
//! | `host.history(limit)` | `Array<host.Change>`: the newest `limit` changes, newest first |
//! | `host.kv(app, prefix, after, limit)` | `Array<host.KvEntry>`: keys of the app's store starting with `prefix` and greater than `after` (`""` for the first page), ascending, at most `limit` (0–500); each value cut to [`crate::kv::PEEK_VALUE_CHARS`] — read only, and empty for an app with no store |
//! | `host.kvValue(app, key)` | `Result<host.KvEntry, Error>`: one key, its value cut past [`crate::kv::READ_VALUE_CHARS`]; an `Err` when the app has no such key |
//! | `host.sqlTables(app)` | `Array<host.SqlTable>`: the tables of the app's SQL store and how many rows each holds — read only, and empty for an app with no store |
//! | `host.sqlQuery(app, statement)` | `Result<host.SqlAnswer, Error>`: one statement that only reads, against that store, under the window's own limits ([`crate::sql::PEEK_ROWS`] and the rest); an `Err` for a statement that writes, one the store refuses, or an app with no store |
//! | `host.queues(app)` | `Array<host.Queue>`: each queue the app declares (`[[queues]]`) with what is due, what is held and how many dead letters it has, and the settings the host delivers it by; after them, any queue the store holds messages for that the app no longer declares |
//! | `host.queueMessages(app, limit)` | `Array<host.QueueMessage>`: the messages still on those queues, what is due first, at most `limit` (0–[`crate::queue::PEEK_MESSAGES`]) — a read that leases nothing |
//! | `host.queueDead(app, limit)` | `Array<host.QueueMessage>`: the dead letters, newest buried first, each with `why`; at most `limit` as above |
//! | `host.queueMessage(app, id)` | `Result<host.QueueMessage, Error>`: one of them whole, its body cut past [`crate::queue::READ_BODY_CHARS`]; an `Err` for an id no queue of the app holds |
//! | `host.series(app, over)` | `Array<host.Point>`: what the app did over `over` — `"hour"`, `"day"` or `"month"` — as the host's history keeps it ([`crate::series`]), oldest first: a row a minute for the first two and a row an hour for the last, because the store keeps the two at those granularities and [`crate::series::window`] is the one place that says which. The same rows `GET /_host/series` answers, as values; an `over` that is none of the three is an hour, as there. Empty — not an `Err` — for a host that keeps no history (no `--data`, or `minicloud test`), so a page that draws a graph shows "nothing yet" before there is one; never more than [`SERIES_ROWS`] rows, the newest if there would be |
//! | `host.logs(app, limit)` |`Array<host.LogLine>`: the last `limit` (0–[`crate::logs::LOG_LINES`]) lines of the app's log, oldest first |
//! | `host.allLogs(limit)` | `host.AllLogs`: every app's log as one sequence — the last `limit` lines of it, oldest first, each saying which app it came from — and `apps`, every app whose ring holds a line. The host merges the rings, and "the last `limit`" is the last `limit` of its whole log, so one app that is talking fills it ([`crate::manage`]) |
//! | `host.setEnabled(app, enabled, who)` | `Result<String, Error>`: routes to the app again, or stops; nothing is reloaded |
//! | `host.configure(app, settings, who)` | `Result<String, Error>`: the app's grant, allowlist and limits become `settings`, if the app loads with them |
//! | `host.reset(app, who)` | `Result<String, Error>`: drops the admin's changes, back to `app.toml` |
//! | `host.secrets()` | `Array<host.Secret>`: every secret stored or used — `name`, `set`, `updatedMs` (0 when unset), `apps` that use it — never a value |
//! | `host.setSecret(name, value, who)` | `Result<String, Error>`: stores the value ([`crate::secrets`]) and reloads the apps that use it; the message says what each reload came to |
//! | `host.deleteSecret(name, force, who)` | `Result<String, Error>`: refused while an app uses it, unless `force` |
//!
//! The five that change something are answered **pending**: the run parks
//! while the app is reloaded on a blocking thread — parsed, checked,
//! admitted, lowered, prepared and compiled, as an update is — and holds no
//! worker. What they may do, and the persistence, are
//! [`crate::manage`]'s; `who` is what the admin app says of its user, and
//! goes into the change history as it is.
//!
//! The capability is `admin`, and only the app named `admin` may be granted
//! it ([`crate::config::ADMIN_APP`]); the module's instance in any other
//! app has no handle on the host at all, so even a grant that slipped
//! through would reach nothing.

use std::collections::BTreeSet;
use std::sync::Arc;

use cove_runtime::{
    Effect, FieldSchema, HostAnswer, HostApi, HostType, ModuleSchema, OperationSchema, Reentry,
    RuntimeError, Transfer, TypeSchema, Value,
};

use crate::code::CodeFile;
use crate::config::{LimitsFile, ADMIN_CAPABILITY};
use crate::highlight::Piece;
use crate::hosts::{AppContext, HostModule, PendingWork};
use crate::logs::Line;
use crate::manage::{AllLogs, AppInfo, AppLine, KvRow, QueueInfo, SecretInfo, Settings, Via};
use crate::overrides::{Change, Control};
use crate::queue::Peeked;
use crate::series::Sample;
use crate::sql::{Answer as SqlAnswer, Table as SqlTable};

const STR: HostType = HostType::String;
const INT: HostType = HostType::Int;
const STRINGS: HostType = HostType::Array(&HostType::String);
/// The most rows one `host.series` answers. The windows hold 1440 minutes
/// and 720 hours at most, but that is what the store *should* hold: a sweep
/// that has not run, or a retention set longer, would otherwise make one call
/// as large as the table. Past it the newest rows are kept, because a graph
/// that loses a stretch loses the end it is looking at.
pub const SERIES_ROWS: usize = 2000;

const CHANGED: HostType = HostType::Result(&HostType::String, &HostType::Error);

const fn op(
    name: &'static str,
    params: &'static [HostType],
    result: HostType,
    effect: Effect,
) -> OperationSchema {
    OperationSchema {
        name,
        params,
        variadic: false,
        result,
        capability: ADMIN_CAPABILITY,
        effect,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

const fn field(name: &'static str, ty: HostType) -> FieldSchema {
    FieldSchema { name, ty }
}

const LIMITS_FIELDS: &[FieldSchema] = &[
    field("maxHostCalls", INT),
    field("deadlineMs", INT),
    field("maxHeapWords", INT),
    field("maxInFlight", INT),
    field("maxQueued", INT),
    field("maxRequestBytes", INT),
    field("maxResponseBytes", INT),
];

/// The `host` module, capability `admin`.
pub const ADMIN: ModuleSchema = ModuleSchema {
    name: "host",
    capability: ADMIN_CAPABILITY,
    operations: &[
        op(
            "apps",
            &[],
            HostType::Array(&HostType::Named("host.App")),
            Effect::Read,
        ),
        op("capabilities", &[], STRINGS, Effect::Read),
        // The app's own version, so a page can put it in the URL of a file
        // the host serves (`assets/`) and a deploy change the URL. It takes
        // no app name because only `admin` is granted `admin`, and it is a
        // `String` and not a `host.App` because the whole of one costs every
        // app's counters and a `stat` of its store — too much for a call
        // every page makes.
        op("selfVersion", &[], STR, Effect::Read),
        op(
            "code",
            &[STR, INT],
            HostType::Array(&HostType::Named("host.CodeFile")),
            Effect::Read,
        ),
        op(
            "codeDisk",
            &[STR],
            HostType::Named("host.CodeDisk"),
            Effect::Read,
        ),
        op(
            "codeFile",
            &[STR, STR],
            HostType::Result(&HostType::Named("host.CodeFile"), &HostType::Error),
            Effect::Read,
        ),
        op(
            "history",
            &[INT],
            HostType::Array(&HostType::Named("host.Change")),
            Effect::Read,
        ),
        op(
            "kv",
            &[STR, STR, STR, INT],
            HostType::Array(&HostType::Named("host.KvEntry")),
            Effect::Read,
        ),
        op(
            "kvValue",
            &[STR, STR],
            HostType::Result(&HostType::Named("host.KvEntry"), &HostType::Error),
            Effect::Read,
        ),
        op(
            "logs",
            &[STR, INT],
            HostType::Array(&HostType::Named("host.LogLine")),
            Effect::Read,
        ),
        op(
            "allLogs",
            &[INT],
            HostType::Named("host.AllLogs"),
            Effect::Read,
        ),
        op(
            "setEnabled",
            &[STR, HostType::Bool, STR],
            CHANGED,
            Effect::IrreversibleWrite,
        ),
        op(
            "configure",
            &[STR, HostType::Named("host.Settings"), STR],
            CHANGED,
            Effect::IrreversibleWrite,
        ),
        op("reset", &[STR, STR], CHANGED, Effect::IrreversibleWrite),
        op(
            "secrets",
            &[],
            HostType::Array(&HostType::Named("host.Secret")),
            Effect::Read,
        ),
        op(
            "setSecret",
            &[STR, STR, STR],
            CHANGED,
            Effect::IrreversibleWrite,
        ),
        op(
            "deleteSecret",
            &[STR, HostType::Bool, STR],
            CHANGED,
            Effect::IrreversibleWrite,
        ),
        op(
            "sqlTables",
            &[STR],
            HostType::Array(&HostType::Named("host.SqlTable")),
            Effect::Read,
        ),
        op(
            "sqlQuery",
            &[STR, STR],
            HostType::Result(&HostType::Named("host.SqlAnswer"), &HostType::Error),
            Effect::Read,
        ),
        op(
            "series",
            &[STR, STR],
            HostType::Array(&HostType::Named("host.Point")),
            Effect::Read,
        ),
        op(
            "queues",
            &[STR],
            HostType::Array(&HostType::Named("host.Queue")),
            Effect::Read,
        ),
        op(
            "queueMessages",
            &[STR, INT],
            HostType::Array(&HostType::Named("host.QueueMessage")),
            Effect::Read,
        ),
        op(
            "queueDead",
            &[STR, INT],
            HostType::Array(&HostType::Named("host.QueueMessage")),
            Effect::Read,
        ),
        op(
            "queueMessage",
            &[STR, INT],
            HostType::Result(&HostType::Named("host.QueueMessage"), &HostType::Error),
            Effect::Read,
        ),
    ],
    types: &[
        // `maxHeapWords` 0 is "no limit but the runtime's"; every other
        // value is the limit itself.
        TypeSchema {
            name: "Limits",
            cases: &[],
            fields: LIMITS_FIELDS,
        },
        TypeSchema {
            name: "Failure",
            cases: &[],
            fields: &[
                field("atMs", INT),
                field("kind", STR),
                field("status", INT),
                field("version", STR),
                field("message", STR),
            ],
        },
        TypeSchema {
            name: "App",
            cases: &[],
            fields: &[
                field("name", STR),
                // `serving`, `disabled`, `refused` or `removed`.
                field("state", STR),
                // Why it was refused; "" otherwise.
                field("reason", STR),
                field("version", STR),
                // `native`, `vm`, or "" for an app not loaded.
                field("tier", STR),
                field("entry", STR),
                field("hosts", STRINGS),
                field("required", STRINGS),
                // Whether `required` is a lower bound.
                field("requiredOpen", HostType::Bool),
                field("granted", STRINGS),
                // What the admin added to and removed from `app.toml`'s grant.
                field("grantAdded", STRINGS),
                field("grantRemoved", STRINGS),
                field("fetchAllow", STRINGS),
                field("limits", HostType::Named("host.Limits")),
                // The `[limits]` keys the admin set.
                field("limitsChanged", STRINGS),
                // Whether this is the admin app itself.
                field("isAdmin", HostType::Bool),
                field("served", INT),
                field("ok", INT),
                field("errors", INT),
                field("rejected", INT),
                field("inFlight", INT),
                field("queued", INT),
                // -1 for an app with no store open.
                field("kvKeys", INT),
                field("kvBytes", INT),
                // `[kv]`'s quotas: what that store may hold. 0 for an app
                // with no version loaded.
                field("kvMaxKeys", INT),
                field("kvMaxBytes", INT),
                // The SQL store on disk; -1 for an app that has none, as
                // `kvKeys` is -1 for an app with no store open.
                field("sqlBytes", INT),
                field("sqlMaxBytes", INT),
                // Why `[access]` is written and off; "" where Access is on
                // and "" where the app asks for none.
                field("accessOff", STR),
                field(
                    "recentErrors",
                    HostType::Array(&HostType::Named("host.Failure")),
                ),
            ],
        },
        TypeSchema {
            name: "Settings",
            cases: &[],
            fields: &[
                field("grant", STRINGS),
                field("fetchAllow", STRINGS),
                field("limits", HostType::Named("host.Limits")),
            ],
        },
        TypeSchema {
            name: "Secret",
            cases: &[],
            fields: &[
                field("name", STR),
                // Whether the store has it.
                field("set", HostType::Bool),
                // When it was last set; 0 when it is not.
                field("updatedMs", INT),
                // The apps whose app.toml takes a secret from it.
                field("apps", STRINGS),
            ],
        },
        TypeSchema {
            name: "KvEntry",
            cases: &[],
            fields: &[
                field("key", STR),
                field("value", STR),
                // The whole value's length in bytes.
                field("bytes", INT),
                // Whether `value` is shorter than the whole.
                field("cut", HostType::Bool),
            ],
        },
        // One run of a file's text and what it is ([`crate::highlight`]).
        // The kind is a `String` because a `TypeSchema`'s `cases` carry no
        // payload — the same reason `CodeFile.kind` is one — and because a
        // page that meets a kind it has no colour for shows the text, which
        // is what lets the host grow one.
        //
        // These are pieces and not markup on purpose. The admin app puts
        // every value it shows through `escapeHtml`, and a file's own text
        // is the value most likely to hold `<img src=y>`; a host that
        // returned `<span>`s would split the escaping between the two
        // sides of the boundary and leave the page unable to check it.
        TypeSchema {
            name: "CodePiece",
            cases: &[],
            fields: &[
                // `plain`, `keyword`, `type`, `ident`, `string`, `number`,
                // `comment`, `doc` or `punct`.
                field("kind", STR),
                // The file's own bytes, escaped by nothing.
                field("text", STR),
            ],
        },
        // What the app's directory holds against the version that is
        // running ([`crate::code::disk_check`]), as a count.
        //
        // `looked` is here because the comparison reads every file and a
        // dashboard cannot afford that on a large deploy: a page told
        // `looked: false` says it did not look, which is the one answer a
        // page must never guess at.
        TypeSchema {
            name: "CodeDisk",
            cases: &[],
            fields: &[
                field("looked", HostType::Bool),
                field("differing", INT),
                field("bytes", INT),
                // The size past which nothing is read.
                field("budget", INT),
            ],
        },
        // One row of what an app's deploy carries ([`crate::code`]).
        // Whether the file is text is `isText` beside `text` rather than
        // two cases of one type: a `TypeSchema`'s `cases` are names and
        // carry no payload, so a schema cannot say "text, or these bytes"
        // — the same reason `sql` answers a cell as an `Option<String>`.
        TypeSchema {
            name: "CodeFile",
            cases: &[],
            fields: &[
                // Within the app: `app.toml`, `admin.cove`,
                // `text/text.cove`, `assets/style.css`.
                field("name", STR),
                // The last part of the name, which is what a row at its
                // depth shows.
                field("label", STR),
                // How many directories deep the name is. The host decided
                // the order and the nesting, so the page indents by this
                // and decides nothing.
                field("depth", INT),
                // `config` for `app.toml`, `cove`, `asset`, or `dir` for a
                // directory row.
                field("kind", STR),
                // The whole file's length in bytes; for a directory, what
                // the files under it come to.
                field("bytes", INT),
                // The text; "" in a listing, which carries none, and ""
                // for a file that is not UTF-8.
                field("text", STR),
                // What the text is coloured by, which concatenated is the
                // text itself. Empty in a listing, and empty wherever
                // `plain` says why there are none.
                field(
                    "pieces",
                    HostType::Array(&HostType::Named("host.CodePiece")),
                ),
                // Why the text is shown plain — `cut`, `dense` or
                // `unknown` — and "" where the pieces are the text.
                field("plain", STR),
                // Whether the file is UTF-8, so `text` is the file. A PNG
                // or a font in `assets/` is not, and is to be looked at
                // where the host serves it.
                field("isText", HostType::Bool),
                // Whether `text` is shorter than the whole.
                field("cut", HostType::Bool),
                // How the file of the same name in the app's directory
                // compares: `same`, `differs`, `missing` or `unread`; ""
                // for a directory.
                field("onDisk", STR),
            ],
        },
        // One table of an app's SQL store ([`crate::sql::peek_tables`]).
        TypeSchema {
            name: "SqlTable",
            cases: &[],
            fields: &[
                field("name", STR),
                // How many rows it holds; 0 where `counted` is false.
                field("rows", INT),
                // Whether the count was taken. A count is a statement under
                // the window's deadline, and a table too large to count
                // inside it is listed saying so rather than left out.
                field("counted", HostType::Bool),
            ],
        },
        // What one of the window's statements answered. The same shape as
        // `sql.Rows`, declared again here because `sql.`'s types are the
        // app-facing module's: the admin app is granted `admin` and not
        // `sql`, so it has no `sql.Rows` to be given one of.
        TypeSchema {
            name: "SqlAnswer",
            cases: &[],
            fields: &[
                field("columns", STRINGS),
                field("rows", HostType::Array(&HostType::Named("host.SqlRow"))),
            ],
        },
        // A cell is an `Option<String>` for the reason `sql` gives: a
        // `TypeSchema`'s `cases` carry no payload, so a value that is
        // sometimes a number and sometimes text cannot be declared, and
        // everything is its decimal or its text. `None` is `NULL`, which is
        // how `NULL` stays apart from the empty string.
        TypeSchema {
            name: "SqlRow",
            cases: &[],
            fields: &[field(
                "values",
                HostType::Array(&HostType::Option(&HostType::String)),
            )],
        },
        // One row of an app's history ([`crate::series`]): what it did over
        // one minute or one hour, as the increase over it and not a running
        // total. There is no `app` — the caller named it — and the columns
        // are the store's, so a graph can draw any of them.
        TypeSchema {
            name: "Point",
            cases: &[],
            fields: &[
                // The start of the minute or hour the row covers, in seconds
                // since the epoch. Seconds, unlike the `atMs` of a log line:
                // it is the store's own key and always a multiple of 60.
                field("atUnix", INT),
                field("served", INT),
                field("ok", INT),
                field("errors", INT),
                field("rejected", INT),
                field("workerMs", INT),
                // Microseconds, estimated from the request-time histogram
                // over the row's own span, so a quantile of a minute and not
                // of all time.
                field("p50Us", INT),
                field("p99Us", INT),
            ],
        },
        // One queue of an app ([`crate::queue`]), with its counts whole:
        // what the listing below shows is at most
        // [`crate::queue::PEEK_MESSAGES`] of them, so a page needs the
        // numbers from somewhere that is not the listing.
        TypeSchema {
            name: "Queue",
            cases: &[],
            fields: &[
                field("name", STR),
                // Where a message is delivered, below the app's prefix; ""
                // for a queue the app no longer declares.
                field("path", STR),
                // Messages the next delivery tick may take.
                field("due", INT),
                // Messages whose due time is still ahead: a delivery's
                // lease, or a failed delivery waiting for its retry. One
                // `due` column carries both, so the host cannot say which,
                // and neither does the page.
                field("held", INT),
                field("dead", INT),
                // Whether the app's `[[queues]]` still declares it. A queue
                // taken out of `app.toml` keeps what was on it and nothing
                // will deliver it; its settings below are 0.
                field("declared", HostType::Bool),
                field("maxDepth", INT),
                field("maxAttempts", INT),
                // Milliseconds, as every other duration this module
                // carries, though `[[queues]]` is written in seconds.
                field("retryMs", INT),
                field("leaseMs", INT),
                field("batch", INT),
                field("maxBodyBytes", INT),
            ],
        },
        // One message of a queue, waiting or buried.
        //
        // The two are one type, with `why` and `buriedMs` empty for a
        // message still on the queue: a `TypeSchema`'s `cases` are names and
        // carry no payload, so "waiting, or buried with this reason" cannot
        // be said as one type whichever way it is cut, and two types would
        // be eleven fields written twice for a difference of two. `state` says
        // which a message is, so nothing has to be inferred from an empty
        // `why`.
        TypeSchema {
            name: "QueueMessage",
            cases: &[],
            fields: &[
                field("id", INT),
                field("queue", STR),
                // The body, cut to [`crate::queue::PEEK_BODY_CHARS`].
                field("body", STR),
                // The whole body's length in bytes.
                field("bytes", INT),
                // Whether `body` is shorter than the whole.
                field("cut", HostType::Bool),
                // Deliveries tried, the one in flight counted: the host
                // counts a try when it leases the message.
                field("attempts", INT),
                // `due`, `held` or `dead`.
                field("state", STR),
                // When it may next be tried. For a dead letter, the due
                // time it had when it was buried, which is past.
                field("dueMs", INT),
                field("sentMs", INT),
                // Why it was buried; "" unless `state` is `dead`.
                field("why", STR),
                // When it was buried; 0 unless `state` is `dead`.
                field("buriedMs", INT),
            ],
        },
        // A line of a log, and the app that wrote it. The app is on the line
        // rather than only on the call that asked for it, because a line of
        // the host's whole log has to say where it came from and a line of
        // one app's already knows; one type is one `logTable` in the admin
        // app, with the column drawn or not.
        TypeSchema {
            name: "LogLine",
            cases: &[],
            fields: &[
                field("app", STR),
                field("atMs", INT),
                field("level", STR),
                field("text", STR),
            ],
        },
        // The answer to `allLogs`: who has a log, and the lines. `apps` is
        // not read off `lines` — an app whose last line is older than the
        // window still has a log to offer — and the two come from one call
        // so that they are of the same instant.
        TypeSchema {
            name: "AllLogs",
            cases: &[],
            fields: &[
                field("apps", STRINGS),
                field("lines", HostType::Array(&HostType::Named("host.LogLine"))),
            ],
        },
        TypeSchema {
            name: "Change",
            cases: &[],
            fields: &[
                field("atMs", INT),
                field("who", STR),
                field("app", STR),
                field("action", STR),
                field("detail", STR),
                field("outcome", STR),
            ],
        },
    ],
    resources: &[],
};

pub(crate) struct AdminModule;

impl HostModule for AdminModule {
    fn schema(&self) -> ModuleSchema {
        ADMIN
    }

    fn instantiate(&self, app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        Ok(Box::new(AdminHost {
            control: app.control.clone(),
            io: app.io.clone(),
        }))
    }
}

struct AdminHost {
    /// The host; `None` in every app but the admin app, and in `minicloud
    /// test`.
    control: Option<Arc<Control>>,
    io: tokio::runtime::Handle,
}

fn strings(items: impl IntoIterator<Item = impl Into<String>>) -> Value {
    Value::array(items.into_iter().map(|item| Value::string(item.into())))
}

fn int(value: impl TryInto<i64>) -> Value {
    Value::int(value.try_into().unwrap_or(i64::MAX))
}

fn limits_value(limits: &LimitsFile) -> Value {
    let deadline_ms = limits
        .deadline
        .as_deref()
        .and_then(|text| crate::config::parse_duration(text).ok())
        .map_or(0, |deadline| deadline.as_millis() as i64);
    Value::structure(
        "host.Limits",
        vec![
            ("maxHostCalls", int(limits.max_host_calls.unwrap_or(0))),
            ("deadlineMs", Value::int(deadline_ms)),
            ("maxHeapWords", int(limits.max_heap_words.unwrap_or(0))),
            ("maxInFlight", int(limits.max_in_flight.unwrap_or(0))),
            ("maxQueued", int(limits.max_queued.unwrap_or(0))),
            (
                "maxRequestBytes",
                int(limits.max_request_bytes.unwrap_or(0)),
            ),
            (
                "maxResponseBytes",
                int(limits.max_response_bytes.unwrap_or(0)),
            ),
        ],
    )
}

fn app_value(info: AppInfo) -> Value {
    Value::structure(
        "host.App",
        vec![
            ("name", Value::string(info.name)),
            ("state", Value::string(info.state)),
            ("reason", Value::string(info.reason)),
            ("version", Value::string(info.version)),
            ("tier", Value::string(info.tier)),
            ("entry", Value::string(info.entry)),
            ("hosts", strings(info.hosts)),
            ("required", strings(info.required)),
            ("requiredOpen", Value::bool(info.required_open)),
            ("granted", strings(info.granted)),
            ("grantAdded", strings(info.grant_added)),
            ("grantRemoved", strings(info.grant_removed)),
            ("fetchAllow", strings(info.fetch_allow)),
            ("limits", limits_value(&info.limits)),
            ("limitsChanged", strings(info.limits_changed)),
            ("isAdmin", Value::bool(info.is_admin)),
            ("served", int(info.served)),
            ("ok", int(info.ok)),
            ("errors", int(info.errors)),
            ("rejected", int(info.rejected)),
            ("inFlight", int(info.in_flight)),
            ("queued", int(info.queued)),
            (
                "kvKeys",
                info.kv.map_or(Value::int(-1), |(keys, _)| int(keys)),
            ),
            (
                "kvBytes",
                info.kv.map_or(Value::int(-1), |(_, bytes)| int(bytes)),
            ),
            ("kvMaxKeys", int(info.kv_max_keys)),
            ("kvMaxBytes", int(info.kv_max_bytes)),
            ("sqlBytes", info.sql_bytes.map_or(Value::int(-1), int)),
            ("sqlMaxBytes", int(info.sql_max_bytes)),
            ("accessOff", Value::string(info.access_off)),
            (
                "recentErrors",
                Value::array(info.recent_errors.into_iter().map(|failure| {
                    Value::structure(
                        "host.Failure",
                        vec![
                            ("atMs", int(failure.at_ms)),
                            ("kind", Value::string(failure.kind)),
                            ("status", int(failure.status)),
                            ("version", Value::string(failure.version)),
                            ("message", Value::string(failure.message)),
                        ],
                    )
                })),
            ),
        ],
    )
}

fn secret_value(info: SecretInfo) -> Value {
    Value::structure(
        "host.Secret",
        vec![
            ("name", Value::string(info.name)),
            ("set", Value::bool(info.set)),
            ("updatedMs", int(info.updated_ms.unwrap_or(0))),
            ("apps", strings(info.apps)),
        ],
    )
}

fn kv_value(row: KvRow) -> Value {
    Value::structure(
        "host.KvEntry",
        vec![
            ("key", Value::string(row.key)),
            ("value", Value::string(row.value)),
            ("bytes", int(row.bytes)),
            ("cut", Value::bool(row.cut)),
        ],
    )
}

fn code_piece_value(piece: Piece) -> Value {
    Value::structure(
        "host.CodePiece",
        vec![
            ("kind", Value::string(piece.kind)),
            ("text", Value::string(piece.text)),
        ],
    )
}

fn code_file_value(file: CodeFile) -> Value {
    Value::structure(
        "host.CodeFile",
        vec![
            ("name", Value::string(file.name)),
            ("label", Value::string(file.label)),
            ("depth", int(file.depth)),
            ("kind", Value::string(file.kind)),
            ("bytes", int(file.bytes)),
            ("text", Value::string(file.text)),
            (
                "pieces",
                Value::array(file.pieces.into_iter().map(code_piece_value)),
            ),
            ("plain", Value::string(file.plain)),
            ("isText", Value::bool(file.is_text)),
            ("cut", Value::bool(file.cut)),
            ("onDisk", Value::string(file.on_disk)),
        ],
    )
}

fn sql_table_value(table: SqlTable) -> Value {
    Value::structure(
        "host.SqlTable",
        vec![
            ("name", Value::string(table.name)),
            ("rows", Value::int(table.rows)),
            ("counted", Value::bool(table.counted)),
        ],
    )
}

/// What a statement answered. A cell is `Some` of its text or `None` for
/// `NULL`: the two are not the same cell, and a window that showed them
/// alike would hide the one thing a nullable column is about.
fn sql_answer_value(answer: SqlAnswer) -> Value {
    Value::structure(
        "host.SqlAnswer",
        vec![
            (
                "columns",
                Value::array(answer.columns.into_iter().map(Value::string)),
            ),
            (
                "rows",
                Value::array(answer.rows.into_iter().map(|row| {
                    Value::structure(
                        "host.SqlRow",
                        vec![(
                            "values",
                            Value::array(row.into_iter().map(|cell| match cell {
                                Some(text) => Value::some(Value::string(text)),
                                None => Value::none(),
                            })),
                        )],
                    )
                })),
            ),
        ],
    )
}

fn point_value(row: Sample) -> Value {
    Value::structure(
        "host.Point",
        vec![
            ("atUnix", Value::int(row.at_unix)),
            ("served", int(row.served)),
            ("ok", int(row.ok)),
            ("errors", int(row.errors)),
            ("rejected", int(row.rejected)),
            ("workerMs", int(row.worker_ms)),
            ("p50Us", int(row.p50_us)),
            ("p99Us", int(row.p99_us)),
        ],
    )
}

fn queue_value(queue: QueueInfo) -> Value {
    Value::structure(
        "host.Queue",
        vec![
            ("name", Value::string(queue.name)),
            ("path", Value::string(queue.path)),
            ("due", Value::int(queue.due)),
            ("held", Value::int(queue.held)),
            ("dead", Value::int(queue.dead)),
            ("declared", Value::bool(queue.declared)),
            ("maxDepth", Value::int(queue.max_depth)),
            ("maxAttempts", int(queue.max_attempts)),
            ("retryMs", int(queue.retry_ms)),
            ("leaseMs", int(queue.lease_ms)),
            ("batch", int(queue.batch)),
            ("maxBodyBytes", int(queue.max_body_bytes)),
        ],
    )
}

/// One message of a queue. The store keeps a `due` and a `sent` in seconds,
/// where this module says every time in milliseconds — `atMs` on a log line
/// and on a change — so they are carried as milliseconds and the page's own
/// clock needs nothing of its own.
fn queue_message_value(message: Peeked) -> Value {
    Value::structure(
        "host.QueueMessage",
        vec![
            ("id", Value::int(message.id)),
            ("queue", Value::string(message.queue)),
            ("body", Value::string(message.body)),
            ("bytes", Value::int(message.bytes)),
            ("cut", Value::bool(message.cut)),
            ("attempts", Value::int(message.attempts)),
            ("state", Value::string(message.state)),
            ("dueMs", int(message.due_unix.saturating_mul(1000))),
            ("sentMs", int(message.sent_unix.saturating_mul(1000))),
            ("why", Value::string(message.why)),
            ("buriedMs", int(message.buried_unix.saturating_mul(1000))),
        ],
    )
}

fn log_line_value(app: &str, line: Line) -> Value {
    Value::structure(
        "host.LogLine",
        vec![
            ("app", Value::string(app.to_string())),
            ("atMs", int(line.at_ms)),
            ("level", Value::string(line.level)),
            ("text", Value::string(line.text)),
        ],
    )
}

fn app_line_value(line: AppLine) -> Value {
    Value::structure(
        "host.LogLine",
        vec![
            ("app", Value::string(line.app)),
            ("atMs", int(line.at_ms)),
            ("level", Value::string(line.level)),
            ("text", Value::string(line.text)),
        ],
    )
}

fn all_logs_value(all: AllLogs) -> Value {
    Value::structure(
        "host.AllLogs",
        vec![
            (
                "apps",
                Value::array(all.apps.into_iter().map(Value::string)),
            ),
            (
                "lines",
                Value::array(all.lines.into_iter().map(app_line_value)),
            ),
        ],
    )
}

fn change_value(change: Change) -> Value {
    Value::structure(
        "host.Change",
        vec![
            ("atMs", int(change.unix_ms)),
            ("who", Value::string(change.who)),
            ("app", Value::string(change.app)),
            ("action", Value::string(change.action)),
            ("detail", Value::string(change.detail)),
            ("outcome", Value::string(change.outcome)),
        ],
    )
}

/// The strings of an `Array<String>` argument.
fn string_items(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::items)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A `host.Settings` argument as [`Settings`], or why it is not one: a
/// negative limit, or a zero where zero means nothing. The config's own
/// rules (at least one in flight, a heap under the ceiling, allowlist
/// entries that parse) are the reload's to apply.
fn settings_of(value: &Value) -> Result<Settings, String> {
    let limits = value.field("limits").ok_or("the settings have no limits")?;
    let get = |name: &str| -> Result<i64, String> {
        let n = limits
            .field(name)
            .and_then(Value::as_int)
            .ok_or_else(|| format!("`{name}` is missing"))?;
        if n < 0 {
            return Err(format!("`{name}` cannot be negative ({n})"));
        }
        Ok(n)
    };
    let positive = |name: &str| -> Result<u64, String> {
        match get(name)? {
            0 => Err(format!("`{name}` must be at least 1")),
            n => Ok(n as u64),
        }
    };
    let heap = get("maxHeapWords")? as u64;
    let limits = LimitsFile {
        max_host_calls: Some(get("maxHostCalls")? as u64),
        deadline: Some(format!("{}ms", positive("deadlineMs")?)),
        max_call_depth: None,
        max_heap_words: (heap > 0).then_some(heap),
        max_in_flight: Some(positive("maxInFlight")? as usize),
        max_queued: Some(get("maxQueued")? as usize),
        max_request_bytes: Some(get("maxRequestBytes")? as usize),
        max_response_bytes: Some(get("maxResponseBytes")? as usize),
        ..LimitsFile::default()
    };
    let grant: BTreeSet<String> = string_items(value.field("grant"))
        .into_iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    let mut fetch_allow = Vec::new();
    for entry in string_items(value.field("fetchAllow")) {
        let entry = entry.trim().to_string();
        if !entry.is_empty() && !fetch_allow.contains(&entry) {
            fetch_allow.push(entry);
        }
    }
    Ok(Settings {
        grant,
        fetch_allow,
        limits,
    })
}

impl AdminHost {
    fn control(&self) -> Result<&Arc<Control>, RuntimeError> {
        self.control.as_ref().ok_or_else(|| {
            RuntimeError::new(
                "admin: this app has no host to administer (only the app `admin` of a running \
                 host does)",
            )
        })
    }

    /// The change `op` asks for, as the future that makes it.
    fn change(
        &self,
        op: &str,
        args: &[Value],
    ) -> Result<impl std::future::Future<Output = Result<Transfer, RuntimeError>>, RuntimeError>
    {
        let control = Arc::clone(self.control()?);
        let text = |at: usize| {
            args.get(at)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let app = text(0);
        enum Asked {
            Enable(bool),
            Configure(Result<Settings, String>),
            Reset,
            SetSecret(String),
            DeleteSecret(bool),
        }
        let (asked, who) = match op {
            "setEnabled" => (
                Asked::Enable(args.get(1).and_then(Value::as_bool).unwrap_or(true)),
                text(2),
            ),
            "configure" => (
                Asked::Configure(args.get(1).map_or(Err("no settings".into()), settings_of)),
                text(2),
            ),
            "reset" => (Asked::Reset, text(1)),
            // The first argument is the secret's name; the value goes to the
            // store and nowhere else.
            "setSecret" => (Asked::SetSecret(text(1)), text(2)),
            "deleteSecret" => (
                Asked::DeleteSecret(args.get(1).and_then(Value::as_bool).unwrap_or(false)),
                text(2),
            ),
            other => {
                return Err(RuntimeError::new(format!(
                    "`host` declares no operation `{other}`"
                )))
            }
        };
        Ok(async move {
            let Some(front) = control.front() else {
                return Err(RuntimeError::new("admin: the host is shutting down"));
            };
            let via = Via::AdminApp(who);
            let outcome = match asked {
                Asked::Enable(enabled) => front.set_enabled(&app, enabled, &via).await,
                Asked::Configure(Ok(settings)) => front.configure(&app, settings, &via).await,
                Asked::Configure(Err(why)) => {
                    front.record_refusal(&app, "configure", &via, &why);
                    Err(crate::manage::ChangeError::Refused(why))
                }
                Asked::Reset => front.reset(&app, &via).await,
                Asked::SetSecret(value) => front
                    .set_secret(&app, &value, &via)
                    .await
                    .map(|change| change.message),
                Asked::DeleteSecret(force) => front
                    .delete_secret(&app, force, &via)
                    .await
                    .map(|change| change.message),
            };
            Ok(match outcome {
                Ok(message) => Transfer::ok(Transfer::string(message)),
                Err(error) => Transfer::err(Transfer::error(error.to_string())),
            })
        })
    }
}

impl HostApi for AdminHost {
    fn module_schema(&self) -> ModuleSchema {
        ADMIN
    }

    /// The reads, at once; a change where the run cannot park, waited for
    /// on the worker.
    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // Answered before the host is asked for, and "" where there is none
        // — an app that is not the admin app, or `minicloud test`. Every
        // other operation is about a running host and is an error without
        // one; this one is what a page stamps a URL with or does without, so
        // the absence is an answer and not a failure.
        if op == "selfVersion" {
            return Ok(Value::string(
                self.control
                    .as_ref()
                    .and_then(|control| control.front())
                    .map(|front| front.self_version())
                    .unwrap_or_default(),
            ));
        }
        // Like `selfVersion`, an absence is an answer: no host attached (`minicloud
        // test`) or no series store (a host started without `--data`) is
        // "nothing yet", an empty array, and not a failure — a page that asks
        // for a graph before there is one should draw an empty one.
        if op == "series" {
            let app = args.first().and_then(Value::as_str).unwrap_or_default();
            let over = args.get(1).and_then(Value::as_str).unwrap_or_default();
            let store = self
                .control
                .as_ref()
                .and_then(|control| control.front())
                .and_then(|front| front.series.clone());
            let Some(store) = store else {
                return Ok(Value::array(std::iter::empty::<Value>()));
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs() as i64);
            let (secs, hourly) = crate::series::window(over);
            let store = store.lock().unwrap_or_else(|p| p.into_inner());
            let rows = if hourly {
                store.hours(Some(app), now - secs)
            } else {
                store.minutes(Some(app), now - secs)
            }
            .map_err(|why| RuntimeError::new(format!("admin: series: {why}")))?;
            let skip = rows.len().saturating_sub(SERIES_ROWS);
            return Ok(Value::array(rows.into_iter().skip(skip).map(point_value)));
        }
        let control = self.control()?;
        match op {
            "apps" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                Ok(Value::array(front.app_infos().into_iter().map(app_value)))
            }
            "capabilities" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                Ok(strings(front.capabilities()))
            }
            "secrets" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                Ok(Value::array(
                    front.secret_infos().into_iter().map(secret_value),
                ))
            }
            "code" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                let app = args.first().and_then(Value::as_str).unwrap_or_default();
                let limit = args.get(1).and_then(Value::as_int).unwrap_or(0);
                Ok(Value::array(
                    front.app_code(app, limit).into_iter().map(code_file_value),
                ))
            }
            "codeDisk" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                let app = args.first().and_then(Value::as_str).unwrap_or_default();
                let check = front.app_code_disk(app);
                Ok(Value::structure(
                    "host.CodeDisk",
                    vec![
                        ("looked", Value::bool(check.looked)),
                        ("differing", int(check.differing)),
                        ("bytes", int(check.bytes)),
                        ("budget", int(crate::code::DISK_CHECK_BYTES)),
                    ],
                ))
            }
            "codeFile" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                let text = |at: usize| args.get(at).and_then(Value::as_str).unwrap_or_default();
                Ok(match front.app_code_file(text(0), text(1)) {
                    Ok(file) => Value::ok(code_file_value(file)),
                    Err(why) => Value::err(Value::error(why)),
                })
            }
            "kv" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                let text = |at: usize| args.get(at).and_then(Value::as_str).unwrap_or_default();
                let limit = args.get(3).and_then(Value::as_int).unwrap_or(0);
                let rows = front
                    .app_kv(text(0), text(1), text(2), limit.clamp(0, 500))
                    .map_err(|why| RuntimeError::new(format!("admin: kv: {why}")))?;
                Ok(Value::array(rows.into_iter().map(kv_value)))
            }
            "kvValue" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                let text = |at: usize| args.get(at).and_then(Value::as_str).unwrap_or_default();
                let (app, key) = (text(0), text(1));
                Ok(match front.app_kv_value(app, key) {
                    Ok(Some(row)) => Value::ok(kv_value(row)),
                    Ok(None) => {
                        Value::err(Value::error(format!("no key `{key}` in `{app}`'s store")))
                    }
                    Err(why) => Value::err(Value::error(why)),
                })
            }
            "sqlTables" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                let app = args.first().and_then(Value::as_str).unwrap_or_default();
                let tables = front
                    .app_sql_tables(app)
                    .map_err(|why| RuntimeError::new(format!("admin: sql: {why}")))?;
                Ok(Value::array(tables.into_iter().map(sql_table_value)))
            }
            "sqlQuery" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                let text = |at: usize| args.get(at).and_then(Value::as_str).unwrap_or_default();
                // Every way a statement can fail is the window's `Err` and
                // not the host's error: the refusal is the answer the page
                // shows, and a statement that writes is the commonest of
                // them.
                Ok(match front.app_sql_query(text(0), text(1)) {
                    Ok(answer) => Value::ok(sql_answer_value(answer)),
                    Err(why) => Value::err(Value::error(why)),
                })
            }
            "queues" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                let app = args.first().and_then(Value::as_str).unwrap_or_default();
                let queues = front
                    .app_queues(app)
                    .map_err(|why| RuntimeError::new(format!("admin: queue: {why}")))?;
                Ok(Value::array(queues.into_iter().map(queue_value)))
            }
            // Both listings are bounded by the window's own limit, not by
            // what the page asks for: a queue's `max_depth` is the app's and
            // may be thousands.
            "queueMessages" | "queueDead" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                let app = args.first().and_then(Value::as_str).unwrap_or_default();
                let limit = args
                    .get(1)
                    .and_then(Value::as_int)
                    .unwrap_or(0)
                    .clamp(0, crate::queue::PEEK_MESSAGES);
                let found = if op == "queueDead" {
                    front.app_queue_dead(app, limit)
                } else {
                    front.app_queue_messages(app, limit)
                };
                let found =
                    found.map_err(|why| RuntimeError::new(format!("admin: queue: {why}")))?;
                Ok(Value::array(found.into_iter().map(queue_message_value)))
            }
            "queueMessage" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                let app = args.first().and_then(Value::as_str).unwrap_or_default();
                let id = args.get(1).and_then(Value::as_int).unwrap_or(0);
                Ok(match front.app_queue_message(app, id) {
                    Ok(Some(message)) => Value::ok(queue_message_value(message)),
                    // An id that is on no queue of this app's — a message
                    // delivered since the page was drawn, or one of another
                    // app's — is the window's `Err` and not the host's
                    // error: it is an ordinary answer to a stale link.
                    Ok(None) => Value::err(Value::error(format!(
                        "no message {id} on `{app}`'s queues; it may have been delivered since"
                    ))),
                    Err(why) => Value::err(Value::error(why)),
                })
            }
            "logs" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                let limit = args.get(1).and_then(Value::as_int).unwrap_or(0);
                let limit = limit.clamp(0, crate::logs::LOG_LINES as i64) as usize;
                let app = args.first().and_then(Value::as_str).unwrap_or_default();
                Ok(Value::array(
                    front
                        .app_logs(app, limit)
                        .into_iter()
                        .map(|line| log_line_value(app, line)),
                ))
            }
            "allLogs" => {
                let front = control
                    .front()
                    .ok_or_else(|| RuntimeError::new("admin: the host is shutting down"))?;
                let limit = args.first().and_then(Value::as_int).unwrap_or(0);
                let limit = limit.clamp(0, crate::logs::LOG_LINES as i64) as usize;
                Ok(all_logs_value(front.all_logs(limit)))
            }
            "history" => {
                let limit = args.first().and_then(Value::as_int).unwrap_or(50);
                let limit = limit.clamp(0, 1000) as usize;
                Ok(Value::array(
                    control.history.newest(limit).into_iter().map(change_value),
                ))
            }
            _ => {
                let change = self.change(op, &args)?;
                self.io.block_on(change).map(Transfer::into_value)
            }
        }
    }

    fn call_parkable(&self, op: &str, args: Vec<Value>, _back: &mut dyn Reentry) -> HostAnswer {
        match op {
            "setEnabled" | "configure" | "reset" | "setSecret" | "deleteSecret" => {
                match self.change(op, &args) {
                    Ok(change) => PendingWork::new("host.change", change).answer(),
                    Err(error) => HostAnswer::Ready(Err(error)),
                }
            }
            _ => HostAnswer::Ready(self.call(op, args)),
        }
    }
}
