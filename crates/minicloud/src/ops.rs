//! The operations views: what the host is doing, per app, as JSON and as one
//! HTML page.
//!
//! | path | what |
//! | --- | --- |
//! | `GET /_host/stats` | every app's counters, gauges, version and store usage, and the server's |
//! | `GET /_host/apps/<app>` | one app: the same, its versions, and its recent errors |
//! | `GET /_host/metrics` | the same counters as OpenMetrics, for Prometheus to scrape |
//! | `GET /_host/series?app=&over=hour\|day\|month` | what each app did, minute by minute (or hour by hour), as JSON |
//! | `GET /_host/platform?over=hour\|day\|month` | the host's process and machine ([`crate::platform`]) now, and minute by minute (or hour by hour), as JSON |
//! | `GET /_host/timeline?app=&over=\|since=&until=` | where each request of the window waited, ran on which worker, parked at which host call and resumed ([`crate::timeline`]), with the workers' run segments, three strips and a summary, as JSON; `since`/`until` are Unix milliseconds, `over` `10s`, `1m`, `5m` or seconds (a minute by default) |
//! | `GET /_host/timeline/<id>` | one request of it, its phases whole |
//! | `GET /_host/live` | the requests in flight now and where each is, and what each worker is running |
//! | `GET /_host/load` | the load run going on ([`crate::load`]) and the last one: its figures, what it has sent and how each was answered, p50/p99 so far, and the targets the host has no app for. Started and stopped with `POST /_host/load` and `/_host/load/stop` on the admin listener, with its token ([`crate::admin`]) |
//! | `GET /_host/apps/<app>/logs?n=` | its recent log lines, as text |
//! | `GET /_host/apps/<app>/logs?follow=1` | the same, and then every line as it is written, as server-sent events |
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
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;

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
    /// What each app did, minute by minute; `None` for a host that keeps
    /// no history.
    pub series: Option<&'a std::sync::Mutex<crate::series::Series>>,
}

/// The keys summed into the stats' `totals`.
const TOTALS: [&str; 18] = [
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
    "gc_collections",
    "gc_runs",
    "allocated_words",
    "worker_ms",
    "updates",
];

/// What `GET /_host/series` answers: the rows of the history.
///
/// `over` is a window rather than a row count because the two tables have
/// different granularities: an hour and a day are minutes, a month is
/// hours, and asking for "the last 500 rows" would mean something
/// different in each.
pub fn series(context: &OpsContext, app: Option<&str>, over: &str) -> Json {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    let (secs, hourly) = crate::series::window(over);
    let Some(series) = context.series else {
        return json!({"over": over, "kept": false, "rows": []});
    };
    let series = series.lock().unwrap_or_else(|p| p.into_inner());
    let rows = if hourly {
        series.hours(app, now - secs)
    } else {
        series.minutes(app, now - secs)
    };
    match rows {
        Ok(rows) => json!({
            "over": over,
            "kept": true,
            "rows": rows.iter().map(|row| json!({
                "app": row.app,
                "at_unix": row.at_unix,
                "served": row.served,
                "ok": row.ok,
                "errors": row.errors,
                "rejected": row.rejected,
                "worker_ms": row.worker_ms,
                "p50_us": row.p50_us,
                "p99_us": row.p99_us,
                "gc_collections": row.gc_collections,
                "gc_runs": row.gc_runs,
                "allocated_words": row.allocated_words,
            })).collect::<Vec<Json>>(),
        }),
        Err(why) => json!({"over": over, "kept": true, "error": why, "rows": []}),
    }
}

/// A platform figure as JSON: the number, or `null` for one the host could
/// not read.
fn known(value: Option<u64>) -> Json {
    value.map_or(Json::Null, Json::from)
}

/// The platform now, as `GET /_host/platform` answers it in `now`.
pub fn platform_now_json(now: &crate::platform::PlatformSample) -> Json {
    json!({
        "memory_bytes": known(now.memory_bytes),
        "memory_max_bytes": known(now.memory_max_bytes),
        "rss_bytes": known(now.rss_bytes),
        "cpu_usage_usec": known(now.cpu_usage_usec),
        "cpu_throttled_usec": known(now.cpu_throttled_usec),
        "cpu_throttled_periods": known(now.cpu_throttled_periods),
        "cpu_quota_millicores": known(now.cpu_quota_millicores),
        "disk_total_bytes": known(now.disk_total_bytes),
        "disk_available_bytes": known(now.disk_available_bytes),
        "store_bytes": known(now.store_bytes),
        "load1_milli": known(now.load1_milli),
        "host_memory_total_bytes": known(now.host_memory_total_bytes),
        "host_memory_available_bytes": known(now.host_memory_available_bytes),
        "in_flight": now.in_flight,
        "queued": now.queued,
        "busy": now.busy,
        "workers": now.workers,
    })
}

/// What `GET /_host/platform` answers: the platform `now`, and the rows of
/// its history over `over`, a window as [`series`]' is.
///
/// A row's `cpu_usec`, `cpu_throttled_usec` and `cpu_throttled_periods` are
/// what the counters grew by over the row's span (a minute, or an hour for
/// `month`); every other column is a gauge (see [`crate::series::HostRow`]).
pub fn platform(context: &OpsContext, now: &crate::platform::PlatformSample, over: &str) -> Json {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    let (secs, hourly) = crate::series::window(over);
    let current = platform_now_json(now);
    let Some(series) = context.series else {
        return json!({"over": over, "kept": false, "now": current, "rows": []});
    };
    let series = series.lock().unwrap_or_else(|p| p.into_inner());
    let rows = if hourly {
        series.host_hours(unix - secs)
    } else {
        series.host_minutes(unix - secs)
    };
    match rows {
        Ok(rows) => json!({
            "over": over,
            "kept": true,
            "now": current,
            "rows": rows.iter().map(|row| json!({
                "at_unix": row.at_unix,
                "memory_bytes": known(row.memory_bytes),
                "memory_max_bytes": known(row.memory_max_bytes),
                "rss_bytes": known(row.rss_bytes),
                "cpu_usec": known(row.cpu_usec),
                "cpu_throttled_usec": known(row.cpu_throttled_usec),
                "cpu_throttled_periods": known(row.cpu_throttled_periods),
                "cpu_quota_millicores": known(row.cpu_quota_millicores),
                "disk_total_bytes": known(row.disk_total_bytes),
                "disk_available_bytes": known(row.disk_available_bytes),
                "store_bytes": known(row.store_bytes),
                "load1_milli": known(row.load1_milli),
                "host_memory_total_bytes": known(row.host_memory_total_bytes),
                "host_memory_available_bytes": known(row.host_memory_available_bytes),
                "in_flight": row.in_flight,
                "queued": row.queued,
                "busy": row.busy,
                "workers": row.workers,
                "gc_collections": known(row.gc_collections),
                "gc_runs": known(row.gc_runs),
                "allocated_words": known(row.allocated_words),
            })).collect::<Vec<Json>>(),
        }),
        Err(why) => json!({"over": over, "kept": true, "now": current, "error": why, "rows": []}),
    }
}

/// What `GET /_host/metrics` answers: the counters in the text exposition
/// format Prometheus scrapes.
///
/// Every series is labelled by app, because the host's own totals are a
/// sum anyone scraping can do and the per-app numbers are not recoverable
/// from them. The request times are a cumulative histogram
/// ([`crate::histogram`]), so a quantile is the scraper's to compute with
/// `histogram_quantile` over whatever window it wants — the host's own
/// estimate over all time is in the stats, and is a different question.
///
/// The host's own figures (`minicloud_host_*`, from `platform`) come last
/// and have no label: there is one host. A figure the host could not read
/// is left out, family and all, rather than written as zero.
pub fn metrics(context: &OpsContext, platform: &crate::platform::PlatformSample) -> String {
    let mut out = String::new();
    out.push_str("# HELP minicloud_up Whether the host is answering.\n");
    out.push_str("# TYPE minicloud_up gauge\nminicloud_up 1\n");
    out.push_str("# HELP minicloud_build_info The version of the host.\n");
    out.push_str("# TYPE minicloud_build_info gauge\n");
    out.push_str(&format!(
        "minicloud_build_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str("# HELP minicloud_uptime_seconds How long the host has been up.\n");
    out.push_str("# TYPE minicloud_uptime_seconds gauge\n");
    out.push_str(&format!(
        "minicloud_uptime_seconds {:.3}\n",
        context.started.elapsed().as_secs_f64()
    ));

    let slots = context.engine.slots();
    let mut counter = |name: &str, help: &str, kind: &str, values: Vec<(String, String)>| {
        out.push_str(&format!("# HELP minicloud_{name} {help}\n"));
        out.push_str(&format!("# TYPE minicloud_{name} {kind}\n"));
        for (labels, value) in values {
            out.push_str(&format!("minicloud_{name}{labels} {value}\n"));
        }
    };
    let per_app = |read: &dyn Fn(&Arc<Slot>) -> u64| -> Vec<(String, String)> {
        slots
            .iter()
            .map(|slot| {
                (
                    format!("{{app=\"{}\"}}", label(&slot.name)),
                    read(slot).to_string(),
                )
            })
            .collect()
    };
    counter(
        "requests_total",
        "Requests an app answered, with any status.",
        "counter",
        per_app(&|slot| slot.counters.served.load(Ordering::Relaxed)),
    );
    counter(
        "requests_ok_total",
        "Requests an app answered 2xx-4xx itself.",
        "counter",
        per_app(&|slot| slot.counters.ok.load(Ordering::Relaxed)),
    );
    let mut errors = Vec::new();
    for slot in &slots {
        for kind in crate::stats::ErrorKind::ALL {
            errors.push((
                format!("{{app=\"{}\",kind=\"{}\"}}", label(&slot.name), kind.name()),
                slot.counters.errors(kind).to_string(),
            ));
        }
    }
    counter(
        "errors_total",
        "Runs a limit or a fault ended, by kind.",
        "counter",
        errors,
    );
    let mut rejected = Vec::new();
    for slot in &slots {
        let json = slot.counters.to_json(0, 0);
        if let Some(reasons) = json["rejected"].as_object() {
            for (reason, count) in reasons {
                rejected.push((
                    format!("{{app=\"{}\",reason=\"{reason}\"}}", label(&slot.name)),
                    count.as_u64().unwrap_or_default().to_string(),
                ));
            }
        }
    }
    counter(
        "rejected_total",
        "Requests turned away before they reached an app's queue, by reason.",
        "counter",
        rejected,
    );
    counter(
        "worker_seconds_total",
        "Time workers spent running an app's code.",
        "counter",
        slots
            .iter()
            .map(|slot| {
                (
                    format!("{{app=\"{}\"}}", label(&slot.name)),
                    format!(
                        "{:.6}",
                        slot.counters.worker_ns.load(Ordering::Relaxed) as f64 / 1e9
                    ),
                )
            })
            .collect(),
    );
    let gauges = [
        ("in_flight", "Requests admitted and not answered."),
        ("queued", "Requests waiting to start."),
    ];
    for (name, help) in gauges {
        let values = slots
            .iter()
            .map(|slot| {
                let (in_flight, queued) = context.engine.queue.gauges(slot.index);
                (
                    format!("{{app=\"{}\"}}", label(&slot.name)),
                    if name == "in_flight" {
                        in_flight
                    } else {
                        queued
                    }
                    .to_string(),
                )
            })
            .collect();
        counter(name, help, "gauge", values);
    }
    counter(
        "app_gc_collections_total",
        "Garbage collections an app's runs made, summed over finished runs.",
        "counter",
        per_app(&|slot| slot.counters.gc_collections.load(Ordering::Relaxed)),
    );
    counter(
        "app_gc_runs_total",
        "Finished runs of an app that collected at least once.",
        "counter",
        per_app(&|slot| slot.counters.gc_runs.load(Ordering::Relaxed)),
    );
    counter(
        "app_allocated_bytes_total",
        "Bytes an app's runs allocated, reuse counted each time (eight a word).",
        "counter",
        per_app(&|slot| {
            slot.counters
                .allocated_words
                .load(Ordering::Relaxed)
                .saturating_mul(8)
        }),
    );
    counter(
        "assets_served_total",
        "Static files the host answered for an app.",
        "counter",
        per_app(&|slot| slot.counters.assets.load(Ordering::Relaxed)),
    );
    counter(
        "trigger_runs_total",
        "Runs the host started on a schedule.",
        "counter",
        per_app(&|slot| slot.counters.trigger_runs.load(Ordering::Relaxed)),
    );

    // The histogram, which has a shape of its own: one `_bucket` series per
    // bound with a `le` label, then `_sum` and `_count`.
    out.push_str("# HELP minicloud_request_duration_seconds Admission to answer.\n");
    out.push_str("# TYPE minicloud_request_duration_seconds histogram\n");
    for slot in &slots {
        let app = label(&slot.name);
        let snapshot = slot.counters.latency.snapshot();
        let mut running = 0;
        for (bound, index) in crate::histogram::BOUNDS_US.iter().zip(0..) {
            running += snapshot.buckets[index];
            out.push_str(&format!(
                "minicloud_request_duration_seconds_bucket{{app=\"{app}\",le=\"{:.6}\"}} {running}\n",
                *bound as f64 / 1e6
            ));
        }
        out.push_str(&format!(
            "minicloud_request_duration_seconds_bucket{{app=\"{app}\",le=\"+Inf\"}} {}\n",
            snapshot.count
        ));
        out.push_str(&format!(
            "minicloud_request_duration_seconds_sum{{app=\"{app}\"}} {:.6}\n",
            snapshot.sum_us as f64 / 1e6
        ));
        out.push_str(&format!(
            "minicloud_request_duration_seconds_count{{app=\"{app}\"}} {}\n",
            snapshot.count
        ));
    }
    let seconds = |usec: Option<u64>| usec.map(|n| format!("{:.6}", n as f64 / 1e6));
    let whole = |n: Option<u64>| n.map(|n| n.to_string());
    let host: [(&str, &str, &str, Option<String>); 13] = [
        (
            "memory_current_bytes",
            "gauge",
            "Memory the host's cgroup is charged for (memory.current).",
            whole(platform.memory_bytes),
        ),
        (
            "memory_max_bytes",
            "gauge",
            "The host's cgroup's memory limit (memory.max); absent for none.",
            whole(platform.memory_max_bytes),
        ),
        (
            "rss_bytes",
            "gauge",
            "The host process's resident set.",
            whole(platform.rss_bytes),
        ),
        (
            "cpu_usage_seconds_total",
            "counter",
            "CPU time the host's cgroup used (cpu.stat usage_usec).",
            seconds(platform.cpu_usage_usec),
        ),
        (
            "cpu_throttled_seconds_total",
            "counter",
            "Time the CPU quota held the host's cgroup back (cpu.stat throttled_usec).",
            seconds(platform.cpu_throttled_usec),
        ),
        (
            "cpu_quota_cores",
            "gauge",
            "The host's cgroup's CPU quota, in CPUs (cpu.max); absent for none.",
            platform
                .cpu_quota_millicores
                .map(|m| format!("{:.3}", m as f64 / 1000.0)),
        ),
        (
            "disk_available_bytes",
            "gauge",
            "Free space for the host on the data directory's disk.",
            whole(platform.disk_available_bytes),
        ),
        (
            "disk_total_bytes",
            "gauge",
            "The size of the data directory's disk.",
            whole(platform.disk_total_bytes),
        ),
        (
            "load1",
            "gauge",
            "The machine's one-minute load average.",
            platform
                .load1_milli
                .map(|m| format!("{:.3}", m as f64 / 1000.0)),
        ),
        (
            "memory_available_bytes",
            "gauge",
            "Memory the machine could still give (MemAvailable).",
            whole(platform.host_memory_available_bytes),
        ),
        (
            "workers_busy",
            "gauge",
            "Workers running a run now.",
            Some(platform.busy.to_string()),
        ),
        (
            "workers",
            "gauge",
            "Workers in all.",
            Some(platform.workers.to_string()),
        ),
        (
            "queued",
            "gauge",
            "Requests waiting to start, over every app.",
            Some(platform.queued.to_string()),
        ),
    ];
    for (name, kind, help, value) in host {
        if let Some(value) = value {
            out.push_str(&format!("# HELP minicloud_host_{name} {help}\n"));
            out.push_str(&format!("# TYPE minicloud_host_{name} {kind}\n"));
            out.push_str(&format!("minicloud_host_{name} {value}\n"));
        }
    }
    out.push_str("# EOF\n");
    out
}

/// The requests of each of the last minutes, as a line.
///
/// Scaled to its own largest minute, because the question it answers is
/// "what is the shape of this app's hour" and not "how does it compare to
/// the app below it"; the largest is in the `title`, which is where a
/// number belongs when the picture is a shape.
fn sparkline(minutes: &[u64]) -> String {
    let largest = minutes.iter().copied().max().unwrap_or(0);
    if minutes.len() < 2 || largest == 0 {
        return "<span class=muted>–</span>".to_string();
    }
    let (width, height) = (120.0_f64, 20.0_f64);
    let step = width / (minutes.len() - 1) as f64;
    let points: Vec<String> = minutes
        .iter()
        .enumerate()
        .map(|(at, served)| {
            let x = at as f64 * step;
            let y = height - (*served as f64 / largest as f64) * height;
            format!("{x:.1},{y:.1}")
        })
        .collect();
    format!(
        "<svg width=\"{width:.0}\" height=\"{height:.0}\" viewBox=\"0 0 {width:.0} {height:.0}\" \
         role=img aria-label=\"requests a minute over the last hour\">\
         <title>up to {largest} a minute over {} minute(s)</title>\
         <polyline fill=none stroke=currentColor stroke-width=1 points=\"{}\"/></svg>",
        minutes.len(),
        points.join(" ")
    )
}

/// A label value, with what the exposition format cannot carry escaped.
fn label(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Takes a sample of every app's counters each minute and writes what
/// changed over that minute to the host's history ([`crate::series`]), then
/// rolls up and prunes at the top of each hour.
///
/// The counters are running totals, so what is kept is the **difference**
/// since the last sample: a row is what that app did in that minute, which
/// is the only shape a rate can be read off later. The first tick after a
/// start records nothing, because there is nothing to take a difference
/// from — a host that counted its totals as one minute's work would show a
/// spike at every restart.
pub(crate) async fn sample(front: Arc<crate::server::Front>) {
    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() as i64);
        let next = (now / 60 + 1) * 60;
        tokio::time::sleep(std::time::Duration::from_secs((next - now).max(1) as u64)).await;
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() as i64);
        sample_now(&front, at);
    }
}

/// Takes one sample and keeps it, and at the top of an hour rolls up and
/// prunes. Answers how many rows were written.
///
/// The ticking task calls this a moment after each minute begins; a test
/// calls it with a time of its own, as it does a schedule's.
pub(crate) fn sample_now(front: &crate::server::Front, at: i64) -> usize {
    let Some(series) = &front.series else {
        return 0;
    };
    let mut before = front.sampled.lock().unwrap_or_else(|p| p.into_inner());
    let mut samples = Vec::new();
    for slot in front.engine.slots() {
        let (totals, histogram) = totals_of(&slot);
        if let Some((was, seen)) = before.get(&slot.name) {
            let over = histogram.since(seen);
            samples.push(crate::series::Sample {
                app: slot.name.clone(),
                at_unix: at - 60,
                served: totals.served.saturating_sub(was.served),
                ok: totals.ok.saturating_sub(was.ok),
                errors: totals.errors.saturating_sub(was.errors),
                rejected: totals.rejected.saturating_sub(was.rejected),
                worker_ms: totals.worker_ms.saturating_sub(was.worker_ms),
                p50_us: over.quantile_us(0.50).unwrap_or(0),
                p99_us: over.quantile_us(0.99).unwrap_or(0),
                gc_collections: totals.gc_collections.saturating_sub(was.gc_collections),
                gc_runs: totals.gc_runs.saturating_sub(was.gc_runs),
                allocated_words: totals.allocated_words.saturating_sub(was.allocated_words),
            });
        }
        before.insert(slot.name.clone(), (totals, histogram));
    }
    drop(before);
    // The platform, read before the store is locked: the files are the
    // kernel's and quick, but a test's source is anybody's.
    let platform = front.platform_now();
    let was = front
        .platform_sampled
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .replace(platform.clone());
    let series = series.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(was) = was {
        let mut row = host_row(&was, &platform, at - 60);
        // The apps' heaps over the same minute, summed: the platform's
        // view of what all of them allocated and collected.
        let sum = |field: fn(&crate::series::Sample) -> u64| samples.iter().map(field).sum();
        row.gc_collections = Some(sum(|s| s.gc_collections));
        row.gc_runs = Some(sum(|s| s.gc_runs));
        row.allocated_words = Some(sum(|s| s.allocated_words));
        if let Err(why) = series.record_host(&row) {
            eprintln!("minicloud: cannot keep the platform's history: {why}");
        }
    }
    if !samples.is_empty() {
        if let Err(why) = series.record(&samples) {
            eprintln!("minicloud: cannot keep the history: {why}");
            return 0;
        }
    }
    // At the top of the hour: roll the hour that ended into the hour table
    // and drop what is past its keeping.
    if at % 3600 < 60 {
        if let Err(why) = series.sweep(
            at,
            crate::series::KEEP_MINUTES_SECS,
            crate::series::KEEP_HOURS_SECS,
        ) {
            eprintln!("minicloud: cannot sweep the history: {why}");
        }
    }
    samples.len()
}

/// The host's row for the minute that ends with `now`: what the CPU
/// counters grew by since `was`, and every other figure as `now` has it.
///
/// A counter known at one end and not the other is unknown over the minute,
/// and one that went backwards (a cgroup that was made anew) counts from
/// nothing rather than wrapping.
pub(crate) fn host_row(
    was: &crate::platform::PlatformSample,
    now: &crate::platform::PlatformSample,
    at_unix: i64,
) -> crate::series::HostRow {
    let grew = |before: Option<u64>, after: Option<u64>| {
        let (before, after) = (before?, after?);
        Some(after.checked_sub(before).unwrap_or(after))
    };
    crate::series::HostRow {
        at_unix,
        memory_bytes: now.memory_bytes,
        memory_max_bytes: now.memory_max_bytes,
        rss_bytes: now.rss_bytes,
        cpu_usec: grew(was.cpu_usage_usec, now.cpu_usage_usec),
        cpu_throttled_usec: grew(was.cpu_throttled_usec, now.cpu_throttled_usec),
        cpu_throttled_periods: grew(was.cpu_throttled_periods, now.cpu_throttled_periods),
        cpu_quota_millicores: now.cpu_quota_millicores,
        disk_total_bytes: now.disk_total_bytes,
        disk_available_bytes: now.disk_available_bytes,
        store_bytes: now.store_bytes,
        load1_milli: now.load1_milli,
        host_memory_total_bytes: now.host_memory_total_bytes,
        host_memory_available_bytes: now.host_memory_available_bytes,
        in_flight: now.in_flight,
        queued: now.queued,
        busy: now.busy,
        workers: now.workers,
        // The apps', which `sample_now` sums from their rows.
        gc_collections: None,
        gc_runs: None,
        allocated_words: None,
    }
}

/// One app's running totals now, and its request times.
fn totals_of(slot: &Arc<Slot>) -> (crate::series::Sample, crate::histogram::Snapshot) {
    let counters = &slot.counters;
    let json = counters.to_json(0, 0);
    let sum = |group: &str| {
        json[group]
            .as_object()
            .map(|counts| counts.values().filter_map(Json::as_u64).sum())
            .unwrap_or_default()
    };
    (
        crate::series::Sample {
            app: slot.name.clone(),
            at_unix: 0,
            served: json["served"].as_u64().unwrap_or_default(),
            ok: json["ok"].as_u64().unwrap_or_default(),
            errors: sum("errors"),
            rejected: sum("rejected"),
            worker_ms: json["worker_ms"].as_f64().unwrap_or_default() as u64,
            p50_us: 0,
            p99_us: 0,
            gc_collections: json["gc_collections"].as_u64().unwrap_or_default(),
            gc_runs: json["gc_runs"].as_u64().unwrap_or_default(),
            allocated_words: json["allocated_words"].as_u64().unwrap_or_default(),
        },
        counters.latency.snapshot(),
    )
}

/// How often a following answer writes a comment to keep the connection
/// through a proxy from being closed as idle. Server-sent events ignore a
/// line that starts with `:`.
const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);

/// `GET /_host/apps/<app>/logs?follow=1`: the last `n` lines, then every
/// line as it is written, as server-sent events.
///
/// The answer never ends on its own. It ends when the client goes away,
/// which the send on the body's channel is what notices — there is nothing
/// to cancel and nothing to clean up but the task itself.
pub fn follow(slot: &Arc<Slot>, n: usize) -> hyper::Response<crate::server::OutBody> {
    let (chunks, body) = tokio::sync::mpsc::channel(64);
    let mut live = slot.logs.follow();
    let backlog = slot.logs.tail(n);
    let name = slot.name.clone();
    tokio::spawn(async move {
        let opening = format!(
            ": following `{name}`; the last {} line(s) first\n\n",
            backlog.lines().count()
        );
        if chunks.send(Bytes::from(opening)).await.is_err() {
            return;
        }
        for line in backlog.lines() {
            if chunks.send(event(line)).await.is_err() {
                return;
            }
        }
        loop {
            let next = tokio::time::timeout(HEARTBEAT, live.recv()).await;
            let chunk = match next {
                Ok(Ok(line)) => event(&line),
                // Past the ring's backlog: say so rather than leave a hole.
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(missed))) => Bytes::from(
                    format!(": {missed} line(s) missed; the log outran this\n\n"),
                ),
                // The app and its ring are gone: an app that was removed.
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return,
                Err(_elapsed) => Bytes::from_static(b": ping\n\n"),
            };
            if chunks.send(chunk).await.is_err() {
                return;
            }
        }
    });
    crate::server::to_streamed(
        200,
        &[
            ("content-type", "text/event-stream; charset=utf-8"),
            ("cache-control", "no-store"),
            // Tells nginx not to buffer an answer it would otherwise hold.
            ("x-accel-buffering", "no"),
        ],
        body,
    )
}

/// One line as a server-sent event.
fn event(line: &str) -> Bytes {
    Bytes::from(format!("data: {line}\n\n"))
}

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
            if !app.queues.is_empty() {
                let depths = context
                    .data
                    .and_then(|data| {
                        crate::queue::usage(&data.join(&slot.name).join("queue.sqlite3"))
                    })
                    .unwrap_or_default();
                object.insert(
                    "queues".into(),
                    json!(app
                        .queues
                        .iter()
                        .map(|queue| {
                            let (waiting, dead) = depths
                                .iter()
                                .find(|(name, _, _)| *name == queue.name)
                                .map(|(_, waiting, dead)| (*waiting, *dead))
                                .unwrap_or((0, 0));
                            json!({
                                "name": queue.name,
                                "path": queue.path,
                                "waiting": waiting,
                                "dead": dead,
                                "max_depth": queue.max_depth,
                                "max_attempts": queue.max_attempts,
                            })
                        })
                        .collect::<Vec<Json>>()),
                );
            }
            if app.granted.contains("sql") {
                let bytes = context
                    .data
                    .and_then(|data| crate::sql::usage(&data.join(&slot.name).join("sql.sqlite3")));
                object.insert(
                    "sql".into(),
                    json!({
                        "bytes": bytes,
                        "max_bytes": app.sql.max_bytes,
                    }),
                );
            }
            if !app.triggers.is_empty() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs() as i64);
                let at = crate::cron::Minute::from_unix(now);
                object.insert(
                    "triggers".into(),
                    json!(app
                        .triggers
                        .iter()
                        .map(|trigger| json!({
                            "cron": trigger.cron.as_str(),
                            "method": trigger.method,
                            "path": trigger.path,
                            "next_unix_ms": trigger
                                .cron
                                .next_after(at)
                                .map(|next| next.to_unix() * 1000),
                        }))
                        .collect::<Vec<Json>>()),
                );
            }
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
        "<th>last hour</th><th>errors</th><th>cancelled</th><th>rejected</th>\
         <th>triggers</th><th>assets</th><th>kv</th><th>sql</th><th>fetch</th></tr>",
    );
    // One query for every app's last hour, drawn beside its counters.
    let recent = series(context, None, "hour");
    let mut lines: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for row in recent["rows"].as_array().into_iter().flatten() {
        if let Some(app) = row["app"].as_str() {
            lines
                .entry(app.to_string())
                .or_default()
                .push(row["served"].as_u64().unwrap_or_default());
        }
    }
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
        let hour = sparkline(lines.get(name).map(Vec::as_slice).unwrap_or(&[]));
        let triggers = match app["triggers"].as_array() {
            Some(list) if !list.is_empty() => format!(
                "{}: {} run, {} skipped, {} failed",
                list.iter()
                    .filter_map(|t| t["cron"].as_str())
                    .map(escape)
                    .collect::<Vec<String>>()
                    .join(", "),
                cell(&app["trigger_runs"]),
                cell(&app["trigger_skipped"]),
                cell(&app["trigger_failed"])
            ),
            _ => "–".to_string(),
        };
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
        let sql = if app["sql"].is_object() {
            format!(
                "{} / {} bytes",
                cell(&app["sql"]["bytes"]),
                cell(&app["sql"]["max_bytes"])
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
            "<td>{hour}</td><td>{errors}</td><td>{}</td><td>{rejected}</td><td>{triggers}</td>\
             <td>{assets}</td><td>{kv}</td><td>{sql}</td>\
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
