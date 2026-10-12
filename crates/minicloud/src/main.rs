//! `minicloud`: serve, check and test the Cove apps in a directory.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use minicloud::config::RemovedKeys;
use minicloud::{deploy, toolchain};
use minicloud::{Backend, Forwarding, Host, HostModules, OpsListener, PublicOrigin, ServeOptions};

#[derive(Parser)]
#[command(
    name = "minicloud",
    version = env!("MINICLOUD_VERSION"),
    about = "Hosts many small Cove web apps on one machine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load every app and serve them over HTTP.
    Serve {
        /// The directory holding one directory per app.
        #[arg(long, default_value = "apps")]
        apps: PathBuf,
        /// Where apps keep their state (`<data>/<app>/kv.sqlite3`).
        #[arg(long, default_value = "data")]
        data: PathBuf,
        /// Where to listen.
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: String,
        /// Worker threads that run Cove (default: one per hardware thread).
        #[arg(long)]
        workers: Option<usize>,
        /// Threads for HTTP and pending host work.
        #[arg(long, default_value_t = 2)]
        io_threads: usize,
        /// How long a run may hold a worker while others wait, in
        /// milliseconds; 0 never asks a run to yield.
        #[arg(long, default_value_t = 2.0)]
        slice: f64,
        /// Connections open at once; one more is answered 503.
        #[arg(long, default_value_t = 10_000)]
        max_connections: usize,
        /// Requests admitted at once over every app; one more is answered 503.
        #[arg(long, default_value_t = 10_000)]
        max_in_flight: usize,
        /// Which tier runs the apps: auto (native where available), vm, native.
        #[arg(long, default_value = "auto")]
        backend: Backend,
        /// Print no `log` lines.
        #[arg(long)]
        quiet: bool,
        /// Where the admin listener (updates) listens; keep it on localhost.
        #[arg(long, default_value = "127.0.0.1:8081")]
        admin: String,
        /// Run without an admin listener: no updates but a restart.
        #[arg(long)]
        no_admin: bool,
        /// Which listener serves `/_host/` (stats, ops page, app details,
        /// logs): `public`, unauthenticated as before, or `admin`, where the
        /// public listener answers 404 for all of `/_host/`. Use `admin`
        /// behind a reverse proxy.
        #[arg(long, default_value = "public")]
        ops_listener: OpsListener,
        /// The origin clients reach the host at, such as
        /// `https://tools.example`: apps are told this scheme and host
        /// whatever the request says (behind a TLS-terminating proxy).
        #[arg(long)]
        public_origin: Option<PublicOrigin>,
        /// Believe the request's `X-Forwarded-Proto` and `X-Forwarded-Host`
        /// (only behind a proxy that sets them); without it, and without
        /// `--public-origin`, apps are told `http` and the request's `Host`.
        #[arg(long)]
        trust_proxy: bool,
        /// The certificate chain the public listener serves, leaf first, in
        /// PEM. With `--tls-key`, the host terminates TLS itself and
        /// speaks HTTP/2 as well; without both, it is plain HTTP for a
        /// proxy in front. SIGHUP reads the pair again, for a renewal.
        #[arg(long, value_name = "FILE", requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        /// The private key for `--tls-cert`, in PEM.
        #[arg(long, value_name = "FILE", requires = "tls_cert")]
        tls_key: Option<PathBuf>,
        /// On SIGTERM or Ctrl-C, how long to wait for requests in flight, in
        /// seconds; new ones are answered 503 meanwhile.
        #[arg(long, default_value_t = 10.0)]
        shutdown_grace: f64,
    },
    /// Load an app's directory again and switch the running host to the new
    /// version if it loads (or add the app, for a new name). In-flight
    /// requests finish on the old version.
    Update {
        app: String,
        #[command(flatten)]
        admin: AdminArgs,
    },
    /// Send an app's directory to the running host, which checks it with its
    /// config and env and, only if it loads, writes it to `<apps>/<name>`
    /// (keeping the version it replaces for `rollback`) and updates to it.
    /// Packs `app.toml` and the `.cove` files only; refuses symbolic links.
    Deploy {
        /// The app's directory, or `-` for a tar archive of it on stdin
        /// (`tar -C <dir> -c . | minicloud deploy - --name <app>`).
        source: PathBuf,
        /// The app's name; the directory's name by default.
        #[arg(long)]
        name: Option<String>,
        /// Write into this apps directory instead, with no running host,
        /// checking as `minicloud check` does; the host loads it at its next
        /// start (or `minicloud update`). What `deploy/install.sh` uses.
        #[arg(long, value_name = "APPS")]
        into: Option<PathBuf>,
        /// With `--into`: the host's data directory, whose secret store
        /// (`<data>/_host/secrets`) `{ store = "…" }` secrets are checked
        /// against.
        #[arg(long, requires = "into")]
        data: Option<PathBuf>,
        #[command(flatten)]
        admin: AdminArgs,
    },
    /// Put back the version of an app its last deploy replaced, through the
    /// same update (a second rollback undoes the first).
    Rollback {
        app: String,
        #[command(flatten)]
        admin: AdminArgs,
    },
    /// Stop routing to an app on the running host.
    Remove {
        app: String,
        #[command(flatten)]
        admin: AdminArgs,
    },
    /// Route to a disabled app again (kept across restarts).
    Enable {
        app: String,
        #[command(flatten)]
        admin: AdminArgs,
    },
    /// Stop routing to an app, keeping it loaded and its data (kept across
    /// restarts). Works on the admin app too, which the admin app cannot do.
    Disable {
        app: String,
        #[command(flatten)]
        admin: AdminArgs,
    },
    /// Drop the admin app's changes to an app's grant, allowlist and limits,
    /// and reload it from its app.toml.
    Reset {
        app: String,
        #[command(flatten)]
        admin: AdminArgs,
    },
    /// What an app's queues hold, and its dead letters.
    Queues {
        app: String,
        /// Put a dead letter back on its queue.
        #[arg(long, value_name = "ID")]
        revive: Option<i64>,
        #[command(flatten)]
        admin: AdminArgs,
    },
    /// An app's recent log lines, and with `--follow` every line as it is
    /// written, until Ctrl-C.
    Logs {
        app: String,
        /// How many lines to show first.
        #[arg(short, long, default_value_t = 200)]
        n: usize,
        /// Keep the connection open and print every line as it happens.
        #[arg(short, long)]
        follow: bool,
        #[command(flatten)]
        admin: AdminArgs,
    },
    /// Check apps against the host's schemas and grants; non-zero if any
    /// would be refused.
    Check {
        #[arg(long, default_value = "apps")]
        apps: PathBuf,
        /// The host's data directory: `{ store = "…" }` secrets are looked
        /// up in its secret store (`<data>/_host/secrets`). Without it there
        /// is no store, and an app that takes a secret from it is refused.
        #[arg(long)]
        data: Option<PathBuf>,
        /// The apps are already deployed, not being put forward: a key
        /// `app.toml` may no longer say (`limits.fuel`, Cove ADR 0091) is a
        /// warning, as when the host starts, rather than a refusal.
        /// `install.sh` checks the installed apps this way.
        #[arg(long)]
        deployed: bool,
        /// Only these apps.
        names: Vec<String>,
    },
    /// Run apps' `test fn`s with the host's modules, grants and limits.
    ///
    /// A secret that cannot be resolved here — its environment variable
    /// unset, its file missing, its store entry not set — does not stop the
    /// tests: it is given the value `minicloud-test-placeholder-<name>`, and
    /// the run says which secrets were. A secret only gates `auth.check` and
    /// a `[fetch.headers]` header, which a test should not reach for real.
    Test {
        #[arg(long, default_value = "apps")]
        apps: PathBuf,
        /// The host's data directory, whose secret store is tried first for
        /// `{ store = "…" }` secrets.
        #[arg(long)]
        data: Option<PathBuf>,
        /// Only tests whose qualified name contains this.
        #[arg(long)]
        filter: Option<String>,
        /// Only these apps.
        names: Vec<String>,
    },
    /// The running host's secret store (`<data>/_host/secrets`), which
    /// `app.toml` reads with `[secrets] x = { store = "<name>" }`. Write-only:
    /// nothing prints a value.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
}

#[derive(Subcommand)]
enum SecretCommand {
    /// Set or replace a secret, reading its value from stdin (not echoed on
    /// a terminal; one trailing newline is dropped), and reload the apps that
    /// use it.
    Set {
        name: String,
        #[command(flatten)]
        admin: AdminArgs,
    },
    /// Every secret stored or used: name, set or not, when it was set, and
    /// the apps that use it.
    List {
        #[command(flatten)]
        admin: AdminArgs,
    },
    /// Delete a secret. Refused while an app uses it, unless `--force`,
    /// which reloads those apps without it (they are then refused).
    Delete {
        name: String,
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        admin: AdminArgs,
    },
}

/// How `update` and `remove` reach the running host.
#[derive(clap::Args)]
struct AdminArgs {
    /// The host's admin listener.
    #[arg(long, default_value = "127.0.0.1:8081")]
    admin: String,
    /// The file holding the admin token; the host's `<data>/admin.token`.
    #[arg(long, default_value = "data/admin.token")]
    token_file: PathBuf,
}

/// Sends one admin request; prints the answer; fails unless it was a 2xx.
fn admin(args: &AdminArgs, method: reqwest::Method, path: &str) -> ExitCode {
    admin_with(args, method, path, None)
}

/// [`admin`], with a body.
fn admin_with(
    args: &AdminArgs,
    method: reqwest::Method,
    path: &str,
    body: Option<(&str, Vec<u8>)>,
) -> ExitCode {
    let token = match std::fs::read_to_string(&args.token_file) {
        Ok(token) => token.trim().to_string(),
        Err(e) => {
            eprintln!(
                "minicloud: cannot read the admin token from `{}`: {e}",
                args.token_file.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let url = format!("http://{}{path}", args.admin);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("minicloud: {e}");
            return ExitCode::FAILURE;
        }
    };
    let answer = runtime.block_on(async {
        let mut request = reqwest::Client::new()
            .request(method, &url)
            .bearer_auth(token)
            .timeout(Duration::from_secs(600));
        if let Some((content_type, body)) = body {
            request = request.header("content-type", content_type).body(body);
        }
        let response = request.send().await?;
        let status = response.status();
        Ok::<_, reqwest::Error>((status, response.text().await?))
    });
    match answer {
        Ok((status, body)) if status.is_success() => {
            print!("{body}");
            ExitCode::SUCCESS
        }
        Ok((status, body)) => {
            eprint!("minicloud: {status}\n{body}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("minicloud: cannot reach the admin listener at {url}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `minicloud logs --follow`: the lines as the host writes them, until the
/// connection is broken or the program is interrupted.
///
/// The answer is server-sent events ([`minicloud::ops::follow`]), which is
/// a line prefixed `data: ` for each log line and a line starting with `:`
/// for the host's own remarks — a heartbeat, or a notice that the log
/// outran this reader. The remarks go to standard error so that the log
/// itself can be piped.
fn logs_follow(args: &AdminArgs, app: &str, n: usize) -> ExitCode {
    let token = match std::fs::read_to_string(&args.token_file) {
        Ok(token) => token.trim().to_string(),
        Err(e) => {
            eprintln!(
                "minicloud: cannot read the admin token from `{}`: {e}",
                args.token_file.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let url = format!("http://{}/apps/{app}/logs?n={n}&follow=1", args.admin);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("minicloud: {e}");
            return ExitCode::FAILURE;
        }
    };
    let followed = runtime.block_on(async {
        let response = reqwest::Client::new()
            .get(&url)
            .bearer_auth(token)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            eprint!("minicloud: {status}\n{}", response.text().await?);
            return Ok(false);
        }
        let mut response = response;
        let mut pending = String::new();
        while let Some(chunk) = response.chunk().await? {
            pending.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(at) = pending.find('\n') {
                let line: String = pending.drain(..=at).collect();
                let line = line.trim_end_matches('\n');
                if let Some(said) = line.strip_prefix("data: ") {
                    println!("{said}");
                } else if let Some(remark) = line.strip_prefix(": ") {
                    if remark != "ping" {
                        eprintln!("minicloud: {remark}");
                    }
                }
            }
        }
        Ok::<bool, reqwest::Error>(true)
    });
    match followed {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("minicloud: cannot follow `{app}`'s log at {url}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Serve {
            apps,
            data,
            addr,
            workers,
            io_threads,
            slice,
            max_connections,
            max_in_flight,
            backend,
            quiet,
            admin,
            no_admin,
            ops_listener,
            public_origin,
            trust_proxy,
            tls_cert,
            tls_key,
            shutdown_grace,
        } => {
            let mut options = ServeOptions::new(apps);
            options.addr = addr;
            options.data = Some(data);
            if let Some(workers) = workers {
                options.workers = workers.max(1);
            }
            options.io_threads = io_threads;
            options.slice = (slice > 0.0).then(|| Duration::from_secs_f64(slice / 1e3));
            options.max_connections = max_connections;
            options.max_in_flight = max_in_flight;
            options.backend = backend;
            options.quiet = quiet;
            options.admin = (!no_admin).then_some(admin);
            options.ops_listener = ops_listener;
            options.forwarding = Forwarding {
                public_origin,
                trust_proxy,
            };
            options.tls = match (tls_cert, tls_key) {
                (Some(cert), Some(key)) => Some(minicloud::tls::Pair { cert, key }),
                _ => None,
            };
            eprintln!("minicloud: loading apps from {}", options.apps.display());
            let host = match Host::start(options) {
                Ok(host) => host,
                Err(why) => {
                    eprintln!("minicloud: {why}");
                    return ExitCode::FAILURE;
                }
            };
            eprintln!("minicloud {}", env!("MINICLOUD_VERSION"));
            eprint!("{}", host.banner());
            host.wait_for_signal();
            eprintln!("minicloud: shutting down");
            let left = host.shutdown(Duration::from_secs_f64(shutdown_grace.max(0.0)));
            if left > 0 {
                eprintln!("minicloud: {left} request(s) still in flight after {shutdown_grace} s; stopping anyway");
            }
            ExitCode::SUCCESS
        }
        Command::Update { app, admin: args } => {
            admin(&args, reqwest::Method::POST, &format!("/apps/{app}/update"))
        }
        Command::Deploy {
            source,
            name,
            into,
            data,
            admin: args,
        } => deploy_command(&source, name, into.as_deref(), data.as_deref(), &args),
        Command::Rollback { app, admin: args } => admin(
            &args,
            reqwest::Method::POST,
            &format!("/apps/{app}/rollback"),
        ),
        Command::Remove { app, admin: args } => {
            admin(&args, reqwest::Method::DELETE, &format!("/apps/{app}"))
        }
        Command::Enable { app, admin: args } => {
            admin(&args, reqwest::Method::POST, &format!("/apps/{app}/enable"))
        }
        Command::Disable { app, admin: args } => admin(
            &args,
            reqwest::Method::POST,
            &format!("/apps/{app}/disable"),
        ),
        Command::Reset { app, admin: args } => {
            admin(&args, reqwest::Method::POST, &format!("/apps/{app}/reset"))
        }
        Command::Queues {
            app,
            revive,
            admin: args,
        } => match revive {
            Some(id) => admin(
                &args,
                reqwest::Method::POST,
                &format!("/apps/{app}/queues/revive/{id}"),
            ),
            None => admin(&args, reqwest::Method::GET, &format!("/apps/{app}/queues")),
        },
        Command::Logs {
            app,
            n,
            follow,
            admin: args,
        } => {
            if follow {
                logs_follow(&args, &app, n)
            } else {
                admin(
                    &args,
                    reqwest::Method::GET,
                    &format!("/apps/{app}/logs?n={n}"),
                )
            }
        }
        Command::Check {
            apps,
            data,
            deployed,
            names,
        } => finish(toolchain::check_as(
            &apps,
            &names,
            &HostModules::standard(),
            data.as_deref(),
            if deployed {
                RemovedKeys::Ignore
            } else {
                RemovedKeys::Refuse
            },
        )),
        Command::Test {
            apps,
            data,
            filter,
            names,
        } => finish(toolchain::test_with(
            &apps,
            &names,
            filter.as_deref(),
            &HostModules::standard(),
            data.as_deref(),
        )),
        Command::Secret { command } => secret_command(command),
    }
}

/// `minicloud secret set|list|delete`, through the admin listener.
fn secret_command(command: SecretCommand) -> ExitCode {
    match command {
        SecretCommand::List { admin: args } => admin(&args, reqwest::Method::GET, "/secrets"),
        SecretCommand::Delete {
            name,
            force,
            admin: args,
        } => admin(
            &args,
            reqwest::Method::DELETE,
            &format!("/secrets/{name}{}", if force { "?force=1" } else { "" }),
        ),
        SecretCommand::Set { name, admin: args } => {
            if let Err(why) = minicloud::secrets::valid_name(&name) {
                eprintln!("minicloud: {why}");
                return ExitCode::FAILURE;
            }
            let value = match read_secret_value(&name) {
                Ok(value) => value,
                Err(why) => {
                    eprintln!("minicloud: {why}");
                    return ExitCode::FAILURE;
                }
            };
            admin_with(
                &args,
                reqwest::Method::PUT,
                &format!("/secrets/{name}"),
                Some(("application/octet-stream", value.into_bytes())),
            )
        }
    }
}

/// A secret's value from stdin: on a terminal, one line typed without
/// echo; otherwise everything, with one trailing newline dropped.
fn read_secret_value(name: &str) -> Result<String, String> {
    use std::io::IsTerminal;
    let stdin = std::io::stdin();
    let mut value = String::new();
    if stdin.is_terminal() {
        eprint!("value for secret `{name}` (not echoed): ");
        let echo_off = set_echo(false);
        let read = stdin.lock().read_line(&mut value);
        if echo_off {
            set_echo(true);
        }
        eprintln!();
        read.map_err(|e| format!("cannot read the value: {e}"))?;
    } else {
        stdin
            .lock()
            .take(minicloud::secrets::MAX_VALUE_BYTES as u64 + 2)
            .read_to_string(&mut value)
            .map_err(|e| format!("cannot read the value from stdin: {e}"))?;
    }
    if let Some(rest) = value.strip_suffix('\n') {
        value = rest.strip_suffix('\r').unwrap_or(rest).to_string();
    }
    if value.is_empty() {
        return Err(format!("no value for secret `{name}` on stdin"));
    }
    Ok(value)
}

/// Turns the terminal's echo on or off with `stty`; whether it did.
fn set_echo(on: bool) -> bool {
    std::process::Command::new("stty")
        .arg(if on { "echo" } else { "-echo" })
        .stdin(std::process::Stdio::inherit())
        .status()
        .is_ok_and(|status| status.success())
}

fn finish(report: Result<toolchain::Report, String>) -> ExitCode {
    match report {
        Ok(report) => {
            eprint!("{}", report.err);
            print!("{}", report.out);
            if report.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(why) => {
            eprintln!("minicloud: {why}");
            ExitCode::FAILURE
        }
    }
}

/// `minicloud deploy`: packs the app (or reads it from stdin), then sends it
/// to the running host or, with `--into`, writes it into an apps directory.
fn deploy_command(
    source: &Path,
    name: Option<String>,
    into: Option<&Path>,
    data: Option<&Path>,
    args: &AdminArgs,
) -> ExitCode {
    let limits = deploy::Limits::standard();
    let from_stdin = source == Path::new("-");
    let files = if from_stdin {
        let mut archive = Vec::new();
        let read = std::io::stdin()
            .lock()
            .take(limits.max_archive_bytes() + 1)
            .read_to_end(&mut archive);
        if let Err(e) = read {
            eprintln!("minicloud: cannot read the archive from stdin: {e}");
            return ExitCode::FAILURE;
        }
        deploy::unpack(&archive, limits)
    } else {
        deploy::collect(source, limits)
    };
    let files = match files {
        Ok(files) => files,
        Err(why) => {
            eprintln!("minicloud: deploy refused: {why}");
            return ExitCode::FAILURE;
        }
    };
    let name = match name {
        Some(name) => name,
        None if from_stdin => {
            eprintln!("minicloud: `deploy -` needs `--name <app>`");
            return ExitCode::FAILURE;
        }
        None => match std::fs::canonicalize(source)
            .ok()
            .and_then(|dir| Some(dir.file_name()?.to_str()?.to_string()))
        {
            Some(name) => name,
            None => {
                eprintln!(
                    "minicloud: cannot name the app from `{}`; pass --name",
                    source.display()
                );
                return ExitCode::FAILURE;
            }
        },
    };
    if !files.skipped.is_empty() {
        eprintln!(
            "minicloud: not part of the app, left out: {}",
            files.skipped.join(", ")
        );
    }
    eprintln!(
        "minicloud: deploying `{name}`: {} file(s), {} bytes",
        files.files.len(),
        files.bytes()
    );
    if let Some(apps) = into {
        return finish(deploy::deploy_into(
            apps,
            &name,
            &files,
            &HostModules::standard(),
            data,
        ));
    }
    let archive = match deploy::pack(&files) {
        Ok(archive) => archive,
        Err(why) => {
            eprintln!("minicloud: {why}");
            return ExitCode::FAILURE;
        }
    };
    admin_with(
        args,
        reqwest::Method::POST,
        &format!("/apps/{name}/deploy"),
        Some(("application/x-tar", archive)),
    )
}
