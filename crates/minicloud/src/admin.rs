//! The admin listener: updates, on an address of its own, behind a token.
//!
//! A second HTTP listener (`--admin`, default `127.0.0.1:8081`), so that the
//! public listener — the one a reverse proxy forwards — has no way to change
//! what runs, and the admin address can be kept on localhost whatever the
//! public one is. Every request needs `Authorization: Bearer <token>`; the
//! token is `--admin-token`, or the contents of `<data>/admin.token`, which
//! the host writes with a fresh random token (mode 0600) the first time it
//! starts without one. `minicloud update` reads the same file.
//!
//! | request | does |
//! | --- | --- |
//! | `POST /apps/<app>/update` | loads `<apps>/<app>` as the app's next version and routes to it if it loads; a new name adds the app. 200 with the old and new versions; 422 with the diagnostics, the current version still serving; 404 for no such directory |
//! | `POST /apps/<app>/deploy` | body: a tar archive of the app ([`crate::deploy`]). Checks it with this host's config; only if it loads, writes it to `<apps>/<app>` (keeping the version it replaces) and updates to it. 200 as `update`; 422 with the diagnostics, no file changed and the current version still serving; 400 for an archive that is refused |
//! | `POST /apps/<app>/rollback` | swaps `<apps>/<app>` with the version the last deploy replaced, and updates to it if it loads. 200 as `update`; 404 if none is kept; 422 as `update` |
//! | `DELETE /apps/<app>` | stops routing to the app; what is in flight finishes |
//! | `POST /apps/<app>/enable`, `/disable` | routes to the app again, or stops ([`crate::manage`]); its data stays |
//! | `POST /apps/<app>/reset` | drops the admin app's changes to the app's configuration and reloads it from `app.toml` |
//! | `GET /apps` | the stats, as `GET /_host/stats` |
//! | `GET /changes?n=50` | the change history, newest first, as JSON |
//! | `GET /secrets` | the secret store ([`crate::secrets`]), as JSON: each secret stored or used — `name`, `set`, `updated_ms`, `apps` (those that use it) — **never a value** |
//! | `GET /apps/<app>/queues` | what each of its queues holds and its dead letters, as JSON |
//! | `POST /apps/<app>/queues/revive/<id>` | puts a dead letter back on its queue |
//! | `GET /apps/<app>/logs?n=&follow=1` | its recent log lines as text, or — with `follow` — those and every line as it is written, as server-sent events (`minicloud logs`) |
//! | `PUT /secrets/<name>` | body: the value. Stores it (mode 0600, atomically), then reloads every app that uses it. 200 with each app's reload; 422 for a name or value refused |
//! | `DELETE /secrets/<name>` | removes it. 409 while an app uses it, unless `?force=1`, which reloads those apps without it (they are refused); 404 if it is not set |
//!
//! These are the emergency exits when the admin app is broken or locked
//! out: they do not go through it, and they may do what it may not —
//! disable the admin app, or reset it.
//!
//! Anything without the token is 401, and changes nothing — except, under
//! `--ops-listener admin`, a `GET` under `/_host/` ([`crate::ops`]): the
//! operations views are read-only and served here without the token, so that
//! a browser through an SSH tunnel can read them. Their `Host` has to be a
//! loopback name (`localhost`, `127.0.0.1`, `[::1]`), so that a page on
//! another site cannot reach them by rebinding its own name to 127.0.0.1.

use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::json;

use crate::convert::Reply;
use crate::deploy::{self, Limits};
use crate::manage::{ChangeError, SecretChange, Via};
use crate::ops::{self, OpsListener};
use crate::sched::Installed;
use crate::server::{is_ops_path, json_reply, to_response, Front, UpdateError};

/// Serves the admin listener until the runtime stops.
pub(crate) async fn serve(front: Arc<Front>, listener: tokio::net::TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        let front = Arc::clone(&front);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let front = Arc::clone(&front);
                async move { Ok::<_, Infallible>(handle(&front, request).await) }
            });
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

async fn handle(front: &Front, request: Request<Incoming>) -> Response<crate::server::OutBody> {
    // The operations views are on this listener with `--ops-listener
    // admin`, and the one that follows a log is written as it goes.
    // `GET /apps/<app>/logs?follow=1`, with the token: `minicloud logs
    // --follow`, which does not depend on where the operations views are.
    if request.method() == Method::GET && authorized(front, &request) {
        if let Some(streamed) = follow_app_logs(front, request.uri()) {
            return streamed;
        }
    }
    if front.options.ops_listener == OpsListener::Admin
        && request.method() == Method::GET
        && is_loopback_host(
            request
                .headers()
                .get(hyper::header::HOST)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default(),
        )
    {
        if let Some(streamed) = front.follow_logs(request.uri()) {
            return streamed;
        }
    }
    to_response(reply(front, request).await)
}

/// The queue store of `name`, for the admin listener's queue routes.
fn queues_of(
    front: &Front,
    name: &str,
) -> Result<std::sync::Arc<std::sync::Mutex<crate::queue::Queues>>, String> {
    let Some(data) = front.options.data.as_deref() else {
        return Err("this host keeps no queues".to_string());
    };
    if front.engine.slot_named(name).is_none() {
        return Err(format!("no app named `{name}`"));
    }
    crate::queue::shared(&data.join(name).join("queue.sqlite3"))
}

/// How many lines `?n=` asks for, bounded by what the ring keeps.
fn log_lines(uri: &hyper::Uri) -> usize {
    uri.query()
        .and_then(|query| {
            form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == "n")
                .and_then(|(_, n)| n.parse().ok())
        })
        .unwrap_or(200)
        .min(crate::logs::LOG_LINES)
}

/// `GET /apps/<app>/logs?follow=1`, the admin listener's own, answered as
/// the operations view is ([`crate::ops::follow`]).
fn follow_app_logs(front: &Front, uri: &hyper::Uri) -> Option<Response<crate::server::OutBody>> {
    let name = uri.path().strip_prefix("/apps/")?.strip_suffix("/logs")?;
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let follows = uri.query().is_some_and(|query| {
        form_urlencoded::parse(query.as_bytes())
            .any(|(key, value)| key == "follow" && (value == "1" || value == "true"))
    });
    if !follows {
        return None;
    }
    match front.engine.slot_named(name) {
        Some(slot) => Some(crate::ops::follow(&slot, log_lines(uri))),
        None => Some(to_response(Reply::text(
            404,
            format!("no app named `{name}`\n"),
        ))),
    }
}

/// Whether the request presents the admin token.
fn authorized(front: &Front, request: &Request<Incoming>) -> bool {
    let presented = request
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    !front.admin_token.is_empty() && same(presented.trim(), &front.admin_token)
}

async fn reply(front: &Front, request: Request<Incoming>) -> Reply {
    if front.options.ops_listener == OpsListener::Admin && is_ops_path(request.uri().path()) {
        if request.method() != Method::GET && request.method() != Method::HEAD {
            return Reply::text(405, "the operations views are read-only\n")
                .with_header("allow", "GET, HEAD");
        }
        let host = request
            .headers()
            .get(hyper::header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !is_loopback_host(host) {
            return Reply::text(
                403,
                "minicloud admin: the operations views answer only a loopback `Host`\n",
            );
        }
        return front.ops_reply(request.uri().path(), request.uri().query());
    }
    if !authorized(front, &request) {
        return Reply::text(
            401,
            "minicloud admin: a valid `Authorization: Bearer <token>` is required\n",
        )
        .with_header("www-authenticate", "Bearer");
    }
    let path = request.uri().path().to_string();
    let method = request.method().clone();
    let (name, verb) = match path.strip_prefix("/apps/") {
        Some(rest) => match rest.split_once('/') {
            Some((name, verb)) => (name.to_string(), verb.to_string()),
            None => (rest.to_string(), String::new()),
        },
        None => (String::new(), String::new()),
    };
    let changed = |result: Result<String, ChangeError>| match result {
        Ok(message) => Reply::text(200, format!("{message}\n")),
        Err(ChangeError::NotFound(why)) => Reply::text(404, format!("{why}\n")),
        Err(ChangeError::Refused(why)) => Reply::text(422, format!("{why}\n")),
        Err(ChangeError::InUse(why)) => Reply::text(409, format!("{why}\n")),
    };
    let secret = path
        .strip_prefix("/secrets/")
        .unwrap_or_default()
        .to_string();
    let force = request.uri().query().is_some_and(|query| {
        form_urlencoded::parse(query.as_bytes())
            .any(|(k, v)| k == "force" && (v == "1" || v == "true" || v == "yes"))
    });
    match (method, path.as_str()) {
        (Method::GET, "/apps") => json_reply(&ops::stats(&front_ops(front))),
        (Method::GET, _) if verb == "queues" && !name.is_empty() => match queues_of(front, &name) {
            Ok(store) => {
                let store = store.lock().unwrap_or_else(|p| p.into_inner());
                let depths = store.depths().unwrap_or_default();
                let dead = store.dead(None, 50).unwrap_or_default();
                json_reply(&json!({
                    "queues": depths.iter().map(|(name, waiting, dead)| json!({
                        "name": name, "waiting": waiting, "dead": dead,
                    })).collect::<Vec<_>>(),
                    "dead": dead.iter().map(|(message, why)| json!({
                        "id": message.id,
                        "queue": message.queue,
                        "attempts": message.attempts,
                        "sent_unix": message.sent_unix,
                        "body": message.body,
                        "why": why,
                    })).collect::<Vec<_>>(),
                }))
            }
            Err(why) => Reply::text(404, format!("{why}\n")),
        },
        (Method::POST, _) if verb.starts_with("queues/revive/") && !name.is_empty() => {
            let id = verb.trim_start_matches("queues/revive/").parse::<i64>();
            match (queues_of(front, &name), id) {
                (Ok(store), Ok(id)) => {
                    let store = store.lock().unwrap_or_else(|p| p.into_inner());
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |since| since.as_secs() as i64);
                    match store.revive(id, now) {
                        Ok(true) => Reply::text(
                            200,
                            format!("dead letter {id} of `{name}` is on its queue again\n"),
                        ),
                        Ok(false) => {
                            Reply::text(404, format!("`{name}` has no dead letter {id}\n"))
                        }
                        Err(why) => Reply::text(500, format!("{why}\n")),
                    }
                }
                (Err(why), _) => Reply::text(404, format!("{why}\n")),
                (_, Err(_)) => Reply::text(400, "the id is a number\n".to_string()),
            }
        }
        (Method::GET, _) if verb == "logs" && !name.is_empty() => {
            match front.engine.slot_named(&name) {
                Some(slot) => Reply::text(200, slot.logs.tail(log_lines(request.uri()))),
                None => Reply::text(404, format!("no app named `{name}`\n")),
            }
        }
        (Method::GET, "/changes") => {
            let n = request
                .uri()
                .query()
                .and_then(|query| {
                    form_urlencoded::parse(query.as_bytes())
                        .find(|(key, _)| key == "n")
                        .and_then(|(_, n)| n.parse().ok())
                })
                .unwrap_or(50);
            json_reply(&json!(front.control.history.newest(n)))
        }
        (Method::POST, _) if !name.is_empty() && (verb == "enable" || verb == "disable") => {
            changed(
                front
                    .set_enabled(&name, verb == "enable", &Via::Listener)
                    .await,
            )
        }
        (Method::POST, _) if !name.is_empty() && verb == "reset" => {
            changed(front.reset(&name, &Via::Listener).await)
        }
        (Method::POST, _) if verb == "update" && !name.is_empty() => {
            updated("update", &name, front.update(&name).await)
        }
        (Method::POST, _) if verb == "rollback" && !name.is_empty() => {
            updated("rollback", &name, front.rollback(&name).await)
        }
        (Method::POST, _) if verb == "deploy" && !name.is_empty() => {
            let limits = Limits::standard();
            let body = match Limited::new(request.into_body(), limits.max_archive_bytes() as usize)
                .collect()
                .await
            {
                Ok(body) => body.to_bytes(),
                Err(e) => {
                    return Reply::text(
                        400,
                        format!("deploy of `{name}` refused: cannot read the archive: {e}\n"),
                    )
                }
            };
            match deploy::unpack(&body, limits) {
                Ok(files) => updated("deploy", &name, front.deploy(&name, files).await),
                Err(why) => Reply::text(400, format!("deploy of `{name}` refused: {why}\n")),
            }
        }
        (Method::GET, "/secrets") => json_reply(&json!(front
            .secret_infos()
            .into_iter()
            .map(|info| json!({
                "name": info.name,
                "set": info.set,
                "updated_ms": info.updated_ms,
                "apps": info.apps,
            }))
            .collect::<Vec<_>>())),
        (Method::PUT, _) if !secret.is_empty() => {
            let body = match Limited::new(request.into_body(), crate::secrets::MAX_VALUE_BYTES + 1)
                .collect()
                .await
            {
                Ok(body) => body.to_bytes(),
                Err(_) => {
                    return Reply::text(
                        422,
                        format!(
                            "secret `{secret}`: the value is over the store's {} bytes\n",
                            crate::secrets::MAX_VALUE_BYTES
                        ),
                    )
                }
            };
            let Ok(value) = String::from_utf8(body.to_vec()) else {
                return Reply::text(422, format!("secret `{secret}`: the value is not UTF-8\n"));
            };
            secret_changed(front.set_secret(&secret, &value, &Via::Listener).await)
        }
        (Method::DELETE, _) if !secret.is_empty() => {
            secret_changed(front.delete_secret(&secret, force, &Via::Listener).await)
        }
        (Method::DELETE, _) if !name.is_empty() && verb.is_empty() => match front.remove(&name) {
            Some(version) => Reply::text(200, format!("removed `{name}` (was {version})\n")),
            None => Reply::text(404, format!("no app named `{name}` is routed to\n")),
        },
        _ => Reply::text(
            404,
            "minicloud admin: POST /apps/<app>/update, DELETE /apps/<app>, \
             POST /apps/<app>/deploy|rollback|enable|disable|reset, GET /apps, \
             GET /changes, GET /secrets, PUT|DELETE /secrets/<name>\n",
        ),
    }
}

/// The answer to a secret's change: what each app's reload came to. Never
/// the value.
fn secret_changed(result: Result<SecretChange, ChangeError>) -> Reply {
    match result {
        Ok(change) => json_reply(&json!({
            "secret": change.secret,
            "message": change.message,
            "reloaded": change
                .reloaded
                .iter()
                .map(|r| json!({ "app": r.app, "ok": r.ok, "outcome": r.outcome }))
                .collect::<Vec<_>>(),
        })),
        Err(ChangeError::NotFound(why)) => Reply::text(404, format!("{why}\n")),
        Err(ChangeError::Refused(why)) => Reply::text(422, format!("{why}\n")),
        Err(ChangeError::InUse(why)) => Reply::text(409, format!("{why}\n")),
    }
}

/// The answer to an update, a deploy or a rollback.
fn updated(action: &str, name: &str, result: Result<Installed, UpdateError>) -> Reply {
    match result {
        Ok(installed) => {
            let mut reply = json_reply(&json!({
                "app": installed.app,
                "version": installed.version,
                "previous": installed.previous,
            }));
            reply.status = 200;
            reply
        }
        Err(UpdateError::NotFound(why)) => Reply::text(404, format!("{why}\n")),
        Err(UpdateError::Refused { current, why }) => Reply::text(
            422,
            format!(
                "{action} of `{name}` refused; still serving {}:\n{why}\n",
                current.as_deref().unwrap_or("nothing")
            ),
        ),
    }
}

fn front_ops(front: &Front) -> ops::OpsContext<'_> {
    front.ops_context()
}

/// Whether a `Host` header names this machine's loopback, with or without a
/// port.
fn is_loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    let name = name.to_ascii_lowercase();
    name == "localhost"
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Compares two tokens in time that depends on their lengths only.
fn same(presented: &str, token: &str) -> bool {
    presented.len() == token.len()
        && presented
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

/// The token in `<data>/admin.token`, writing a fresh one there first if
/// there is none.
pub fn token_file(data: Option<&Path>) -> Result<String, String> {
    let Some(data) = data else {
        return Err(
            "the admin listener needs --admin-token or a data directory for admin.token"
                .to_string(),
        );
    };
    let path = data.join("admin.token");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let token = text.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    std::fs::create_dir_all(data)
        .map_err(|e| format!("cannot create `{}`: {e}", data.display()))?;
    let token = fresh_token()?;
    write_private(&path, &token).map_err(|e| format!("cannot write `{}`: {e}", path.display()))?;
    Ok(token)
}

/// 32 random bytes, as hex, from the operating system.
fn fresh_token() -> Result<String, String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|e| {
            format!("cannot read /dev/urandom for an admin token ({e}); pass --admin-token")
        })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn write_private(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    writeln!(file, "{token}")
}

#[cfg(not(unix))]
fn write_private(path: &Path, token: &str) -> std::io::Result<()> {
    std::fs::write(path, format!("{token}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_names_are_loopback() {
        for host in [
            "localhost",
            "localhost:8791",
            "127.0.0.1:8791",
            "[::1]:8791",
            "LOCALHOST",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in [
            "",
            "evil.example",
            "evil.example:8791",
            "10.0.0.1",
            "localhost.evil",
            "127.evil.example",
        ] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn tokens_compare_whole() {
        assert!(same("abc", "abc"));
        assert!(!same("abd", "abc"));
        assert!(!same("ab", "abc"));
        assert!(!same("", "abc"));
    }

    #[test]
    fn a_token_file_is_made_once_and_read_after() {
        let dir = std::env::temp_dir().join(format!("minicloud-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = token_file(Some(&dir)).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(token_file(Some(&dir)).unwrap(), first);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
