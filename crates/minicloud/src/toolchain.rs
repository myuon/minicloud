//! `minicloud check` and `minicloud test`: the toolchain an app's author
//! runs, with the host's schemas and the host's modules.
//!
//! `cove check` cannot see `log`, `timer` or `kv`: they are this crate's,
//! and cove#151 was closed by decision — the embedder ships its own checker.
//! `check` runs the compile, the admission and the lowering `serve` runs at
//! load ([`apps::compile`], [`apps::admit`], [`apps::lower`]), so it cannot
//! pass an app the server would refuse. `test` runs each `test fn` the way
//! `cove test` does — through Cove's own `cove_runtime::testing`, lowered as
//! an entry of its own, on the VM, an `Err` is a failure pointing at its
//! assertion — with the app's grant (not the test's derived one), the app's
//! limits, and the host's modules answered through their blocking path, since
//! a test runs to its end on one thread.
//!
//! **Secrets.** Neither runs with a host, so a `{ store = "…" }` secret is
//! looked up in the store of the data directory `--data` names
//! (`<data>/_host/secrets`, read only), and without `--data` there is no
//! store. `check` refuses an app whose secret cannot be resolved, as `serve`
//! would. `test` does not: a secret only gates `auth.check` and a
//! `[fetch.headers]` header, which a test has no business reaching for real,
//! so a secret it cannot resolve — store, environment or file — is given a
//! placeholder value (`minicloud-test-placeholder-<name>`, see
//! [`crate::config::placeholder`]) and the run says which, on stderr.

use std::path::Path;
use std::sync::Arc;

use cove_diag::{render, Diagnostic, Severity};
use cove_runtime::testing::{self, TestBackend, TestRun};
use cove_sema::resolve::DeclaredTest;
use cove_sema::HostSchemas;

use crate::apps::{self, App, AppState, Compiled};
use crate::config::{RemovedKeys, SecretSource};
use crate::hosts::HostModules;
use crate::secrets::SecretStore;

/// What a command printed, and whether it succeeded.
#[derive(Debug, Default)]
pub struct Report {
    pub out: String,
    pub err: String,
    pub ok: bool,
}

/// `minicloud check [app…]`.
///
/// Each app's modules are checked against the host's schemas and their
/// notices printed as `cove check` prints them; then each app gets one line —
/// what its entry requires, what `app.toml` grants, and whether the host
/// would load it. Fails if any app would be refused.
pub fn check(root: &Path, only: &[String], modules: &HostModules) -> Result<Report, String> {
    check_with(root, only, modules, None)
}

/// The secret store of the data directory `data`, if one is named.
fn store_of(data: Option<&Path>) -> Result<Option<Arc<SecretStore>>, String> {
    data.map(|data| SecretStore::open_data(data).map(Arc::new))
        .transpose()
}

/// [`check`], with `store` secrets looked up in `data`'s store.
pub fn check_with(
    root: &Path,
    only: &[String],
    modules: &HostModules,
    data: Option<&Path>,
) -> Result<Report, String> {
    check_as(root, only, modules, data, RemovedKeys::Refuse)
}

/// [`check_with`], with a removed `app.toml` key treated as `removed` says:
/// [`RemovedKeys::Ignore`] for apps that are already deployed (`minicloud
/// check --deployed`, which `install.sh` runs over the installed apps before
/// it switches to a new release), whose removed keys are warnings, printed,
/// rather than refusals.
pub fn check_as(
    root: &Path,
    only: &[String],
    modules: &HostModules,
    data: Option<&Path>,
    removed: RemovedKeys,
) -> Result<Report, String> {
    let source = SecretSource {
        store: store_of(data)?,
        placeholders: false,
    };
    let mut report = Report::default();
    let mut refused = 0;
    let mut warnings = 0;
    let dirs = apps::app_dirs(root, only)?;
    for (name, dir) in &dirs {
        let (mut app, config) =
            apps::describe_as(name, dir, apps::Lineage::first(), None, &source, removed);
        for warning in config.iter().flat_map(|config| &config.warnings) {
            report
                .err
                .push_str(&format!("warning: [{name}] {warning}\n"));
        }
        if config.is_some() {
            app.state = match apps::compile(dir, name, modules) {
                Ok(compiled) => {
                    for notice in &compiled.program.notices {
                        if notice.severity == Severity::Warning {
                            warnings += 1;
                        }
                        report.err.push_str(&render(&compiled.sources, notice));
                    }
                    match verdict(&mut app, &compiled, modules) {
                        Ok(()) => AppState::Refused(String::new()),
                        Err(why) => AppState::Refused(why),
                    }
                }
                Err(why) => {
                    report.err.push_str(&diagnostics_of(&why));
                    AppState::Refused(why)
                }
            };
        }
        let line = match &app.state {
            AppState::Refused(why) if why.is_empty() => "ok".to_string(),
            AppState::Refused(why) => {
                refused += 1;
                // A refusal that is not a diagnostic already printed above —
                // the grant, a spawn, the lowering — is printed in full.
                let printed = ["does not parse", "does not check", "checks with warnings"];
                if !printed.iter().any(|stage| why.starts_with(stage)) {
                    if let Some((_, rest)) = why.split_once('\n') {
                        report.err.push_str(rest);
                        if !rest.ends_with('\n') {
                            report.err.push('\n');
                        }
                    }
                }
                format!("REFUSED: {}", why.lines().next().unwrap_or_default())
            }
            AppState::Ready(_) => unreachable!("check does not prepare"),
        };
        let open = if app.open { " (lower bound)" } else { "" };
        report.out.push_str(&format!(
            "{:<10} requires [{}]{open}  granted [{}]  {line}\n",
            app.name,
            apps::list(&app.required),
            apps::list(&app.granted),
        ));
    }
    report.out.push_str(&format!(
        "checked {} app(s) against the host's schemas ({}); {warnings} warning(s), {refused} refused\n",
        dirs.len(),
        modules
            .schemas()
            .iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>()
            .join(", "),
    ));
    report.ok = refused == 0;
    Ok(report)
}

fn verdict(app: &mut App, compiled: &Compiled, modules: &HostModules) -> Result<(), String> {
    apps::admit(app, compiled)?;
    apps::lower(app, compiled, modules).map(drop)
}

/// What a module that did not compile prints: the diagnostics alone.
fn diagnostics_of(why: &str) -> String {
    for stage in ["does not parse:\n", "does not check:\n"] {
        if let Some(rendered) = why.strip_prefix(stage) {
            return rendered.to_string();
        }
    }
    format!("{why}\n")
}

/// `minicloud test [app…]`: every `test fn` in each app's modules.
pub fn test(
    root: &Path,
    only: &[String],
    filter: Option<&str>,
    modules: &HostModules,
) -> Result<Report, String> {
    test_with(root, only, filter, modules, None)
}

/// [`test()`], with `store` secrets looked up in `data`'s store first.
pub fn test_with(
    root: &Path,
    only: &[String],
    filter: Option<&str>,
    modules: &HostModules,
    data: Option<&Path>,
) -> Result<Report, String> {
    let source = SecretSource {
        store: store_of(data)?,
        placeholders: true,
    };
    let mut report = Report::default();
    let (mut ran, mut failed, mut uncompiled) = (0, 0, 0);
    // What `fetch` waits on: the tests answer every host call blocking, and
    // a blocking fetch is the same future, waited for on this thread.
    let io = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the I/O runtime: {e}"))?;
    for (name, dir) in apps::app_dirs(root, only)? {
        let (app, config) = apps::describe(&name, &dir, &source);
        if let Some(config) = &config {
            if !config.placeholders.is_empty() {
                report.err.push_str(&format!(
                    "note: `{name}` runs its tests with a placeholder for secret(s) {}: not set \
                     here (see `minicloud test --help`)\n",
                    config
                        .placeholders
                        .iter()
                        .map(|secret| format!("`{secret}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        if config.is_none() {
            uncompiled += 1;
            if let AppState::Refused(why) = &app.state {
                report.out.push_str(&format!("fail  {name:<10} {why}\n"));
            }
            continue;
        }
        let compiled = match apps::compile(&dir, &name, modules) {
            Ok(compiled) => compiled,
            Err(why) => {
                uncompiled += 1;
                report
                    .out
                    .push_str(&format!("fail  {name:<10} does not compile\n"));
                report.err.push_str(&diagnostics_of(&why));
                continue;
            }
        };
        let sources = Arc::new(compiled.sources);
        let program = Arc::new(compiled.program);
        let tests = program.tests();
        // The app's own tests, not the standard library's.
        for test in tests.iter().filter(|test| {
            !(test.module == "std" || test.module.starts_with("std."))
                && filter.is_none_or(|filter| test.qualified_name().contains(filter))
        }) {
            ran += 1;
            let line = format!("{name:<10} {}", test.qualified_name());
            match run_test(test, &app, modules, &sources, &program, io.handle()) {
                None => report.out.push_str(&format!("ok    {line}\n")),
                Some(diagnostic) => {
                    failed += 1;
                    report.out.push_str(&format!("fail  {line}\n"));
                    report.err.push_str(&render(&sources, &diagnostic));
                }
            }
        }
    }
    let mut summary = format!("ran {ran} test(s), {} passed", ran - failed);
    if failed > 0 {
        summary.push_str(&format!(", {failed} failed"));
    }
    if uncompiled > 0 {
        summary.push_str(&format!("; {uncompiled} app(s) did not compile"));
    }
    report.out.push_str(&summary);
    report.out.push('\n');
    report.ok = failed == 0 && uncompiled == 0;
    Ok(report)
}

/// Runs one test as `cove test` would — through Cove's own
/// [`testing::TestRun`], which lowers it as an entry of its own, runs it on
/// the VM and reports its outcome by `cove test`'s rules, a lowering refusal
/// labelled where each gap is — with the app's grant, modules and limits; the
/// diagnostic to report when it failed.
fn run_test(
    test: &DeclaredTest,
    app: &App,
    modules: &HostModules,
    sources: &Arc<cove_diag::SourceMap>,
    program: &Arc<cove_sema::resolve::Program>,
    io: &tokio::runtime::Handle,
) -> Option<Diagnostic> {
    let missing = test
        .entry
        .required_capabilities
        .iter()
        .map(|capability| capability.as_str().to_string())
        .find(|capability| !app.granted.contains(capability));
    if let Some(missing) = missing {
        return Some(
            Diagnostic::error(
                testing::FAILED,
                format!(
                    "test `{}` requires `{missing}`, which app.toml does not grant app `{}`",
                    test.qualified_name(),
                    app.name
                ),
            )
            .at(test.entry.decl.name.span)
            .rule("`minicloud test` grants a test what the host grants its app."),
        );
    }
    // A registry per test, with its state in memory: no test sees what
    // another left in the store, and nothing reaches the data directory.
    let context = app.context(true, None, io);
    let hosts =
        match modules.registry(&app.granted, &context) {
            Ok(hosts) => hosts,
            Err(why) => {
                return Some(
                    Diagnostic::error(testing::FAILED, format!("app `{}` {why}", app.name))
                        .at(test.entry.decl.name.span),
                )
            }
        };
    let schemas = HostSchemas::only(modules.schemas());
    let run = TestRun {
        program,
        sources,
        schemas: &schemas,
        backend: TestBackend::Vm,
        limits: Some(app.limits.run.clone()),
    };
    run.run(test, hosts).map(|failure| failure.diagnostic)
}
