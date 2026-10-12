//! `minicloud`: many small Cove web apps on one machine.
//!
//! Each directory under `apps/` holding an `app.toml` is an app. The host
//! loads every app once ([`apps`]): checks it against the host's schemas,
//! refuses it if its entry requires a capability it is not granted or its
//! code can `spawn`, lowers and prepares it, and compiles it on the native
//! tier where this machine has one. A request to `/<app>/...` is admitted
//! into that app's queue by the HTTP front ([`server`], hyper on tokio), and a
//! shared pool of worker threads ([`sched`]) runs it in a fresh `OwnedVm`:
//! parked at a host call that answers pending, yielded at a safepoint when
//! its slice is up and others wait, resumed on any worker, apps served round
//! robin.
//!
//! A `GET` or `HEAD` whose path names a file in the app's `assets/`
//! directory is answered from that directory ([`assets`]) and no run
//! happens: an app's answer is a Cove `String`, which is UTF-8, so an image
//! is the host's to serve.
//!
//! An app is updated in place ([`admin`], [`server::Host::update`]): the
//! next version is loaded off the request path and routed to only if it
//! loads; requests admitted to the old version finish on it, and it is
//! dropped with its last one ([`sched::Slot`]). An app from anywhere else
//! is deployed onto the host the same way, its files sent to the admin
//! listener and written into the apps directory only if they load
//! ([`deploy`]). [`ops`] is what the host shows of all this.
//!
//! The README is the walkthrough; this crate's tests start the host
//! in-process on a free port.

pub mod access;
pub mod admin;
pub mod admin_module;
pub mod apps;
pub mod assets;
pub mod config;
pub mod convert;
pub mod cron;
pub mod deploy;
pub mod fetch;
pub mod hosts;
pub mod kv;
pub mod logs;
pub mod manage;
pub mod ops;
pub mod overrides;
pub mod proxy;
pub mod router;
pub mod sched;
pub mod secrets;
pub mod server;
pub mod stats;
pub mod sys;
pub mod toolchain;
pub mod triggers;

pub use apps::{App, AppState, Backend, LoadOptions};
pub use hosts::{HostModule, HostModules, PendingWork};
pub use ops::OpsListener;
pub use proxy::{Forwarding, PublicOrigin};
pub use router::{PathPrefix, Route, Router};
pub use server::{Host, ServeOptions, UpdateError};
