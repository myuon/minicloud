//! Load the host puts on its own apps (`crate::load`): `POST /_host/load`
//! and `/_host/load/stop` on the admin listener, `GET /_host/load`, and the
//! requests it sends showing in the timeline as any other.
//!
//! The runs are short and on few connections; nothing asserts a rate or a
//! latency, only what was counted and how the run ended.

mod common;

use std::net::SocketAddr;

use common::*;
use serde_json::Value as Json;

/// `POST path` on the admin listener, with the token, and `body`.
fn admin_post(host: &minicloud::Host, path: &str, body: &str) -> Answer {
    send_raw(
        host.admin_addr.unwrap(),
        format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Authorization: Bearer {ADMIN_TOKEN}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
}

fn status(addr: SocketAddr) -> Json {
    let answer = get(addr, "/_host/load");
    assert_eq!(answer.status, 200, "{}", answer.body);
    serde_json::from_str(&answer.body).expect("JSON")
}

/// Waits for the run going on to end, and answers the status then.
fn ended(host: &minicloud::Host) -> Json {
    wait_until("the load run to end", || {
        !status(host.addr)["running"].as_bool().unwrap()
    });
    status(host.addr)
}

fn n(value: &Json) -> u64 {
    value
        .as_u64()
        .unwrap_or_else(|| panic!("not a count: {value}"))
}

#[test]
fn a_closed_loop_run_is_counted_and_its_requests_are_in_the_timeline() {
    let apps = apps(&[sample("hello"), sample("slow")]);
    let host = start(&apps, 2);
    let idle = status(host.addr);
    assert_eq!(idle["running"], false);
    assert_eq!(idle["current"], Json::Null);
    assert_eq!(idle["last"], Json::Null);

    let started = admin_post(
        &host,
        "/_host/load",
        r#"{"mix":"wait","rate":0,"connections":2,"seconds":1}"#,
    );
    assert_eq!(started.status, 200, "{}", started.body);
    let answer: Json = serde_json::from_str(&started.body).unwrap();
    assert_eq!(answer["running"], true);
    assert_eq!(answer["current"]["mix"], "wait");
    assert_eq!(answer["current"]["state"], "running");

    let last = ended(&host)["last"].clone();
    assert_eq!(last["state"], "finished", "{last}");
    assert_eq!(last["connections"], 2);
    assert_eq!(last["targets"], serde_json::json!(["slow"]));
    assert_eq!(last["not_deployed"], serde_json::json!([]));
    // Each request parks three times for 60 ms: two connections for a
    // second ask a handful, every one answered within the grace.
    let ok = n(&last["ok"]);
    assert!(ok >= 2, "{last}");
    assert_eq!(n(&last["sent"]), ok, "{last}");
    assert_eq!(n(&last["answered"]), ok, "{last}");
    assert_eq!(n(&last["errors"]), 0, "{last}");
    assert!(n(&last["p50_us"]) > 0, "{last}");
    assert!(n(&last["p99_us"]) >= n(&last["p50_us"]), "{last}");

    // They went through the public listener, so the timeline has them all.
    let timeline = get(host.addr, "/_host/timeline?app=slow&over=5m");
    let timeline: Json = serde_json::from_str(&timeline.body).unwrap();
    assert_eq!(n(&timeline["summary"]["requests"]), ok, "{timeline}");
    let request = &timeline["requests"][0];
    assert_eq!(request["path"], "/slow/");
    assert_eq!(request["status"], 200);
}

#[test]
fn a_second_run_is_refused_and_a_run_stops_when_asked() {
    let apps = apps(&[sample("hello"), sample("slow")]);
    let host = start(&apps, 2);
    let first = admin_post(
        &host,
        "/_host/load",
        r#"{"mix":"wait","connections":2,"seconds":60}"#,
    );
    assert_eq!(first.status, 200, "{}", first.body);
    let second = admin_post(&host, "/_host/load", r#"{"mix":"light"}"#);
    assert_eq!(second.status, 409, "{}", second.body);
    assert!(
        second.body.contains("a load run is going on"),
        "{}",
        second.body
    );

    wait_until("a request of the run to be answered", || {
        n(&status(host.addr)["current"]["ok"]) > 0
    });
    let stopped = admin_post(&host, "/_host/load/stop", "");
    assert_eq!(stopped.status, 200, "{}", stopped.body);
    let last = ended(&host)["last"].clone();
    assert_eq!(last["state"], "stopped", "{last}");
    assert!(n(&last["elapsed_ms"]) < 60_000, "{last}");

    // Nothing to stop now; and another run may start.
    let again = admin_post(&host, "/_host/load/stop", "");
    assert_eq!(again.status, 409, "{}", again.body);
    let next = admin_post(
        &host,
        "/_host/load",
        r#"{"mix":"light","connections":1,"seconds":1}"#,
    );
    assert_eq!(next.status, 200, "{}", next.body);
    assert_eq!(ended(&host)["last"]["mix"], "light");
}

#[test]
fn a_target_the_host_does_not_have_is_reported_and_left_alone() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 2);
    // Every target missing: the run ends at once, saying why.
    let started = admin_post(
        &host,
        "/_host/load",
        r#"{"mix":"cpu","connections":2,"seconds":30}"#,
    );
    assert_eq!(started.status, 200, "{}", started.body);
    let last = ended(&host)["last"].clone();
    assert_eq!(last["state"], "finished", "{last}");
    assert_eq!(last["not_deployed"], serde_json::json!(["crunch"]));
    assert_eq!(
        last["problems"],
        serde_json::json!(["target app crunch is not deployed"])
    );
    assert!(n(&last["elapsed_ms"]) < 30_000, "{last}");
    assert_eq!(n(&last["ok"]), 0, "{last}");
    assert!(n(&last["client_errors"]) >= 1, "{last}");

    // Some missing: the others are still asked.
    let started = admin_post(
        &host,
        "/_host/load",
        r#"{"mix":"mixed","connections":2,"seconds":1}"#,
    );
    assert_eq!(started.status, 200, "{}", started.body);
    let last = ended(&host)["last"].clone();
    assert_eq!(
        last["not_deployed"],
        serde_json::json!(["crunch", "slow"]),
        "{last}"
    );
    assert!(n(&last["ok"]) > 0, "{last}");
}

#[test]
fn starting_takes_the_admin_token_and_figures_within_their_caps() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 1);
    // Not on the public listener, whose operations views anyone may read.
    let public = post(host.addr, "/_host/load", r#"{"mix":"light"}"#);
    assert_eq!(public.status, 405, "{}", public.body);
    // Not without the token.
    let anonymous = post(
        host.admin_addr.unwrap(),
        "/_host/load",
        r#"{"mix":"light"}"#,
    );
    assert_eq!(anonymous.status, 401, "{}", anonymous.body);
    for (body, says) in [
        (r#"{"mix":"heavy"}"#, "none of light, cpu, wait or mixed"),
        (r#"{"rate":501}"#, "rate is 0 (closed loop) to 500"),
        (r#"{"connections":65}"#, "connections is 1 to 64"),
        (r#"{"seconds":61}"#, "seconds is 1 to 60"),
        ("not json", "not JSON"),
    ] {
        let refused = admin_post(&host, "/_host/load", body);
        assert_eq!(refused.status, 422, "{body}: {}", refused.body);
        assert!(refused.body.contains(says), "{body}: {}", refused.body);
    }
    assert_eq!(status(host.addr)["running"], false);
    assert_eq!(status(host.addr)["last"], Json::Null);
}

#[test]
fn an_open_loop_run_sends_at_its_rate() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 2);
    let started = admin_post(
        &host,
        "/_host/load",
        r#"{"mix":"light","rate":20,"connections":2,"seconds":1}"#,
    );
    assert_eq!(started.status, 200, "{}", started.body);
    let last = ended(&host)["last"].clone();
    // Due at 0, 50, …, 950 ms: twenty, however fast they are answered.
    assert_eq!(n(&last["sent"]), 20, "{last}");
    assert_eq!(n(&last["ok"]), 20, "{last}");
    assert_eq!(last["rate"], 20);
}
