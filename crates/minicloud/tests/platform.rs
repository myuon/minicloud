//! The host's own figures (`crate::platform`): what its process and machine
//! have left, kept a minute at a time beside the apps' rows, and read back
//! as JSON and as the scraper's lines.
//!
//! The figures are the test's, through `Host::set_platform_source`, so that
//! nothing here depends on the machine it runs on; the sampler is called
//! with fixed times, as `metrics.rs` calls it.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use minicloud::platform::PlatformSample;

/// 2027-01-15 08:01:00 UTC: a sample is for the minute that just ended.
const AT: i64 = 1_800_000_060;

fn figures(cpu_usec: u64, memory: u64) -> PlatformSample {
    PlatformSample {
        memory_bytes: Some(memory),
        memory_max_bytes: Some(2 << 30),
        cpu_usage_usec: Some(cpu_usec),
        cpu_throttled_usec: Some(cpu_usec / 100),
        cpu_throttled_periods: Some(cpu_usec / 1_000_000),
        cpu_quota_millicores: Some(2000),
        rss_bytes: None,
        disk_total_bytes: Some(100 << 30),
        disk_available_bytes: Some(40 << 30),
        store_bytes: Some(1 << 20),
        load1_milli: Some(750),
        host_memory_total_bytes: Some(8 << 30),
        host_memory_available_bytes: Some(5 << 30),
        in_flight: 1,
        queued: 0,
        busy: 1,
        workers: 2,
    }
}

/// `GET /_host/platform?over=` as JSON.
fn platform(host: &minicloud::Host, over: &str) -> serde_json::Value {
    let answer = get(host.addr, &format!("/_host/platform?over={over}"));
    assert_eq!(answer.status, 200, "{}", answer.body);
    serde_json::from_str(&answer.body).expect("JSON")
}

#[test]
fn a_minute_of_the_platform_is_its_gauges_and_what_the_cpu_used() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 2);
    let now = Arc::new(Mutex::new(figures(10_000_000, 600 << 20)));
    let source = Arc::clone(&now);
    host.set_platform_source(Arc::new(move || source.lock().unwrap().clone()));

    // The first sample has nothing to take the CPU's difference from, and
    // keeps nothing, as an app's first does not.
    host.sample_now(AT);
    let json = platform(&host, "day");
    assert_eq!(json["kept"], true);
    assert_eq!(json["rows"].as_array().map(Vec::len), Some(0), "{json}");
    assert_eq!(json["now"]["memory_bytes"], 600 << 20);

    // A minute in which the cgroup used 30 s of CPU: a quarter of two CPUs.
    *now.lock().unwrap() = figures(40_000_000, 900 << 20);
    host.sample_now(AT + 60);
    *now.lock().unwrap() = figures(40_000_000, 700 << 20);
    host.sample_now(AT + 120);
    let json = platform(&host, "day");
    let rows = json["rows"].as_array().cloned().unwrap_or_default();
    assert_eq!(rows.len(), 2, "{json}");
    assert_eq!(rows[0]["at_unix"], AT);
    assert_eq!(rows[0]["cpu_usec"], 30_000_000);
    assert_eq!(rows[0]["cpu_throttled_usec"], 300_000);
    assert_eq!(rows[0]["cpu_throttled_periods"], 30);
    assert_eq!(rows[0]["memory_bytes"], 900 << 20);
    assert_eq!(rows[0]["memory_max_bytes"], 2u64 << 30);
    assert_eq!(rows[0]["cpu_quota_millicores"], 2000);
    assert_eq!(rows[0]["load1_milli"], 750);
    assert_eq!(rows[0]["workers"], 2);
    assert!(rows[0]["rss_bytes"].is_null(), "unknown is null, not 0");
    // A quiet minute is a minute of no CPU.
    assert_eq!(rows[1]["cpu_usec"], 0);
    assert_eq!(rows[1]["memory_bytes"], 700 << 20);
}

#[test]
fn the_scraper_reads_the_platform_now() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 2);
    host.set_platform_source(Arc::new(|| figures(2_500_000, 1024)));
    let text = get(host.addr, "/_host/metrics").body;
    for expected in [
        "# TYPE minicloud_host_memory_current_bytes gauge",
        "minicloud_host_memory_current_bytes 1024",
        "minicloud_host_memory_max_bytes 2147483648",
        "# TYPE minicloud_host_cpu_usage_seconds_total counter",
        "minicloud_host_cpu_usage_seconds_total 2.500000",
        "minicloud_host_cpu_throttled_seconds_total 0.025000",
        "minicloud_host_cpu_quota_cores 2.000",
        "minicloud_host_disk_available_bytes 42949672960",
        "minicloud_host_disk_total_bytes 107374182400",
        "minicloud_host_load1 0.750",
        "minicloud_host_workers_busy 1",
        "minicloud_host_workers 2",
        "minicloud_host_queued 0",
    ] {
        assert!(text.contains(expected), "no `{expected}` in:\n{text}");
    }
    // What the host could not read is not there at all.
    assert!(!text.contains("minicloud_host_rss_bytes"), "{text}");
    assert!(text.ends_with("# EOF\n"));
    for line in text.lines() {
        assert!(
            line.starts_with('#') || line.split(' ').count() == 2,
            "not an exposition line: {line:?}"
        );
    }
}

#[test]
fn the_live_platform_never_fails() {
    // No source: the machine's own files, whatever this one has. The
    // engine's figures are always there; the rest may be anything.
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 3);
    let now = host.platform_now();
    assert_eq!(now.workers, 3);
    assert!(now.busy <= 3);
    let json = platform(&host, "hour");
    assert_eq!(json["now"]["workers"], 3);
    #[cfg(unix)]
    assert!(
        json["now"]["disk_total_bytes"]
            .as_u64()
            .is_some_and(|n| n > 0),
        "the data directory is on a disk: {json}"
    );
}
