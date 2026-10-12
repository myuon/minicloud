//! Metrics (#53): how long requests took, what that was a minute ago, and
//! the whole of it in the format a scraper reads.

mod common;

use common::*;

/// 2027-01-15 08:01:00 UTC and the minutes after it: a sample is taken for
/// the minute that just ended, so the time given is the end of it.
const AT: i64 = 1_800_000_060;

fn served() -> Apps {
    apps(&[sample("hello"), sample("crunch")])
}

#[test]
fn the_time_every_request_took_is_a_distribution() {
    let apps = served();
    let host = start(&apps, 2);
    for _ in 0..20 {
        assert_eq!(get(host.addr, "/hello/").status, 200);
    }
    let latency = app_stats(&host, "hello")["latency_us"].clone();
    assert_eq!(latency["count"], 20);
    for quantile in ["p50", "p90", "p99"] {
        let value = latency[quantile].as_u64().unwrap_or_else(|| {
            panic!("`{quantile}` is a number: {latency}");
        });
        assert!(value > 0, "{quantile} of {latency}");
    }
    assert!(
        latency["p50"].as_u64() <= latency["p99"].as_u64(),
        "the quantiles are in order: {latency}"
    );
    // An app that answered nothing has a distribution of nothing.
    let quiet = app_stats(&host, "crunch")["latency_us"].clone();
    assert_eq!(quiet["count"], 0);
    assert!(quiet["p50"].is_null(), "{quiet}");
}

#[test]
fn the_metrics_are_what_a_scraper_reads() {
    let apps = served();
    let host = start(&apps, 2);
    for _ in 0..3 {
        assert_eq!(get(host.addr, "/hello/").status, 200);
    }
    let answer = get(host.addr, "/_host/metrics");
    assert_eq!(answer.status, 200);
    assert_eq!(
        answer.header("content-type"),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    let text = answer.body;
    for expected in [
        "# TYPE minicloud_up gauge",
        "minicloud_up 1",
        "minicloud_requests_total{app=\"hello\"} 3",
        "minicloud_requests_ok_total{app=\"hello\"} 3",
        "minicloud_requests_total{app=\"crunch\"} 0",
        "# TYPE minicloud_request_duration_seconds histogram",
        "minicloud_request_duration_seconds_count{app=\"hello\"} 3",
        "minicloud_request_duration_seconds_bucket{app=\"hello\",le=\"+Inf\"} 3",
        "minicloud_errors_total{app=\"hello\",kind=\"deadline\"} 0",
        "minicloud_rejected_total{app=\"hello\",reason=\"queue_full\"} 0",
        "# EOF",
    ] {
        assert!(text.contains(expected), "no `{expected}` in:\n{text}");
    }
    // Every `# TYPE` is followed by at least one series of that name, and
    // every line is either a comment or `name value`.
    for line in text.lines() {
        assert!(
            line.starts_with('#') || line.split(' ').count() == 2,
            "not an exposition line: {line:?}"
        );
    }
}

#[test]
fn a_minute_is_kept_as_what_changed_over_it() {
    let apps = served();
    let host = start(&apps, 2);
    // The first sample has nothing to take a difference from.
    assert_eq!(host.sample_now(AT), 0);
    assert_eq!(series(&host, "hello").len(), 0);

    for _ in 0..5 {
        assert_eq!(get(host.addr, "/hello/").status, 200);
    }
    assert_eq!(host.sample_now(AT + 60), 2, "one row an app");
    let minutes = series(&host, "hello");
    assert_eq!(minutes.len(), 1);
    assert_eq!(minutes[0]["served"], 5);
    assert_eq!(minutes[0]["ok"], 5);
    assert_eq!(minutes[0]["errors"], 0);
    assert_eq!(minutes[0]["at_unix"], AT);
    assert!(
        minutes[0]["p50_us"].as_u64().is_some_and(|us| us > 0),
        "{:?}",
        minutes[0]
    );

    // The next minute is what happened in *it*, not the running total.
    assert_eq!(get(host.addr, "/hello/").status, 200);
    assert_eq!(host.sample_now(AT + 120), 2);
    let minutes = series(&host, "hello");
    assert_eq!(minutes.len(), 2);
    assert_eq!(minutes[1]["served"], 1);

    // A minute in which nothing happened is kept as a minute of nothing,
    // which is what tells a gap in the traffic from a gap in the host.
    assert_eq!(host.sample_now(AT + 180), 2);
    let minutes = series(&host, "hello");
    assert_eq!(minutes.len(), 3);
    assert_eq!(minutes[2]["served"], 0);
}

#[test]
fn the_operations_page_draws_the_last_hour() {
    let apps = served();
    let host = start(&apps, 2);
    host.sample_now(AT);
    for minute in 1..=3 {
        assert_eq!(get(host.addr, "/hello/").status, 200);
        host.sample_now(AT + minute * 60);
    }
    let page = get(host.addr, "/_host/ui").body;
    assert!(page.contains("last hour"), "a column for it");
    assert!(
        page.contains("<svg") && page.contains("<polyline"),
        "a line of the minutes: {}",
        &page[..page.len().min(400)]
    );
}

/// `GET /_host/series?app=` as the rows it answers.
fn series(host: &minicloud::Host, app: &str) -> Vec<serde_json::Value> {
    let answer = get(host.addr, &format!("/_host/series?app={app}&over=day"));
    assert_eq!(answer.status, 200);
    let json: serde_json::Value = serde_json::from_str(&answer.body).expect("JSON");
    assert_eq!(json["kept"], true, "a host with a data directory keeps it");
    json["rows"].as_array().cloned().unwrap_or_default()
}
