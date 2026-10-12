//! The operations views: what the host is doing, per app, as JSON and as one
//! HTML page.
//!
//! | path | what |
//! | --- | --- |
//! | `GET /_host/stats` | every app's counters, gauges, version and store usage, and the server's |
//! | `GET /_host/apps/<app>` | one app: the same, its versions, and its recent errors |
//! | `GET /_host/apps/<app>/logs?n=` | its recent log lines, as text |
//! | `GET /_host/ui` | a page of all of it; every value from an app is escaped |
//!
//! They are read-only and unauthenticated. Where they are served is
//! [`OpsListener`] (`--ops-listener`): on the public listener (the default,
//! as before), or only on the admin listener, where the public one answers
//! 404 for all of `/_host/`. Behind a reverse proxy use `admin`: error
//! messages name source lines, and the logs are the apps' own.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use serde_json::{json, Map, Value as Json};

use crate::apps::{list, AppState};
use crate::sched::{Engine, Slot};
use crate::stats::ServerCounters;

/// Which listener serves the views.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OpsListener {
    /// The public listener, unauthenticated, as before `--ops-listener`.
    #[default]
    Public,
    /// The admin listener only, without its token (they are read-only, and
    /// the admin listener is kept on localhost); the public listener
    /// answers 404 under `/_host/`.
    Admin,
}

impl std::str::FromStr for OpsListener {
    type Err = String;
    fn from_str(text: &str) -> Result<OpsListener, String> {
        match text {
            "public" => Ok(OpsListener::Public),
            "admin" => Ok(OpsListener::Admin),
            other => Err(format!("`{other}` is not an ops listener: public or admin")),
        }
    }
}

/// What the views read besides the engine.
pub struct OpsContext<'a> {
    pub engine: &'a Engine,
    pub counters: &'a ServerCounters,
    pub started: Instant,
    pub max_in_flight: usize,
    pub data: Option<&'a Path>,
}

/// The keys summed into the stats' `totals`.
const TOTALS: [&str; 15] = [
    "served",
    "ok",
    "assets_served",
    "in_flight",
    "queued",
    "parked",
    "parks",
    "yields",
    "yield_requests",
    "yields_declined",
    "overdue_yields",
    "blocking_host_calls",
    "instructions",
    "worker_ms",
    "updates",
];

/// One app's entry: counters, gauges, and what its current version is.
pub fn app_json(context: &OpsContext, slot: &Slot) -> Json {
    let (in_flight, queued) = context.engine.queue.gauges(slot.index);
    let mut entry = slot.counters.to_json(in_flight, queued);
    let object = entry.as_object_mut().expect("an object");
    let (alive, programs) = slot.alive();
    object.insert("versions_alive".into(), json!(alive));
    object.insert("programs_alive".into(), json!(programs));
    match slot.current() {
        None => {
            object.insert("state".into(), json!("removed"));
            object.insert("version".into(), Json::Null);
        }
        Some(app) => {
            let (state, tier, reason) = match &app.state {
                AppState::Ready(ready) => ("ready", Json::from(ready.tier), Json::Null),
                AppState::Refused(why) => ("refused", Json::Null, Json::from(why.as_str())),
            };
            // Disabled is the admin's, over whatever the version is.
            let state = if slot.enabled() { state } else { "disabled" };
            object.insert("state".into(), json!(state));
            object.insert("version".into(), json!(app.version));
            object.insert("tier".into(), tier);
            object.insert("refused".into(), reason);
            object.insert("required".into(), json!(list(&app.required)));
            object.insert("granted".into(), json!(list(&app.granted)));
            object.insert("hosts".into(), json!(app.hosts));
            object.insert("overridden".into(), json!(app.overridden.is_some()));
            object.insert(
                "limits".into(),
                json!({
                    "deadline_ms": app.limits.run.deadline.map(|d| d.as_millis() as u64),
                    "max_host_calls": app.limits.run.max_host_calls,
                    "max_in_flight": app.limits.max_in_flight,
                    "max_queued": app.limits.max_queued,
                }),
            );
            if !app.assets.is_empty() {
                object.insert(
                    "assets".into(),
                    json!({
                        "files": app.assets.len(),
                        "bytes": app.assets.bytes(),
                        "max_bytes": app.asset_policy.max_bytes,
                    }),
                );
            }
            if app.granted.contains("kv") {
                let usage = context
                    .data
                    .and_then(|data| crate::kv::usage(&data.join(&slot.name).join("kv.sqlite3")));
                object.insert(
                    "kv".into(),
                    json!({
                        "keys": usage.map(|u| u.0),
                        "bytes": usage.map(|u| u.1),
                        "max_keys": app.kv.max_keys,
                        "max_bytes": app.kv.max_bytes,
                    }),
                );
            }
        }
    }
    entry
}

/// `GET /_host/stats`.
pub fn stats(context: &OpsContext) -> Json {
    let mut apps = Map::new();
    let mut totals: BTreeMap<&str, f64> = BTreeMap::new();
    for slot in context.engine.slots() {
        let entry = app_json(context, &slot);
        for key in TOTALS {
            *totals.entry(key).or_default() += entry[key].as_f64().unwrap_or_default();
        }
        for group in ["errors", "rejected"] {
            let sum: f64 = entry[group]
                .as_object()
                .map(|counts| counts.values().filter_map(Json::as_f64).sum())
                .unwrap_or_default();
            *totals.entry(group).or_default() += sum;
        }
        apps.insert(slot.name.clone(), entry);
    }
    let totals: Map<String, Json> = totals
        .into_iter()
        .map(|(key, value)| {
            let value = if key == "worker_ms" {
                json!(value)
            } else {
                json!(value as u64)
            };
            (key.to_string(), value)
        })
        .collect();
    let read = |counter: &std::sync::atomic::AtomicU64| counter.load(Ordering::Relaxed);
    json!({
        "uptime_s": context.started.elapsed().as_secs_f64(),
        "workers": context.engine.workers(),
        "slice_ms": context.engine.slice.map(|slice| slice.as_secs_f64() * 1e3),
        "connections": read(&context.counters.connections),
        "rejected_connections": read(&context.counters.rejected_connections),
        "not_found": read(&context.counters.not_found),
        "admitted": context.engine.queue.admitted(),
        "max_in_flight": context.max_in_flight,
        "totals": totals,
        "apps": apps,
    })
}

/// `GET /_host/apps/<app>`: [`app_json`] with the versions and recent errors.
pub fn app_detail(context: &OpsContext, slot: &Slot) -> Json {
    let mut entry = app_json(context, slot);
    let object = entry.as_object_mut().expect("an object");
    object.insert("name".into(), json!(slot.name));
    object.insert("versions".into(), Json::Array(slot.versions()));
    object.insert(
        "recent_errors".into(),
        Json::Array(slot.counters.recent_errors()),
    );
    object.insert("native".into(), native_report(slot));
    entry
}

/// What the native tier made of the current version: how many functions it
/// compiled and which it left on the encoded tier, with the code generator's
/// reason. A compiled function calling one of those runs it in the dispatch
/// loop, and a run below such a call declines to yield (ADR 0085) — so this
/// is where an app's `yields_declined` and `overdue_yields` are explained.
/// `null` for a version on the encoded VM.
fn native_report(slot: &Slot) -> Json {
    let Some(app) = slot.current() else {
        return Json::Null;
    };
    let Some(ready) = app.ready() else {
        return Json::Null;
    };
    let Some(native) = ready.program.native() else {
        return Json::Null;
    };
    let program = ready.program.program();
    let refusals: Vec<Json> = native
        .refusals()
        .iter()
        .map(|refused| {
            let mut entry = json!({"function": refused.name, "reason": refused.reason});
            // The instruction the code generator stopped at, and where the
            // source wrote it.
            let function = program.function(refused.id);
            if let Some(pc) = refused.at.map(|pc| pc as usize) {
                if let Some(inst) = function.code.get(pc) {
                    let debug = format!("{inst:?}");
                    let variant = debug
                        .split(|c: char| !c.is_alphanumeric())
                        .next()
                        .unwrap_or_default();
                    entry["instruction"] = json!(variant);
                }
                if let Some(span) = function.spans.get(pc) {
                    let file = ready.sources.get(span.file);
                    let (line, column) = file.line_col(span.start);
                    entry["at"] = json!(format!(
                        "{}:{line}:{column}",
                        ready.sources.path(span.file).display()
                    ));
                    entry["source"] = json!(file.line_text(line).trim());
                }
            }
            entry
        })
        .collect();
    json!({
        "reachable": native.reachable(),
        "compiled": native.compiled(),
        "refused": native.refused(),
        "refusals": refusals,
    })
}

/// Escapes text for HTML.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A JSON value as table-cell text.
fn cell(value: &Json) -> String {
    match value {
        Json::Null => "–".to_string(),
        Json::String(text) => escape(text),
        Json::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => format!("{f:.1}"),
            _ => n.to_string(),
        },
        other => escape(&other.to_string()),
    }
}

/// `GET /_host/ui`: one page, no script, no external resource.
pub fn page(context: &OpsContext) -> String {
    let stats = stats(context);
    let mut html = String::from(
        "<!doctype html><meta charset=utf-8><title>minicloud</title>\
         <meta http-equiv=refresh content=5>\
         <style>body{font:14px system-ui,sans-serif;margin:1.5em}\
         table{border-collapse:collapse;margin:.5em 0 1.5em}\
         td,th{border:1px solid #ccc;padding:.2em .5em;text-align:right}\
         th:first-child,td:first-child{text-align:left}code{font-size:12px}</style>\
         <h1>minicloud</h1>",
    );
    html.push_str(&format!(
        "<p>up {:.0} s · {} workers · slice {} ms · {} admitted (max {}) · {} connections ({} refused)</p>",
        stats["uptime_s"].as_f64().unwrap_or_default(),
        cell(&stats["workers"]),
        cell(&stats["slice_ms"]),
        cell(&stats["admitted"]),
        cell(&stats["max_in_flight"]),
        cell(&stats["connections"]),
        cell(&stats["rejected_connections"]),
    ));
    let columns = [
        ("version", "version"),
        ("state", "state"),
        ("tier", "tier"),
        ("served", "served"),
        ("ok", "ok"),
        ("in_flight", "in flight"),
        ("queued", "queued"),
        ("parked", "parked"),
        ("parks", "parks"),
        ("yields", "yields"),
        ("yields_declined", "declined"),
        ("overdue_yields", "overdue"),
        ("worker_ms", "worker ms"),
        ("instructions", "instructions"),
        ("versions_alive", "versions alive"),
    ];
    html.push_str("<h2>Apps</h2><table><tr><th>app</th>");
    for (_, title) in columns {
        html.push_str(&format!("<th>{title}</th>"));
    }
    html.push_str(
        "<th>errors</th><th>cancelled</th><th>rejected</th><th>assets</th><th>kv</th>\
         <th>fetch</th></tr>",
    );
    let apps = stats["apps"].as_object().cloned().unwrap_or_default();
    for (name, app) in &apps {
        html.push_str(&format!(
            "<tr><td><a href=\"/_host/apps/{0}\">{0}</a></td>",
            escape(name)
        ));
        for (key, _) in columns {
            html.push_str(&format!("<td>{}</td>", cell(&app[key])));
        }
        let errors: u64 = app["errors"]
            .as_object()
            .map(|e| e.values().filter_map(Json::as_u64).sum())
            .unwrap_or_default();
        let rejected: u64 = app["rejected"]
            .as_object()
            .map(|e| e.values().filter_map(Json::as_u64).sum())
            .unwrap_or_default();
        let assets = if app["assets"].is_object() {
            format!(
                "{} file(s), {} bytes, {} answered ({} not modified)",
                cell(&app["assets"]["files"]),
                cell(&app["assets"]["bytes"]),
                cell(&app["assets_served"]),
                cell(&app["assets_not_modified"])
            )
        } else {
            "–".to_string()
        };
        let kv = if app["kv"].is_object() {
            format!(
                "{} / {} keys, {} / {} bytes",
                cell(&app["kv"]["keys"]),
                cell(&app["kv"]["max_keys"]),
                cell(&app["kv"]["bytes"]),
                cell(&app["kv"]["max_bytes"])
            )
        } else {
            "–".to_string()
        };
        html.push_str(&format!(
            "<td>{errors}</td><td>{}</td><td>{rejected}</td><td>{assets}</td><td>{kv}</td>\
             <td>{} calls, {} refused, {} failed</td></tr>",
            cell(&app["errors"]["cancelled"]),
            cell(&app["fetch"]["calls"]),
            cell(&app["fetch"]["refused"]),
            cell(&app["fetch"]["errors"]),
        ));
    }
    html.push_str("</table><h2>Recent errors</h2>");
    for slot in context.engine.slots() {
        let recent = slot.counters.recent_errors();
        if recent.is_empty() {
            continue;
        }
        html.push_str(&format!(
            "<h3>{}</h3><table><tr><th>when (unix ms)</th><th>kind</th><th>status</th><th>version</th><th>message</th></tr>",
            escape(&slot.name)
        ));
        for error in recent.iter().rev().take(20) {
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td style=text-align:left><code>{}</code></td></tr>",
                cell(&error["unix_ms"]),
                cell(&error["kind"]),
                cell(&error["status"]),
                cell(&error["version"]),
                cell(&error["message"]),
            ));
        }
        html.push_str("</table>");
    }
    html.push_str("<h2>Recent log lines</h2>");
    for slot in context.engine.slots() {
        let tail = slot.logs.tail(10);
        if tail.is_empty() {
            continue;
        }
        html.push_str(&format!(
            "<h3>{}</h3><pre>{}</pre>",
            escape(&slot.name),
            escape(&tail)
        ));
    }
    html.push_str("<p>JSON: <a href=\"/_host/stats\">/_host/stats</a>, <code>/_host/apps/&lt;app&gt;</code>, <code>/_host/apps/&lt;app&gt;/logs</code></p>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping() {
        assert_eq!(
            escape("<script>alert('x' & \"y\")</script>"),
            "&lt;script&gt;alert(&#39;x&#39; &amp; &quot;y&quot;)&lt;/script&gt;"
        );
    }
}
