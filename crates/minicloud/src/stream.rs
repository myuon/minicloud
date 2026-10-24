//! `stream`: an answer a run writes as it goes.
//!
//! | operation | answers |
//! | --- | --- |
//! | `stream.open(status, headers)` | `Result<Unit, Error>`: the head, sent now |
//! | `stream.send(chunk)` | `Result<Unit, Error>`: one chunk, when the client has taken the ones before it |
//!
//! A run answered once, at its end: a `web.Response` with the whole body in
//! it. That is right for a page and impossible for the things that are not
//! pages — server-sent events, a long export, a model's tokens as they
//! come. `stream.open` sends the status and the headers **while the run is
//! still going**, and each `stream.send` is a chunk on the connection.
//!
//! ```cove
//! stream.open(200, Map.of(MapEntry(key: "content-type", value: "text/event-stream")))
//! stream.send("data: one\n\n")
//! stream.send("data: two\n\n")
//! ```
//!
//! The `web.Response` the handler returns afterwards is **not** the answer:
//! the answer has been going out since `open`. Returning one is still how a
//! handler ends, so return anything (`web.Response(status: 200, …)`); the
//! stream ends when the run does.
//!
//! # Which run is calling
//!
//! A module instance is the app's, shared by every run of it, and a host
//! call carries no run with it. A worker runs **one** run at a time, so the
//! scheduler puts the run's stream in a thread-local around every slice of
//! it ([`current`]), and a call reads it there. Outside a run there is
//! none, and `open` is the app's `Err` rather than a panic.
//!
//! # What bounds it
//!
//! Everything that bounds a run, unchanged. `stream.send` **parks** — it
//! answers pending and the run gives up its worker until the client has
//! taken the chunks before it — so a slow reader costs a parked run and no
//! worker at all, and backpressure is the channel's. The run's `deadline`
//! ends the stream, so an app that means to hold one open sets a deadline
//! that says so; `max_host_calls` bounds how many chunks there may be; and
//! each chunk is held to `max_response_bytes`, which is the one limit that
//! cannot mean the whole body any more.
//!
//! A streamed answer carries no `x-cove-run-*` headers: they say what a run
//! cost, and the head goes out before the run has cost it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use cove_runtime::value::MapKey;
use cove_runtime::{
    Effect, HostAnswer, HostApi, HostType, ModuleSchema, OperationSchema, Reentry, RuntimeError,
    Transfer, Value,
};
use tokio::sync::{mpsc, oneshot};

use crate::convert::Reply;
use crate::hosts::{AppContext, HostModule, PendingWork};

/// The capability that opens this module.
pub const CAPABILITY: &str = "stream";

/// How many chunks may be written but not yet taken by the client.
///
/// Small on purpose: the point of parking on a full channel is that a run
/// which is faster than its reader waits, rather than filling memory with
/// an answer nobody is reading.
const AHEAD: usize = 8;

/// Where one run's answer goes.
///
/// It holds the one-shot the HTTP side is waiting on, so that whoever
/// answers first — the run returning, or `stream.open` — is the answer, and
/// the other is a no-op rather than a second answer.
#[derive(Debug)]
pub struct Answering {
    sender: Mutex<Option<oneshot::Sender<Reply>>>,
    streaming: AtomicBool,
    /// The chunks of a streamed answer, once `open` has sent the head.
    chunks: Mutex<Option<mpsc::Sender<Bytes>>>,
    /// The connection, for a request that asked to be upgraded: taken by
    /// `socket.accept` ([`crate::socket`]) and by nothing else.
    upgrade: Mutex<Option<hyper::upgrade::OnUpgrade>>,
    /// The key the client sent, which the `101` has to answer.
    websocket_key: Mutex<Option<String>>,
    /// The socket this run is holding, once it has accepted one.
    socket: Mutex<Option<Arc<crate::socket::Socket>>>,
}

impl Answering {
    /// A run's answer, to be sent on `sender`.
    pub fn new(sender: oneshot::Sender<Reply>) -> Arc<Answering> {
        Arc::new(Answering {
            sender: Mutex::new(Some(sender)),
            streaming: AtomicBool::new(false),
            chunks: Mutex::new(None),
            upgrade: Mutex::new(None),
            websocket_key: Mutex::new(None),
            socket: Mutex::new(None),
        })
    }

    /// Keeps what a request that asked to be upgraded needs, for a run that
    /// accepts it.
    pub fn upgradable(&self, upgrade: hyper::upgrade::OnUpgrade, key: String) {
        *self.upgrade.lock().unwrap_or_else(|p| p.into_inner()) = Some(upgrade);
        *self.websocket_key.lock().unwrap_or_else(|p| p.into_inner()) = Some(key);
    }

    /// The socket this run is holding, if it has accepted one.
    pub fn socket(&self) -> Option<Arc<crate::socket::Socket>> {
        self.socket
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Answers `101`, takes the connection, and starts the task that holds
    /// it ([`crate::socket::hold`]).
    ///
    /// `io` is the host's I/O runtime: this is called from a worker
    /// thread, which is not in one, and the task that holds the socket has
    /// to be.
    pub fn upgrade(&self, ahead: usize, io: &tokio::runtime::Handle) -> Result<(), String> {
        if self.socket().is_some() {
            return Err("socket.accept: this run has a socket already".to_string());
        }
        let key = self
            .websocket_key
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| "socket.accept: this request did not ask for a WebSocket".to_string())?;
        let Some(upgrade) = self
            .upgrade
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        else {
            return Err("socket.accept: this request cannot be upgraded".to_string());
        };
        let sent = self.answer(Reply {
            status: 101,
            headers: vec![
                ("upgrade".to_string(), "websocket".to_string()),
                ("connection".to_string(), "Upgrade".to_string()),
                (
                    "sec-websocket-accept".to_string(),
                    crate::socket::accept_key(&key),
                ),
            ],
            body: Bytes::new(),
            stream: None,
            origin: crate::convert::Origin::Answer,
        });
        if !sent {
            return Err("socket.accept: this run has answered already".to_string());
        }
        self.streaming.store(true, Ordering::Relaxed);
        let (outgoing, to_client) = mpsc::channel(ahead);
        let (from_client, incoming) = mpsc::channel(ahead);
        *self.socket.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(Arc::new(crate::socket::Socket {
                outgoing,
                incoming: tokio::sync::Mutex::new(incoming),
            }));
        io.spawn(async move {
            if let Ok(upgraded) = upgrade.await {
                crate::socket::hold(upgraded, to_client, from_client).await;
            }
        });
        Ok(())
    }

    /// Answers with `reply`, if nothing has answered yet. `false` when the
    /// answer has already gone — a run that opened a stream, or a client
    /// that went away.
    pub fn answer(&self, reply: Reply) -> bool {
        let Some(sender) = self.sender.lock().unwrap_or_else(|p| p.into_inner()).take() else {
            return false;
        };
        sender.send(reply).is_ok()
    }

    /// Whether the head of a streamed answer has gone out, which is what
    /// makes the run's returned response not the answer.
    pub fn is_streaming(&self) -> bool {
        self.streaming.load(Ordering::Relaxed)
    }

    /// Ends a streamed answer, which is the end of its body, and closes a
    /// socket this run was holding. Called when the run ends, however it
    /// ends.
    pub fn finish(&self) {
        *self.chunks.lock().unwrap_or_else(|p| p.into_inner()) = None;
        *self.socket.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// Sends the head and keeps the channel its body is written on.
    fn open(&self, status: u16, headers: Vec<(String, String)>) -> Result<(), String> {
        if self.streaming.load(Ordering::Relaxed) {
            return Err("this run has opened its answer already".to_string());
        }
        let (chunks, body) = mpsc::channel(AHEAD);
        let sent = self.answer(Reply {
            status,
            headers,
            body: Bytes::new(),
            stream: Some(body),
            origin: crate::convert::Origin::Answer,
        });
        if !sent {
            return Err("this run has answered already".to_string());
        }
        self.streaming.store(true, Ordering::Relaxed);
        *self.chunks.lock().unwrap_or_else(|p| p.into_inner()) = Some(chunks);
        Ok(())
    }

    /// Where a chunk goes, for a run that has opened its answer.
    fn chunks(&self) -> Option<mpsc::Sender<Bytes>> {
        self.chunks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

thread_local! {
    /// The run this worker is running, for the host calls it makes.
    static RUNNING: std::cell::RefCell<Option<Arc<Answering>>> =
        const { std::cell::RefCell::new(None) };
}

/// The answer of the run this thread is running, if it is running one.
pub fn current() -> Option<Arc<Answering>> {
    RUNNING.with(|running| running.borrow().clone())
}

/// Runs `step` with `answering` as [`current`], and puts back whatever was
/// there before — which is nothing, a worker running one run at a time.
pub fn as_current<T>(answering: &Arc<Answering>, step: impl FnOnce() -> T) -> T {
    RUNNING.with(|running| *running.borrow_mut() = Some(Arc::clone(answering)));
    let out = step();
    RUNNING.with(|running| *running.borrow_mut() = None);
    out
}

const fn op(name: &'static str, params: &'static [HostType], effect: Effect) -> OperationSchema {
    OperationSchema {
        name,
        params,
        variadic: false,
        result: HostType::Result(&HostType::Unit, &HostType::Error),
        capability: CAPABILITY,
        effect,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

/// The `stream` module.
pub const STREAM: ModuleSchema = ModuleSchema {
    name: "stream",
    capability: CAPABILITY,
    operations: &[
        op(
            "open",
            &[
                HostType::Int,
                HostType::Map(&HostType::String, &HostType::String),
            ],
            // Irreversible: the head is on the connection once it is sent.
            Effect::IrreversibleWrite,
        ),
        op("send", &[HostType::String], Effect::IrreversibleWrite),
    ],
    types: &[],
    resources: &[],
};

pub(crate) struct StreamModule;

impl HostModule for StreamModule {
    fn schema(&self) -> ModuleSchema {
        STREAM
    }

    fn instantiate(&self, app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        Ok(Box::new(StreamHost {
            max_chunk_bytes: app.max_response_bytes,
            counters: Arc::clone(&app.counters),
        }))
    }
}

struct StreamHost {
    max_chunk_bytes: usize,
    counters: Arc<crate::stats::AppCounters>,
}

impl StreamHost {
    /// The reply for `open`, or why it was refused.
    fn open(&self, args: &[Value]) -> Result<(), String> {
        let Some(answering) = current() else {
            return Err("stream.open: there is no run here to answer".to_string());
        };
        let status = args.first().and_then(Value::as_int).unwrap_or_default();
        let status = u16::try_from(status)
            .ok()
            .filter(|status| (100..=599).contains(status))
            .ok_or_else(|| format!("stream.open: {status} is not an HTTP status (100-599)"))?;
        let entries = args
            .get(1)
            .and_then(Value::entries)
            .ok_or_else(|| "stream.open: the headers are not a Map".to_string())?;
        let mut headers = Vec::new();
        for (key, value) in entries {
            let MapKey::Str(name) = key else {
                return Err("stream.open: a header name is not a String".to_string());
            };
            let name = name.to_ascii_lowercase();
            let value = value
                .as_str()
                .ok_or_else(|| format!("stream.open: header `{name}` is not a String"))?;
            // The framing is the HTTP layer's, as it is for a whole answer.
            if crate::convert::FRAMING.contains(&name.as_str()) {
                continue;
            }
            if hyper::header::HeaderName::from_bytes(name.as_bytes()).is_err()
                || hyper::header::HeaderValue::from_str(value).is_err()
            {
                return Err(format!("stream.open: header `{name}` cannot be sent"));
            }
            headers.push((name, value.to_string()));
        }
        if !headers.iter().any(|(name, _)| name == "content-type") {
            headers.push((
                "content-type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            ));
        }
        answering
            .open(status, headers)
            .map_err(|why| format!("stream.open: {why}"))
    }

    /// The chunk and where it goes, or why it cannot be sent.
    fn chunk(&self, args: &[Value]) -> Result<(mpsc::Sender<Bytes>, Bytes), String> {
        let Some(answering) = current() else {
            return Err("stream.send: there is no run here to answer".to_string());
        };
        let Some(chunks) = answering.chunks() else {
            return Err("stream.send: this run has not opened its answer".to_string());
        };
        let chunk = args.first().and_then(Value::as_str).unwrap_or_default();
        if chunk.len() > self.max_chunk_bytes {
            return Err(format!(
                "stream.send: a chunk of {} bytes is past this app's max_response_bytes of {}",
                chunk.len(),
                self.max_chunk_bytes
            ));
        }
        Ok((chunks, Bytes::from(chunk.to_string())))
    }
}

/// An app's `Err`, as a value.
fn refused(why: String) -> Value {
    Value::err(Value::error(why))
}

impl HostApi for StreamHost {
    fn module_schema(&self) -> ModuleSchema {
        STREAM
    }

    /// The blocking answer, for a call made where the run cannot park.
    ///
    /// `send` blocks the worker until the client has room, which is what
    /// parking exists to avoid, so it is counted like any other blocking
    /// host call.
    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "open" => Ok(match self.open(&args) {
                Ok(()) => Value::ok(Value::unit()),
                Err(why) => refused(why),
            }),
            "send" => {
                let (chunks, chunk) = match self.chunk(&args) {
                    Ok(sending) => sending,
                    Err(why) => return Ok(refused(why)),
                };
                self.counters
                    .blocking_host_calls
                    .fetch_add(1, Ordering::Relaxed);
                Ok(match chunks.blocking_send(chunk) {
                    Ok(()) => Value::ok(Value::unit()),
                    Err(_) => refused("stream.send: the client is gone".to_string()),
                })
            }
            other => Err(RuntimeError::new(format!(
                "`stream` declares no operation `{other}`"
            ))),
        }
    }

    fn call_parkable(&self, op: &str, args: Vec<Value>, _back: &mut dyn Reentry) -> HostAnswer {
        if op != "send" {
            return HostAnswer::Ready(self.call(op, args));
        }
        let (chunks, chunk) = match self.chunk(&args) {
            Ok(sending) => sending,
            Err(why) => return HostAnswer::Ready(Ok(refused(why))),
        };
        PendingWork::new("stream.send", async move {
            Ok(match chunks.send(chunk).await {
                Ok(()) => Transfer::ok(Transfer::Unit),
                // The connection was dropped: the run is told, and may
                // stop or go on doing whatever else it was doing.
                Err(_) => Transfer::err(Transfer::error("stream.send: the client is gone")),
            })
        })
        .answer()
    }
}
