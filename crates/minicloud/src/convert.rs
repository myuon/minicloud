//! Between HTTP and the `web.Request` / `web.Response` values an app sees.

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
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
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
        }
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

/// The `web.Request` an app's `handle` is invoked with.
pub fn request_value(request: &AppRequest) -> Value {
    Value::structure(
        "web.Request",
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
    /// Not a `web.Response`, a status outside 100–599, or a header HTTP
    /// cannot carry.
    Invalid(String),
    /// A body larger than the app's `max_response_bytes`.
    TooLarge { bytes: usize, limit: usize },
}

/// Headers an app may not set: the HTTP layer owns the framing.
const FRAMING: [&str; 5] = [
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
];

/// A `web.Response` value as the reply to send.
pub fn response_of(value: &Value, max_bytes: usize) -> Result<Reply, BadResponse> {
    let invalid = |why: String| BadResponse::Invalid(why);
    let field = |name: &str| {
        value.field(name).ok_or_else(|| {
            invalid(format!(
                "the handler answered {value}, not a `web.Response`"
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
    })
}
