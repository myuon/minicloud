//! The HTTP front: hyper on tokio, handing every request to the scheduler.
//!
//! hyper owns the protocol — HTTP/1.1 keep-alive, pipelining, chunked
//! request bodies, header limits and timeouts — and tokio owns the sockets,
//! so an idle connection costs a task and no thread, and waiting on many of
//! them is epoll/kqueue rather than a `poll(2)` over every socket (the edge
//! sample's cove#590). No Cove code runs on tokio's threads: a request is
//! admitted into its app's queue ([`crate::sched`]) and the connection's task
//! awaits a oneshot that a worker answers.
//!
//! What is refused here, before anything runs:
//!
//! | condition | status |
//! | --- | --- |
//! | `--max-connections` open | 503, `Retry-After: 1`, connection closed |
//! | no app by that name | 404 |
//! | the app was refused at load | 503, with the reason (no `Retry-After`) |
//! | body larger than the app's `max_request_bytes` | 413 |
//! | body that is not UTF-8 | 400 |
//! | the app has `max_queued` requests waiting | 429, `Retry-After: 1` |
//! | the server has `--max-in-flight` requests admitted | 503, `Retry-After: 1` |
//!
//! And what is *answered* here, before anything runs: a `GET` or `HEAD`
//! whose path names a file in the app's `assets/` directory
//! ([`crate::assets`]), with the bytes or a `304`.
//!
//! A second listener, the **admin** one ([`crate::admin`]), takes updates.
//! An update loads the new version off the request path and switches the
//! app's route to it only if it loaded; see [`Host::update`].

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::Value as Json;
use tokio::io::AsyncWriteExt;
use tokio::sync::{oneshot, Semaphore};

use crate::apps::{load_all, load_as, App, AppState, Backend, Lineage, LoadOptions};
use crate::config::RemovedKeys;
use crate::convert::{AppRequest, Reply};
use crate::hosts::HostModules;
use crate::ops::{self, OpsContext, OpsListener};
use crate::overrides::Control;
use crate::proxy::Forwarding;
use crate::router::{ByHostname, Router};
use crate::sched::{Cancel, Engine, Flight, Installed, Start};
use crate::stats::{Rejection, ServerCounters};

/// How to start a host.
#[derive(Clone)]
pub struct ServeOptions {
    /// The directory holding one directory per app.
    pub apps: PathBuf,
    /// Where to listen; port 0 picks a free one.
    pub addr: String,
    /// Threads that run Cove.
    pub workers: usize,
    /// Threads that run the HTTP front and pending host work.
    pub io_threads: usize,
    /// How long a run may hold a worker while others wait; `None` never asks.
    pub slice: Option<Duration>,
    /// Connections open at once; one more is answered 503 and closed.
    pub max_connections: usize,
    /// Requests admitted (queued, running or parked) over every app.
    pub max_in_flight: usize,
    pub backend: Backend,
    /// Whether `log` prints nothing.
    pub quiet: bool,
    pub modules: HostModules,
    /// Where apps keep their state, one directory per app; `None` keeps it
    /// in memory, gone at exit.
    pub data: Option<PathBuf>,
    /// Where the admin listener listens; `None` has none (no updates but a
    /// restart).
    pub admin: Option<String>,
    /// The admin token. `None` reads `<data>/admin.token`, writing a fresh
    /// random one there first if there is none.
    pub admin_token: Option<String>,
    /// Which listener serves `/_host/` (`--ops-listener`).
    pub ops_listener: OpsListener,
    /// What apps are told of the client's scheme and host
    /// (`--public-origin`, `--trust-proxy`).
    pub forwarding: Forwarding,
    /// The certificate and key the public listener terminates TLS with
    /// (`--tls-cert`, `--tls-key`); `None` is plain HTTP, for a host
    /// behind a proxy that holds them ([`crate::tls`]).
    pub tls: Option<crate::tls::Pair>,
    /// What the host asks an authority for, so that it holds its own
    /// certificate ([`crate::acme`]); `None` for a pair something else
    /// renews, or no TLS at all.
    pub acme: Option<crate::acme::Order>,
    /// Where the plain listener that answers ACME challenges and redirects
    /// everything else to `https://` listens; `None` has none.
    pub acme_http: Option<String>,
}

impl ServeOptions {
    /// The defaults `minicloud serve` uses, for `apps`.
    pub fn new(apps: impl Into<PathBuf>) -> ServeOptions {
        ServeOptions {
            apps: apps.into(),
            addr: "127.0.0.1:8080".to_string(),
            workers: std::thread::available_parallelism().map_or(4, |n| n.get()),
            io_threads: 2,
            slice: Some(Duration::from_millis(2)),
            max_connections: 10_000,
            max_in_flight: 10_000,
            backend: Backend::Auto,
            quiet: false,
            modules: HostModules::standard(),
            data: Some(PathBuf::from("data")),
            admin: Some("127.0.0.1:8081".to_string()),
            admin_token: None,
            ops_listener: OpsListener::Public,
            forwarding: Forwarding::default(),
            tls: None,
            acme: None,
            acme_http: None,
        }
    }
}

/// What the connection tasks share.
pub(crate) struct Front {
    pub(crate) engine: Arc<Engine>,
    counters: ServerCounters,
    connections: Arc<Semaphore>,
    started: Instant,
    pub(crate) options: ServeOptions,
    /// How an update loads a version: the options the first load had.
    pub(crate) load: LoadOptions,
    /// One update (or admin change) at a time, so that two cannot race for
    /// a version number.
    pub(crate) updating: tokio::sync::Mutex<()>,
    /// The admin's changes and their history ([`crate::overrides`]).
    pub(crate) control: Arc<Control>,
    /// What runs an app on a schedule ([`crate::triggers`]).
    pub(crate) triggers: Arc<crate::triggers::Triggers>,
    /// The `[rate]` buckets of every app ([`crate::rate`]).
    pub(crate) rates: crate::rate::Limiter,
    /// What each app did, minute by minute ([`crate::series`]); `None` for
    /// a host with no data directory, which keeps no history.
    pub(crate) series: Option<Arc<std::sync::Mutex<crate::series::Series>>>,
    /// Each app's running totals as the last sample saw them, which is what
    /// the next one takes its difference from ([`crate::ops::sample_now`]).
    pub(crate) sampled: std::sync::Mutex<
        std::collections::HashMap<String, (crate::series::Sample, crate::histogram::Snapshot)>,
    >,
    /// Where [`Front::platform_now`] reads the host's process and machine
    /// from: `None` for the live files ([`crate::platform::read_system`]),
    /// or what a test put there ([`Host::set_platform_source`]).
    pub(crate) platform_source: std::sync::RwLock<Option<PlatformSource>>,
    /// The platform as the last sample saw it, which the next one takes the
    /// CPU's difference from.
    pub(crate) platform_sampled: std::sync::Mutex<Option<crate::platform::PlatformSample>>,
    pub(crate) admin_token: String,
    /// Set by [`Host::shutdown`]: new requests are answered 503.
    draining: AtomicBool,
    /// What the public listener accepts with, when it terminates TLS.
    pub(crate) certificates: Option<Arc<crate::tls::Certificates>>,
}

/// What a test reads the platform from instead of the live files.
pub type PlatformSource = Arc<dyn Fn() -> crate::platform::PlatformSample + Send + Sync>;

/// Why an update did not happen.
#[derive(Debug)]
pub enum UpdateError {
    /// No directory with an `app.toml` by that name.
    NotFound(String),
    /// The new version did not load; the current one still serves. The
    /// reason, with the diagnostics.
    Refused {
        current: Option<String>,
        why: String,
    },
}

/// A running host.
pub struct Host {
    /// Where it listens.
    pub addr: SocketAddr,
    /// Where the admin listener listens, if there is one.
    pub admin_addr: Option<SocketAddr>,
    front: Arc<Front>,
    runtime: Option<tokio::runtime::Runtime>,
    _workers: Vec<JoinHandle<()>>,
}

impl Host {
    /// Loads the apps, binds, and starts the threads. Returns once the host
    /// is accepting. An app that is refused is reported and not served; the
    /// error is only for a host that cannot start at all.
    pub fn start(options: ServeOptions) -> Result<Host, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(options.io_threads.max(1))
            .thread_name("minicloud-io")
            .enable_all()
            .build()
            .map_err(|e| format!("cannot start the I/O runtime: {e}"))?;
        let control = Arc::new(Control::open(options.data.as_deref())?);
        // Where apps put work that outlives a request ([`crate::defer`]).
        // The channel is made before the apps are loaded because their
        // modules take the sending end; the host drains it once the engine
        // that runs the work exists.
        let (defer_to, deferred) = tokio::sync::mpsc::channel(1024);
        let load = LoadOptions {
            backend: options.backend,
            quiet: options.quiet,
            modules: options.modules.clone(),
            data: options.data.clone(),
            io: runtime.handle().clone(),
            control: Some(Arc::clone(&control)),
            // Every app found at start is one already deployed: an `app.toml`
            // an earlier host accepted is not refused for a key this one
            // removed. A deploy or an update refuses it (`load_next`).
            removed_keys: RemovedKeys::Ignore,
            defer_to: Some(defer_to),
        };
        let mut apps = load_all(&options.apps, &load)?;
        refuse_taken_hostnames(&mut apps);
        Host::launch(runtime, apps, options, load, control, deferred)
    }

    /// Starts a host over apps already loaded on `runtime`.
    fn launch(
        runtime: tokio::runtime::Runtime,
        apps: Vec<App>,
        options: ServeOptions,
        load: LoadOptions,
        control: Arc<Control>,
        deferred: tokio::sync::mpsc::Receiver<crate::defer::Deferred>,
    ) -> Result<Host, String> {
        if options.ops_listener == OpsListener::Admin && options.admin.is_none() {
            return Err(
                "--ops-listener admin needs the admin listener; drop --no-admin".to_string(),
            );
        }
        let listener = std::net::TcpListener::bind(&options.addr)
            .map_err(|e| format!("cannot listen on `{}`: {e}", options.addr))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let admin = match &options.admin {
            Some(at) => {
                let admin = std::net::TcpListener::bind(at)
                    .map_err(|e| format!("cannot listen on `{at}` for the admin: {e}"))?;
                admin.set_nonblocking(true).map_err(|e| e.to_string())?;
                Some(admin)
            }
            None => None,
        };
        let admin_addr = admin
            .as_ref()
            .map(|admin| admin.local_addr().map_err(|e| e.to_string()))
            .transpose()?;
        let admin_token = match (&options.admin_token, &options.admin) {
            (Some(token), _) => token.clone(),
            (None, None) => String::new(),
            (None, Some(_)) => crate::admin::token_file(options.data.as_deref())?,
        };
        let disabled: Vec<String> = apps
            .iter()
            .filter(|app| !control.overrides.enabled(&app.name))
            .map(|app| app.name.clone())
            .collect();
        let engine = Arc::new(Engine::new(
            apps,
            options.workers,
            options.max_in_flight,
            options.slice,
            runtime.handle().clone(),
        ));
        for name in &disabled {
            if let Some(slot) = engine.slot_named(name) {
                slot.set_enabled(false);
            }
        }
        let workers = engine.start_workers()?;
        runtime.spawn(Arc::clone(&engine).monitor());
        // A queue is a file, so a host with nowhere to keep one delivers
        // nothing: an app may still send, into a store that lives and dies
        // with the process (`minicloud test`).
        let options_data_kept = options.data.is_some();
        let options_acme = options.acme.clone();
        let options_acme_http = options.acme_http.clone();
        // The history is the host's own state, beside the admin's changes
        // and the secret store, and is lost with the data directory as they
        // are. A host that keeps nothing still answers every current value.
        let series = match &options.data {
            Some(data) => {
                match crate::series::Series::open(&data.join("_host").join("series.sqlite3")) {
                    Ok(series) => Some(Arc::new(std::sync::Mutex::new(series))),
                    Err(why) => {
                        eprintln!("minicloud: no history will be kept: {why}");
                        None
                    }
                }
            }
            None => None,
        };
        // Read before anything is served: a certificate that does not read
        // is a host that does not start, rather than one that listens and
        // refuses every connection. With `--acme-domain` the host may have
        // none yet, which is not an error: it is what the first order is
        // for, and the listener waits for it.
        let challenges = Arc::new(crate::acme::Challenges::default());
        let pair = match (&options.tls, &options.acme) {
            (Some(pair), _) => Some(pair.clone()),
            (None, Some(order)) => Some(order.pair()),
            (None, None) => None,
        };
        let certificates = match (&pair, &options.acme) {
            (Some(pair), None) => Some(Arc::new(crate::tls::Certificates::load(pair.clone())?)),
            (Some(pair), Some(_)) => match crate::tls::Certificates::load(pair.clone()) {
                Ok(certificates) => Some(Arc::new(certificates)),
                Err(why) => {
                    eprintln!("minicloud: no certificate yet ({why}); ordering one");
                    Some(Arc::new(crate::tls::Certificates::none(pair.clone())))
                }
            },
            (None, _) => None,
        };
        let triggers = Arc::new(crate::triggers::Triggers::new(
            Arc::clone(&engine),
            options.forwarding.clone(),
            runtime.handle().clone(),
        ));
        let front = Arc::new(Front {
            engine,
            counters: ServerCounters::default(),
            connections: Arc::new(Semaphore::new(options.max_connections.max(1))),
            started: Instant::now(),
            options,
            load,
            updating: tokio::sync::Mutex::new(()),
            admin_token,
            draining: AtomicBool::new(false),
            control: Arc::clone(&control),
            triggers: Arc::clone(&triggers),
            rates: crate::rate::Limiter::default(),
            certificates,
            series,
            sampled: std::sync::Mutex::new(std::collections::HashMap::new()),
            platform_source: std::sync::RwLock::new(None),
            platform_sampled: std::sync::Mutex::new(None),
        });
        control.attach(&front);
        let (listener, admin) = {
            let _entered = runtime.enter();
            (
                tokio::net::TcpListener::from_std(listener).map_err(|e| e.to_string())?,
                admin
                    .map(tokio::net::TcpListener::from_std)
                    .transpose()
                    .map_err(|e| e.to_string())?,
            )
        };
        // Always, whether an app has a schedule or not: an app deployed
        // onto the running host brings its own, and a timer that wakes once
        // a minute to find nothing due costs nothing.
        runtime.spawn(crate::triggers::tick(Arc::clone(&triggers)));
        runtime.spawn(crate::defer::deliver(Arc::clone(&front), deferred));
        if options_data_kept {
            runtime.spawn(crate::queue_host::deliver(Arc::clone(&front)));
        }
        if front.series.is_some() {
            runtime.spawn(crate::ops::sample(Arc::clone(&front)));
        }
        if let (Some(order), Some(certificates)) = (&options_acme, &front.certificates) {
            runtime.spawn(crate::acme::keep(
                order.clone(),
                Arc::clone(&challenges),
                Arc::clone(certificates),
            ));
        }
        if let Some(at) = &options_acme_http {
            let listener = std::net::TcpListener::bind(at)
                .map_err(|e| format!("cannot listen on `{at}` for ACME: {e}"))?;
            listener.set_nonblocking(true).map_err(|e| e.to_string())?;
            let listener = {
                let _entered = runtime.enter();
                tokio::net::TcpListener::from_std(listener).map_err(|e| e.to_string())?
            };
            runtime.spawn(crate::acme::serve_challenges(
                listener,
                Arc::clone(&challenges),
            ));
        }
        runtime.spawn(Arc::clone(&front).accept(listener));
        if let Some(admin) = admin {
            runtime.spawn(crate::admin::serve(Arc::clone(&front), admin));
        }
        Ok(Host {
            addr,
            admin_addr,
            front,
            runtime: Some(runtime),
            _workers: workers,
        })
    }

    /// The version each app routes to now, loaded or refused; removed apps
    /// are left out.
    pub fn apps(&self) -> Vec<Arc<App>> {
        self.front
            .engine
            .slots()
            .iter()
            .filter_map(|slot| slot.current())
            .collect()
    }

    /// Runs every `[[triggers]]` schedule due in the minute
    /// `unix_seconds` is in, and answers how many runs it started.
    ///
    /// The host's own clock calls this a moment after each minute begins
    /// ([`crate::triggers::tick`]); a test calls it with a time of its own.
    pub fn fire_due(&self, unix_seconds: i64) -> usize {
        self.front.triggers.fire_due(unix_seconds)
    }

    /// Takes one sample of every app's counters into the host's history,
    /// as the minute's tick does, and answers how many rows it wrote.
    ///
    /// The first call after a start writes none: there is nothing to take a
    /// difference from, and a host that counted its running totals as one
    /// minute's work would show a spike at every restart.
    pub fn sample_now(&self, at_unix: i64) -> usize {
        ops::sample_now(&self.front, at_unix)
    }

    /// Reads the host's process and machine from `source` from now on,
    /// instead of the live files and the engine's gauges: what a test does
    /// so that the platform's figures, in the history and on a page, are
    /// its own and not the machine's it runs on.
    pub fn set_platform_source(&self, source: PlatformSource) {
        *self
            .front
            .platform_source
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Some(source);
    }

    /// What the host's process and machine look like now
    /// ([`crate::platform`]).
    pub fn platform_now(&self) -> crate::platform::PlatformSample {
        self.front.platform_now()
    }

    /// What `GET /_host/stats` answers.
    pub fn stats(&self) -> Json {
        ops::stats(&self.front.ops())
    }

    /// What `GET /_host/apps/<app>` answers.
    pub fn app_detail(&self, name: &str) -> Option<Json> {
        let slot = self.front.engine.slot_named(name)?;
        Some(ops::app_detail(&self.front.ops(), &slot))
    }

    /// Loads the app `name` again from its directory and, if it loads,
    /// routes to the new version: what `POST /apps/<name>/update` on the
    /// admin listener does.
    pub fn update(&self, name: &str) -> Result<Installed, UpdateError> {
        let runtime = self.runtime.as_ref().expect("running");
        runtime.block_on(self.front.update(name))
    }

    /// The admin token.
    pub fn admin_token(&self) -> &str {
        &self.front.admin_token
    }

    /// The startup banner: one line per app, and where to look.
    pub fn banner(&self) -> String {
        let mut out = String::new();
        for app in self.apps() {
            out.push_str(&format!("  {}\n", app.describe()));
        }
        let options = &self.front.options;
        out.push_str(&format!(
            "\nlistening on http://{} — {} worker thread(s), {}, a fresh isolate per request, \
             apps served round robin\n",
            self.addr,
            self.front.engine.workers(),
            match options.slice {
                Some(slice) => format!(
                    "a run yields after {:.1} ms while others wait",
                    slice.as_secs_f64() * 1e3
                ),
                None => "no time slice".to_string(),
            },
        ));
        let ops_at = match (options.ops_listener, self.admin_addr) {
            (OpsListener::Admin, Some(admin)) => admin,
            _ => self.addr,
        };
        out.push_str(&format!(
            "  stats: curl -s http://{ops_at}/_host/stats   ops page: http://{ops_at}/_host/ui{}\n",
            match options.ops_listener {
                OpsListener::Admin => "  (admin listener only)",
                OpsListener::Public => "",
            }
        ));
        if let Some(order) = &options.acme {
            out.push_str(&format!(
                "  ACME: {} from the {} authority, kept in {}\n",
                order.domains.join(", "),
                if order.staging { "staging" } else { "live" },
                order.dir.display()
            ));
            if let Some(at) = &options.acme_http {
                out.push_str(&format!(
                    "  challenges and redirects to https on http://{at}\n"
                ));
            }
        }
        if let Some(pair) = &options.tls {
            out.push_str(&format!(
                "  TLS: {} with {} — HTTP/1.1 and HTTP/2; SIGHUP reads them again\n",
                pair.cert.display(),
                pair.key.display()
            ));
        }
        if let Some(origin) = &options.forwarding.public_origin {
            out.push_str(&format!("  public origin: {origin}\n"));
        } else if options.forwarding.trust_proxy {
            out.push_str("  trusting X-Forwarded-Proto and X-Forwarded-Host\n");
        }
        if let Some(admin) = self.admin_addr {
            out.push_str(&format!(
                "  admin: http://{admin} (token in {}); update with `minicloud update <app>`\n",
                match (&self.front.options.admin_token, &self.front.options.data) {
                    (Some(_), _) => "--admin-token".to_string(),
                    (None, Some(data)) => data.join("admin.token").display().to_string(),
                    (None, None) => "admin.token".to_string(),
                }
            ));
        }
        out
    }

    /// Reads the TLS pair again, for a certificate that has been renewed
    /// on disk. The pair that is serving is kept if the new one does not
    /// read ([`crate::tls`]).
    pub fn reload_certificates(&self) -> Result<(), String> {
        match &self.front.certificates {
            Some(certificates) => certificates.reload(),
            None => Err("this host terminates no TLS".to_string()),
        }
    }

    /// Blocks until Ctrl-C (SIGINT) or, on Unix, SIGTERM — what
    /// `systemctl stop` sends.
    pub fn wait_for_signal(&self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        runtime.block_on(async {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                if let Ok(mut term) = signal(SignalKind::terminate()) {
                    // SIGHUP is a renewal hook's way of saying the files
                    // have changed; it is not a reason to stop.
                    let mut hangup = signal(SignalKind::hangup()).ok();
                    loop {
                        let hung_up = async {
                            match &mut hangup {
                                Some(hangup) => hangup.recv().await,
                                None => std::future::pending().await,
                            }
                        };
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => return,
                            _ = term.recv() => return,
                            _ = hung_up => match self.reload_certificates() {
                                Ok(()) => eprintln!("minicloud: the TLS certificate was read again"),
                                Err(why) => eprintln!("minicloud: SIGHUP: {why}"),
                            },
                        }
                    }
                }
            }
            let _ = tokio::signal::ctrl_c().await;
        });
    }

    /// Stops admitting requests — each new one is answered 503 with
    /// `Connection: close` — and waits up to `grace` for those admitted to
    /// be answered. Returns how many were still unanswered.
    pub fn shutdown(&self, grace: Duration) -> usize {
        self.front.draining.store(true, Ordering::SeqCst);
        let until = Instant::now() + grace;
        loop {
            let left = self.front.engine.queue.admitted();
            if left == 0 || Instant::now() >= until {
                return left;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.front.engine.close();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// What a connection refused at the limit is told.
const TOO_MANY_CONNECTIONS: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
content-type: text/plain; charset=utf-8\r\nretry-after: 1\r\nconnection: close\r\n\
content-length: 37\r\n\r\nminicloud: too many open connections\n";

impl Front {
    async fn accept(self: Arc<Self>, listener: tokio::net::TcpListener) {
        let accepting = self.certificates.clone();
        loop {
            let (mut stream, from) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    // Out of file descriptors, most likely: back off rather
                    // than spin.
                    eprintln!("minicloud: accept: {error}");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            };
            self.counters.connections.fetch_add(1, Ordering::Relaxed);
            let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                self.counters
                    .rejected_connections
                    .fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let _ = stream.write_all(TOO_MANY_CONNECTIONS).await;
                    let _ = stream.shutdown().await;
                });
                continue;
            };
            let front = Arc::clone(&self);
            let accepting = accepting.clone();
            tokio::spawn(async move {
                let _ = stream.set_nodelay(true);
                let service = service_fn(move |request| {
                    let front = Arc::clone(&front);
                    async move { Ok::<_, Infallible>(front.handle(request, from.ip()).await) }
                });
                match &accepting {
                    // TLS, and then HTTP/1.1 or HTTP/2 as ALPN chose. The
                    // handshake is on this connection's task, so one that
                    // never finishes costs this task and no other.
                    Some(accepting) => {
                        let accepted = tokio::time::timeout(
                            Duration::from_secs(10),
                            tokio_rustls::TlsAcceptor::from(accepting.now()).accept(stream),
                        )
                        .await;
                        if let Ok(Ok(stream)) = accepted {
                            let _ = hyper_util::server::conn::auto::Builder::new(
                                hyper_util::rt::TokioExecutor::new(),
                            )
                            .http1()
                            .timer(TokioTimer::new())
                            .header_read_timeout(Duration::from_secs(10))
                            .max_buf_size(64 * 1024)
                            // With upgrades, so that a `101` hands the
                            // connection over instead of ending it
                            // ([`crate::socket`]).
                            .serve_connection_with_upgrades(TokioIo::new(stream), service)
                            .await;
                        }
                    }
                    None => {
                        let _ = http1::Builder::new()
                            .timer(TokioTimer::new())
                            .header_read_timeout(Duration::from_secs(10))
                            .max_buf_size(64 * 1024)
                            .serve_connection(TokioIo::new(stream), service)
                            .with_upgrades()
                            .await;
                    }
                }
                drop(permit);
            });
        }
    }

    async fn handle(
        &self,
        request: Request<Incoming>,
        from: std::net::IpAddr,
    ) -> Response<OutBody> {
        // An answer written as it goes, before the ones the host has whole.
        if self.options.ops_listener == OpsListener::Public {
            if let Some(streamed) = self.follow_logs(request.uri()) {
                return streamed;
            }
        }
        let takes_gzip = crate::convert::takes_gzip(
            request
                .headers()
                .get(hyper::header::ACCEPT_ENCODING)
                .and_then(|value| value.to_str().ok()),
        );
        let reply = self.reply(request, from).await;
        to_response(reply.compressed(takes_gzip))
    }

    /// `GET /_host/apps/<app>/logs?follow=1`: the app's recent lines, then
    /// every line as it is written, as server-sent events.
    ///
    /// `None` for any other request, which is answered the ordinary way.
    pub(crate) fn follow_logs(&self, uri: &hyper::Uri) -> Option<Response<OutBody>> {
        let name = uri
            .path()
            .strip_prefix("/_host/apps/")?
            .strip_suffix("/logs")?;
        if name.is_empty() || name.contains('/') {
            return None;
        }
        let query: BTreeMap<String, String> = uri
            .query()
            .map(|query| {
                form_urlencoded::parse(query.as_bytes())
                    .into_owned()
                    .collect()
            })
            .unwrap_or_default();
        if !matches!(query.get("follow").map(String::as_str), Some("1" | "true")) {
            return None;
        }
        let n = query
            .get("n")
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(50)
            .min(crate::logs::LOG_LINES);
        let Some(slot) = self.engine.slot_named(name) else {
            return Some(to_response(Reply::text(
                404,
                format!("no app named `{name}`\n"),
            )));
        };
        Some(crate::ops::follow(&slot, n))
    }

    async fn reply(&self, request: Request<Incoming>, from: std::net::IpAddr) -> Reply {
        if self.draining.load(Ordering::Relaxed) {
            return Reply::text(503, "minicloud is shutting down; try again shortly\n")
                .with_header("retry-after", "1")
                .with_header("connection", "close");
        }
        let path = request.uri().path().to_string();
        let host = request
            .headers()
            .get(hyper::header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(|host| host_name(host).to_string());
        let router = ByHostname {
            hosts: self.engine.hostnames(),
        };
        let route = router.route(host.as_deref(), &path);
        // A hostname that routes to an app is the app's, whole: the host's
        // own paths are on the others.
        if route.as_ref().is_none_or(|route| route.hostname.is_none()) {
            if is_ops_path(&path) {
                return match self.options.ops_listener {
                    OpsListener::Public => self.ops_reply(&path, request.uri().query()),
                    OpsListener::Admin => {
                        Reply::text(404, format!("minicloud has nothing at `{path}`\n"))
                    }
                };
            }
            if path == "/" {
                return Reply::text(200, self.index());
            }
        }
        let Some(route) = route else {
            self.counters.not_found.fetch_add(1, Ordering::Relaxed);
            return Reply::text(404, "no app here; try /\n");
        };
        let found = self
            .engine
            .slot_named(&route.app)
            .and_then(|slot| Some((slot.index, slot.enabled(), slot.current()?)));
        let Some((index, enabled, app)) = found else {
            self.counters.not_found.fetch_add(1, Ordering::Relaxed);
            return Reply::text(404, format!("no app named `{}`\n", route.app));
        };
        if !enabled {
            app.counters.rejected(Rejection::Disabled);
            return Reply::text(
                503,
                format!("app `{}` is disabled by the administrator\n", app.name),
            );
        }
        if let AppState::Refused(why) = &app.state {
            return Reply::text(503, format!("app `{}` was not loaded: {why}\n", app.name));
        }

        // Past the app's `[rate]`, before anything else is done for the
        // request: a caller that is over its allowance costs the host a
        // map lookup and nothing more ([`crate::rate`]).
        if let Some(policy) = &app.rate {
            let who = match &policy.key {
                crate::config::RateKey::Address => from.to_string(),
                crate::config::RateKey::App => String::new(),
                crate::config::RateKey::Header(name) => request
                    .headers()
                    .get(name.as_str())
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
            };
            let limit = crate::rate::RateLimit {
                requests: policy.requests,
                per: policy.per,
                burst: policy.burst,
            };
            if let crate::rate::Allowance::Refused { retry_after_secs } =
                self.rates.check(&app.name, &who, limit)
            {
                app.counters.rejected(Rejection::RateLimited);
                return Reply::text(
                    429,
                    format!(
                        "app `{}` takes {} request(s) every {:?} from one caller; try again \
                         in {retry_after_secs}s\n",
                        app.name, policy.requests, policy.per
                    ),
                )
                .with_header("retry-after", retry_after_secs.to_string());
            }
        }

        // The app's own files, answered here: no queue, no isolate, no run
        // ([`crate::assets`]). A `GET` or a `HEAD` whose path names one of
        // them never reaches `handle`; any other method, and any path that
        // names no file, goes on as before, so a `POST` to the path of a
        // file is the app's to answer.
        let method = request.method().clone();
        if method == hyper::Method::GET || method == hyper::Method::HEAD {
            if let Some(asset) = app.assets.get(&route.path) {
                let none_match = request
                    .headers()
                    .get(hyper::header::IF_NONE_MATCH)
                    .and_then(|value| value.to_str().ok());
                return asset_reply(asset, none_match, &app);
            }
        }

        let limit = app.limits.max_request_bytes;
        let too_large = || {
            app.counters.rejected(Rejection::TooLarge);
            Reply::text(
                413,
                format!(
                    "app `{}` accepts request bodies of at most {limit} bytes\n",
                    app.name
                ),
            )
        };
        let declared = request
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if declared.is_some_and(|length| length > limit as u64) {
            return too_large();
        }
        let method = method.as_str().to_string();
        let query: BTreeMap<String, String> = request
            .uri()
            .query()
            .map(|query| {
                form_urlencoded::parse(query.as_bytes())
                    .into_owned()
                    .collect()
            })
            .unwrap_or_default();
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        for (name, value) in request.headers() {
            let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
            headers
                .entry(name.as_str().to_string())
                .and_modify(|joined| {
                    joined.push_str(", ");
                    joined.push_str(&value);
                })
                .or_insert(value);
        }
        // Whatever a client sends, it did not come from the host's clock:
        // the header is the host's alone ([`crate::triggers`]), so an app
        // may trust it to tell a scheduled run from a request.
        for header in crate::internal::INTERNAL_HEADERS {
            headers.remove(header);
        }
        // How the client reached the host: see `crate::proxy`.
        self.options
            .forwarding
            .apply(&mut headers, route.hostname.as_deref());
        // Where the app is mounted, so that it can write links to itself: the
        // host's, whatever the client sent. (Behind a proxy that mounts the
        // host under a prefix of its own, prepend it there.)
        let mount = path
            .strip_suffix(route.path.as_str())
            .unwrap_or(&path)
            .trim_end_matches('/')
            .to_string();
        headers.insert("x-forwarded-prefix".to_string(), mount);
        // A request that asks for a WebSocket keeps its connection, for the
        // run to take with `socket.accept` ([`crate::socket`]). The key is
        // the client's; the host answers it in the `101`.
        let websocket = headers
            .get("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
            .then(|| headers.get("sec-websocket-key").cloned())
            .flatten();
        let mut request = request;
        let upgrade = websocket
            .is_some()
            .then(|| hyper::upgrade::on(&mut request));
        let body = match Limited::new(request.into_body(), limit).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(error) if error.downcast_ref::<LengthLimitError>().is_some() => return too_large(),
            Err(error) => {
                app.counters.rejected(Rejection::BadRequest);
                return Reply::text(400, format!("cannot read the request body: {error}\n"));
            }
        };
        let Ok(body) = String::from_utf8(body.to_vec()) else {
            app.counters.rejected(Rejection::BadRequest);
            return Reply::text(400, "the request body is not UTF-8\n");
        };

        let (reply, answer) = oneshot::channel();
        let cancel = Cancel::new();
        let answering = crate::stream::Answering::new(reply);
        if let (Some(upgrade), Some(key)) = (upgrade, websocket) {
            answering.upgradable(upgrade, key);
        }
        let start = Box::new(Start {
            request: AppRequest {
                method,
                path: route.path,
                query: query.into_iter().collect(),
                headers: headers.into_iter().collect(),
                body,
            },
            flight: Flight {
                id: self.engine.next_id(),
                app: index,
                version: Arc::clone(&app),
                accepted: Instant::now(),
                reply: answering,
                cancel: cancel.clone(),
                tally: Default::default(),
            },
        });
        if let Err(why) = self.engine.queue.admit(index, start) {
            app.counters.rejected(why);
            return match why {
                Rejection::QueueFull => Reply::text(
                    429,
                    format!(
                        "app `{}` has {} requests waiting already; try again later\n",
                        app.name, app.limits.max_queued
                    ),
                ),
                _ => Reply::text(
                    503,
                    format!(
                        "minicloud has {} requests in flight already; try again later\n",
                        self.options.max_in_flight
                    ),
                ),
            }
            .with_header("retry-after", "1");
        }
        // hyper drops this future when the client closes its connection
        // before the answer; the guard turns that drop into a cancellation.
        let guard = CancelOnDrop(Some(cancel));
        let answered = answer.await;
        guard.disarm();
        match answered {
            Ok(reply) => reply,
            Err(_) => Reply::text(
                500,
                format!(
                    "app `{}`: the request ended without an answer (the host is shutting down, \
                     or failed running it)\n",
                    app.name
                ),
            ),
        }
    }

    fn index(&self) -> String {
        let mut out = String::from("minicloud: Cove apps, one isolate per request\n\n");
        for slot in self.engine.slots() {
            let Some(app) = slot.current() else {
                continue;
            };
            let at = match app.hosts.first() {
                Some(host) => format!("//{host}/"),
                None => format!("/{}/", app.name),
            };
            let disabled = if slot.enabled() { "" } else { "  (disabled)" };
            out.push_str(&format!("  {at}  {}{disabled}\n", app.describe()));
        }
        if self.options.ops_listener == OpsListener::Public {
            out.push_str("\n  /_host/ui  the operations page\n");
            out.push_str("  /_host/stats  per-app counters, queues and work, as JSON\n");
            out.push_str("  /_host/apps/<app>  one app, its versions and recent errors, as JSON\n");
            out.push_str("  /_host/apps/<app>/logs?n=200  an app's recent log lines\n");
        }
        out
    }

    /// Answers a `GET` under `/_host/` ([`crate::ops`]), on whichever
    /// listener serves them.
    pub(crate) fn ops_reply(&self, path: &str, query: Option<&str>) -> Reply {
        if path == "/_host/stats" {
            return json_reply(&ops::stats(&self.ops()));
        }
        if path == "/_host/metrics" {
            return Reply {
                status: 200,
                headers: vec![(
                    "content-type".into(),
                    // What Prometheus asks for, and what it answers with:
                    // the text exposition format, version 0.0.4.
                    "text/plain; version=0.0.4; charset=utf-8".into(),
                )],
                body: Bytes::from(ops::metrics(&self.ops(), &self.platform_now())),
                stream: None,
            };
        }
        if path == "/_host/series" {
            let asked: BTreeMap<String, String> = query
                .map(|query| {
                    form_urlencoded::parse(query.as_bytes())
                        .into_owned()
                        .collect()
                })
                .unwrap_or_default();
            return json_reply(&ops::series(
                &self.ops(),
                asked.get("app").map(String::as_str),
                asked.get("over").map(String::as_str).unwrap_or("hour"),
            ));
        }
        if path == "/_host/platform" {
            let over = query
                .and_then(|query| {
                    form_urlencoded::parse(query.as_bytes())
                        .find(|(key, _)| key == "over")
                        .map(|(_, over)| over.into_owned())
                })
                .unwrap_or_else(|| "hour".to_string());
            return json_reply(&ops::platform(&self.ops(), &self.platform_now(), &over));
        }
        if path == "/_host/ui" {
            return Reply {
                status: 200,
                headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
                body: Bytes::from(ops::page(&self.ops())),
                stream: None,
            };
        }
        if let Some(name) = path
            .strip_prefix("/_host/apps/")
            .filter(|n| !n.contains('/'))
        {
            return match self.engine.slot_named(name) {
                Some(slot) => json_reply(&ops::app_detail(&self.ops(), &slot)),
                None => Reply::text(404, format!("no app named `{name}`\n")),
            };
        }
        if let Some(name) = path
            .strip_prefix("/_host/apps/")
            .and_then(|rest| rest.strip_suffix("/logs"))
        {
            let Some(app) = self.engine.slot_named(name) else {
                return Reply::text(404, format!("no app named `{name}`\n"));
            };
            let lines = query
                .and_then(|query| {
                    form_urlencoded::parse(query.as_bytes())
                        .find(|(key, _)| key == "n")
                        .and_then(|(_, n)| n.parse().ok())
                })
                .unwrap_or(200);
            return Reply::text(200, app.logs.tail(lines));
        }
        Reply::text(404, format!("minicloud has nothing at `{path}`\n"))
    }

    /// What the host's process and machine look like now: what a test put
    /// in [`Front::platform_source`], or the live files and the engine's
    /// gauges.
    pub(crate) fn platform_now(&self) -> crate::platform::PlatformSample {
        let source = self
            .platform_source
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(source) = source {
            return source();
        }
        let mut sample = crate::platform::read_system(self.options.data.as_deref());
        for slot in self.engine.slots() {
            let (in_flight, queued) = self.engine.queue.gauges(slot.index);
            sample.in_flight += in_flight as u64;
            sample.queued += queued as u64;
        }
        sample.busy = self.engine.busy() as u64;
        sample.workers = self.engine.workers() as u64;
        sample
    }

    pub(crate) fn ops_context(&self) -> OpsContext<'_> {
        self.ops()
    }

    fn ops(&self) -> OpsContext<'_> {
        OpsContext {
            engine: &self.engine,
            counters: &self.counters,
            started: self.started,
            max_in_flight: self.options.max_in_flight,
            data: self.options.data.as_deref(),
            series: self.series.as_deref(),
        }
    }

    /// Loads `name` from `<apps>/<name>` as the app's next version, off the
    /// request path (on a blocking thread, neither a worker nor the I/O
    /// runtime's), and routes to it only if it loaded. A new name is a new
    /// app. Requests already admitted — queued, running, yielded or parked —
    /// finish on the version they were admitted to.
    pub(crate) async fn update(&self, name: &str) -> Result<Installed, UpdateError> {
        let result = {
            let _one_at_a_time = self.updating.lock().await;
            self.update_locked(name).await
        };
        self.record_update(name, "update", &result);
        result
    }

    /// Writes `files` as the app `name`'s next version and updates to it, if
    /// it loads with this host's config; keeps the version it replaces for
    /// [`Front::rollback`]. A refused deploy changes no file ([`crate::deploy`]).
    pub(crate) async fn deploy(
        &self,
        name: &str,
        files: crate::deploy::AppFiles,
    ) -> Result<Installed, UpdateError> {
        let result = {
            let _one_at_a_time = self.updating.lock().await;
            self.deploy_locked(name, files).await
        };
        self.record_update(name, "deploy", &result);
        result
    }

    /// Swaps the app `name` with the version its last deploy replaced, and
    /// updates to it if it loads ([`crate::deploy`]).
    pub(crate) async fn rollback(&self, name: &str) -> Result<Installed, UpdateError> {
        let result = {
            let _one_at_a_time = self.updating.lock().await;
            self.rollback_locked(name).await
        };
        self.record_update(name, "rollback", &result);
        result
    }

    fn record_update(&self, name: &str, action: &str, result: &Result<Installed, UpdateError>) {
        let outcome = match result {
            Ok(installed) => format!(
                "applied: {} -> {}",
                installed.previous.as_deref().unwrap_or("nothing"),
                installed.version
            ),
            Err(UpdateError::NotFound(why)) => format!("refused: {why}"),
            Err(UpdateError::Refused { why, .. }) => {
                format!("refused: {}", why.lines().next().unwrap_or_default())
            }
        };
        self.control
            .history
            .record(crate::manage::LISTENER, name, action, "", &outcome);
    }

    /// The version `name` routes to now, if any.
    fn current_version(&self, name: &str) -> Option<String> {
        self.engine
            .slot_named(name)
            .and_then(|slot| slot.current())
            .map(|app| app.version.clone())
    }

    async fn update_locked(&self, name: &str) -> Result<Installed, UpdateError> {
        let dir = self.options.apps.join(name);
        if !dir.join("app.toml").is_file() {
            return Err(UpdateError::NotFound(format!(
                "no app named `{name}` in `{}` (an app is a directory with an `app.toml`)",
                self.options.apps.display()
            )));
        }
        let app = self.load_next(name, &dir, RemovedKeys::Refuse).await?;
        Ok(self.install_loaded(name, app))
    }

    async fn deploy_locked(
        &self,
        name: &str,
        files: crate::deploy::AppFiles,
    ) -> Result<Installed, UpdateError> {
        let refused = |why: String| UpdateError::Refused {
            current: self.current_version(name),
            why,
        };
        crate::apps::valid_name(name).map_err(refused)?;
        let places = crate::deploy::Places::new(&self.options.apps, name);
        if let Err(why) = places.stage(&files) {
            places.unstage();
            return Err(refused(why));
        }
        let mut app = match self
            .load_next(name, &places.staged, RemovedKeys::Refuse)
            .await
        {
            Ok(app) => app,
            Err(error) => {
                places.unstage();
                return Err(error);
            }
        };
        if let Err(why) = places.switch_in() {
            places.unstage();
            return Err(refused(why));
        }
        // Loaded from the staged copy, which is now the app's directory.
        app.dir = places.current.clone();
        Ok(self.install_loaded(name, app))
    }

    async fn rollback_locked(&self, name: &str) -> Result<Installed, UpdateError> {
        crate::apps::valid_name(name).map_err(UpdateError::NotFound)?;
        let places = crate::deploy::Places::new(&self.options.apps, name);
        if !places.has_previous() {
            return Err(UpdateError::NotFound(format!(
                "no previous version of `{name}` is kept (a deploy keeps the one it replaces)"
            )));
        }
        // The version a deploy replaced was accepted once; it is put back,
        // not put forward, so a key removed since is ignored, not refused.
        let mut app = self
            .load_next(name, &places.previous, RemovedKeys::Ignore)
            .await?;
        places.swap().map_err(|why| UpdateError::Refused {
            current: self.current_version(name),
            why,
        })?;
        app.dir = places.current.clone();
        Ok(self.install_loaded(name, app))
    }

    /// Loads `dir` as the app `name`'s next version, with a removed
    /// `app.toml` key treated as `removed` says; a refusal is counted and
    /// logged, and is the error. Called with the update lock held.
    async fn load_next(
        &self,
        name: &str,
        dir: &std::path::Path,
        removed: RemovedKeys,
    ) -> Result<App, UpdateError> {
        let slot = self.engine.slot_named(name);
        let current = self.current_version(name);
        let lineage = match &slot {
            Some(slot) => Lineage {
                counters: Arc::clone(&slot.counters),
                logs: Arc::clone(&slot.logs),
                number: slot.next_version(),
            },
            None => Lineage::first(),
        };
        let load = LoadOptions {
            removed_keys: removed,
            ..self.load.clone()
        };
        let owned = name.to_string();
        let dir = dir.to_path_buf();
        let mut app = tokio::task::spawn_blocking(move || load_as(&owned, &dir, &load, lineage))
            .await
            .map_err(|e| UpdateError::Refused {
                current: current.clone(),
                why: format!("the load failed: {e}"),
            })?;
        self.refuse_taken_hostnames(&mut app);
        if let AppState::Refused(why) = &app.state {
            app.counters.updates_refused.fetch_add(1, Ordering::Relaxed);
            let line = format!(
                "update to {} refused, still serving {}: {}",
                app.version,
                current.as_deref().unwrap_or("nothing"),
                why.lines().next().unwrap_or_default()
            );
            app.logs.push("host", &line);
            eprintln!("minicloud: [{name}] {line}");
            return Err(UpdateError::Refused {
                current,
                why: why.clone(),
            });
        }
        Ok(app)
    }

    /// Routes to `app`, a version [`Front::load_next`] loaded.
    fn install_loaded(&self, name: &str, app: App) -> Installed {
        let counters = Arc::clone(&app.counters);
        let logs = Arc::clone(&app.logs);
        let installed = self.engine.install(app);
        counters.updates.fetch_add(1, Ordering::Relaxed);
        let line = format!(
            "updated: {} -> {}",
            installed.previous.as_deref().unwrap_or("nothing"),
            installed.version
        );
        logs.push("host", &line);
        eprintln!("minicloud: [{name}] {line}");
        installed
    }

    /// Stops routing to `name`; what is in flight finishes.
    pub(crate) fn remove(&self, name: &str) -> Option<String> {
        let removed = self.engine.remove(name);
        self.control.history.record(
            crate::manage::LISTENER,
            name,
            "remove",
            "",
            &match &removed {
                Some(version) => format!("applied: was {version}"),
                None => "refused: not routed to".to_string(),
            },
        );
        let removed = removed?;
        if let Some(slot) = self.engine.slot_named(name) {
            slot.logs.push("host", &format!("removed (was {removed})"));
        }
        eprintln!("minicloud: [{name}] removed (was {removed})");
        Some(removed)
    }
}

/// A `Host` header's name, without its port.
fn host_name(host: &str) -> &str {
    match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or(host),
    }
}

/// Refuses every app that claims a hostname an app before it (in load
/// order) claims: a hostname reaches one app.
fn refuse_taken_hostnames(apps: &mut [App]) {
    let mut taken: BTreeMap<String, String> = BTreeMap::new();
    for app in apps.iter_mut() {
        let clash = app
            .hosts
            .iter()
            .find_map(|host| Some((host.clone(), taken.get(host)?.clone())));
        match clash {
            Some((host, owner)) => {
                app.state = AppState::Refused(format!(
                    "`route.hosts`: `{host}` already reaches the app `{owner}`"
                ));
            }
            None => {
                for host in &app.hosts {
                    taken.insert(host.clone(), app.name.clone());
                }
            }
        }
    }
}

impl Front {
    /// Refuses `app` if another app routed to now claims one of its
    /// hostnames.
    pub(crate) fn refuse_taken_hostnames(&self, app: &mut App) {
        let clash = app
            .hosts
            .iter()
            .find_map(|host| Some((host.clone(), self.engine.host_claimed_by(host, &app.name)?)));
        if let Some((host, owner)) = clash {
            app.state = AppState::Refused(format!(
                "`route.hosts`: `{host}` already reaches the app `{owner}`"
            ));
        }
    }
}

/// Whether `path` is one of the host's own, under `/_host`.
pub(crate) fn is_ops_path(path: &str) -> bool {
    path == "/_host" || path.starts_with("/_host/")
}

/// A JSON reply.
pub(crate) fn json_reply(value: &Json) -> Reply {
    let mut body = serde_json::to_string_pretty(value).unwrap_or_default();
    body.push('\n');
    Reply {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: Bytes::from(body),
        stream: None,
    }
}

/// Cancels its request when dropped armed: when the connection's task drops
/// the request's future because the client went away.
struct CancelOnDrop(Option<Cancel>);

impl CancelOnDrop {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            cancel.cancel();
        }
    }
}

/// One of an app's static files as the reply: the bytes, or `304 Not
/// Modified` when the client already has them.
///
/// The headers are the host's, not the app's: the content type from the
/// file's extension, a strong `etag` over its bytes, the `cache-control`
/// `[assets]` asks for, and `nosniff` — the type is known, so a browser must
/// not guess another. `x-cove-app-version` names the version the file came
/// with, as it names the version that answered a run.
fn asset_reply(asset: &crate::assets::Asset, if_none_match: Option<&str>, app: &App) -> Reply {
    let modified = !if_none_match.is_some_and(|tags| asset.matches(tags));
    app.counters.asset(modified);
    let mut reply = Reply {
        status: if modified { 200 } else { 304 },
        headers: vec![
            ("etag".to_string(), asset.etag.clone()),
            (
                "cache-control".to_string(),
                app.asset_policy.cache_control.clone(),
            ),
            ("x-content-type-options".to_string(), "nosniff".to_string()),
            ("x-cove-app-version".to_string(), app.version.clone()),
        ],
        body: Bytes::new(),
        stream: None,
    };
    if modified {
        reply
            .headers
            .push(("content-type".to_string(), asset.content_type.to_string()));
        reply.body = asset.bytes.clone();
    }
    reply
}

/// The body of every answer the host sends.
///
/// Boxed because not every answer is bytes the host already has: an
/// operations view that follows an app's log writes lines as they happen
/// ([`Front::follow_logs`]), and a body written as it goes cannot be a
/// `Full`. A run's answer is still one buffer — an app's response is a Cove
/// `String`, so there is nothing of an app's to stream yet (#51).
pub(crate) type OutBody = BoxBody<Bytes, Infallible>;

/// A body fed from a channel: each [`Bytes`] sent is a chunk written out,
/// and the end of the channel is the end of the body.
struct ChannelBody {
    chunks: tokio::sync::mpsc::Receiver<Bytes>,
}

impl hyper::body::Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
        self.chunks
            .poll_recv(context)
            .map(|chunk| chunk.map(|bytes| Ok(hyper::body::Frame::data(bytes))))
    }
}

/// An answer whose body is written as it goes: the headers now, the chunks
/// as they are sent on `chunks`.
pub(crate) fn to_streamed(
    status: u16,
    headers: &[(&str, &str)],
    chunks: tokio::sync::mpsc::Receiver<Bytes>,
) -> Response<OutBody> {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(BodyExt::boxed(ChannelBody { chunks }))
        .unwrap_or_else(|_| {
            let mut response = Response::new(BodyExt::boxed(Full::new(Bytes::from_static(
                b"minicloud: cannot build the response\n",
            ))));
            *response.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
            response
        })
}

/// A reply as the hyper response.
pub(crate) fn to_response(reply: Reply) -> Response<OutBody> {
    // An answer a run is still writing ([`crate::stream`]): the head now,
    // the chunks as they are sent.
    if let Some(chunks) = reply.stream {
        let headers: Vec<(&str, &str)> = reply
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        return to_streamed(reply.status, &headers, chunks);
    }
    let mut builder = Response::builder().status(reply.status);
    for (name, value) in &reply.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
        .body(BodyExt::boxed(Full::new(reply.body)))
        .unwrap_or_else(|error| {
            let mut response = Response::new(BodyExt::boxed(Full::new(Bytes::from(format!(
                "minicloud: cannot build the response: {error}\n"
            )))));
            *response.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
            response
        })
}
