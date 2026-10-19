//! The request timeline (`crate::timeline`): what `/_host/timeline`,
//! `/_host/timeline/<id>` and `/_host/live` say of requests that parked,
//! yielded, were cancelled and were turned away.
//!
//! As everywhere in these tests, nothing asserts a duration: what is
//! asserted is the shape — which phases, in which order, on which worker,
//! ended how — and a test waits for the host's own views to say a request
//! is where it wants it.

mod common;

use std::io::Write;
use std::net::{SocketAddr, TcpStream};

use common::*;
use serde_json::Value as Json;

fn json(host: &minicloud::Host, path: &str) -> Json {
    let answer = get(host.addr, path);
    assert_eq!(answer.status, 200, "{path}: {}", answer.body);
    serde_json::from_str(&answer.body).expect("JSON")
}

/// The requests of `app` in the last five minutes, oldest first.
fn requests(host: &minicloud::Host, app: &str) -> Vec<Json> {
    json(host, &format!("/_host/timeline?app={app}&over=5m"))["requests"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// A request's phases as their kinds, waits left out: a wait of no length
/// is not recorded, so whether there is one is the scheduler's luck.
fn steps(request: &Json) -> Vec<String> {
    request["phases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|phase| phase["kind"].as_str().unwrap().to_string())
        .filter(|kind| kind != "wait")
        .collect()
}

/// The phases follow each other without a gap, and each is in order.
fn contiguous(request: &Json) {
    let phases = request["phases"].as_array().unwrap();
    for phase in phases {
        assert!(
            phase["from_us"].as_u64() <= phase["to_us"].as_u64(),
            "{phase}"
        );
    }
    for pair in phases.windows(2) {
        assert_eq!(pair[0]["to_us"], pair[1]["from_us"], "{request}");
    }
}

/// A `GET path` whose answer is never read: dropping the stream is the
/// client going away.
fn open(addr: SocketAddr, path: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    stream
}

#[test]
fn a_request_that_sleeps_runs_parks_and_resumes() {
    let apps = apps(&[sample("hello"), sample("slow")]);
    let host = start(&apps, 2);
    let answer = get(host.addr, "/slow/?ms=20&times=2");
    assert_eq!(answer.status, 200, "{}", answer.body);

    let slow = requests(&host, "slow");
    assert_eq!(slow.len(), 1, "{slow:?}");
    let request = &slow[0];
    // The path is kept, and the query string is not.
    assert_eq!(request["path"], "/slow/");
    assert_eq!(request["method"], "GET");
    assert_eq!(request["app"], "slow");
    assert_eq!(request["outcome"], "ok");
    assert_eq!(request["status"], 200);
    assert_eq!(request["reason"], Json::Null);
    assert_eq!(request["parks"], 2);
    assert_eq!(request["yields"], 0);
    assert!(request["ended_us"].is_u64() && request["latency_us"].is_u64());
    assert!(request["instructions"].as_u64().is_some_and(|n| n > 0));
    assert_eq!(steps(request), ["run", "park", "run", "park", "run"]);
    contiguous(request);
    let phases = request["phases"].as_array().unwrap();
    assert_eq!(phases[0]["kind"], "wait");
    assert_eq!(phases[0]["after"], "arrival");
    let parks: Vec<_> = phases.iter().filter(|p| p["kind"] == "park").collect();
    for park in &parks {
        assert_eq!(park["op"], "timer.sleep");
        assert_eq!(park["by"], "answer");
    }
    let runs: Vec<_> = phases.iter().filter(|p| p["kind"] == "run").collect();
    assert_eq!(runs[0]["began"], "start");
    assert_eq!(runs[1]["began"], "resume");
    assert_eq!(runs[2]["began"], "resume");
    let moves = runs
        .windows(2)
        .filter(|pair| pair[0]["worker"] != pair[1]["worker"])
        .count();
    assert_eq!(
        request["moves"], moves,
        "a move is a resume on another worker"
    );

    // Its detail is the same request.
    let id = request["id"].as_u64().unwrap();
    let detail = json(&host, &format!("/_host/timeline/{id}"));
    assert_eq!(detail["id"], id);
    assert_eq!(detail["phases"], request["phases"]);
    assert_eq!(get(host.addr, "/_host/timeline/123456789").status, 404);
    assert_eq!(get(host.addr, "/_host/timeline/nope").status, 404);

    // The window: its three runs on the workers, the strips, the summary.
    let window = json(&host, "/_host/timeline?app=slow&over=5m");
    let segments = window["segments"].as_array().unwrap();
    assert_eq!(
        segments.iter().filter(|s| s["id"] == id).count(),
        3,
        "{segments:?}"
    );
    assert_eq!(window["workers"], 2);
    for strip in ["running_milli", "parked_milli", "waiting_milli"] {
        assert_eq!(
            window["strips"][strip].as_array().map(Vec::len),
            Some(200),
            "{strip}"
        );
    }
    let apps = window["summary"]["apps"].as_array().unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0]["app"], "slow");
    assert_eq!(apps[0]["requests"], 1);
    assert_eq!(window["keep"]["secs"], 300);
    assert_eq!(window["keep"]["requests"], 20000);
    // The views asked above are the host's, not an app's: not recorded.
    assert!(requests(&host, "hello").is_empty());
}

#[test]
fn a_run_that_yields_continues_where_a_worker_is_free() {
    // One worker and two spinners: the one running is asked to yield every
    // slice, since the other waits.
    let apps = apps(&[sample_with("hello", "[limits]\ndeadline = \"120s\"\n")]);
    let host = start(&apps, 1);
    let first = open(host.addr, "/hello/spin");
    let second = open(host.addr, "/hello/spin");
    wait_until("a spinner to have yielded", || {
        requests(&host, "hello")
            .iter()
            .any(|r| r["yields"].as_u64() > Some(0))
    });
    let live = json(&host, "/_host/live");
    assert_eq!(live["total"], 2, "{live}");
    let workers = live["workers"].as_array().unwrap();
    assert_eq!(workers.len(), 1);
    let states: Vec<_> = live["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["state"].as_str().unwrap().to_string())
        .collect();
    assert!(
        states
            .iter()
            .all(|s| ["waiting", "running", "yielded"].contains(&s.as_str())),
        "{states:?}"
    );
    drop(first);
    drop(second);
    wait_until("both spinners to be cancelled", || {
        count(&host, "hello", "errors.cancelled") == 2
    });

    let hello = requests(&host, "hello");
    assert_eq!(hello.len(), 2);
    let yielded = hello
        .iter()
        .find(|r| r["yields"].as_u64() > Some(0))
        .unwrap();
    contiguous(yielded);
    let phases = yielded["phases"].as_array().unwrap();
    let after_yield = phases
        .iter()
        .position(|p| p["kind"] == "wait" && p["after"] == "yield")
        .expect("a wait after a yield");
    assert_eq!(phases[after_yield + 1]["kind"], "run");
    assert_eq!(phases[after_yield + 1]["began"], "continue");
    // One worker: nothing moves.
    assert!(phases.iter().all(|p| p["moved"] == false));
    assert_eq!(yielded["moves"], 0);
    for request in &hello {
        assert_eq!(request["outcome"], "error");
        assert_eq!(request["reason"], "cancelled");
        assert_eq!(request["status"], 499);
        assert_eq!(request["path"], "/hello/spin");
    }
    assert_eq!(json(&host, "/_host/live")["total"], 0);
}

#[test]
fn live_shows_a_parked_request_and_its_cancel_ends_the_park() {
    let apps = apps(&[sample("hello"), sample("slow")]);
    let host = start(&apps, 2);
    let parked = open(host.addr, "/slow/?ms=60000&secret=1");
    wait_until("the slow run to park", || {
        json(&host, "/_host/live")["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["state"] == "parked")
    });
    let live = json(&host, "/_host/live");
    let row = &live["requests"][0];
    assert_eq!(row["app"], "slow");
    assert_eq!(row["path"], "/slow/", "no query string");
    assert_eq!(row["op"], "timer.sleep");
    assert!(row["worker"].is_u64(), "the worker it parked from");
    assert!(row["since_us"].as_u64() >= row["arrived_us"].as_u64());
    // It holds no worker.
    for worker in live["workers"].as_array().unwrap() {
        assert!(worker["request"].is_null() || worker["request"]["app"] != "slow");
    }
    // In the timeline, it is in flight, its park still open.
    let slow = requests(&host, "slow");
    assert_eq!(slow[0]["outcome"], "in_flight");
    assert_eq!(slow[0]["status"], Json::Null);
    let last = slow[0]["phases"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["kind"], "park");
    assert_eq!(last["open"], true);

    drop(parked);
    wait_until("the parked run to be cancelled", || {
        count(&host, "slow", "errors.cancelled") == 1
    });
    let slow = requests(&host, "slow");
    let request = &slow[0];
    assert_eq!(request["reason"], "cancelled");
    assert_eq!(request["status"], 499);
    let park = request["phases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["kind"] == "park")
        .unwrap()
        .clone();
    assert_eq!(park["by"], "cancel");
    assert_eq!(park["open"], false);
    assert!(json(&host, "/_host/live")["requests"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn a_request_turned_away_is_recorded_with_why() {
    let config = "grant = []\n[rate]\nrequests = 1\nper = \"1m\"\n";
    let apps = apps(&[sample_with("hello", config)]);
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/hello/").status, 200);
    assert_eq!(get(host.addr, "/hello/?again=1").status, 429);
    let hello = requests(&host, "hello");
    assert_eq!(hello.len(), 2);
    assert_eq!(hello[0]["outcome"], "ok");
    assert_eq!(hello[1]["outcome"], "rejected");
    assert_eq!(hello[1]["reason"], "rate_limited");
    assert_eq!(hello[1]["status"], 429);
    assert_eq!(hello[1]["path"], "/hello/");
    assert_eq!(hello[1]["phases"], serde_json::json!([]));
    let summary = json(&host, "/_host/timeline?over=5m")["summary"].clone();
    assert_eq!(summary["rejected"], 1);
    assert_eq!(summary["apps"][0]["rejected"], 1);
}
