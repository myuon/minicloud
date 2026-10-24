//! Changing a running app's configuration: what the admin app does through
//! the `host` module ([`crate::admin_module`]), and what `minicloud
//! enable`/`disable`/`reset` do through the admin listener.
//!
//! **A change is a re-check, never a revocation.** Cove's capabilities are
//! decided before a program runs — the checker derives what the entry
//! requires and the host refuses what is not granted — so there is no taking
//! a capability away from a run in progress. A change to an app's grant,
//! allowlist or limits loads the app again with the change applied, exactly
//! as `minicloud update` loads a new version (parse, check, admit, lower,
//! prepare, compile, off the request path), and routes to that version:
//! requests already admitted finish on the version they were admitted to.
//!
//! What comes of it:
//!
//! | the reload | the change | the app |
//! | --- | --- | --- |
//! | loads | kept, written to `overrides.json` | serves the new version |
//! | does not admit: its entry requires a capability the change took away | kept | **refused**, answering 503 with the reason; every other app serves |
//! | anything else — a limit out of range, an allowlist entry or capability that does not parse, an unknown capability, `admin` for an app that is not `admin`, a check that fails | **not kept** | serves as before; the reasons are the answer |
//!
//! Taking a capability away from an app that needs it is a decision the
//! admin may make — that is how an app is stopped from using something — so
//! it is applied; a change that is merely wrong is refused. The admin app is
//! the exception to the first row: a change that would leave *it* refused,
//! or without `admin`, is refused, as is disabling it — through the admin app.
//! The admin listener (`minicloud disable admin`, `minicloud reset admin`)
//! can still do either: it is the emergency exit, and it does not depend on
//! the admin app working.
//!
//! **Disabling** stops routing to the app and nothing else: it keeps its
//! version loaded, its queue, counters, log and store. New requests are
//! answered 503 (`app … is disabled by the administrator`, counted as
//! `rejected.disabled`); those already admitted finish. Enabling routes to it
//! again, with its data as it was.
//!
//! **Secrets** ([`crate::secrets`]) are changed here too. Setting one
//! writes the store first and then reloads every app whose `app.toml` takes
//! a secret from it (`{ store = "<name>" }`), as an update would: an app
//! that loads is routed to with the new value and requests in flight finish
//! on the old; one that does not keeps serving what it served, and the
//! answer says so — the value stays stored either way. Deleting one that an
//! app uses is refused unless forced; forced, the apps are reloaded without
//! it and installed refused, so the value is no longer used anywhere. The
//! history records the secret's name and the action, never the value.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::Value as Json;

use crate::apps::{load_as, load_with, AppState, Lineage};
use crate::code::CodeFile;
use crate::config::{
    read_app_with, store_references, AppOverride, LimitsFile, RemovedKeys, ADMIN_APP,
    ADMIN_CAPABILITY,
};
use crate::server::Front;

/// Who the change history says made a change through the admin listener.
pub const LISTENER: &str = "admin listener";

/// Where a change came from.
pub enum Via {
    /// The admin app, with what it says of its user.
    AdminApp(String),
    /// The admin listener: the CLI, on the machine, with the admin token.
    Listener,
}

impl Via {
    fn who(&self) -> String {
        match self {
            Via::AdminApp(who) if who.trim().is_empty() => "admin app".to_string(),
            Via::AdminApp(who) => format!("admin app: {}", who.trim()),
            Via::Listener => LISTENER.to_string(),
        }
    }

    fn is_admin_app(&self) -> bool {
        matches!(self, Via::AdminApp(_))
    }
}

/// Why a change was not made.
#[derive(Debug)]
pub enum ChangeError {
    /// No app by that name.
    NotFound(String),
    /// The change was refused; the configuration is as it was.
    Refused(String),
    /// A secret an app uses, not deleted without `force`.
    InUse(String),
}

impl std::fmt::Display for ChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeError::NotFound(why) | ChangeError::Refused(why) | ChangeError::InUse(why) => {
                f.write_str(why)
            }
        }
    }
}

/// One secret as the admin sees it — never its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretInfo {
    pub name: String,
    /// Whether the store has it.
    pub set: bool,
    /// When it was last set, if it is.
    pub updated_ms: Option<u64>,
    /// The apps whose `app.toml` takes a secret from it.
    pub apps: Vec<String>,
}

/// What came of reloading one app after a secret changed.
#[derive(Clone, Debug)]
pub struct Reloaded {
    pub app: String,
    /// Whether it serves the new version.
    pub ok: bool,
    /// `serving v3-…`, `refused, still serving v2-…: why`, or `now
    /// refused: why`.
    pub outcome: String,
}

/// What a secret change did.
#[derive(Clone, Debug)]
pub struct SecretChange {
    pub secret: String,
    pub message: String,
    pub reloaded: Vec<Reloaded>,
}

/// What an app's configuration is to become: the grant, the allowlist, and
/// the limits (each key set to its effective value).
#[derive(Clone, Debug)]
pub struct Settings {
    pub grant: BTreeSet<String>,
    pub fetch_allow: Vec<String>,
    pub limits: LimitsFile,
}

/// One recent error of an app.
pub struct Failure {
    pub at_ms: u64,
    pub kind: String,
    pub status: u64,
    pub version: String,
    pub message: String,
}

/// One app as the admin app lists it.
pub struct AppInfo {
    pub name: String,
    /// `serving`, `disabled`, `refused` or `removed`.
    pub state: String,
    pub reason: String,
    pub version: String,
    pub tier: String,
    pub entry: String,
    pub hosts: Vec<String>,
    pub required: Vec<String>,
    pub required_open: bool,
    pub granted: Vec<String>,
    pub grant_added: Vec<String>,
    pub grant_removed: Vec<String>,
    pub fetch_allow: Vec<String>,
    pub limits: LimitsFile,
    pub limits_changed: Vec<String>,
    pub is_admin: bool,
    pub served: u64,
    pub ok: u64,
    pub errors: u64,
    pub rejected: u64,
    pub in_flight: u64,
    pub queued: u64,
    /// Garbage collections and runs that collected, words allocated, and
    /// runs that failed for want of heap (`errors.heap`), all since the
    /// host started ([`crate::stats::AppCounters`]).
    pub gc_collections: u64,
    pub gc_runs: u64,
    pub allocated_words: u64,
    pub heap_failures: u64,
    /// `(keys, bytes)` in its store, when one is open.
    pub kv: Option<(u64, u64)>,
    /// `[kv] max_keys` and `[kv] max_bytes`: what that store may hold, so a
    /// page can say when it is near them rather than only how large it is.
    pub kv_max_keys: u64,
    pub kv_max_bytes: u64,
    /// The app's SQL store on disk, in bytes, when it has one.
    pub sql_bytes: Option<u64>,
    /// `[sql] max_bytes`.
    pub sql_max_bytes: u64,
    /// Why `[access]` is written and off ([`crate::access::Access::off`]);
    /// "" where Access is on, and "" where the app asks for none. An app
    /// that asked for Access and did not get it is serving to anybody who
    /// reaches it, which is worth saying on its page.
    pub access_off: String,
    pub recent_errors: Vec<Failure>,
}

/// The `[limits]` keys `limits` sets.
fn limit_keys(limits: &LimitsFile) -> Vec<String> {
    let json = serde_json::to_value(limits).unwrap_or_default();
    json.as_object()
        .map(|keys| keys.keys().cloned().collect())
        .unwrap_or_default()
}

/// A JSON group of counters, summed.
fn sum(group: &Json) -> u64 {
    group
        .as_object()
        .map(|counts| counts.values().filter_map(Json::as_u64).sum())
        .unwrap_or_default()
}

/// Why the SQL window answered no rows, as the page says it.
///
/// The statement's own refusals are SQLite's words and the limits' names,
/// which are what a person reading the page can act on; a failure of the
/// file is said as the host's, so that the two are not mistaken for each
/// other.
fn sql_why(error: crate::sql::SqlError) -> String {
    match error {
        crate::sql::SqlError::App(why) => why,
        crate::sql::SqlError::Storage(why) => format!("admin: the store failed: {why}"),
    }
}

/// Now, in seconds since the epoch, as the queue store keeps a `due` and a
/// `sent`: whether a message is due is a comparison with this.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

/// One queue of an app, as the admin app sees it: what it holds now, and
/// the settings the host delivers it by.
///
/// The counts are whole ([`crate::queue::peek_depths`]), not a page's worth,
/// so a listing cut at the window's limit can say how much it is not
/// showing.
pub(crate) struct QueueInfo {
    pub name: String,
    /// Where a message is delivered, below the app's prefix.
    pub path: String,
    pub due: i64,
    pub held: i64,
    pub dead: i64,
    /// Whether the app's `[[queues]]` still declares it. A queue taken out
    /// of `app.toml` keeps whatever was on it, and nothing will ever
    /// deliver it: the window shows it saying so, because a store that
    /// holds work nobody will do is the thing this page is for. Its
    /// settings are 0, because it no longer has any.
    pub declared: bool,
    pub max_depth: i64,
    pub max_attempts: u32,
    pub retry_ms: u64,
    pub lease_ms: u64,
    pub batch: u32,
    pub max_body_bytes: u64,
}

/// One line of some app's log, with the app it came from.
///
/// The app is on the line and not on the call because the host's whole log
/// is one sequence: a page that mixes two apps' lines has to say which app
/// each came from, and a page that shows one app's has the name already and
/// draws no column for it.
pub(crate) struct AppLine {
    pub app: String,
    /// Milliseconds since the epoch, as [`crate::logs::Line`] read it; 0 for
    /// a line it could not read.
    pub at_ms: u64,
    pub level: String,
    pub text: String,
}

/// Every app's log as one sequence, and who has one.
pub(crate) struct AllLogs {
    /// Every app whose ring holds a line, in load order.
    ///
    /// This is not "the apps in `lines`": an app that logged an hour ago is
    /// here even when nothing of its is in the window, because the page
    /// offers these as the way to each app's own log and a way in that comes
    /// and goes with `?n=` is not a way in.
    pub apps: Vec<String>,
    /// The last lines, oldest first.
    pub lines: Vec<AppLine>,
}

/// One key of an app's store, as the admin app sees it.
pub(crate) struct KvRow {
    pub key: String,
    pub value: String,
    /// The whole value's length in bytes.
    pub bytes: u64,
    /// Whether `value` is shorter than the whole.
    pub cut: bool,
}

impl From<crate::kv::Peek> for KvRow {
    fn from(peek: crate::kv::Peek) -> KvRow {
        KvRow {
            key: peek.key,
            value: peek.value,
            bytes: peek.bytes,
            cut: peek.cut,
        }
    }
}

impl Front {
    /// Every app the host has, in load order.
    pub(crate) fn app_infos(&self) -> Vec<AppInfo> {
        let context = self.ops_context();
        self.engine
            .slots()
            .iter()
            .map(|slot| {
                let json = crate::ops::app_json(&context, slot);
                let over = self.control.overrides.get(&slot.name);
                let mut info = AppInfo {
                    name: slot.name.clone(),
                    state: "removed".to_string(),
                    reason: String::new(),
                    version: String::new(),
                    tier: String::new(),
                    entry: String::new(),
                    hosts: Vec::new(),
                    required: Vec::new(),
                    required_open: false,
                    granted: Vec::new(),
                    grant_added: over
                        .as_ref()
                        .map(|o| o.grant_add.clone())
                        .unwrap_or_default(),
                    grant_removed: over
                        .as_ref()
                        .map(|o| o.grant_remove.clone())
                        .unwrap_or_default(),
                    fetch_allow: Vec::new(),
                    limits: LimitsFile::default(),
                    limits_changed: over
                        .as_ref()
                        .map(|o| limit_keys(&o.limits))
                        .unwrap_or_default(),
                    is_admin: slot.name == ADMIN_APP,
                    served: json["served"].as_u64().unwrap_or_default(),
                    ok: json["ok"].as_u64().unwrap_or_default(),
                    errors: sum(&json["errors"]),
                    rejected: sum(&json["rejected"]),
                    in_flight: json["in_flight"].as_u64().unwrap_or_default(),
                    queued: json["queued"].as_u64().unwrap_or_default(),
                    gc_collections: json["gc_collections"].as_u64().unwrap_or_default(),
                    gc_runs: json["gc_runs"].as_u64().unwrap_or_default(),
                    allocated_words: json["allocated_words"].as_u64().unwrap_or_default(),
                    heap_failures: json["errors"]["heap"].as_u64().unwrap_or_default(),
                    kv: json["kv"]["keys"]
                        .as_u64()
                        .zip(json["kv"]["bytes"].as_u64()),
                    kv_max_keys: 0,
                    kv_max_bytes: 0,
                    sql_bytes: None,
                    sql_max_bytes: 0,
                    access_off: String::new(),
                    recent_errors: slot
                        .counters
                        .recent_errors()
                        .iter()
                        .rev()
                        .take(10)
                        .map(|error| Failure {
                            at_ms: error["unix_ms"].as_u64().unwrap_or_default(),
                            kind: error["kind"].as_str().unwrap_or_default().to_string(),
                            status: error["status"].as_u64().unwrap_or_default(),
                            version: error["version"].as_str().unwrap_or_default().to_string(),
                            message: error["message"].as_str().unwrap_or_default().to_string(),
                        })
                        .collect(),
                };
                if let Some(app) = slot.current() {
                    let (state, reason, tier) = match &app.state {
                        AppState::Ready(ready) if slot.enabled() => {
                            ("serving", String::new(), ready.tier)
                        }
                        AppState::Ready(ready) => ("disabled", String::new(), ready.tier),
                        AppState::Refused(why) if slot.enabled() => ("refused", why.clone(), ""),
                        AppState::Refused(why) => ("disabled", why.clone(), ""),
                    };
                    info.state = state.to_string();
                    info.reason = reason;
                    info.tier = tier.to_string();
                    info.version = app.version.clone();
                    info.entry = app.entry.clone();
                    info.hosts = app.hosts.clone();
                    info.required = app.required.iter().cloned().collect();
                    info.required_open = app.open;
                    info.granted = app.granted.iter().cloned().collect();
                    info.fetch_allow = app.fetch_allow.clone();
                    info.limits = app.limits.as_file();
                    info.kv_max_keys = app.kv.max_keys;
                    info.kv_max_bytes = app.kv.max_bytes;
                    info.sql_max_bytes = app.sql.max_bytes;
                    info.access_off = app.access.off.clone().unwrap_or_default();
                    // Two `stat`s, and only for an app that has a store:
                    // the host keeps no file for one that is not granted
                    // `sql`, so there would be nothing to size.
                    if app.granted.contains("sql") {
                        info.sql_bytes = self
                            .sql_path(&slot.name)
                            .and_then(|path| crate::sql::usage(&path));
                    }
                }
                info
            })
            .collect()
    }

    /// The version of the admin app itself — the string every answer of
    /// its carries as `x-cove-app-version` ([`crate::sched::VERSION_HEADER`])
    /// — or "" when the host cannot say which that is.
    ///
    /// It takes no app name because only the app named `admin` is ever
    /// granted `admin` ([`ADMIN_APP`]), so "the app asking" is that slot and
    /// nothing else. It is a slot lookup and one `String` clone, which is
    /// what lets every page ask: [`Front::app_infos`] answers the same
    /// question only by reading every app's counters, quotas and store size.
    ///
    /// "" rather than an error for an admin app with no version loaded: the
    /// caller stamps a URL with this or leaves it alone, and a page that
    /// could not be told the version is not a page that should fail.
    pub(crate) fn self_version(&self) -> String {
        self.engine
            .slot_named(ADMIN_APP)
            .and_then(|slot| slot.current())
            .map(|app| app.version.clone())
            .unwrap_or_default()
    }

    /// Every capability the host's modules declare: what a grant may name.
    pub(crate) fn capabilities(&self) -> Vec<String> {
        let mut capabilities: Vec<String> = self
            .options
            .modules
            .schemas()
            .iter()
            .flat_map(|schema| {
                std::iter::once(schema.capability)
                    .chain(schema.operations.iter().map(|op| op.capability))
            })
            .map(str::to_string)
            .collect();
        capabilities.sort();
        capabilities.dedup();
        capabilities
    }

    /// The file of `app`'s store, for an app the host has: a name that is
    /// not one of its slots names nothing, so no path leaves the data
    /// directory.
    fn kv_path(&self, app: &str) -> Option<std::path::PathBuf> {
        self.engine.slot_named(app)?;
        let data = self.options.data.as_ref()?;
        Some(data.join(app).join("kv.sqlite3"))
    }

    /// Keys of `app`'s store. Empty for an app that has none — not granted
    /// `kv`, never written to, or a host with no data directory.
    pub(crate) fn app_kv(
        &self,
        app: &str,
        prefix: &str,
        from: &str,
        limit: i64,
    ) -> Result<Vec<KvRow>, String> {
        let Some(path) = self.kv_path(app) else {
            return Ok(Vec::new());
        };
        Ok(crate::kv::peek(&path, prefix, from, limit)?
            .into_iter()
            .map(KvRow::from)
            .collect())
    }

    /// One key of `app`'s store, whole (cut past
    /// [`crate::kv::READ_VALUE_CHARS`]).
    pub(crate) fn app_kv_value(&self, app: &str, key: &str) -> Result<Option<KvRow>, String> {
        let Some(path) = self.kv_path(app) else {
            return Ok(None);
        };
        Ok(crate::kv::read(&path, key)?.map(KvRow::from))
    }

    /// The file of `app`'s SQL store, under the same guard as
    /// [`Front::kv_path`]: a name that is not one of the host's slots names
    /// no path, so nothing a request can write reaches outside the data
    /// directory.
    fn sql_path(&self, app: &str) -> Option<std::path::PathBuf> {
        self.engine.slot_named(app)?;
        let data = self.options.data.as_ref()?;
        Some(data.join(app).join("sql.sqlite3"))
    }

    /// The tables of `app`'s SQL store and how many rows each holds. Empty
    /// for an app that has none — not granted `sql`, never written to, or a
    /// host with no data directory ([`crate::sql::peek_tables`]).
    pub(crate) fn app_sql_tables(&self, app: &str) -> Result<Vec<crate::sql::Table>, String> {
        let Some(path) = self.sql_path(app) else {
            return Ok(Vec::new());
        };
        crate::sql::peek_tables(&path).map_err(sql_why)
    }

    /// Runs one read-only statement against `app`'s SQL store
    /// ([`crate::sql::peek_query`]).
    ///
    /// Both kinds of failure come back as the window's `Err` rather than the
    /// host's error: a store that cannot be read is exactly what somebody on
    /// this page came to find out, and a page that answered 500 would say
    /// less than the message does.
    pub(crate) fn app_sql_query(
        &self,
        app: &str,
        statement: &str,
    ) -> Result<crate::sql::Answer, String> {
        let Some(path) = self.sql_path(app) else {
            return Err(format!("the host has no app `{app}`"));
        };
        crate::sql::peek_query(&path, statement).map_err(sql_why)
    }

    /// The version of `app` new requests go to, for an app the host has: a
    /// name that is not one of its slots names nothing, the same guard
    /// [`Front::kv_path`] rests on.
    fn deployed(&self, app: &str) -> Option<Arc<crate::apps::App>> {
        self.engine.slot_named(app)?.current()
    }

    /// Every file of the version `app` is running, at most `limit`
    /// ([`crate::code`]). Empty for an app the host does not have, and for
    /// one whose slot has no version left (a removed app).
    pub(crate) fn app_code(&self, app: &str, limit: i64) -> Vec<CodeFile> {
        match self.deployed(app) {
            Some(found) => crate::code::listing(&found, limit),
            None => Vec::new(),
        }
    }

    /// How many files of `app`'s deploy the app's directory holds
    /// differently ([`crate::code::disk_check`]), for the Overview, which
    /// wants the number and not the list.
    pub(crate) fn app_code_disk(&self, app: &str) -> crate::code::DiskCheck {
        match self.deployed(app) {
            Some(found) => crate::code::disk_check(&found, crate::code::DISK_CHECK_BYTES),
            None => crate::code::DiskCheck {
                looked: true,
                differing: 0,
                bytes: 0,
            },
        }
    }

    /// One file of the version `app` is running, by the name a listing gave
    /// it, its text cut past [`crate::code::READ_TEXT_BYTES`].
    pub(crate) fn app_code_file(&self, app: &str, name: &str) -> Result<CodeFile, String> {
        match self.deployed(app) {
            Some(found) => crate::code::one(&found, name),
            None => Err(format!("the host has no app `{app}` with a version loaded")),
        }
    }

    /// The file of `app`'s queue store, under the same guard as
    /// [`Front::kv_path`]: a name that is not one of the host's slots names
    /// no path, so nothing a request can write reaches outside the data
    /// directory.
    fn queue_path(&self, app: &str) -> Option<std::path::PathBuf> {
        self.engine.slot_named(app)?;
        let data = self.options.data.as_ref()?;
        Some(data.join(app).join("queue.sqlite3"))
    }

    /// Each queue of `app` and what it holds now (issue 90).
    ///
    /// **The queues are the app's `[[queues]]`**, not the store's rows: a
    /// queue declared and never sent to has no rows, and leaving it out
    /// would read as "there is no such queue" when the answer is "nothing is
    /// waiting on it". A queue the store has rows for and the app no longer
    /// declares is listed after them, saying so.
    ///
    /// Empty for an app the host does not have or has no version of.
    pub(crate) fn app_queues(&self, app: &str) -> Result<Vec<QueueInfo>, String> {
        let Some(found) = self.deployed(app) else {
            return Ok(Vec::new());
        };
        let depths = match self.queue_path(app) {
            Some(path) => crate::queue::peek_depths(&path, now_unix())?,
            None => Vec::new(),
        };
        // What the store holds for a queue by that name; nothing at all for
        // one that has never been sent to.
        let counts = |name: &str| {
            depths
                .iter()
                .find(|depth| depth.name == name)
                .map(|depth| (depth.due, depth.held, depth.dead))
                .unwrap_or((0, 0, 0))
        };
        let mut out: Vec<QueueInfo> = found
            .queues
            .iter()
            .map(|policy| {
                let (due, held, dead) = counts(&policy.name);
                QueueInfo {
                    name: policy.name.clone(),
                    path: policy.path.clone(),
                    due,
                    held,
                    dead,
                    declared: true,
                    max_depth: policy.max_depth,
                    max_attempts: policy.max_attempts,
                    retry_ms: policy.retry.as_millis() as u64,
                    lease_ms: policy.lease.as_millis() as u64,
                    batch: policy.batch,
                    max_body_bytes: policy.max_body_bytes,
                }
            })
            .collect();
        for depth in &depths {
            if found.queues.iter().any(|policy| policy.name == depth.name) {
                continue;
            }
            out.push(QueueInfo {
                name: depth.name.clone(),
                path: String::new(),
                due: depth.due,
                held: depth.held,
                dead: depth.dead,
                declared: false,
                max_depth: 0,
                max_attempts: 0,
                retry_ms: 0,
                lease_ms: 0,
                batch: 0,
                max_body_bytes: 0,
            });
        }
        Ok(out)
    }

    /// The messages still on `app`'s queues, at most `limit`
    /// ([`crate::queue::peek_waiting`]). A read: nothing is leased, counted
    /// or moved by it.
    pub(crate) fn app_queue_messages(
        &self,
        app: &str,
        limit: i64,
    ) -> Result<Vec<crate::queue::Peeked>, String> {
        let Some(path) = self.queue_path(app) else {
            return Ok(Vec::new());
        };
        crate::queue::peek_waiting(&path, now_unix(), limit)
    }

    /// The dead letters of `app`'s queues, newest buried first, at most
    /// `limit` ([`crate::queue::peek_dead`]).
    pub(crate) fn app_queue_dead(
        &self,
        app: &str,
        limit: i64,
    ) -> Result<Vec<crate::queue::Peeked>, String> {
        let Some(path) = self.queue_path(app) else {
            return Ok(Vec::new());
        };
        crate::queue::peek_dead(&path, limit)
    }

    /// One message of `app`'s queues, waiting or buried, with its body cut
    /// at [`crate::queue::READ_BODY_CHARS`] rather than the listing's
    /// length ([`crate::queue::peek_one`]).
    pub(crate) fn app_queue_message(
        &self,
        app: &str,
        id: i64,
    ) -> Result<Option<crate::queue::Peeked>, String> {
        let Some(path) = self.queue_path(app) else {
            return Ok(None);
        };
        crate::queue::peek_one(&path, id, now_unix())
    }

    /// The last `n` lines of `app`'s log, oldest first; empty for an app the
    /// host does not have.
    pub(crate) fn app_logs(&self, app: &str, n: usize) -> Vec<crate::logs::Line> {
        self.engine
            .slot_named(app)
            .map(|slot| slot.logs.tail_lines(n))
            .unwrap_or_default()
    }

    /// Every app's log as one sequence, the last `n` lines of it, oldest
    /// first (issue #108).
    ///
    /// **"The last `n`" is the last `n` of the host's whole log**, and not a
    /// sample of each app's: every line of the host's log is a line of some
    /// app's, so the newest `n` across the host are all among the newest `n`
    /// of the apps they came from. Taking `n` from each ring and cutting the
    /// merge back to `n` is therefore exactly those lines, however many apps
    /// there are.
    ///
    /// Which means **one app that is talking fills the page**, and the
    /// others' lines fall off the end. That is what the last `n` lines of a
    /// log *are*; the page's `?n=` is how to see past the talking, and an
    /// app's own Logs is how to read it alone. The alternative — `n` lines
    /// from each app, kept — is a page whose top line is a different time
    /// per app, which is not one sequence and is the thing this answers.
    ///
    /// A line [`crate::logs::Line`] could not read has no time of its own
    /// (`at_ms` 0). It is never dropped: a log that loses lines is the worst
    /// thing this could do. It is not sorted to the epoch either, which
    /// would bury it under everything and cut it first — it is ordered by
    /// the line before it in its own ring, which is where the ring put it,
    /// and it still says 0, because that is all that is known about it.
    pub(crate) fn all_logs(&self, n: usize) -> AllLogs {
        let mut apps = Vec::new();
        // The sort key beside the line: the line's own time, or the last one
        // before it in the same ring.
        let mut keyed: Vec<(u64, AppLine)> = Vec::new();
        for slot in self.engine.slots() {
            let tail = slot.logs.tail_lines(n);
            if tail.is_empty() {
                continue;
            }
            apps.push(slot.name.clone());
            let mut last = 0;
            for line in tail {
                if line.at_ms != 0 {
                    last = line.at_ms;
                }
                keyed.push((
                    last,
                    AppLine {
                        app: slot.name.clone(),
                        at_ms: line.at_ms,
                        level: line.level,
                        text: line.text,
                    },
                ));
            }
        }
        // Stable, so two apps that logged in the same millisecond stay in
        // load order and a reload of the page is the same page.
        keyed.sort_by_key(|(at, _)| *at);
        let over = keyed.len().saturating_sub(n);
        AllLogs {
            apps,
            lines: keyed.drain(over..).map(|(_, line)| line).collect(),
        }
    }

    /// Records a change refused before anything was tried.
    pub(crate) fn record_refusal(&self, app: &str, action: &str, via: &Via, why: &str) {
        self.control
            .history
            .record(&via.who(), app, action, "", &format!("refused: {why}"));
    }

    /// Routes to `name` again, or stops routing to it.
    pub(crate) async fn set_enabled(
        &self,
        name: &str,
        enabled: bool,
        via: &Via,
    ) -> Result<String, ChangeError> {
        let action = if enabled { "enable" } else { "disable" };
        let _one_at_a_time = self.updating.lock().await;
        let result = (|| {
            let Some(slot) = self.engine.slot_named(name) else {
                return Err(ChangeError::NotFound(format!("no app named `{name}`")));
            };
            if !enabled && name == ADMIN_APP && via.is_admin_app() {
                return Err(ChangeError::Refused(format!(
                    "the admin app cannot disable itself; on the machine, `minicloud disable \
                     {ADMIN_APP}` can"
                )));
            }
            let mut over = self
                .control
                .overrides
                .get(name)
                .unwrap_or_else(AppOverride::none);
            over.enabled = enabled;
            self.control
                .overrides
                .set(name, over)
                .map_err(ChangeError::Refused)?;
            let was = slot.enabled();
            slot.set_enabled(enabled);
            let line = match (was, enabled) {
                (true, true) => "enabled (it was)".to_string(),
                (false, false) => "disabled (it was)".to_string(),
                (_, true) => "enabled: routed to again".to_string(),
                (_, false) => "disabled: not routed to; in-flight requests finish".to_string(),
            };
            slot.logs.push("host", &format!("{line} ({})", via.who()));
            eprintln!("minicloud: [{name}] {line}");
            Ok(format!("`{name}` {line}"))
        })();
        self.record(name, action, "", via, &result);
        result
    }

    /// Makes `name`'s grant, allowlist and limits `settings`, if the app
    /// loads with them (or is refused only for a capability they take away).
    pub(crate) async fn configure(
        &self,
        name: &str,
        settings: Settings,
        via: &Via,
    ) -> Result<String, ChangeError> {
        let detail = describe_settings(&settings);
        let _one_at_a_time = self.updating.lock().await;
        let result = self.reconfigure(name, Some(settings), via).await;
        self.record(name, "configure", &detail, via, &result);
        result
    }

    /// Drops the admin's changes to `name`'s configuration: back to its
    /// `app.toml`. Whether it is enabled stays as it is.
    pub(crate) async fn reset(&self, name: &str, via: &Via) -> Result<String, ChangeError> {
        let _one_at_a_time = self.updating.lock().await;
        let result = self.reconfigure(name, None, via).await;
        self.record(name, "reset", "back to app.toml", via, &result);
        result
    }

    fn record(
        &self,
        name: &str,
        action: &str,
        detail: &str,
        via: &Via,
        result: &Result<String, ChangeError>,
    ) {
        let outcome = match result {
            Ok(message) => format!("applied: {message}"),
            Err(error) => format!("refused: {error}"),
        };
        self.control
            .history
            .record(&via.who(), name, action, detail, &outcome);
    }

    /// Loads `name` with `settings` (or with none of the admin's changes),
    /// and installs and keeps the result if it may be.
    async fn reconfigure(
        &self,
        name: &str,
        settings: Option<Settings>,
        via: &Via,
    ) -> Result<String, ChangeError> {
        let Some(slot) = self.engine.slot_named(name) else {
            return Err(ChangeError::NotFound(format!("no app named `{name}`")));
        };
        let dir = self.options.apps.join(name);
        // The app as its `app.toml` alone says: what the change is relative
        // to.
        let source = self.load.secret_source();
        // The `app.toml` is the deployed one, which the admin is not putting
        // forward: a key removed since it was deployed is ignored here as it
        // is at the host's start (`load_with` logs it).
        let file =
            read_app_with(&dir, name, None, &source, RemovedKeys::Ignore).map_err(|why| {
                ChangeError::Refused(format!(
                    "its app.toml does not read, so nothing is changed: {why}"
                ))
            })?;
        let current = self
            .control
            .overrides
            .get(name)
            .unwrap_or_else(AppOverride::none);
        let next = match &settings {
            Some(settings) => {
                let known = self.capabilities();
                let unknown: Vec<&String> = settings
                    .grant
                    .iter()
                    .filter(|capability| !known.contains(capability))
                    .collect();
                if let Some(capability) = unknown.first() {
                    return Err(ChangeError::Refused(format!(
                        "`{capability}` is not a capability this host has ({})",
                        known.join(", ")
                    )));
                }
                if name == ADMIN_APP && !settings.grant.contains(ADMIN_CAPABILITY) {
                    return Err(ChangeError::Refused(format!(
                        "the admin app keeps `{ADMIN_CAPABILITY}`: without it nothing could \
                         change the configuration but the CLI"
                    )));
                }
                AppOverride::between(
                    &file,
                    &settings.grant,
                    &settings.fetch_allow,
                    &settings.limits,
                    current.enabled,
                )
            }
            None => AppOverride {
                enabled: current.enabled,
                ..AppOverride::none()
            },
        };
        // The config's own rules first: they need no compiling.
        read_app_with(&dir, name, Some(&next), &source, RemovedKeys::Ignore)
            .map_err(ChangeError::Refused)?;

        let lineage = Lineage {
            counters: Arc::clone(&slot.counters),
            logs: Arc::clone(&slot.logs),
            queues: Arc::clone(&slot.current_queues),
            number: slot.next_version(),
        };
        let load = self.load.clone();
        let owned = name.to_string();
        let candidate = next.clone();
        let mut app = tokio::task::spawn_blocking(move || {
            load_with(&owned, &dir, &load, lineage, Some(&candidate))
        })
        .await
        .map_err(|e| ChangeError::Refused(format!("the reload failed: {e}")))?;
        self.refuse_taken_hostnames(&mut app);
        let refused = match &app.state {
            AppState::Ready(_) => None,
            AppState::Refused(why) => Some(why.clone()),
        };
        if let Some(why) = &refused {
            // Refused for want of a capability the change took away: that
            // is the change doing what it says, for any app but the admin
            // app. Anything else is a change that does not work.
            let lacks = !app.required.is_empty() && !app.required.is_subset(&app.granted);
            if !lacks || (name == ADMIN_APP && via.is_admin_app()) {
                return Err(ChangeError::Refused(format!(
                    "`{name}` would not load with this change, so it was not made:\n{why}"
                )));
            }
        }
        // Kept first, then routed to: nothing between them can fail or be
        // cancelled, so the host never serves a change a restart would not
        // find.
        self.control
            .overrides
            .set(name, next)
            .map_err(ChangeError::Refused)?;
        let counters = Arc::clone(&app.counters);
        let logs = Arc::clone(&app.logs);
        let installed = self.engine.install(app);
        counters.updates.fetch_add(1, Ordering::Relaxed);
        let line = match &refused {
            None => format!(
                "reconfigured ({}): {} -> {}",
                via.who(),
                installed.previous.as_deref().unwrap_or("nothing"),
                installed.version
            ),
            Some(why) => format!(
                "reconfigured ({}) and now refused: {}",
                via.who(),
                why.lines().next().unwrap_or_default()
            ),
        };
        logs.push("host", &line);
        eprintln!("minicloud: [{name}] {line}");
        Ok(match refused {
            None => format!(
                "`{name}` serves {} with the new configuration",
                installed.version
            ),
            Some(why) => format!(
                "`{name}` is now refused, answering 503 to every request: {}",
                why.lines().next().unwrap_or_default()
            ),
        })
    }
}

impl Front {
    /// Every secret the store has or an app takes from it, by name.
    pub(crate) fn secret_infos(&self) -> Vec<SecretInfo> {
        let mut infos: std::collections::BTreeMap<String, SecretInfo> = self
            .control
            .secrets
            .list()
            .into_iter()
            .map(|stored| {
                (
                    stored.name.clone(),
                    SecretInfo {
                        name: stored.name,
                        set: true,
                        updated_ms: Some(stored.updated_ms),
                        apps: Vec::new(),
                    },
                )
            })
            .collect();
        for (app, names) in self.store_users() {
            for name in names {
                infos
                    .entry(name.clone())
                    .or_insert_with(|| SecretInfo {
                        name,
                        set: false,
                        updated_ms: None,
                        apps: Vec::new(),
                    })
                    .apps
                    .push(app.clone());
            }
        }
        infos.into_values().collect()
    }

    /// Every app routed to (serving, disabled or refused), with the store
    /// names its `app.toml` takes secrets from.
    fn store_users(&self) -> Vec<(String, BTreeSet<String>)> {
        self.engine
            .slots()
            .iter()
            .filter_map(|slot| {
                let app = slot.current()?;
                let names = store_references(&app.dir);
                (!names.is_empty()).then(|| (slot.name.clone(), names))
            })
            .collect()
    }

    /// The apps that take a secret from the store's `name`.
    fn users_of(&self, name: &str) -> Vec<String> {
        self.store_users()
            .into_iter()
            .filter(|(_, names)| names.contains(name))
            .map(|(app, _)| app)
            .collect()
    }

    /// Sets the stored secret `name` to `value`, then reloads every app that
    /// uses it. The value is kept whatever the reloads do.
    pub(crate) async fn set_secret(
        &self,
        name: &str,
        value: &str,
        via: &Via,
    ) -> Result<SecretChange, ChangeError> {
        let _one_at_a_time = self.updating.lock().await;
        let replaced = self.control.secrets.contains(name);
        let result = match self.control.secrets.set(name, value) {
            Ok(()) => {
                let verb = if replaced { "replaced" } else { "set" };
                Ok(self
                    .reload_users(name, &format!("secret `{name}` {verb}"), false, via)
                    .await)
            }
            Err(why) => Err(ChangeError::Refused(why)),
        };
        self.record_secret(name, "secret set", via, &result);
        result
    }

    /// Deletes the stored secret `name`. Refused while an app uses it unless
    /// `force`; forced, those apps are reloaded without it, and installed
    /// refused.
    pub(crate) async fn delete_secret(
        &self,
        name: &str,
        force: bool,
        via: &Via,
    ) -> Result<SecretChange, ChangeError> {
        let _one_at_a_time = self.updating.lock().await;
        let result = async {
            if !self.control.secrets.contains(name) {
                return Err(ChangeError::NotFound(format!("no secret `{name}` is set")));
            }
            let users = self.users_of(name);
            if !users.is_empty() && !force {
                return Err(ChangeError::InUse(format!(
                    "secret `{name}` is used by {}: deleted, they would be refused at their \
                     next load. Delete it with force to do that now",
                    users.join(", ")
                )));
            }
            self.control
                .secrets
                .delete(name)
                .map_err(ChangeError::Refused)?;
            Ok(self
                .reload_users(name, &format!("secret `{name}` deleted"), true, via)
                .await)
        }
        .await;
        self.record_secret(name, "secret delete", via, &result);
        result
    }

    /// Reloads every app that uses the stored secret `name`, saying `why`
    /// in its log. `install_refused`: an app that no longer loads is
    /// installed refused (a forced delete), rather than left serving what it
    /// served (a set).
    async fn reload_users(
        &self,
        name: &str,
        why: &str,
        install_refused: bool,
        via: &Via,
    ) -> SecretChange {
        let mut reloaded = Vec::new();
        for app in self.users_of(name) {
            reloaded.push(
                self.reload_for_secret(&app, why, install_refused, via)
                    .await,
            );
        }
        let message = if reloaded.is_empty() {
            format!("{why}; no app uses it")
        } else {
            format!(
                "{why}; {}",
                reloaded
                    .iter()
                    .map(|r| format!("`{}` {}", r.app, r.outcome))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        };
        SecretChange {
            secret: name.to_string(),
            message,
            reloaded,
        }
    }

    async fn reload_for_secret(
        &self,
        name: &str,
        why: &str,
        install_refused: bool,
        via: &Via,
    ) -> Reloaded {
        let reloaded = |ok: bool, outcome: String| Reloaded {
            app: name.to_string(),
            ok,
            outcome,
        };
        let Some(slot) = self.engine.slot_named(name) else {
            return reloaded(false, "is not routed to".to_string());
        };
        let current = slot.current().map(|app| app.version.clone());
        let lineage = Lineage {
            counters: Arc::clone(&slot.counters),
            logs: Arc::clone(&slot.logs),
            queues: Arc::clone(&slot.current_queues),
            number: slot.next_version(),
        };
        let dir = self.options.apps.join(name);
        let load = self.load.clone();
        let owned = name.to_string();
        let loaded =
            tokio::task::spawn_blocking(move || load_as(&owned, &dir, &load, lineage)).await;
        let mut app = match loaded {
            Ok(app) => app,
            Err(e) => return reloaded(false, format!("did not reload: {e}")),
        };
        self.refuse_taken_hostnames(&mut app);
        let refused = match &app.state {
            AppState::Ready(_) => None,
            AppState::Refused(why) => Some(why.lines().next().unwrap_or_default().to_string()),
        };
        let counters = Arc::clone(&app.counters);
        let logs = Arc::clone(&app.logs);
        let (ok, line, outcome) = match refused {
            None => {
                let installed = self.engine.install(app);
                counters.updates.fetch_add(1, Ordering::Relaxed);
                (
                    true,
                    format!(
                        "reloaded, {why} ({}): {} -> {}",
                        via.who(),
                        installed.previous.as_deref().unwrap_or("nothing"),
                        installed.version
                    ),
                    format!("serving {}", installed.version),
                )
            }
            Some(reason) if install_refused => {
                self.engine.install(app);
                counters.updates.fetch_add(1, Ordering::Relaxed);
                (
                    false,
                    format!("reloaded, {why} ({}), and now refused: {reason}", via.who()),
                    format!("now refused: {reason}"),
                )
            }
            Some(reason) => {
                counters.updates_refused.fetch_add(1, Ordering::Relaxed);
                let still = current.as_deref().unwrap_or("nothing");
                (
                    false,
                    format!(
                        "reload after {why} ({}) refused, still serving {still}: {reason}",
                        via.who()
                    ),
                    format!("refused, still serving {still}: {reason}"),
                )
            }
        };
        logs.push("host", &line);
        eprintln!("minicloud: [{name}] {line}");
        reloaded(ok, outcome)
    }

    fn record_secret(
        &self,
        name: &str,
        action: &str,
        via: &Via,
        result: &Result<SecretChange, ChangeError>,
    ) {
        let outcome = match result {
            Ok(change) => format!("applied: {}", change.message),
            Err(error) => format!("refused: {error}"),
        };
        self.control
            .history
            .record(&via.who(), "", action, &format!("`{name}`"), &outcome);
    }
}

/// What a configure asked for, for the history.
fn describe_settings(settings: &Settings) -> String {
    let limits = serde_json::to_value(&settings.limits).unwrap_or_default();
    let limits = limits
        .as_object()
        .map(|keys| {
            keys.iter()
                .map(|(key, value)| match value {
                    Json::String(text) => format!("{key}={text}"),
                    other => format!("{key}={other}"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    format!(
        "grant [{}]; fetch.allow [{}]; {limits}",
        settings
            .grant
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", "),
        settings.fetch_allow.join(", "),
    )
}
