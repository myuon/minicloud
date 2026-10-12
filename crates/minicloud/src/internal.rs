//! A run the host asks for, with no client on the other end.
//!
//! A schedule ([`crate::triggers`]) and work an app defers
//! ([`crate::defer`]) both want the same thing: the app's own entry, over a
//! request the host builds, admitted to the app's queue exactly as the HTTP
//! front admits one. Everything downstream is then the same code — round
//! robin, the slice, parking, the deadline, the host-call budget, the
//! counters, the version a run is pinned to — and the app routes it with
//! the routing it already has.
//!
//! What the host adds is a header saying who asked, which it **strips from
//! every request that comes off a connection** (`server.rs`), so an app may
//! trust it:
//!
//! | header | set by |
//! | --- | --- |
//! | `x-minicloud-trigger` | a `[[triggers]]` schedule, to the path it asked for |
//! | `x-minicloud-deferred` | `defer.run`, to the path the app asked for |
//! | `x-minicloud-queue` | a message being delivered, to the queue's name |
//!
//! The answer comes back on a `oneshot`, which the caller may wait on to
//! count and log the outcome, or drop — dropping it does not cancel the
//! run, which is the point: nobody is waiting for it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::oneshot;

use crate::apps::App;
use crate::convert::{AppRequest, Reply};
use crate::proxy::Forwarding;
use crate::sched::{Cancel, Engine, Flight, Start};
use crate::stats::Rejection;

/// The header a schedule's run carries, naming the path it asked for.
pub const TRIGGER_HEADER: &str = "x-minicloud-trigger";

/// The header a deferred run carries, naming the path the app asked for.
pub const DEFERRED_HEADER: &str = "x-minicloud-deferred";

/// The header a queue's delivery carries, naming the queue.
pub const QUEUE_HEADER: &str = "x-minicloud-queue";

/// Every header the host sets on a run of its own and never takes from a
/// client.
pub const INTERNAL_HEADERS: [&str; 3] = [TRIGGER_HEADER, DEFERRED_HEADER, QUEUE_HEADER];

/// What the host wants run, and what to tell the app about it.
pub struct Asked<'a> {
    pub app: &'a Arc<App>,
    /// The index of the app's queue.
    pub slot: usize,
    pub forwarding: &'a Forwarding,
    /// The header that says who asked, and what it asked for.
    pub by: (&'a str, &'a str),
    pub method: &'a str,
    pub path: &'a str,
    pub body: String,
}

/// Admits a run the host asked for, and answers where its reply will
/// arrive.
///
/// The app is told how it is reached as a request off a connection is told
/// it, so a job that writes a link to itself writes the same one a page
/// would.
pub fn admit(engine: &Engine, asked: Asked<'_>) -> Result<oneshot::Receiver<Reply>, Rejection> {
    let Asked {
        app,
        slot,
        forwarding,
        by,
        method,
        path,
        body,
    } = asked;
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    headers.insert(by.0.to_string(), by.1.to_string());
    forwarding.apply(&mut headers, app.hosts.first().map(String::as_str));
    headers.insert(
        "x-forwarded-prefix".to_string(),
        if app.hosts.is_empty() {
            format!("/{}", app.name)
        } else {
            String::new()
        },
    );
    let (reply, answer) = oneshot::channel();
    let start = Box::new(Start {
        request: AppRequest {
            method: method.to_string(),
            path: path.to_string(),
            query: Vec::new(),
            headers: headers.into_iter().collect(),
            body,
        },
        flight: Flight {
            id: engine.next_id(),
            app: slot,
            version: Arc::clone(app),
            accepted: Instant::now(),
            reply: crate::stream::Answering::new(reply),
            cancel: Cancel::new(),
            tally: Default::default(),
        },
    });
    match engine.queue.admit(slot, start) {
        Ok(()) => Ok(answer),
        Err(why) => {
            app.counters.rejected(why);
            Err(why)
        }
    }
}

/// The first line of a body, cut, for a log line about a run nobody saw.
pub fn first_line(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let line = text.lines().next().unwrap_or_default();
    if line.len() <= 200 {
        return line.to_string();
    }
    let mut end = 200;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}
