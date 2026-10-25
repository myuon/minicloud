//! Between HTTP and the `std.web.Request` / `std.web.Response` values an app sees.

use bytes::Bytes;
use cove_runtime::value::MapKey;
use cove_runtime::Value;

/// A request as the scheduler carries it to a worker: `Send`, unlike the
/// [`Value`] it becomes there.
#[derive(Clone, Debug, Default)]
pub struct AppRequest {
    pub method: String,
    /// The path below the app's prefix: `/hello/a/b` reaches `hello` with
    /// `/a/b`. Always starts with `/`.
    pub path: String,
    /// Decoded query pairs; for a repeated key the last one wins.
    pub query: Vec<(String, String)>,
    /// Header names lowercased; repeated headers joined with `, `.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// What a worker hands back to the HTTP side.
///
/// The body is bytes rather than a `String` because not every reply comes
/// from a run: an app's static file ([`crate::assets`]) is answered by the
/// host as it is on disk, and a favicon is not UTF-8. What a *run* answers
/// is still a Cove `String` — [`response_of`] is where that becomes bytes.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// The rest of the body, for an answer a run writes as it goes
    /// ([`crate::stream`]): each `Bytes` sent is a chunk, and the end of
    /// the channel is the end of the answer. `None` for an answer that is
    /// whole, which every answer but a streamed one is.
    pub stream: Option<tokio::sync::mpsc::Receiver<Bytes>>,
    /// Whether this is a run's own answer or the host's word on a run
    /// that did not answer. A status cannot say which: a run may answer
    /// 503 itself, and the host answers 499 for a client that left.
    pub origin: Origin,
}

/// Who made a [`Reply`], for a caller that must tell an app's answer from
/// the host answering in its place: a queue's delivery
/// ([`crate::queue_host`]), which counts an attempt only for a run that
/// started and settles a message only on the app's own answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Origin {
    /// What the run answered, or a reply with no run behind it at all (a
    /// static file, an operations page).
    #[default]
    Answer,
    /// The host answered for a run whose entry never started: it waited
    /// for a worker past its deadline, or was cancelled while it waited.
    NotStarted,
    /// The host answered for a run that started and did not answer: cut
    /// off at its deadline or by a cancel, failed, or answered something
    /// that is not a response.
    Stopped,
}

impl Reply {
    /// A plain-text reply.
    pub fn text(status: u16, body: impl Into<String>) -> Reply {
        Reply {
            status,
            headers: vec![(
                "content-type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            )],
            body: Bytes::from(body.into()),
            stream: None,
            origin: Origin::Answer,
        }
    }

    /// A reply with a body of these bytes and nothing else said about it.
    pub fn bytes(status: u16, headers: Vec<(String, String)>, body: Bytes) -> Reply {
        Reply {
            status,
            headers,
            body,
            stream: None,
            origin: Origin::Answer,
        }
    }

    /// This reply, said to be made by `origin`.
    pub fn made_by(mut self, origin: Origin) -> Reply {
        self.origin = origin;
        self
    }

    /// This reply with one more header.
    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Reply {
        self.headers.push((name.to_string(), value.into()));
        self
    }
}

fn string_map(pairs: &[(String, String)]) -> Value {
    Value::map(
        pairs
            .iter()
            .map(|(k, v)| (MapKey::Str(k.clone()), Value::string(v.as_str()))),
    )
}

/// The `std.web.Request` an app's `handle` is invoked with.
pub fn request_value(request: &AppRequest) -> Value {
    Value::structure(
        "std.web.Request",
        vec![
            ("method", Value::string(request.method.as_str())),
            ("path", Value::string(request.path.as_str())),
            ("query", string_map(&request.query)),
            ("headers", string_map(&request.headers)),
            ("body", Value::string(request.body.as_str())),
        ],
    )
}

/// Why an app's answer is not a response the host will send.
#[derive(Debug, PartialEq, Eq)]
pub enum BadResponse {
    /// Not a `std.web.Response`, a status outside 100–599, or a header HTTP
    /// cannot carry.
    Invalid(String),
    /// A body larger than the app's `max_response_bytes`.
    TooLarge { bytes: usize, limit: usize },
}

/// Bodies smaller than this are sent as they are.
///
/// Compressing a few hundred bytes costs a pass over them and saves less
/// than the header it has to add; the gain starts where a page does.
pub const COMPRESS_FROM: usize = 1024;

/// What is worth compressing, by content type.
///
/// Text, and the text-shaped application types. An image, a font and a
/// `.wasm` are already compressed, and a second pass over one costs time
/// to make it slightly larger.
fn worth_compressing(content_type: &str) -> bool {
    let kind = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    kind.starts_with("text/")
        || matches!(
            kind.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/manifest+json"
                | "image/svg+xml"
        )
}

/// Whether a client said it takes gzip.
pub fn takes_gzip(accept_encoding: Option<&str>) -> bool {
    accept_encoding.is_some_and(|accepted| {
        accepted.split(',').any(|part| {
            let mut fields = part.split(';');
            let coding = fields.next().unwrap_or_default().trim();
            // `gzip;q=0` is a client saying it does not want it.
            let refused = fields.any(|field| field.trim().replace(' ', "") == "q=0");
            (coding.eq_ignore_ascii_case("gzip") || coding == "*") && !refused
        })
    })
}

impl Reply {
    /// This reply gzipped, if it is worth it and the client takes it.
    ///
    /// The body is already whole — an app's answer is a `String`, a file
    /// is bytes the host holds — so this is one pass over one buffer and
    /// not a stream. A reply that is already encoded, or that is not text,
    /// is left as it is.
    pub fn compressed(mut self, takes_gzip: bool) -> Reply {
        use std::io::Write;
        let content_type = self
            .headers
            .iter()
            .find(|(name, _)| name == "content-type")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        let already = self
            .headers
            .iter()
            .any(|(name, _)| name == "content-encoding");
        // A streamed answer is not compressed: a buffered encoder would
        // hold the lines it is meant to deliver, which for server-sent
        // events is the whole point of them.
        if !takes_gzip
            || self.stream.is_some()
            || already
            || self.body.len() < COMPRESS_FROM
            || !worth_compressing(&content_type)
        {
            return self;
        }
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        if encoder.write_all(&self.body).is_err() {
            return self;
        }
        let Ok(gzipped) = encoder.finish() else {
            return self;
        };
        // Only if it actually helped; a body that grew is a body sent as
        // it was.
        if gzipped.len() >= self.body.len() {
            return self;
        }
        self.body = Bytes::from(gzipped);
        self.headers
            .push(("content-encoding".to_string(), "gzip".to_string()));
        self.headers
            .push(("vary".to_string(), "accept-encoding".to_string()));
        self
    }
}

/// Headers an app may not set: the HTTP layer owns the framing.
pub(crate) const FRAMING: [&str; 5] = [
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
];

/// A `std.web.Response` value as the reply to send.
pub fn response_of(value: &Value, max_bytes: usize) -> Result<Reply, BadResponse> {
    let invalid = |why: String| BadResponse::Invalid(why);
    let field = |name: &str| {
        value.field(name).ok_or_else(|| {
            invalid(format!(
                "the handler answered {value}, not a `std.web.Response`"
            ))
        })
    };
    let status = field("status")?
        .as_int()
        .ok_or_else(|| invalid("`status` is not an Int".to_string()))?;
    let status = u16::try_from(status)
        .ok()
        .filter(|s| (100..=599).contains(s))
        .ok_or_else(|| invalid(format!("status {status} is not an HTTP status (100-599)")))?;
    let body = field("body")?
        .as_str()
        .ok_or_else(|| invalid("`body` is not a String".to_string()))?;
    if body.len() > max_bytes {
        return Err(BadResponse::TooLarge {
            bytes: body.len(),
            limit: max_bytes,
        });
    }
    let mut headers = Vec::new();
    let entries = field("headers")?
        .entries()
        .ok_or_else(|| invalid("`headers` is not a Map".to_string()))?;
    for (key, value) in entries {
        let MapKey::Str(name) = key else {
            return Err(invalid("a header name is not a String".to_string()));
        };
        let name = name.to_ascii_lowercase();
        let value = value
            .as_str()
            .ok_or_else(|| invalid(format!("header `{name}` is not a String")))?;
        if FRAMING.contains(&name.as_str()) {
            continue;
        }
        if hyper::header::HeaderName::from_bytes(name.as_bytes()).is_err()
            || hyper::header::HeaderValue::from_str(value).is_err()
        {
            return Err(invalid(format!("header `{name}` cannot be sent over HTTP")));
        }
        headers.push((name, value.to_string()));
    }
    if !headers.iter().any(|(name, _)| name == "content-type") {
        headers.push((
            "content-type".to_string(),
            "text/plain; charset=utf-8".to_string(),
        ));
    }
    Ok(Reply {
        status,
        headers,
        body: Bytes::from(body.to_string()),
        stream: None,
        origin: Origin::Answer,
    })
}
