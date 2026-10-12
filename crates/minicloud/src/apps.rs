//! Loading an app: read its config, check it once, check its authority,
//! prepare it once, compile it once.
//!
//! Every app is compiled as a package of its own — its modules and the
//! standard library — so an app cannot `use` another app's code, and what the
//! checker derives about its entry is about its code alone. What is paid here
//! is paid once per app: parsing, checking, lowering, `PreparedProgram::new`'s
//! encoding and verification, and on the native tier the code generator. A
//! request pays for an `OwnedVm` and nothing above it.
//!
//! An app is refused — and every other app still starts — when its config
//! does not read, it does not check (warnings included), its entry requires a
//! capability `app.toml` does not grant, or its code can `spawn` a task.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_diag::{render, Diagnostic, Severity, SourceMap};
use cove_ir::Inst;
use cove_runtime::{HostRegistry, OwnedVm, PreparedProgram, Runtime};
use cove_sema::package::{self, Module, Package, Unit};
use cove_sema::resolve::{FnEntry, Program};
use cove_sema::{Compiler, HostSchemas};

use crate::assets::Assets;
use crate::config::{
    read_app_with, AppConfig, AppLimits, AppOverride, AssetPolicy, DeferLimits, FetchPolicy,
    KeyedPolicy, KvLimits, QueuePolicy, RatePolicy, RemovedKeys, SecretSource, Secrets, Trigger,
    ADMIN_APP,
};
use crate::hosts::{AppContext, HostModules};
use crate::logs::LogRing;
use crate::overrides::Control;
use crate::stats::AppCounters;

/// Which tier runs an app's requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    /// The native tier where this host has one (Unix x86-64), and the
    /// encoded VM elsewhere, saying so once.
    #[default]
    Auto,
    /// The encoded VM, always.
    Vm,
    /// The native tier, and an app is refused where there is none.
    Native,
}

impl std::str::FromStr for Backend {
    type Err = String;
    fn from_str(text: &str) -> Result<Backend, String> {
        match text {
            "auto" => Ok(Backend::Auto),
            "vm" => Ok(Backend::Vm),
            "native" => Ok(Backend::Native),
            other => Err(format!("`{other}` is not a backend: auto, vm or native")),
        }
    }
}

/// How apps are loaded.
#[derive(Clone)]
pub struct LoadOptions {
    pub backend: Backend,
    /// Whether `log` prints nothing to standard output.
    pub quiet: bool,
    pub modules: HostModules,
    /// The data directory; each app's state is under `<data>/<app>/`.
    /// `None` keeps every app's state in memory.
    pub data: Option<PathBuf>,
    /// The I/O runtime the modules wait on.
    pub io: tokio::runtime::Handle,
    /// What the admin app's `host` module acts on; given to that app only.
    pub control: Option<Arc<Control>>,
    /// Where an app's deferred work is taken ([`crate::defer`]); `None`
    /// for a host that runs none, such as `minicloud test`.
    pub defer_to: Option<tokio::sync::mpsc::Sender<crate::defer::Deferred>>,
    /// What a removed `app.toml` key does: refuses the app when it is being
    /// put forward (a deploy, an update), is ignored with a warning when an
    /// app already deployed is loaded again (the host's start).
    pub removed_keys: RemovedKeys,
}

impl LoadOptions {
    /// Where an app's `{ store = "…" }` secrets come from: the host's store,
    /// when there is a host.
    pub fn secret_source(&self) -> SecretSource {
        match &self.control {
            Some(control) => SecretSource::store(Arc::clone(&control.secrets)),
            None => SecretSource::default(),
        }
    }
}

/// One app, ready or refused.
pub struct App {
    /// The name that routes to it: `/hello/...` reaches `hello`.
    pub name: String,
    /// This version's number: 1 for the first loaded, one more for each
    /// update.
    pub number: u64,
    /// `v<number>-<hash>`, the hash of the app's files: in the stats, the
    /// logs and every response's `x-cove-app-version`.
    pub version: String,
    pub dir: PathBuf,
    /// `module.function`.
    pub entry: String,
    /// What `app.toml` grants.
    pub granted: BTreeSet<String>,
    /// What the checker derived the entry requires, when it checked.
    pub required: BTreeSet<String>,
    /// Whether `required` is a lower bound: the entry makes a call the call
    /// graph cannot follow.
    pub open: bool,
    pub limits: AppLimits,
    pub kv: KvLimits,
    /// `[sql]`: the app's SQL store, with `sql` granted.
    pub sql: crate::config::SqlLimits,
    /// `[defer]`: how much work it may have outstanding.
    pub defer: DeferLimits,
    /// `[[queues]]`: the queues it may send to, and how they are delivered.
    pub queues: Vec<QueuePolicy>,
    /// `[keyed]`: one run at a time for one key ([`crate::keyed`]).
    pub keyed: Option<KeyedPolicy>,
    pub fetch: FetchPolicy,
    /// `[assets]`: how the files below are answered.
    pub asset_policy: AssetPolicy,
    /// The app's `assets/` directory, read once here and answered by the
    /// HTTP front without a run ([`crate::assets`]). It belongs to this
    /// version: an update switches the files with the code, and a request
    /// in flight keeps the ones it started on.
    pub assets: Arc<Assets>,
    /// What `auth.check` compares against; never shown.
    pub secrets: Secrets,
    /// `[access]`: how `auth.identity` knows who is asking.
    pub access: crate::access::Access,
    /// `[route] hosts`: the hostnames that reach this app, and the only
    /// way to it when there are any.
    pub hosts: Vec<String>,
    /// `[rate]`: how often one caller may reach this app, `None` for no
    /// limit of its own ([`crate::rate`]).
    pub rate: Option<RatePolicy>,
    /// `[[triggers]]`: when the host runs this app itself
    /// ([`crate::triggers`]). Of this version, like everything else here: a
    /// schedule changed by an update takes effect with the version.
    pub triggers: Vec<Trigger>,
    /// `[fetch] allow` as configured, for the admin app to show and edit.
    pub fetch_allow: Vec<String>,
    /// The admin's changes this version was loaded with, if any.
    pub overridden: Option<AppOverride>,
    pub counters: Arc<AppCounters>,
    /// The app's recent log lines: its `log.*` and the host's lines about it.
    pub logs: Arc<LogRing>,
    pub state: AppState,
}

/// Whether an app serves.
pub enum AppState {
    Ready(Box<Ready>),
    /// Not loaded, and why: printed at startup, answered to every request.
    Refused(String),
}

/// Everything a request's isolate is built from, shared by all of them.
pub struct Ready {
    pub module: String,
    pub function: String,
    /// For rendering a runtime error into a response.
    pub sources: Arc<SourceMap>,
    /// The app's grants and module instances, shared by every run: a run's
    /// budget is the run's, never the registry's (cove#577).
    pub hosts: Arc<HostRegistry>,
    pub runtime: Arc<Runtime>,
    /// Encoded, verified — and compiled, on the native tier — once.
    pub program: PreparedProgram,
    /// `"native"` or `"vm"`.
    pub tier: &'static str,
    pub cost: LoadCost,
}

/// What loading one app cost.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadCost {
    pub check: Duration,
    pub prepare: Duration,
    pub functions: usize,
}

impl Ready {
    /// A fresh isolate: its own heap, stack and budget, nothing of any other
    /// run's. With `max_heap_words` the heap may grow to that many words and
    /// no further — a run that needs more fails its allocation with "this run
    /// has no memory left" (ADR 0088) — and without it to the runtime's
    /// default.
    pub fn isolate(&self, max_heap_words: Option<u64>) -> OwnedVm {
        let (runtime, hosts, program) = (
            Arc::clone(&self.runtime),
            Arc::clone(&self.hosts),
            self.program.clone(),
        );
        match max_heap_words {
            Some(words) => OwnedVm::with_heap_words(
                runtime,
                hosts,
                program,
                usize::try_from(words).unwrap_or(usize::MAX),
            ),
            None => OwnedVm::new(runtime, hosts, program),
        }
    }
}

impl App {
    /// What the app's module instances are built with.
    pub fn context(
        &self,
        quiet: bool,
        data: Option<&Path>,
        io: &tokio::runtime::Handle,
    ) -> AppContext {
        self.context_with(quiet, data, io, None, None)
    }

    /// [`App::context`], with the host's control for the admin app — and
    /// for no other app, whatever it is granted — and where the host takes
    /// deferred work, which only a running host has.
    pub fn context_with(
        &self,
        quiet: bool,
        data: Option<&Path>,
        io: &tokio::runtime::Handle,
        control: Option<&Arc<Control>>,
        defer_to: Option<&tokio::sync::mpsc::Sender<crate::defer::Deferred>>,
    ) -> AppContext {
        AppContext {
            app: self.name.clone(),
            quiet,
            counters: Arc::clone(&self.counters),
            logs: Arc::clone(&self.logs),
            granted: self.granted.clone(),
            data: data.map(|root| root.join(&self.name)),
            kv: self.kv.clone(),
            sql: self.sql.clone(),
            defer: self.defer.clone(),
            defer_to: defer_to.cloned(),
            queues: self.queues.clone(),
            keyed: self.keyed.clone(),
            max_response_bytes: self.limits.max_response_bytes,
            fetch: self.fetch.clone(),
            secrets: self.secrets.clone(),
            access: self.access.clone(),
            io: io.clone(),
            control: control.filter(|_| self.name == ADMIN_APP).cloned(),
        }
    }

    pub fn ready(&self) -> Option<&Ready> {
        match &self.state {
            AppState::Ready(ready) => Some(ready),
            AppState::Refused(_) => None,
        }
    }

    /// The one-line account printed at startup and by `minicloud check`.
    pub fn describe(&self) -> String {
        let open = if self.open { " (lower bound)" } else { "" };
        // The files are worth a word: an app that lost its stylesheet to a
        // deploy that left it out should say so here.
        let assets = match self.assets.len() {
            0 => String::new(),
            files => format!(
                ", {files} file(s) in {}/ ({} bytes)",
                crate::assets::DIR,
                self.assets.bytes()
            ),
        };
        let verdict = match &self.state {
            AppState::Ready(ready) => format!(
                "ok: {} fn on {}, checked in {:.1} ms, prepared in {:.1} ms{assets}",
                ready.cost.functions,
                ready.tier,
                ready.cost.check.as_secs_f64() * 1e3,
                ready.cost.prepare.as_secs_f64() * 1e3,
            ),
            AppState::Refused(why) => {
                format!("REFUSED: {}", why.lines().next().unwrap_or_default())
            }
        };
        format!(
            "{:<10} {}  requires [{}]{open}  granted [{}]  {verdict}",
            self.name,
            self.version,
            list(&self.required),
            list(&self.granted),
        )
    }
}

pub fn list(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        "-".to_string()
    } else {
        set.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// The app directories under `root`, sorted: every directory holding an
/// `app.toml`. `only` narrows them to the names it lists.
pub fn app_dirs(root: &Path, only: &[String]) -> Result<Vec<(String, PathBuf)>, String> {
    let entries =
        std::fs::read_dir(root).map_err(|e| format!("cannot read `{}`: {e}", root.display()))?;
    let mut dirs: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("app.toml").is_file())
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_string();
            Some((name, path))
        })
        .collect();
    dirs.sort();
    for name in only {
        if !dirs.iter().any(|(found, _)| found == name) {
            return Err(format!(
                "no app named `{name}` in `{}` (an app is a directory with an `app.toml`)",
                root.display()
            ));
        }
    }
    if !only.is_empty() {
        dirs.retain(|(name, _)| only.contains(name));
    }
    Ok(dirs)
}

/// Loads every app under `root`. A refusal is per app; the error is only for
/// a directory that cannot be read.
pub fn load_all(root: &Path, options: &LoadOptions) -> Result<Vec<App>, String> {
    Ok(app_dirs(root, &[])?
        .into_iter()
        .map(|(name, dir)| load(&name, &dir, options))
        .collect())
}

/// An app as its directory and config describe it, before anything is
/// compiled; refused if the config does not read. `store` secrets are
/// looked up in `source`.
pub fn describe(name: &str, dir: &Path, source: &SecretSource) -> (App, Option<AppConfig>) {
    describe_as(
        name,
        dir,
        Lineage::first(),
        None,
        source,
        RemovedKeys::Refuse,
    )
}

/// What a version of an app inherits from the versions before it: its
/// counters and its log, which belong to the app rather than a version, and
/// its number.
pub struct Lineage {
    pub counters: Arc<AppCounters>,
    pub logs: Arc<LogRing>,
    pub number: u64,
}

impl Lineage {
    /// An app's first version.
    pub fn first() -> Lineage {
        Lineage {
            counters: Arc::new(AppCounters::default()),
            logs: Arc::new(LogRing::default()),
            number: 1,
        }
    }
}

/// [`describe`], for version `lineage.number` of an app, with the admin's
/// changes `over` applied to its `app.toml`.
pub fn describe_as(
    name: &str,
    dir: &Path,
    lineage: Lineage,
    over: Option<&AppOverride>,
    source: &SecretSource,
    removed: RemovedKeys,
) -> (App, Option<AppConfig>) {
    let mut app = App {
        name: name.to_string(),
        number: lineage.number,
        version: format!("v{}-{:08x}", lineage.number, content_hash(dir) as u32),
        dir: dir.to_path_buf(),
        entry: format!("{name}.handle"),
        granted: BTreeSet::new(),
        required: BTreeSet::new(),
        open: false,
        limits: AppLimits::default(),
        kv: KvLimits::default(),
        sql: crate::config::SqlLimits::default(),
        defer: DeferLimits::default(),
        queues: Vec::new(),
        keyed: None,
        fetch: FetchPolicy::default(),
        asset_policy: AssetPolicy::default(),
        assets: Arc::new(Assets::default()),
        secrets: Secrets::default(),
        access: crate::access::Access::default(),
        hosts: Vec::new(),
        rate: None,
        triggers: Vec::new(),
        fetch_allow: Vec::new(),
        overridden: over.filter(|over| over.changes_config()).cloned(),
        counters: lineage.counters,
        logs: lineage.logs,
        state: AppState::Refused(String::new()),
    };
    if let Err(why) = valid_name(name) {
        app.state = AppState::Refused(why);
        return (app, None);
    }
    match read_app_with(dir, name, over, source, removed) {
        Ok(config) => {
            app.hosts = config.hosts.clone();
            app.fetch_allow = config.file_allow.clone();
            app.entry = config.entry.clone();
            app.granted = config.granted.clone();
            app.limits = config.limits.clone();
            app.kv = config.kv.clone();
            app.sql = config.sql.clone();
            app.defer = config.defer.clone();
            app.queues = config.queues.clone();
            app.keyed = config.keyed.clone();
            app.fetch = config.fetch.clone();
            app.secrets = config.secrets.clone();
            app.access = config.access.clone();
            app.asset_policy = config.assets.clone();
            app.triggers = config.triggers.clone();
            app.rate = config.rate.clone();
            // The files are read here, with the config that bounds them, so
            // that an `assets/` directory the host will not answer — a
            // symbolic link, more than `[assets] max_bytes` — refuses the
            // app at load with the reason, as a bad config does, rather
            // than being found out by the first request for it.
            match Assets::load(dir, config.assets.max_bytes) {
                Ok(assets) => app.assets = Arc::new(assets),
                Err(why) => {
                    app.state = AppState::Refused(why);
                    return (app, None);
                }
            }
            (app, Some(config))
        }
        Err(why) => {
            app.state = AppState::Refused(why);
            (app, None)
        }
    }
}

/// FNV-1a over the app's files — `app.toml`, every `.cove` file and
/// everything under `assets/`, with their paths — in a fixed order: the same
/// files hash the same, so a version id says whether two loads were of the
/// same app. A stylesheet is as much the app as its code is, so changing one
/// is a new version ([`crate::assets`]); a hidden name is left out of the
/// hash because it is left out of what is answered.
fn content_hash(dir: &Path) -> u64 {
    let mut files = Vec::new();
    let assets = dir.join(crate::assets::DIR);
    let mut walk = vec![dir.to_path_buf()];
    while let Some(at) = walk.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let under_assets = path.starts_with(&assets);
            if under_assets && entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if path.is_dir() {
                // One level for the modules, every level under `assets/`.
                if at == dir || under_assets {
                    walk.push(path);
                }
            } else if under_assets
                || path.extension().is_some_and(|e| e == "cove")
                || path.file_name().is_some_and(|n| n == "app.toml")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for file in files {
        eat(file
            .strip_prefix(dir)
            .unwrap_or(&file)
            .to_string_lossy()
            .as_bytes());
        eat(&[0]);
        eat(&std::fs::read(&file).unwrap_or_default());
        eat(&[0]);
    }
    hash
}

/// An app's name is its route and its main module's name, so it has to be
/// both: a Cove identifier, and not one of the host's reserved prefixes.
pub(crate) fn valid_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
    if !first_ok || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "`{name}` is not an app name: a letter, then letters, digits or `_` \
             (it is the app's route and its module's name)"
        ));
    }
    Ok(())
}

/// Loads one app.
pub fn load(name: &str, dir: &Path, options: &LoadOptions) -> App {
    load_as(name, dir, options, Lineage::first())
}

/// [`load`], as version `lineage.number` of an app: the host's update. The
/// app's log goes to `<data>/<app>/log.txt` as well as its ring. The
/// admin's stored changes to the app, if any, are applied.
pub fn load_as(name: &str, dir: &Path, options: &LoadOptions, lineage: Lineage) -> App {
    let over = options
        .control
        .as_ref()
        .and_then(|control| control.overrides.get(name));
    load_with(name, dir, options, lineage, over.as_ref())
}

/// [`load_as`], with `over` as the admin's changes — the candidate a
/// change is tried with before it is kept.
pub fn load_with(
    name: &str,
    dir: &Path,
    options: &LoadOptions,
    lineage: Lineage,
    over: Option<&AppOverride>,
) -> App {
    let (mut app, config) = describe_as(
        name,
        dir,
        lineage,
        over,
        &options.secret_source(),
        options.removed_keys,
    );
    if let Some(data) = &options.data {
        if valid_name(name).is_ok() {
            app.logs.attach(data.join(name).join("log.txt"));
        }
    }
    for warning in config.iter().flat_map(|config| &config.warnings) {
        eprintln!("minicloud: [{name}] warning: {warning}");
        app.logs.push("host", &format!("warning: {warning}"));
    }
    if config.is_none() {
        return app;
    }
    app.state = match prepare(&mut app, options) {
        Ok(ready) => AppState::Ready(Box::new(ready)),
        Err(why) => AppState::Refused(why),
    };
    app
}

/// An app's modules, checked against the host's schemas.
pub struct Compiled {
    pub sources: SourceMap,
    pub program: Program,
    pub check: Duration,
}

/// Loads and checks the app in `dir` against `modules`' schemas.
///
/// The `Err` is rendered, after the stage that refused it.
pub fn compile(dir: &Path, name: &str, modules: &HostModules) -> Result<Compiled, String> {
    let started = Instant::now();
    let (sources, package) = load_package(dir, name)?;
    let program = Compiler::new()
        .with_schemas(HostSchemas::only(modules.schemas()))
        .compile(&package)
        .map_err(|items| format!("does not check:\n{}", report(&sources, &items)))?;
    Ok(Compiled {
        sources,
        program,
        check: started.elapsed(),
    })
}

/// Whether a checked app may be loaded: it checks without warnings, it
/// declares its entry, and the entry requires nothing `app.toml` does not
/// grant. Fills in what the entry requires.
///
/// The decision `serve` and `check` share, so that the checker cannot pass an
/// app the server would refuse.
pub fn admit(app: &mut App, compiled: &Compiled) -> Result<(), String> {
    let (module, function) = app
        .entry
        .split_once('.')
        .ok_or_else(|| format!("entry `{}` is not `module.function`", app.entry))?;
    let warnings: Vec<Diagnostic> = compiled
        .program
        .notices
        .iter()
        .filter(|item| item.severity == Severity::Warning)
        .cloned()
        .collect();
    if !warnings.is_empty() {
        return Err(format!(
            "checks with warnings:\n{}",
            report(&compiled.sources, &warnings)
        ));
    }
    let entry = compiled
        .program
        .lookup_fn(module, function)
        .ok_or_else(|| format!("`{module}` declares no `{function}`"))?;
    app.required = entry
        .required_capabilities
        .iter()
        .map(|capability| capability.as_str().to_string())
        .collect();
    app.open = entry.is_capability_open();
    let missing: Vec<String> = app.required.difference(&app.granted).cloned().collect();
    if !missing.is_empty() {
        return Err(format!(
            "`{}` requires {}, which {} does not grant",
            app.entry,
            missing
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(" and "),
            if app.overridden.is_some() {
                "app.toml with the admin's changes"
            } else {
                "app.toml"
            }
        ));
    }
    Ok(())
}

/// Lowers the entry and refuses a program that can `spawn`.
///
/// A spawned task is a thread of its own, outside this host's worker pool and
/// its accounting; while one is alive its parent can neither park nor yield
/// (ADR 0080 §2, ADR 0084 §3), and it has no native tier (ADR 0085). So an
/// app that can spawn could hold a worker for its whole deadline and start
/// threads the pool does not count. Rather than admit it with a limit that
/// holds only in part, the host refuses it here; the run's `max_tasks = 0` is
/// the backstop.
///
/// Whether the entry can spawn is the checker's [`FnEntry::can_spawn`] — it
/// reaches a task `scope` (ADR 0088). That is a lower bound when the entry is
/// capability-open, since a call the graph cannot follow may reach a scope it
/// cannot see; so an open entry in a package where some function opens a
/// scope is decided by the lowered program instead, which holds exactly what
/// the entry reaches. The lowered program is also where the refusal finds the
/// `spawn` to point at; without one, it points at the entry.
pub fn lower(
    app: &App,
    compiled: &Compiled,
    modules: &HostModules,
) -> Result<cove_ir::Program, String> {
    let (module, function) = app.entry.split_once('.').unwrap_or_default();
    let schemas = HostSchemas::only(modules.schemas());
    let lowered = cove_ir::lower_entry(
        &compiled.program,
        &compiled.sources,
        &schemas,
        module,
        function,
    );
    let entry = compiled.program.lookup_fn(module, function);
    if let Some(entry) = entry.filter(|entry| entry.can_spawn) {
        return Err(refuse_spawn(
            &app.entry,
            entry,
            lowered.as_ref().ok(),
            &compiled.sources,
        ));
    }
    let lowered = lowered.map_err(|items| unlowered(&compiled.sources, &items, entry))?;
    if let Some(entry) =
        entry.filter(|entry| entry.is_capability_open() && opens_a_scope(&compiled.program))
    {
        if spawn_site(&lowered).is_some() {
            return Err(refuse_spawn(
                &app.entry,
                entry,
                Some(&lowered),
                &compiled.sources,
            ));
        }
    }
    Ok(lowered)
}

/// Whether any function of the package opens a task `scope` itself.
fn opens_a_scope(program: &Program) -> bool {
    program.modules.values().any(|module| {
        module.functions.values().any(|f| f.direct_spawns)
            || module.methods.values().any(|f| f.direct_spawns)
    })
}

/// The first `spawn` in a lowered program, with the function it is in.
fn spawn_site(program: &cove_ir::Program) -> Option<(String, Option<cove_diag::Span>)> {
    program.functions.iter().find_map(|function| {
        let at = function
            .code
            .iter()
            .position(|inst| matches!(inst, Inst::Spawn { .. }))?;
        Some((
            format!("{}.{}", function.module, function.name),
            function.spans.get(at).copied(),
        ))
    })
}

/// The refusal of an entry that can spawn: at the `spawn` the lowered program
/// reaches, if it lowered and holds one, and at the entry otherwise.
fn refuse_spawn(
    name: &str,
    entry: &FnEntry,
    lowered: Option<&cove_ir::Program>,
    sources: &SourceMap,
) -> String {
    let site = lowered.and_then(spawn_site);
    let message = match &site {
        Some((function, _)) if function != name => format!(
            "`{name}` can spawn a task — `{function}` spawns one — which minicloud does not run"
        ),
        _ => format!("`{name}` can spawn a task, which minicloud does not run"),
    };
    let diagnostic = Diagnostic::error("minicloud::spawn", message.clone())
        .at(site
            .and_then(|(_, span)| span)
            .unwrap_or(entry.decl.name.span))
        .rule(
            "A spawned task runs outside the host's worker pool and keeps its parent \
             from parking or yielding; minicloud refuses an app that can spawn.",
        );
    format!("{message}\n{}", render(sources, &diagnostic))
}

/// A lowering's refusal, rendered: its first message on the summary line, and
/// every diagnostic with its location below — at the entry for one that came
/// without a position of its own, so that none reads as an empty diagnostic
/// (issue 26).
fn unlowered(sources: &SourceMap, items: &[Diagnostic], entry: Option<&FnEntry>) -> String {
    let located: Vec<Diagnostic> = items
        .iter()
        .map(|item| match (item.primary, entry) {
            (None, Some(entry)) => item.clone().at(entry.decl.name.span),
            _ => item.clone(),
        })
        .collect();
    let first = items
        .first()
        .map(|item| item.message.as_str())
        .unwrap_or("no reason was given");
    format!("does not lower: {first}\n{}", report(sources, &located))
}

/// Compiles, checks the grant, lowers, prepares; or says why not.
fn prepare(app: &mut App, options: &LoadOptions) -> Result<Ready, String> {
    let compiled = compile(&app.dir, &app.name, &options.modules)?;
    admit(app, &compiled)?;
    let started = Instant::now();
    let lowered = lower(app, &compiled, &options.modules)?;
    let functions = lowered.functions.len();
    let prepared = PreparedProgram::new(Arc::new(lowered));
    let (program, tier) = match options.backend {
        Backend::Vm => (prepared, "vm"),
        Backend::Native => (
            prepared
                .with_native()
                .map_err(|why| format!("`--backend native`: {why}"))?,
            "native",
        ),
        Backend::Auto => match prepared.clone().with_native() {
            Ok(native) => (native, "native"),
            Err(why) => {
                note_fallback(&why.to_string());
                (prepared, "vm")
            }
        },
    };
    let prepare = started.elapsed();

    let (module, function) = app.entry.split_once('.').unwrap_or_default();
    let (module, function) = (module.to_string(), function.to_string());
    let context = app.context_with(
        options.quiet,
        options.data.as_deref(),
        &options.io,
        options.control.as_ref(),
        options.defer_to.as_ref(),
    );
    let hosts = Arc::new(options.modules.registry(&app.granted, &context)?);
    let sources = Arc::new(compiled.sources);
    let runtime = Arc::new(Runtime::new(
        Arc::new(compiled.program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    ));
    Ok(Ready {
        module,
        function,
        sources,
        hosts,
        runtime,
        program,
        tier,
        cost: LoadCost {
            check: compiled.check,
            prepare,
            functions,
        },
    })
}

/// Says once per process that the native tier is not available here.
fn note_fallback(why: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        eprintln!(
            "minicloud: the native tier is not available on this host ({}); \
             apps run on the encoded VM",
            why.lines().next().unwrap_or_default()
        );
    });
}

/// The app's modules and the standard library, as a package of their own.
///
/// The `.cove` files directly in the app's directory are the module named
/// after the app, loaded with the standard library by Cove's own
/// [`package::load_module`]; each subdirectory holding `.cove` files is a
/// module named after the subdirectory, which the app's files may `use`.
/// Nothing outside the directory is read, and every file is named relative to
/// the app's parent directory, so that an error points at
/// `hello/hello.cove:3`.
pub fn load_package(dir: &Path, name: &str) -> Result<(SourceMap, Package), String> {
    let mut sources = SourceMap::new();
    if dir.file_name().and_then(|n| n.to_str()) != Some(name) {
        return Err(format!(
            "`{}` is not a directory named `{name}`",
            dir.display()
        ));
    }
    let root = dir.parent().unwrap_or(dir);
    let mut package = package::load_module(root, name, &mut sources)
        .map_err(|items| unloaded(&sources, &items))?;
    package.root = dir.to_path_buf();
    let mut subdirs = Vec::new();
    for entry in
        std::fs::read_dir(dir).map_err(|e| format!("cannot read `{}`: {e}", dir.display()))?
    {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            subdirs.push(path);
        }
    }
    subdirs.sort();
    for module_dir in subdirs {
        let Some(module) = module_dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let module = module.to_string();
        let units = read_units(&module_dir, root, &mut sources)?;
        if units.is_empty() {
            continue;
        }
        if package.modules.contains_key(&module) {
            return Err(format!(
                "`{}` is a module named `{module}`, which is already the app's or the \
                 standard library's",
                module_dir.display()
            ));
        }
        package.modules.insert(
            module.clone(),
            Module {
                name: module,
                dir: module_dir,
                units,
            },
        );
    }
    Ok((sources, package))
}

/// What [`package::load_module`] refused, rendered: a file that does not parse
/// as a diagnostic, a directory it cannot read as its message.
fn unloaded(sources: &SourceMap, items: &[Diagnostic]) -> String {
    if items.iter().any(|item| item.primary.is_some()) {
        format!("does not parse:\n{}", report(sources, items))
    } else {
        items
            .iter()
            .map(|item| item.message.clone())
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// The `.cove` files directly in `dir`, parsed, named relative to `root`.
fn read_units(dir: &Path, root: &Path, sources: &mut SourceMap) -> Result<Vec<Unit>, String> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read `{}`: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("cove"))
        .collect();
    paths.sort();
    let mut units = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
        let shown = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let file = sources.add(shown, &text);
        let ast = cove_syntax::parse_file(sources, file)
            .map_err(|items| format!("does not parse:\n{}", report(sources, &items)))?;
        units.push(Unit { file, path, ast });
    }
    Ok(units)
}

/// Diagnostics, rendered the way `cove check` renders them.
pub fn report(sources: &SourceMap, items: &[Diagnostic]) -> String {
    items.iter().map(|item| render(sources, item)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Compiled {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello");
        compile(&dir, "hello", &HostModules::standard()).expect("hello compiles")
    }

    /// Issue 26: a lowering refusal names its first reason on the summary
    /// line and is rendered with a location — its own, or the entry's for one
    /// that came without — so that none reads as an empty diagnostic.
    #[test]
    fn a_lowering_refusal_is_printed_with_its_location() {
        let compiled = hello();
        let entry = compiled.program.lookup_fn("hello", "handle");
        let items = [Diagnostic::error(
            "cove::lower::gap",
            "this construct has no lowering",
        )];
        let why = unlowered(&compiled.sources, &items, entry);
        let (summary, rendered) = why.split_once('\n').unwrap();
        assert_eq!(summary, "does not lower: this construct has no lowering");
        assert!(
            rendered.contains("this construct has no lowering"),
            "{rendered}"
        );
        assert!(rendered.contains("hello/hello.cove:"), "{rendered}");
    }

    /// The app's module and its subdirectories' modules are loaded, named
    /// relative to the apps directory, beside the standard library.
    #[test]
    fn an_app_loads_with_its_modules_and_the_standard_library() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/ledger");
        let (sources, package) = load_package(&dir, "ledger").unwrap();
        for module in ["ledger", "json", "stats"] {
            assert!(package.modules.contains_key(module), "{module}");
        }
        assert!(package.modules.keys().any(|name| name.starts_with("std")));
        let json = &package.modules["json"];
        assert_eq!(
            sources.path(json.units[0].file),
            Path::new("ledger/json/json.cove")
        );
        let Err(missing) = load_package(&dir.join("nope"), "nope") else {
            panic!("a directory that is not there loads");
        };
        assert!(missing.contains("cannot read"), "{missing}");
    }
}
