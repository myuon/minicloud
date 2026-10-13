//! `sql`: each app's relational store, SQLite as Cloudflare's D1 offers it.
//!
//! | operation | answers |
//! | --- | --- |
//! | `sql.query(statement, params)` | `Result<sql.Rows, Error>`: runs a statement that only reads |
//! | `sql.exec(statement, params)` | `Result<sql.Changed, Error>`: runs a statement that writes (or changes the schema) |
//!
//! `sql.Rows` is `{ columns: Array<String>, rows: Array<sql.Row> }`,
//! `sql.Row` is `{ values: Array<Option<String>> }` and `sql.Changed` is
//! `{ rows: Int, lastInsertId: Int }`. It sits next to `kv` and does not
//! replace it: `kv` is the right store for a counter or a blob of state, and
//! this one is for data an app wants to ask questions of.
//!
//! # Values are text
//!
//! A host type here is a struct or an array; `TypeSchema` carries the names
//! of an enum's cases but not their payloads, so a "SQLite value" that is
//! sometimes an integer and sometimes text cannot be declared. Every value is
//! therefore an `Option<String>`: `NULL` is `None`, `TEXT` is itself, and
//! `INTEGER` and `REAL` are their decimal text, which the app parses with the
//! type it expects. A `BLOB` is the app's `Err` (`a blob is not text; store it
//! as hex or base64`) rather than a silently mangled string.
//!
//! Parameters go the other way as `Array<String>` and are **bound as text**.
//! SQLite's type affinity converts that text to a number when it is compared
//! with, or stored in, an `INTEGER` or `REAL` column, so `WHERE id = ?1` with
//! `"42"` finds row 42 and an insert into an integer column stores an integer.
//! That is enough for nearly every statement, and the alternative — a tagged
//! parameter type — would need the payload enum this schema cannot express.
//! The one thing text cannot say is `NULL`: write `nullif(?1, '')` and pass
//! `""`.
//!
//! # Storage
//!
//! One SQLite file per app at `<data>/<app>/sql.sqlite3`, in WAL mode with
//! `synchronous = NORMAL`, for the reasons `kv` gives. The file is the
//! namespace: a connection opened on it can see nothing else, and `ATTACH` is
//! refused (below) so an app cannot name another file.
//!
//! # What the app's SQL may not do
//!
//! The app writes the SQL, so the connection is locked down by an
//! authorizer, which SQLite consults while a statement is *prepared*:
//!
//! | refused | why |
//! | --- | --- |
//! | `ATTACH`, `DETACH` | they name other files on the host's disk |
//! | `PRAGMA` | the host set the pragmas it needs; the app must not turn off `foreign_keys`, lift `max_page_count`, or read the file's path |
//! | `BEGIN`, `COMMIT`, `ROLLBACK` | a transaction held open across host calls would keep the connection locked if the run hit its deadline in between. A single statement is atomic by itself, which is all an ordinary write needs |
//! | `SAVEPOINT`, `RELEASE` | transactions by another name |
//! | `CREATE VIRTUAL TABLE`, `DROP` of one | a virtual table is code from a module, and there is no reason an app should name one |
//!
//! Everything else, including `CREATE TABLE` and `ALTER TABLE`, is allowed:
//! the app owns its schema, and migrating it is the app's job.
//!
//! # Limits
//!
//! `[sql]` in `app.toml`: `max_bytes` (the database file, enforced through
//! SQLite's `max_page_count`, so a write past it fails with `database or disk
//! is full` as the app's `Err`), `max_rows` and `max_result_bytes` (what one
//! `query` answers), `max_statement_ms` and `max_statement_bytes`.
//!
//! A statement that runs too long is stopped by SQLite's progress handler,
//! which is called every thousand virtual-machine steps and asks whether the
//! deadline has passed. The worker is held for the whole call, as it is for
//! `kv`, so the bound is small by default.
//!
//! # The admin's window
//!
//! The admin app reads another app's store through [`peek_tables`] and
//! [`peek_query`] (issue 88), the shape `kv` already has for the same job
//! ([`crate::kv::peek`]): the store the app has open where one is open, the
//! file opened `SQLITE_OPEN_READ_ONLY` where nothing has, and no store at
//! all for an app that has no file. It reads, and a statement that writes is
//! [`Store::query`]'s own refusal rather than a check of its own, so that
//! what "reads" means is decided in one place. Its limits are its own
//! ([`PEEK_ROWS`] and the rest): an app's `[sql]` is for the app, and
//! looking at a store should not mean raising them.
//!
//! # Answered at once, on the worker
//!
//! As for `kv`, every call answers `Ready` on the worker thread that made it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use cove_runtime::{
    Effect, FieldSchema, HostApi, HostType, ModuleSchema, OperationSchema, RuntimeError,
    TypeSchema, Value,
};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::ValueRef;
use rusqlite::{params_from_iter, Connection, ErrorCode, OpenFlags};

use crate::config::SqlLimits;
use crate::hosts::{AppContext, HostModule};

/// The most `?` parameters one statement may have.
const MAX_VARIABLES: i32 = 999;

/// The most columns a table, an index or a result may have.
const MAX_COLUMNS: i32 = 200;

/// The deepest an expression may nest.
const MAX_EXPR_DEPTH: i32 = 200;

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
        capability: "sql",
        effect,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

const STR: HostType = HostType::String;
const STATEMENT: &[HostType] = &[STR, HostType::Array(&STR)];

/// The `sql` module.
pub const SQL: ModuleSchema = ModuleSchema {
    name: "sql",
    capability: "sql",
    operations: &[
        op(
            "query",
            STATEMENT,
            HostType::Result(&HostType::Named("sql.Rows"), &HostType::Error),
            Effect::Read,
        ),
        op(
            "exec",
            STATEMENT,
            HostType::Result(&HostType::Named("sql.Changed"), &HostType::Error),
            Effect::ReversibleWrite,
        ),
    ],
    types: &[
        TypeSchema {
            name: "Rows",
            cases: &[],
            fields: &[
                FieldSchema {
                    name: "columns",
                    ty: HostType::Array(&STR),
                },
                FieldSchema {
                    name: "rows",
                    ty: HostType::Array(&HostType::Named("sql.Row")),
                },
            ],
        },
        TypeSchema {
            name: "Row",
            cases: &[],
            fields: &[FieldSchema {
                name: "values",
                ty: HostType::Array(&HostType::Option(&STR)),
            }],
        },
        TypeSchema {
            name: "Changed",
            cases: &[],
            fields: &[
                FieldSchema {
                    name: "rows",
                    ty: HostType::Int,
                },
                FieldSchema {
                    name: "lastInsertId",
                    ty: HostType::Int,
                },
            ],
        },
    ],
    resources: &[],
};

pub(crate) struct SqlModule;

impl HostModule for SqlModule {
    fn schema(&self) -> ModuleSchema {
        SQL
    }

    fn instantiate(&self, app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        // An app not granted `sql` can never reach the store, so it gets
        // none: no file is created for it.
        let store = if !app.granted.contains("sql") {
            None
        } else {
            Some(match &app.data {
                Some(dir) => shared(&dir.join("sql.sqlite3"), &app.sql)?,
                None => Arc::new(Mutex::new(
                    Store::in_memory(app.sql.clone())
                        .map_err(|why| format!("cannot open its sql store: {why}"))?,
                )),
            })
        };
        Ok(Box::new(SqlHost { store }))
    }
}

/// One app's database.
pub struct Store {
    conn: Connection,
    limits: SqlLimits,
}

/// What a `query` answered.
#[derive(Debug, PartialEq, Eq)]
pub struct Answer {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// What an `exec` changed.
#[derive(Debug, PartialEq, Eq)]
pub struct Changed {
    pub rows: i64,
    pub last_insert_id: i64,
}

/// Why a statement was refused or failed.
#[derive(Debug, PartialEq, Eq)]
pub enum SqlError {
    /// The app's SQL, its parameters, or a limit: the app's `Err`.
    App(String),
    /// The database failed: the host's error.
    Storage(String),
}

impl Store {
    /// The store at `path`, created if it is not there.
    pub fn open(path: &Path, limits: SqlLimits) -> Result<Store, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create `{}`: {e}", dir.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("`{}`: {e}", path.display()))?;
        Store::with(conn, limits)
    }

    /// A store that lives as long as the process.
    pub fn in_memory(limits: SqlLimits) -> Result<Store, String> {
        Store::with(
            Connection::open_in_memory().map_err(|e| e.to_string())?,
            limits,
        )
    }

    fn with(conn: Connection, limits: SqlLimits) -> Result<Store, String> {
        // WAL does not apply to an in-memory database, which answers with a
        // different mode rather than failing; either way it is not an error.
        let _ = conn.execute_batch("PRAGMA journal_mode = WAL");
        conn.execute_batch("PRAGMA synchronous = NORMAL")
            .map_err(|e| e.to_string())?;
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .map_err(|e| e.to_string())?;
        configure(&conn, &limits)?;
        // The authorizer goes in last: it refuses `PRAGMA`, and the pragmas
        // above are ours. Installed any earlier it would refuse the host.
        install_authorizer(&conn);
        Ok(Store { conn, limits })
    }

    /// Applies new limits to the open store, as when a new version of the
    /// app comes up with different ones.
    fn set_limits(&mut self, limits: SqlLimits) {
        // The pragmas in `configure` are ours, so the authorizer steps aside
        // while they run.
        self.conn
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        let _ = configure(&self.conn, &limits);
        install_authorizer(&self.conn);
        self.limits = limits;
    }

    /// Runs a statement that only reads.
    pub fn query(&mut self, statement: &str, params: &[String]) -> Result<Answer, SqlError> {
        let limits = self.limits.clone();
        timed(&self.conn, limits.max_statement_ms, |conn| {
            read_only(conn, &limits, Whom::Query, statement, params)
        })
    }

    /// Runs a statement that writes. A statement that reads is not refused;
    /// it changes no rows.
    pub fn exec(&mut self, statement: &str, params: &[String]) -> Result<Changed, SqlError> {
        let limits = self.limits.clone();
        timed(&self.conn, limits.max_statement_ms, |conn| {
            let mut stmt = prepare(conn, &limits, statement, params, Whom::Exec)?;
            let before = conn.total_changes();
            let mut rows = stmt
                .query(params_from_iter(params.iter()))
                .map_err(|e| failure(e, &limits, Whom::Exec))?;
            // A statement with `RETURNING` answers rows; they are discarded,
            // but must be stepped through for the write to happen.
            while rows
                .next()
                .map_err(|e| failure(e, &limits, Whom::Exec))?
                .is_some()
            {}
            drop(rows);
            let changed = conn.total_changes() - before;
            // Read here, under the same lock as the statement: the
            // connection is shared by every run of the app, so a separate
            // call for it could answer another run's insert. When nothing
            // changed the connection's last id is an older statement's, so
            // it is not reported.
            let last_insert_id = if changed > 0 {
                conn.last_insert_rowid()
            } else {
                0
            };
            Ok(Changed {
                rows: changed as i64,
                last_insert_id,
            })
        })
    }
}

/// Who a statement is being run for.
///
/// It decides two things a message says: what to call the call, and whose
/// limits it ran under. The limits have to be named because they differ — an
/// app's are its `[sql]` in `app.toml`, which it can have raised, and the
/// admin window's are the host's own and not the app's to raise — and a
/// message that said `[sql]` for both would send somebody to edit the wrong
/// number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Whom {
    /// The app's own `sql.query`.
    Query,
    /// The app's own `sql.exec`.
    Exec,
    /// The admin app reading another app's store ([`peek_query`]).
    Window,
}

impl Whom {
    /// What a message calls the call.
    fn who(self) -> &'static str {
        match self {
            Whom::Query => "sql.query",
            Whom::Exec => "sql.exec",
            Whom::Window => "admin",
        }
    }

    /// Whose limits the call ran under, as a possessive a message can use.
    fn whose(self) -> &'static str {
        match self {
            Whom::Query | Whom::Exec => "this app's [sql]",
            Whom::Window => "the admin window's",
        }
    }

    /// What a statement that writes is told. The window is not pointed at
    /// `sql.exec`: nothing reaches another app's store to write to it, and
    /// an admin who has to repair data does it over ssh, deliberately.
    fn refusal(self) -> String {
        match self {
            Whom::Window => {
                "admin: this window only reads; it will not run a statement that writes".into()
            }
            _ => format!("{}: use sql.exec for a statement that writes", self.who()),
        }
    }
}

/// Runs a statement that only reads on `conn`, under `limits`.
///
/// The app's `sql.query` and the admin's window are this one function
/// because "only reads" has to mean one thing. `Statement::readonly` is
/// SQLite's own answer, taken after the statement is prepared and before it
/// is stepped, so a write is refused rather than undone; a second
/// implementation of that for the window would be a second chance to get it
/// wrong.
fn read_only(
    conn: &Connection,
    limits: &SqlLimits,
    whom: Whom,
    statement: &str,
    params: &[String],
) -> Result<Answer, SqlError> {
    let mut stmt = prepare(conn, limits, statement, params, whom)?;
    if !stmt.readonly() {
        return Err(SqlError::App(whom.refusal()));
    }
    let columns: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
    let mut rows = stmt
        .query(params_from_iter(params.iter()))
        .map_err(|e| failure(e, limits, whom))?;
    let mut answer = Vec::new();
    let mut bytes = 0u64;
    while let Some(row) = rows.next().map_err(|e| failure(e, limits, whom))? {
        // A query past `max_rows` is an `Err` and not the first `max_rows`
        // of it: a page that silently showed the start of something would be
        // wrong, where an error is only loud.
        if answer.len() as u64 >= limits.max_rows {
            return Err(SqlError::App(format!(
                "{}: the answer has more than {} max_rows of {}; \
                 add a LIMIT or a narrower WHERE",
                whom.who(),
                whom.whose(),
                limits.max_rows
            )));
        }
        let mut values = Vec::with_capacity(columns.len());
        for at in 0..columns.len() {
            let value = text_of(row.get_ref(at).map_err(|e| failure(e, limits, whom))?, whom)?;
            bytes += value.as_ref().map_or(0, |v| v.len() as u64);
            values.push(value);
        }
        if bytes > limits.max_result_bytes {
            return Err(SqlError::App(format!(
                "{}: the answer is above {} max_result_bytes of {}",
                whom.who(),
                whom.whose(),
                limits.max_result_bytes
            )));
        }
        answer.push(values);
    }
    Ok(Answer {
        columns,
        rows: answer,
    })
}

/// Runs `body` with the statement deadline armed on `conn`: SQLite's
/// progress handler, called every thousand virtual-machine steps, which
/// stops the statement where it is. Cleared again afterwards, so that a
/// connection carries no deadline of a call that is over — which matters
/// most where the connection is shared, as the window's is.
fn timed<T>(
    conn: &Connection,
    ms: u64,
    body: impl FnOnce(&Connection) -> Result<T, SqlError>,
) -> Result<T, SqlError> {
    let until = Instant::now() + Duration::from_millis(ms);
    conn.progress_handler(1000, Some(move || Instant::now() >= until));
    let outcome = body(conn);
    conn.progress_handler(0, None::<fn() -> bool>);
    outcome
}

/// The settings that are not the authorizer. Run before it is installed.
fn configure(conn: &Connection, limits: &SqlLimits) -> Result<(), String> {
    conn.execute_batch(&format!(
        "PRAGMA busy_timeout = {}",
        limits.max_statement_ms
    ))
    .map_err(|e| e.to_string())?;
    // The database's size limit: SQLite counts pages, and refuses to grow
    // past the count with `database or disk is full`.
    let page_size: u64 = conn
        .query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))
        .map_err(|e| e.to_string())?
        .max(1) as u64;
    let pages = (limits.max_bytes / page_size).max(1);
    conn.execute_batch(&format!("PRAGMA max_page_count = {pages}"))
        .map_err(|e| e.to_string())?;
    let saturate = |n: u64| i32::try_from(n).unwrap_or(i32::MAX);
    conn.set_limit(
        Limit::SQLITE_LIMIT_SQL_LENGTH,
        saturate(limits.max_statement_bytes),
    );
    conn.set_limit(
        Limit::SQLITE_LIMIT_LENGTH,
        saturate(limits.max_result_bytes),
    );
    conn.set_limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER, MAX_VARIABLES);
    conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, MAX_COLUMNS);
    conn.set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, MAX_EXPR_DEPTH);
    Ok(())
}

fn install_authorizer(conn: &Connection) {
    conn.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Attach { .. }
        | AuthAction::Detach { .. }
        | AuthAction::Pragma { .. }
        | AuthAction::Transaction { .. }
        | AuthAction::Savepoint { .. }
        | AuthAction::CreateVtable { .. }
        | AuthAction::DropVtable { .. } => Authorization::Deny,
        _ => Authorization::Allow,
    }));
}

/// Prepares `statement` after the checks that need no SQLite, and the one
/// that needs the prepared statement: the parameter count.
fn prepare<'c>(
    conn: &'c Connection,
    limits: &SqlLimits,
    statement: &str,
    params: &[String],
    whom: Whom,
) -> Result<rusqlite::Statement<'c>, SqlError> {
    let who = whom.who();
    if statement.len() as u64 > limits.max_statement_bytes {
        return Err(SqlError::App(format!(
            "{who}: a statement of {} bytes is above {} max_statement_bytes of {}",
            statement.len(),
            whom.whose(),
            limits.max_statement_bytes
        )));
    }
    let stmt = conn
        .prepare(statement)
        .map_err(|e| failure(e, limits, whom))?;
    if has_tail(conn, statement) {
        return Err(SqlError::App(format!(
            "{who}: give one statement per call; the text after the first `;` would be ignored"
        )));
    }
    if stmt.parameter_count() != params.len() {
        return Err(SqlError::App(format!(
            "{who}: the statement takes {} parameters and {} were given",
            stmt.parameter_count(),
            params.len()
        )));
    }
    Ok(stmt)
}

/// How much of the disk one app's store is, for the operations views:
/// the database and the write-ahead log beside it, which holds what is
/// committed and not yet checkpointed. `None` when there is no store.
pub fn usage(path: &Path) -> Option<u64> {
    let database = std::fs::metadata(path).ok()?.len();
    let wal = std::fs::metadata(path.with_extension("sqlite3-wal"))
        .map(|meta| meta.len())
        .unwrap_or(0);
    Some(database + wal)
}

/// Whether `sql` holds a second statement after the first.
///
/// SQLite prepares the first statement and hands back the rest, and rusqlite
/// drops the rest silently unless its `extra_check` feature is on; an app
/// that wrote two statements would see the first run and the second vanish.
/// SQLite is asked directly, the one place this file leaves safe Rust: two
/// prepares and two finalizes of statements that are never stepped.
fn has_tail(conn: &Connection, sql: &str) -> bool {
    use rusqlite::ffi;
    use std::os::raw::{c_char, c_int};
    use std::ptr;

    // Prepares the start of `text`; the statement (null when `text` holds
    // none) and where SQLite stopped reading.
    let prepare = |text: &[u8]| -> Option<(*mut ffi::sqlite3_stmt, usize)> {
        let mut stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        // SAFETY: `text` outlives the call and its length is passed, so
        // SQLite reads nothing past it; `handle` is a live connection that
        // the caller holds `&` to.
        let rc = unsafe {
            ffi::sqlite3_prepare_v2(
                conn.handle(),
                text.as_ptr() as *const c_char,
                text.len() as c_int,
                &mut stmt,
                &mut tail,
            )
        };
        if rc != ffi::SQLITE_OK || tail.is_null() {
            return None;
        }
        Some((stmt, tail as usize - text.as_ptr() as usize))
    };
    let bytes = sql.as_bytes();
    let Some((first, used)) = prepare(bytes) else {
        return false;
    };
    // SAFETY: `first` came from a successful prepare and is finalized once.
    unsafe { ffi::sqlite3_finalize(first) };
    match prepare(&bytes[used.min(bytes.len())..]) {
        Some((second, _)) => {
            let found = !second.is_null();
            // SAFETY: as above; finalizing null is a no-op.
            unsafe { ffi::sqlite3_finalize(second) };
            found
        }
        None => false,
    }
}

/// A value as the app sees it.
fn text_of(value: ValueRef<'_>, whom: Whom) -> Result<Option<String>, SqlError> {
    Ok(match value {
        ValueRef::Null => None,
        ValueRef::Integer(n) => Some(n.to_string()),
        ValueRef::Real(x) => {
            let mut text = x.to_string();
            // `2` for 2.0 would read as an integer.
            if x.is_finite() && !text.contains('.') {
                text.push_str(".0");
            }
            Some(text)
        }
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(_) => {
            return Err(SqlError::App(format!(
                "{}: a blob is not text; {}",
                whom.who(),
                match whom {
                    // The window cannot change how the app stores anything,
                    // so it is told the way to read what is there.
                    Whom::Window => "select hex(…) of that column",
                    _ => "store it as hex or base64",
                }
            )));
        }
    })
}

/// Sorts a SQLite error into the app's and the host's.
///
/// Syntax errors, constraint violations, a full database, a refused
/// statement and an interrupt are all the app's SQL or the app's data. Only
/// a failure of the disk or the file is the host's. The message is SQLite's
/// own text, which names tables and columns but never the file: a path
/// appears only in the `open` error, which does not reach an app.
fn failure(error: rusqlite::Error, limits: &SqlLimits, whom: Whom) -> SqlError {
    use rusqlite::Error;
    let who = whom.who();
    let (code, message) = match &error {
        Error::SqliteFailure(e, message) => (
            Some(e.code),
            message.clone().unwrap_or_else(|| e.to_string()),
        ),
        Error::SqlInputError { error, msg, .. } => (Some(error.code), msg.clone()),
        other => (None, other.to_string()),
    };
    match code {
        Some(ErrorCode::OperationInterrupted) => SqlError::App(format!(
            "{who}: the statement ran longer than {} max_statement_ms ({} ms)",
            whom.whose(),
            limits.max_statement_ms
        )),
        Some(ErrorCode::AuthorizationForStatementDenied) => SqlError::App(format!(
            "{who}: not authorized; ATTACH, PRAGMA, transactions, savepoints and \
             virtual tables are refused"
        )),
        Some(
            ErrorCode::SystemIoFailure
            | ErrorCode::CannotOpen
            | ErrorCode::DatabaseCorrupt
            | ErrorCode::NotADatabase
            | ErrorCode::OutOfMemory
            | ErrorCode::DatabaseBusy
            | ErrorCode::DatabaseLocked,
        ) => SqlError::Storage(message),
        _ => SqlError::App(format!("{who}: {message}")),
    }
}

struct SqlHost {
    /// `None` for an app not granted `sql`, whose calls the boundary refuses.
    store: Option<Arc<Mutex<Store>>>,
}

/// The stores open in this process, by file.
fn open_stores() -> &'static Mutex<HashMap<PathBuf, Weak<Mutex<Store>>>> {
    static OPEN: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<Store>>>>> = OnceLock::new();
    OPEN.get_or_init(Mutex::default)
}

/// The store at `path`: the one already open, if a version of the app has
/// it open, with the new version's limits; or newly opened.
///
/// One connection per file however many versions of the app are alive during
/// an update, so that `last_insert_id` and the deadline have one owner.
fn shared(path: &Path, limits: &SqlLimits) -> Result<Arc<Mutex<Store>>, String> {
    let mut open = open_stores().lock().unwrap();
    if let Some(store) = open.get(path).and_then(Weak::upgrade) {
        store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .set_limits(limits.clone());
        return Ok(store);
    }
    let store = Arc::new(Mutex::new(
        Store::open(path, limits.clone())
            .map_err(|why| format!("cannot open its sql store: {why}"))?,
    ));
    open.insert(path.to_path_buf(), Arc::downgrade(&store));
    Ok(store)
}

// -------------------------------------------------- the admin's window

/// The most rows the admin's window answers, and the only one of its limits
/// a person is likely to meet.
///
/// A page somebody reads, not an app reading its own data: an app's `[sql]
/// max_rows` is the app's — what one of its requests may cost — and reading
/// the store from outside should not mean raising it. An answer past this is
/// an `Err` that says so, as [`Store::query`]'s is, because the start of
/// something shown as though it were the whole of it is the one wrong answer
/// a window like this can give.
pub const PEEK_ROWS: u64 = 200;

/// The values of one of the window's answers, summed. The page cuts each
/// cell again to what a column can show, so this bounds what is carried,
/// not what is read.
pub const PEEK_RESULT_BYTES: u64 = 256 << 10;

/// How long one of the window's statements may run.
///
/// Fifty times an app's default of 20 ms, for the opposite reason: an app
/// serves requests at a rate, and this is one person asking one question, so
/// a `SELECT` with no index to serve it is worth a moment. It stays small
/// because the statement holds a worker for the whole of it — and, where the
/// app has its store open, that store's lock as well, which is the app's own
/// requests waiting. A statement stopped at it is an `Err` that says so,
/// never a short answer.
pub const PEEK_STATEMENT_MS: u64 = 1000;

/// How long one of the window's statements may be. It is typed into a form
/// and carried in a URL.
pub const PEEK_STATEMENT_BYTES: u64 = 4 << 10;

/// The app's tables, from SQLite's own catalogue.
///
/// `sqlite_%` is SQLite's bookkeeping — the sequence table of an
/// `AUTOINCREMENT`, the statistics of an `ANALYZE` — and not the app's data.
/// `type = 'table'` leaves out views, which have no rows of their own.
///
/// No `LIMIT`: [`PEEK_ROWS`] applies to this statement like any other, so a
/// store with more tables than that says so rather than showing part of its
/// catalogue as though it were all of it.
const CATALOGUE: &str = "SELECT name FROM sqlite_master \
                         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name";

/// One table of a store, as the window sees it.
#[derive(Debug, PartialEq, Eq)]
pub struct Table {
    /// Its name, as `sqlite_master` holds it.
    pub name: String,
    /// How many rows it holds; 0 where `counted` is false.
    pub rows: i64,
    /// Whether the count was taken. Counting is a statement of its own under
    /// the window's deadline, and a table too large to count inside it is
    /// listed saying so rather than left out of the list.
    pub counted: bool,
}

/// What the window runs under: its own limits, not the app's.
///
/// `max_bytes` is the file's ceiling, applied by `configure` when a store is
/// opened as one; this path opens no store and writes nothing, so the default
/// stands for it and is never used.
///
/// The connection's own SQLite limits are left alone. Where the app has the
/// store open they are the app's, set from its `[sql]`, and the window is a
/// guest on that connection: a statement of the window's can therefore also
/// be refused by a limit of the app's, which is the conservative way round.
fn peek_limits() -> SqlLimits {
    SqlLimits {
        max_rows: PEEK_ROWS,
        max_result_bytes: PEEK_RESULT_BYTES,
        max_statement_ms: PEEK_STATEMENT_MS,
        max_statement_bytes: PEEK_STATEMENT_BYTES,
        ..SqlLimits::default()
    }
}

/// The tables of the store at `path` and how many rows each holds: the page
/// somebody lands on.
///
/// Empty for an app with no store, which is every app not granted `sql`.
pub fn peek_tables(path: &Path) -> Result<Vec<Table>, SqlError> {
    Ok(with_reader(path, tables)?.unwrap_or_default())
}

/// Runs one statement of the admin's window against the store at `path`.
///
/// Nothing is bound: the window is a statement typed whole, where an app
/// composes one and passes its data as `?` parameters. That is safe here for
/// the reason it is not in an app — the text is the admin's own, not a
/// request's — and it is refused whatever it holds unless SQLite says it only
/// reads.
pub fn peek_query(path: &Path, statement: &str) -> Result<Answer, SqlError> {
    with_reader(path, |conn| peek_on(conn, statement))?.ok_or_else(|| {
        SqlError::App(
            "admin: this app has no sql store; the host keeps no file for an app \
             that is not granted `sql`"
                .into(),
        )
    })
}

/// Runs one of the window's statements on `conn`: the window's limits, and
/// its deadline armed for the length of it.
///
/// Every statement of the window's goes through here, the catalogue and the
/// counts of [`tables`] included, so that each of them is bounded — and so
/// that an app's connection is left with no deadline of ours on it.
fn peek_on(conn: &Connection, statement: &str) -> Result<Answer, SqlError> {
    let limits = peek_limits();
    timed(conn, limits.max_statement_ms, |conn| {
        read_only(conn, &limits, Whom::Window, statement, &[])
    })
}

/// The tables of the store on `conn`, each with its row count.
///
/// The count is a statement per table, so a store of many tables costs many
/// statements — each under the window's deadline, and the catalogue is
/// bounded by [`PEEK_ROWS`], which bounds how many there can be.
fn tables(conn: &Connection) -> Result<Vec<Table>, SqlError> {
    let listed = peek_on(conn, CATALOGUE)?;
    let mut out = Vec::with_capacity(listed.rows.len());
    for row in listed.rows {
        let Some(Some(name)) = row.into_iter().next() else {
            continue;
        };
        // The name goes into the statement's text, because SQLite takes an
        // identifier there and nowhere else: `?` is a value. It came from
        // `sqlite_master` and not from a request, and it is quoted with `"`
        // doubled inside, which is how a quoted identifier carries one.
        let quoted = name.replace('"', "\"\"");
        let count = peek_on(conn, &format!("SELECT count(*) FROM \"{quoted}\""));
        let rows = match &count {
            Ok(answer) => answer
                .rows
                .first()
                .and_then(|row| row.first())
                .and_then(|value| value.as_deref())
                .and_then(|text| text.parse().ok()),
            // A count that failed — a table too large for the deadline, a
            // view the catalogue called a table — leaves the table listed
            // and the number unsaid.
            Err(_) => None,
        };
        out.push(Table {
            name,
            rows: rows.unwrap_or(0),
            counted: rows.is_some(),
        });
    }
    Ok(out)
}

/// Runs `read` on the store at `path`, reading only; `None` when there is no
/// store there.
///
/// The connection is **the app's own** where a version of the app has the
/// store open — the shape `kv`'s own reader ([`crate::kv::peek`]) has, and for
/// the same two reasons. A second connection on a WAL database reads the
/// snapshot its transaction started with and cannot see what the app's
/// connection has written and not yet checkpointed, so a row written a moment
/// ago would be missing from a window whose whole job is to show what is
/// there. And two connections on one file take locks against each other,
/// so a read would be made to wait on a write, or refused as busy. The cost
/// is the other way round — the app's next statement waits for the window's
/// — which is why the window's deadline is short.
///
/// Where nothing has it open the file is opened `SQLITE_OPEN_READ_ONLY`, and
/// a file that is not there is no store rather than an error:
/// [`Store::open`] would create one, and an app not granted `sql` must not
/// come to have a store because somebody looked at it.
fn with_reader<T>(
    path: &Path,
    read: impl FnOnce(&Connection) -> Result<T, SqlError>,
) -> Result<Option<T>, SqlError> {
    let open = open_stores()
        .lock()
        .unwrap()
        .get(path)
        .and_then(Weak::upgrade);
    if let Some(store) = open {
        let store = store.lock().unwrap_or_else(|p| p.into_inner());
        return read(&store.conn).map(Some);
    }
    if !path.is_file() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| SqlError::Storage(format!("`{}`: {e}", path.display())))?;
    // The authorizer belongs to a `Store` and this connection is not one, so
    // it is installed here. Without it this path would allow what the other
    // refuses: `ATTACH` naming any file the host can read (a read-only
    // connection may still attach), `PRAGMA`, and a transaction left open on
    // the file.
    install_authorizer(&conn);
    read(&conn).map(Some)
}

fn storage(why: String) -> RuntimeError {
    RuntimeError::new(format!("sql: the store failed: {why}"))
}

/// The statement's parameters, as the boundary has held them to `Array<String>`.
fn strings(value: &Value) -> Vec<String> {
    value
        .items()
        .unwrap_or_default()
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect()
}

impl HostApi for SqlHost {
    fn module_schema(&self) -> ModuleSchema {
        SQL
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // The boundary held the arity and each argument's type to `SQL`.
        let text = |at: usize| args[at].as_str().unwrap_or_default();
        let Some(store) = &self.store else {
            return Err(RuntimeError::new("sql: this app has no store"));
        };
        let mut store = store.lock().unwrap_or_else(|p| p.into_inner());
        match op {
            "query" => match store.query(text(0), &strings(&args[1])) {
                Ok(answer) => Ok(Value::ok(Value::structure(
                    "sql.Rows",
                    vec![
                        (
                            "columns",
                            Value::array(answer.columns.into_iter().map(Value::string)),
                        ),
                        (
                            "rows",
                            Value::array(answer.rows.into_iter().map(|row| {
                                Value::structure(
                                    "sql.Row",
                                    vec![(
                                        "values",
                                        Value::array(row.into_iter().map(|v| match v {
                                            Some(text) => Value::some(Value::string(text)),
                                            None => Value::none(),
                                        })),
                                    )],
                                )
                            })),
                        ),
                    ],
                ))),
                Err(SqlError::App(why)) => Ok(Value::err(Value::error(why))),
                Err(SqlError::Storage(why)) => Err(storage(why)),
            },
            "exec" => match store.exec(text(0), &strings(&args[1])) {
                Ok(done) => Ok(Value::ok(Value::structure(
                    "sql.Changed",
                    vec![
                        ("rows", Value::int(done.rows)),
                        ("lastInsertId", Value::int(done.last_insert_id)),
                    ],
                ))),
                Err(SqlError::App(why)) => Ok(Value::err(Value::error(why))),
                Err(SqlError::Storage(why)) => Err(storage(why)),
            },
            other => Err(RuntimeError::new(format!(
                "`sql` declares no operation `{other}`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(limits: SqlLimits) -> Store {
        Store::in_memory(limits).unwrap()
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn app(result: Result<impl std::fmt::Debug, SqlError>) -> String {
        match result {
            Err(SqlError::App(why)) => why,
            other => panic!("expected the app's Err, got {other:?}"),
        }
    }

    #[test]
    fn a_table_made_filled_and_read_back_answers_its_columns_and_values() {
        let mut sql = store(SqlLimits::default());
        sql.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)", &[])
            .unwrap();
        sql.exec("INSERT INTO t (name) VALUES (?1), (?2)", &strs(&["a", "b"]))
            .unwrap();
        let answer = sql
            .query("SELECT id, name FROM t ORDER BY id", &[])
            .unwrap();
        assert_eq!(answer.columns, ["id", "name"]);
        assert_eq!(
            answer.rows,
            [
                [Some("1".to_string()), Some("a".to_string())],
                [Some("2".to_string()), Some("b".to_string())]
            ]
        );
    }

    #[test]
    fn null_is_none_and_numbers_are_decimal_text() {
        let mut sql = store(SqlLimits::default());
        let answer = sql
            .query("SELECT NULL, 42, 1.5, 2.0, 'x', -7", &[])
            .unwrap();
        assert_eq!(
            answer.rows[0],
            [
                None,
                Some("42".to_string()),
                Some("1.5".to_string()),
                Some("2.0".to_string()),
                Some("x".to_string()),
                Some("-7".to_string())
            ]
        );
    }

    #[test]
    fn a_blob_is_the_apps_err_and_says_what_to_do() {
        let mut sql = store(SqlLimits::default());
        let why = app(sql.query("SELECT x'00ff'", &[]));
        assert!(why.contains("a blob is not text"), "{why}");
    }

    #[test]
    fn parameters_are_bound_as_data_so_an_injection_is_only_a_string() {
        let mut sql = store(SqlLimits::default());
        sql.exec("CREATE TABLE t (v TEXT)", &[]).unwrap();
        let evil = "'; drop table t; --";
        sql.exec("INSERT INTO t (v) VALUES (?1)", &strs(&[evil]))
            .unwrap();
        let answer = sql
            .query("SELECT v FROM t WHERE v = ?1", &strs(&[evil]))
            .unwrap();
        assert_eq!(answer.rows, [[Some(evil.to_string())]]);
        let tables = sql
            .query("SELECT count(*) FROM sqlite_master WHERE name = 't'", &[])
            .unwrap();
        assert_eq!(tables.rows, [[Some("1".to_string())]]);
    }

    #[test]
    fn text_parameters_compare_as_numbers_against_an_integer_column() {
        let mut sql = store(SqlLimits::default());
        sql.exec("CREATE TABLE t (n INTEGER, m TEXT)", &[]).unwrap();
        sql.exec(
            "INSERT INTO t VALUES (?1, nullif(?2, ''))",
            &strs(&["42", ""]),
        )
        .unwrap();
        let answer = sql
            .query("SELECT typeof(n), m FROM t WHERE n = ?1", &strs(&["42"]))
            .unwrap();
        assert_eq!(answer.rows, [[Some("integer".to_string()), None]]);
    }

    #[test]
    fn a_statement_that_writes_is_refused_by_query() {
        let mut sql = store(SqlLimits::default());
        sql.exec("CREATE TABLE t (v TEXT)", &[]).unwrap();
        let why = app(sql.query("INSERT INTO t VALUES ('x')", &[]));
        assert!(why.contains("sql.exec"), "{why}");
        let count = sql.query("SELECT count(*) FROM t", &[]).unwrap();
        assert_eq!(count.rows, [[Some("0".to_string())]]);
        // The other way is no error: a read run by `exec` changes nothing.
        let done = sql.exec("SELECT 1", &[]).unwrap();
        assert_eq!(
            done,
            Changed {
                rows: 0,
                last_insert_id: 0
            }
        );
    }

    #[test]
    fn an_answer_past_max_rows_is_the_apps_err_and_names_the_limit() {
        let mut sql = store(SqlLimits {
            max_rows: 3,
            ..SqlLimits::default()
        });
        sql.exec("CREATE TABLE t (n INTEGER)", &[]).unwrap();
        for n in 0..5 {
            sql.exec("INSERT INTO t VALUES (?1)", &[n.to_string()])
                .unwrap();
        }
        assert_eq!(
            sql.query("SELECT n FROM t LIMIT 3", &[])
                .unwrap()
                .rows
                .len(),
            3
        );
        let why = app(sql.query("SELECT n FROM t", &[]));
        assert!(why.contains("max_rows of 3"), "{why}");
    }

    #[test]
    fn an_answer_past_max_result_bytes_is_the_apps_err() {
        let mut sql = store(SqlLimits {
            max_result_bytes: 100,
            ..SqlLimits::default()
        });
        let why = app(sql.query(
            "WITH RECURSIVE c(x) AS (SELECT 'abcdefghij' UNION ALL SELECT x FROM c) \
             SELECT x FROM c LIMIT 50",
            &[],
        ));
        assert!(why.contains("max_result_bytes of 100"), "{why}");
    }

    #[test]
    fn attach_pragma_and_transactions_are_refused_by_the_authorizer() {
        let mut sql = store(SqlLimits::default());
        for statement in [
            "ATTACH DATABASE ':memory:' AS other",
            "PRAGMA foreign_keys = OFF",
            "BEGIN",
            "SAVEPOINT s",
            "CREATE VIRTUAL TABLE v USING fts5(x)",
        ] {
            let why = app(sql.exec(statement, &[]));
            assert!(why.contains("not authorized"), "{statement}: {why}");
        }
        // Reading a pragma is refused through `query` too.
        app(sql.query("PRAGMA page_size", &[]));
        // The ordinary things still work, schema changes included.
        sql.exec("CREATE TABLE t (a)", &[]).unwrap();
        sql.exec("ALTER TABLE t ADD COLUMN b", &[]).unwrap();
    }

    #[test]
    fn a_syntax_error_is_the_apps_err_and_names_no_file() {
        let dir = std::env::temp_dir().join(format!("minicloud-sql-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sql.sqlite3");
        {
            let mut sql = Store::open(&path, SqlLimits::default()).unwrap();
            let why = app(sql.query("SELEKT 1", &[]));
            assert!(why.contains("syntax error"), "{why}");
            assert!(!why.contains("minicloud-sql-path"), "{why}");
            assert!(!why.contains("sqlite3"), "{why}");
            let why = app(sql.exec("CREATE TABLE t (a); CREATE TABLE u (a)", &[]));
            assert!(!why.contains("minicloud-sql-path"), "{why}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exec_answers_the_rows_changed_and_the_last_insert_id() {
        let mut sql = store(SqlLimits::default());
        sql.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
            .unwrap();
        assert_eq!(
            sql.exec("INSERT INTO t (v) VALUES ('a')", &[]).unwrap(),
            Changed {
                rows: 1,
                last_insert_id: 1
            }
        );
        assert_eq!(
            sql.exec("INSERT INTO t (v) VALUES ('b')", &[]).unwrap(),
            Changed {
                rows: 1,
                last_insert_id: 2
            }
        );
        let done = sql.exec("UPDATE t SET v = 'z'", &[]).unwrap();
        assert_eq!(done.rows, 2);
        let done = sql
            .exec("DELETE FROM t WHERE id = ?1", &strs(&["9"]))
            .unwrap();
        assert_eq!(
            done,
            Changed {
                rows: 0,
                last_insert_id: 0
            }
        );
    }

    #[test]
    fn a_statement_longer_than_max_statement_bytes_is_refused_naming_the_limit() {
        let mut sql = store(SqlLimits {
            max_statement_bytes: 20,
            ..SqlLimits::default()
        });
        let why = app(sql.query("SELECT 'a long enough string'", &[]));
        assert!(why.contains("max_statement_bytes of 20"), "{why}");
        sql.query("SELECT 1", &[]).unwrap();
    }

    #[test]
    fn parameters_that_do_not_match_the_placeholders_are_refused_saying_how_many() {
        let mut sql = store(SqlLimits::default());
        let why = app(sql.query("SELECT ?1, ?2", &strs(&["a"])));
        assert!(why.contains("takes 2 parameters and 1 were given"), "{why}");
        let why = app(sql.exec("SELECT 1", &strs(&["a"])));
        assert!(why.contains("takes 0 parameters and 1 were given"), "{why}");
    }

    #[test]
    fn a_write_past_max_bytes_is_the_apps_err() {
        let mut sql = store(SqlLimits {
            max_bytes: 64 * 1024,
            ..SqlLimits::default()
        });
        sql.exec("CREATE TABLE t (v BLOB)", &[]).unwrap();
        let mut refused = None;
        for _ in 0..200 {
            match sql.exec("INSERT INTO t VALUES (randomblob(1000))", &[]) {
                Ok(_) => {}
                Err(e) => {
                    refused = Some(e);
                    break;
                }
            }
        }
        let why = app(Err::<(), _>(refused.expect("the database never filled")));
        assert!(why.contains("full"), "{why}");
    }

    #[test]
    fn a_statement_that_runs_past_max_statement_ms_is_stopped_with_the_apps_err() {
        let mut sql = store(SqlLimits {
            max_statement_ms: 1,
            ..SqlLimits::default()
        });
        let why = app(sql.query(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<100000000) \
             SELECT count(*) FROM c",
            &[],
        ));
        assert!(why.contains("max_statement_ms (1 ms)"), "{why}");
        // The handler is gone and the connection is usable.
        sql.query("SELECT 1", &[]).unwrap();
    }

    // ---------------------------------------------- the admin's window

    /// A store in a directory of its own, removed when the test is over.
    /// The window's two paths differ by whether anything has the file open,
    /// so these tests need a file.
    struct OnDisk(PathBuf);

    impl OnDisk {
        fn new(what: &str) -> OnDisk {
            let dir = std::env::temp_dir().join(format!(
                "minicloud-sql-window-{}-{what}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            OnDisk(dir)
        }

        fn path(&self) -> PathBuf {
            self.0.join("sql.sqlite3")
        }

        /// A store on the file, as an app's own would be, and not shared:
        /// nothing is registered in [`open_stores`], so the window opens the
        /// file itself. Dropped before the window reads it.
        fn stocked(&self) -> Store {
            let mut sql = Store::open(&self.path(), SqlLimits::default()).unwrap();
            sql.exec(
                "CREATE TABLE books (id INTEGER PRIMARY KEY, title TEXT)",
                &[],
            )
            .unwrap();
            sql.exec("CREATE TABLE notes (what TEXT)", &[]).unwrap();
            for title in ["Dune", "Ubik"] {
                sql.exec("INSERT INTO books (title) VALUES (?1)", &strs(&[title]))
                    .unwrap();
            }
            sql
        }
    }

    impl Drop for OnDisk {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn window_err(result: Result<impl std::fmt::Debug, SqlError>) -> String {
        match result {
            Err(SqlError::App(why)) => why,
            other => panic!("expected the window's Err, got {other:?}"),
        }
    }

    #[test]
    fn the_window_lists_the_tables_and_counts_their_rows() {
        let on_disk = OnDisk::new("tables");
        drop(on_disk.stocked());
        assert_eq!(
            peek_tables(&on_disk.path()).unwrap(),
            [
                Table {
                    name: "books".to_string(),
                    rows: 2,
                    counted: true
                },
                Table {
                    name: "notes".to_string(),
                    rows: 0,
                    counted: true
                }
            ]
        );
    }

    #[test]
    fn the_catalogue_leaves_out_sqlites_own_tables() {
        let on_disk = OnDisk::new("catalogue");
        let mut sql = on_disk.stocked();
        // An `AUTOINCREMENT` makes `sqlite_sequence`, which is SQLite's
        // bookkeeping and not the app's data.
        sql.exec("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)", &[])
            .unwrap();
        sql.exec("INSERT INTO t DEFAULT VALUES", &[]).unwrap();
        drop(sql);
        let listed: Vec<String> = peek_tables(&on_disk.path())
            .unwrap()
            .into_iter()
            .map(|table| table.name)
            .collect();
        assert_eq!(listed, ["books", "notes", "t"]);
    }

    #[test]
    fn the_window_refuses_every_statement_that_writes() {
        let on_disk = OnDisk::new("reads");
        drop(on_disk.stocked());
        for statement in [
            "INSERT INTO books (title) VALUES ('x')",
            "UPDATE books SET title = 'x'",
            "DELETE FROM books",
            "CREATE TABLE other (v TEXT)",
            "DROP TABLE books",
            "ALTER TABLE books RENAME TO shelf",
        ] {
            let why = window_err(peek_query(&on_disk.path(), statement));
            assert!(why.contains("only reads"), "{statement}: {why}");
        }
        // Nothing of it happened.
        assert_eq!(
            peek_tables(&on_disk.path())
                .unwrap()
                .into_iter()
                .map(|table| (table.name, table.rows))
                .collect::<Vec<(String, i64)>>(),
            [("books".to_string(), 2), ("notes".to_string(), 0)]
        );
    }

    /// The authorizer belongs to a `Store` and the window's own connection is
    /// not one, so it is installed on that connection too. Without it this
    /// path would allow what the app's is refused — `ATTACH` naming another
    /// file, `PRAGMA`, a transaction left open — since none of those is a
    /// write that `readonly` would catch.
    #[test]
    fn the_authorizer_holds_on_the_connection_the_window_opens() {
        let on_disk = OnDisk::new("authorizer");
        drop(on_disk.stocked());
        for statement in [
            "ATTACH DATABASE 'elsewhere.sqlite3' AS other",
            "PRAGMA journal_mode",
            "PRAGMA database_list",
            "BEGIN",
            "COMMIT",
            "SAVEPOINT s",
        ] {
            let why = window_err(peek_query(&on_disk.path(), statement));
            assert!(why.contains("not authorized"), "{statement}: {why}");
        }
    }

    #[test]
    fn two_statements_in_one_are_refused_by_the_window_as_well() {
        let on_disk = OnDisk::new("tail");
        drop(on_disk.stocked());
        let why = window_err(peek_query(&on_disk.path(), "SELECT 1; DROP TABLE books"));
        assert!(why.contains("one statement per call"), "{why}");
        assert_eq!(peek_tables(&on_disk.path()).unwrap().len(), 2);
    }

    #[test]
    fn the_window_says_when_it_has_reached_its_row_limit() {
        let on_disk = OnDisk::new("rows");
        drop(on_disk.stocked());
        let rows = |n: u64| {
            format!(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<{n}) \
                 SELECT x FROM c"
            )
        };
        assert_eq!(
            peek_query(&on_disk.path(), &rows(PEEK_ROWS))
                .unwrap()
                .rows
                .len() as u64,
            PEEK_ROWS
        );
        let why = window_err(peek_query(&on_disk.path(), &rows(PEEK_ROWS + 1)));
        assert!(why.contains(&format!("max_rows of {PEEK_ROWS}")), "{why}");
        // Whose limit it is, said: the app's `[sql]` is not what to raise.
        assert!(why.contains("the admin window's"), "{why}");
    }

    #[test]
    fn a_null_cell_is_not_an_empty_one_in_the_window_either() {
        let on_disk = OnDisk::new("null");
        drop(on_disk.stocked());
        let answer = peek_query(&on_disk.path(), "SELECT NULL, '', 'x', 42").unwrap();
        assert_eq!(
            answer.rows,
            [[
                None,
                Some(String::new()),
                Some("x".to_string()),
                Some("42".to_string())
            ]]
        );
    }

    #[test]
    fn an_app_with_no_store_is_no_tables_and_a_statement_is_told_so() {
        let on_disk = OnDisk::new("absent");
        let path = on_disk.path();
        assert!(!path.exists());
        assert_eq!(peek_tables(&path).unwrap(), []);
        let why = window_err(peek_query(&path, "SELECT 1"));
        assert!(why.contains("no sql store"), "{why}");
        // Looking made no file: a store is the app's to create.
        assert!(!path.exists());
    }

    /// The window goes through the store the app has open where there is one.
    /// A temporary table is the proof: it belongs to the connection that made
    /// it, so a window on a second connection would answer `no such table`.
    #[test]
    fn the_window_reads_through_the_store_the_app_has_open() {
        let on_disk = OnDisk::new("open");
        let path = on_disk.path();
        let store = shared(&path, &SqlLimits::default()).unwrap();
        {
            let mut app = store.lock().unwrap();
            app.exec("CREATE TABLE books (title TEXT)", &[]).unwrap();
            app.exec("CREATE TEMP TABLE scratch (v TEXT)", &[]).unwrap();
            app.exec("INSERT INTO scratch VALUES ('here')", &[])
                .unwrap();
        }
        assert_eq!(
            peek_query(&path, "SELECT v FROM scratch").unwrap().rows,
            [[Some("here".to_string())]]
        );
        // And a row written a moment ago is there, which is the case this is
        // for: a second connection on a WAL database would read the snapshot
        // it started with.
        store
            .lock()
            .unwrap()
            .exec("INSERT INTO books VALUES ('Fresh')", &[])
            .unwrap();
        assert_eq!(
            peek_query(&path, "SELECT title FROM books").unwrap().rows,
            [[Some("Fresh".to_string())]]
        );
        // The window's deadline is not left armed on the app's connection.
        store.lock().unwrap().query("SELECT 1", &[]).unwrap();
    }
}
