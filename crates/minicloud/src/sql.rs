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
use rusqlite::{params_from_iter, Connection, ErrorCode};

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
        self.timed(|conn| {
            let mut stmt = prepare(conn, &limits, statement, params, "sql.query")?;
            if !stmt.readonly() {
                return Err(SqlError::App(
                    "sql.query: use sql.exec for a statement that writes".into(),
                ));
            }
            let columns: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
            let mut rows = stmt
                .query(params_from_iter(params.iter()))
                .map_err(|e| failure(e, &limits, "sql.query"))?;
            let mut answer = Vec::new();
            let mut bytes = 0u64;
            while let Some(row) = rows.next().map_err(|e| failure(e, &limits, "sql.query"))? {
                // A query past `max_rows` is an `Err` and not the first
                // `max_rows` of it: a page that silently showed the start
                // of something would be wrong, where an error is only loud.
                if answer.len() as u64 >= limits.max_rows {
                    return Err(SqlError::App(format!(
                        "sql.query: the answer has more than this app's [sql] max_rows of {}; \
                         add a LIMIT or a narrower WHERE",
                        limits.max_rows
                    )));
                }
                let mut values = Vec::with_capacity(columns.len());
                for at in 0..columns.len() {
                    let value = text_of(
                        row.get_ref(at)
                            .map_err(|e| failure(e, &limits, "sql.query"))?,
                    )?;
                    bytes += value.as_ref().map_or(0, |v| v.len() as u64);
                    values.push(value);
                }
                if bytes > limits.max_result_bytes {
                    return Err(SqlError::App(format!(
                        "sql.query: the answer is above this app's [sql] max_result_bytes of {}",
                        limits.max_result_bytes
                    )));
                }
                answer.push(values);
            }
            Ok(Answer {
                columns,
                rows: answer,
            })
        })
    }

    /// Runs a statement that writes. A statement that reads is not refused;
    /// it changes no rows.
    pub fn exec(&mut self, statement: &str, params: &[String]) -> Result<Changed, SqlError> {
        let limits = self.limits.clone();
        self.timed(|conn| {
            let mut stmt = prepare(conn, &limits, statement, params, "sql.exec")?;
            let before = conn.total_changes();
            let mut rows = stmt
                .query(params_from_iter(params.iter()))
                .map_err(|e| failure(e, &limits, "sql.exec"))?;
            // A statement with `RETURNING` answers rows; they are discarded,
            // but must be stepped through for the write to happen.
            while rows
                .next()
                .map_err(|e| failure(e, &limits, "sql.exec"))?
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

    /// Runs `body` with the statement deadline armed on the connection.
    fn timed<T>(
        &mut self,
        body: impl FnOnce(&Connection) -> Result<T, SqlError>,
    ) -> Result<T, SqlError> {
        let until = Instant::now() + Duration::from_millis(self.limits.max_statement_ms);
        self.conn
            .progress_handler(1000, Some(move || Instant::now() >= until));
        let outcome = body(&self.conn);
        self.conn.progress_handler(0, None::<fn() -> bool>);
        outcome
    }
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
    who: &str,
) -> Result<rusqlite::Statement<'c>, SqlError> {
    if statement.len() as u64 > limits.max_statement_bytes {
        return Err(SqlError::App(format!(
            "{who}: a statement of {} bytes is above this app's [sql] max_statement_bytes of {}",
            statement.len(),
            limits.max_statement_bytes
        )));
    }
    let stmt = conn
        .prepare(statement)
        .map_err(|e| failure(e, limits, who))?;
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
fn text_of(value: ValueRef<'_>) -> Result<Option<String>, SqlError> {
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
            return Err(SqlError::App(
                "sql.query: a blob is not text; store it as hex or base64".into(),
            ))
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
fn failure(error: rusqlite::Error, limits: &SqlLimits, who: &str) -> SqlError {
    use rusqlite::Error;
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
            "{who}: the statement ran longer than [sql] max_statement_ms ({} ms)",
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
}
