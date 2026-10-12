//! `socket`: a WebSocket a run holds, both ways.
//!
//! | operation | answers |
//! | --- | --- |
//! | `socket.accept()` | `Result<Unit, Error>`: the upgrade, taken |
//! | `socket.send(text)` | `Result<Unit, Error>`: one message, when the client has taken the ones before it |
//! | `socket.receive()` | `Result<String, Error>`: the next message, waiting for it |
//! | `socket.close()` | `Result<Unit, Error>` |
//!
//! Everything else here answers: a request comes in, a run answers it, the
//! run ends. A socket is the one thing that does not — it is open for as
//! long as both ends want it, and both ends may speak. The run holds it.
//!
//! ```cove
//! export fn handle(request: web.Request) -> web.Response {
//!   match socket.accept() {
//!     Ok(_) => {}
//!     Err(error) => return web.Response(status: 400, headers: Map.of(), body: error.message)
//!   }
//!   while true {
//!     match socket.receive() {
//!       Ok(said) => { socket.send("you said {said}") }
//!       Err(_) => break      // the client has gone
//!     }
//!   }
//!   web.Response(status: 200, headers: Map.of(), body: "")   // not the answer
//! }
//! ```
//!
//! # How it works
//!
//! The request arrives as a request — the app sees its headers and may
//! refuse it like any other — and `socket.accept()` is what turns it into a
//! socket: the host answers `101` with the key the protocol asks for
//! ([`crate::stream::Answering`] answers once, so the run's returned
//! response is not the answer afterwards), hyper hands the connection over,
//! and a task of the host's reads and writes frames on it.
//!
//! The run talks to that task through two channels, and both ends of it
//! **park**: `send` gives the worker up until the client has taken what is
//! queued, `receive` until there is a message. A socket that nobody is
//! speaking on costs a parked run and no worker at all, which is what makes
//! holding one for minutes reasonable on a host with two of them.
//!
//! # What bounds it
//!
//! The run's `deadline`, which for a socket means **how long it may be
//! held**: an app that means to hold one for an hour says
//! `deadline = "1h"`. `max_host_calls` bounds how many messages there may
//! be, and `max_response_bytes` bounds one message. A run that ends closes
//! the socket, and a socket the client closes is an `Err` from `receive`,
//! which is how the run finds out.
//!
//! The host answers `ping` with `pong` itself: keeping a connection alive
//! is not the app's business.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use cove_runtime::{
    Effect, HostAnswer, HostApi, HostType, ModuleSchema, OperationSchema, Reentry, RuntimeError,
    Transfer, Value,
};
use tokio::sync::mpsc;

use crate::hosts::{AppContext, HostModule, PendingWork};

/// The capability that opens this module.
pub const CAPABILITY: &str = "socket";

/// How many messages may be written but not yet taken by the client, and
/// read but not yet taken by the run.
///
/// Small for the reason a stream's is: the point of parking on a full
/// channel is that whichever end is faster waits, rather than the host
/// holding a conversation nobody is having.
const AHEAD: usize = 16;

/// What a run and the host's socket task say to each other.
///
/// The receiving end is behind a `tokio` mutex rather than a `std` one
/// because a parked `receive` holds it across an await, and a guard that
/// is not `Send` cannot cross one. Nothing contends for it: one run holds
/// one socket.
#[derive(Debug)]
pub struct Socket {
    /// Messages the run is sending, taken by the socket task.
    pub outgoing: mpsc::Sender<String>,
    /// Messages the client has sent, taken by the run.
    pub incoming: tokio::sync::Mutex<mpsc::Receiver<String>>,
}

/// The accept key the protocol asks for: the client's key and a constant,
/// SHA-1, base64.
///
/// The constant is the protocol's own (RFC 6455); it is there to make the
/// answer impossible to confuse with anything else a server might echo.
pub fn accept_key(client_key: &str) -> String {
    use base64::Engine;
    const MAGIC: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let digest = sha1(format!("{client_key}{MAGIC}").as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// SHA-1, which the WebSocket handshake needs and nothing else here does.
///
/// Written out rather than taken from a crate: it is twenty lines, it is
/// the only place the host hashes anything for a protocol, and a hash whose
/// weakness does not matter — the handshake uses it to prove that the
/// server read the key, not to keep a secret — is not worth a dependency.
fn sha1(message: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut padded = message.to_vec();
    let bits = (message.len() as u64) * 8;
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bits.to_be_bytes());
    for block in padded.chunks(64) {
        let mut w = [0u32; 80];
        for (index, word) in block.chunks(4).enumerate() {
            w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (index, word) in w.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut digest = [0u8; 20];
    for (index, word) in h.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// Reads and writes frames on the connection hyper handed over, until one
/// end stops.
///
/// The host answers `ping` itself and closes when the run's outgoing
/// channel is dropped, which is when the run ends.
pub async fn hold(
    upgraded: hyper::upgrade::Upgraded,
    mut outgoing: mpsc::Receiver<String>,
    incoming: mpsc::Sender<String>,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
        hyper_util::rt::TokioIo::new(upgraded),
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        None,
    )
    .await;
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            said = outgoing.recv() => match said {
                Some(text) => {
                    if sink.send(Message::Text(text)).await.is_err() {
                        return;
                    }
                }
                // The run has ended: say goodbye properly rather than
                // dropping the connection on the floor.
                None => {
                    let _ = sink.send(Message::Close(None)).await;
                    let _ = sink.flush().await;
                    return;
                }
            },
            heard = stream.next() => match heard {
                Some(Ok(Message::Text(text))) => {
                    if incoming.send(text).await.is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    // A run reads a `String`, so a binary message is given
                    // to it as text where it is text and dropped where it
                    // is not; nothing of this host can carry bytes to an
                    // app yet (#57).
                    if let Ok(text) = String::from_utf8(bytes) {
                        if incoming.send(text).await.is_err() {
                            return;
                        }
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    if sink.send(Message::Pong(payload)).await.is_err() {
                        return;
                    }
                }
                Some(Ok(_)) => {}
                // A close or a broken frame: the run finds out at its next
                // `receive`, because the channel is dropped with this task.
                Some(Err(_)) | None => return,
            },
        }
    }
}

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
        capability: CAPABILITY,
        effect,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

/// The `socket` module.
pub const SOCKET: ModuleSchema = ModuleSchema {
    name: "socket",
    capability: CAPABILITY,
    operations: &[
        op(
            "accept",
            &[],
            HostType::Result(&HostType::Unit, &HostType::Error),
            Effect::IrreversibleWrite,
        ),
        op(
            "send",
            &[HostType::String],
            HostType::Result(&HostType::Unit, &HostType::Error),
            Effect::IrreversibleWrite,
        ),
        op(
            "receive",
            &[],
            HostType::Result(&HostType::String, &HostType::Error),
            Effect::Read,
        ),
        op(
            "close",
            &[],
            HostType::Result(&HostType::Unit, &HostType::Error),
            Effect::IrreversibleWrite,
        ),
    ],
    types: &[],
    resources: &[],
};

pub(crate) struct SocketModule;

impl HostModule for SocketModule {
    fn schema(&self) -> ModuleSchema {
        SOCKET
    }

    fn instantiate(&self, app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        Ok(Box::new(SocketHost {
            max_message_bytes: app.max_response_bytes,
            counters: Arc::clone(&app.counters),
            io: app.io.clone(),
        }))
    }
}

struct SocketHost {
    max_message_bytes: usize,
    counters: Arc<crate::stats::AppCounters>,
    /// Where the task that holds the socket runs: a worker thread is not
    /// in a runtime, and that task has to be.
    io: tokio::runtime::Handle,
}

/// An app's `Err`, as a value.
fn refused(why: impl Into<String>) -> Value {
    Value::err(Value::error(why.into()))
}

impl SocketHost {
    /// Turns this run's request into a socket, or says why it cannot be
    /// one.
    fn accept(&self) -> Result<(), String> {
        let Some(answering) = crate::stream::current() else {
            return Err("socket.accept: there is no run here to upgrade".to_string());
        };
        answering.upgrade(AHEAD, &self.io)
    }

    /// This run's socket, if it has accepted one.
    fn socket(&self, op: &str) -> Result<Arc<Socket>, String> {
        let Some(answering) = crate::stream::current() else {
            return Err(format!("socket.{op}: there is no run here"));
        };
        answering
            .socket()
            .ok_or_else(|| format!("socket.{op}: this run has not accepted a socket"))
    }
}

impl HostApi for SocketHost {
    fn module_schema(&self) -> ModuleSchema {
        SOCKET
    }

    /// The blocking answers, for a call made where the run cannot park.
    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "accept" => Ok(match self.accept() {
                Ok(()) => Value::ok(Value::unit()),
                Err(why) => refused(why),
            }),
            "close" => Ok(match self.socket(op) {
                // Dropping the run's end is the close: the host's task
                // says goodbye and goes.
                Ok(socket) => {
                    if let Ok(mut incoming) = socket.incoming.try_lock() {
                        incoming.close();
                    }
                    Value::ok(Value::unit())
                }
                Err(why) => refused(why),
            }),
            "send" | "receive" => {
                self.counters
                    .blocking_host_calls
                    .fetch_add(1, Ordering::Relaxed);
                let socket = match self.socket(op) {
                    Ok(socket) => socket,
                    Err(why) => return Ok(refused(why)),
                };
                if op == "send" {
                    let text = args.first().and_then(Value::as_str).unwrap_or_default();
                    if text.len() > self.max_message_bytes {
                        return Ok(refused(format!(
                            "socket.send: a message of {} bytes is past this app's \
                             max_response_bytes of {}",
                            text.len(),
                            self.max_message_bytes
                        )));
                    }
                    Ok(match socket.outgoing.blocking_send(text.to_string()) {
                        Ok(()) => Value::ok(Value::unit()),
                        Err(_) => refused("socket.send: the client is gone"),
                    })
                } else {
                    // Nothing else holds it: one run holds one socket, and
                    // this run is the one asking.
                    Ok(match socket.incoming.try_lock() {
                        Ok(mut incoming) => match incoming.blocking_recv() {
                            Some(text) => Value::ok(Value::string(text)),
                            None => refused("socket.receive: the client is gone"),
                        },
                        Err(_) => refused("socket.receive: this run is already waiting"),
                    })
                }
            }
            other => Err(RuntimeError::new(format!(
                "`socket` declares no operation `{other}`"
            ))),
        }
    }

    fn call_parkable(&self, op: &str, args: Vec<Value>, _back: &mut dyn Reentry) -> HostAnswer {
        if op != "send" && op != "receive" {
            return HostAnswer::Ready(self.call(op, args));
        }
        let socket = match self.socket(op) {
            Ok(socket) => socket,
            Err(why) => return HostAnswer::Ready(Ok(refused(why))),
        };
        if op == "send" {
            let text = args.first().and_then(Value::as_str).unwrap_or_default();
            if text.len() > self.max_message_bytes {
                return HostAnswer::Ready(Ok(refused(format!(
                    "socket.send: a message of {} bytes is past this app's \
                     max_response_bytes of {}",
                    text.len(),
                    self.max_message_bytes
                ))));
            }
            let text = text.to_string();
            return PendingWork::new("socket.send", async move {
                Ok(match socket.outgoing.send(text).await {
                    Ok(()) => Transfer::ok(Transfer::Unit),
                    Err(_) => Transfer::err(Transfer::error("socket.send: the client is gone")),
                })
            })
            .answer();
        }
        PendingWork::new("socket.receive", async move {
            let heard = socket.incoming.lock().await.recv().await;
            Ok(match heard {
                Some(text) => Transfer::ok(Transfer::Str(text)),
                None => Transfer::err(Transfer::error("socket.receive: the client is gone")),
            })
        })
        .answer()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_accept_key_is_the_one_the_protocol_asks_for() {
        // RFC 6455's own example: the key, and what a server must answer.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn the_hash_is_sha1() {
        // The two digests everyone checks a SHA-1 against.
        let hex = |bytes: [u8; 20]| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        // One longer than a block, to exercise the padding.
        assert_eq!(
            hex(sha1(&[b'a'; 1000])),
            "291e9a6c66994949b57ba5e650361e98fc36b1ba"
        );
    }
}
