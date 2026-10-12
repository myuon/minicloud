//! An answer a run writes as it goes (#51).
//!
//! These read the connection while the run is still going, so they hold it
//! open and read until a line appears, as the log-following tests do.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use common::*;

fn feed(config: Option<&'static str>) -> Apps {
    apps(&[
        AppSpec {
            name: "feed",
            from: fixtures().join("feed"),
            config,
        },
        sample("hello"),
    ])
}

/// A connection held open while the run writes to it.
struct Reading {
    stream: TcpStream,
    seen: String,
}

impl Reading {
    fn open(addr: SocketAddr, path: &str) -> Reading {
        let mut stream = TcpStream::connect(addr).expect("connects");
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .unwrap();
        Reading {
            stream,
            seen: String::new(),
        }
    }

    /// Reads until `needle` is there, and answers everything read so far.
    fn until(&mut self, needle: &str) -> &str {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut buffer = [0u8; 4096];
        while !self.seen.contains(needle) {
            assert!(
                Instant::now() < deadline,
                "gave up waiting for {needle:?} in:\n{}",
                self.seen
            );
            match self.stream.read(&mut buffer) {
                Ok(0) => panic!("the connection closed before {needle:?}:\n{}", self.seen),
                Ok(n) => self.seen.push_str(&String::from_utf8_lossy(&buffer[..n])),
                Err(_) => continue,
            }
        }
        &self.seen
    }
}

#[test]
fn the_head_goes_out_before_the_run_ends_and_each_chunk_as_it_is_sent() {
    let apps = feed(None);
    let host = start(&apps, 2);
    // 300 ms between lines: the first is readable long before the last.
    let mut reading = Reading::open(host.addr, "/feed/events?n=3&ms=300");
    let head = reading.until("data: line 0").to_string();
    assert!(head.contains("200 OK"), "{head}");
    assert!(head.contains("content-type: text/event-stream"), "{head}");
    assert!(
        head.contains("transfer-encoding: chunked"),
        "the body is written as it goes: {head}"
    );
    // The run has not ended, so what it cost is not known and not sent.
    assert!(!head.contains("x-cove-run-wall-us"), "{head}");

    reading.until("data: line 2");
    // What the handler returned afterwards is not the answer: the 599 it
    // ends with is nowhere on the connection.
    assert!(!reading.seen.contains("599"), "{}", reading.seen);
    assert!(
        !reading.seen.contains("this is not the answer"),
        "{}",
        reading.seen
    );
    // The last chunk being readable is not the run having ended: it ends
    // once it has returned, which is when it is counted.
    wait_until("the run to end", || count(&host, "feed", "served") == 1);
    assert_eq!(count(&host, "feed", "ok"), 1);
}

#[test]
fn a_run_that_parks_on_a_chunk_holds_no_worker() {
    // One worker, and a stream that waits between its lines: another app
    // answers throughout, which it could not if the stream held the worker.
    let apps = feed(None);
    let host = start(&apps, 1);
    let mut reading = Reading::open(host.addr, "/feed/events?n=4&ms=200");
    reading.until("data: line 0");
    for _ in 0..3 {
        assert_eq!(get(host.addr, "/hello/").status, 200);
    }
    reading.until("data: line 3");
}

#[test]
fn a_chunk_without_an_open_and_a_second_open_are_the_apps_err() {
    let apps = feed(None);
    let host = start(&apps, 2);
    let late = get(host.addr, "/feed/late");
    assert_eq!(late.status, 400);
    assert!(
        late.body.contains("has not opened its answer"),
        "{}",
        late.body
    );

    let mut twice = Reading::open(host.addr, "/feed/twice");
    let said = twice.until("second open").to_string();
    assert!(said.contains("opened its answer already"), "{said}");
}

#[test]
fn a_chunk_past_max_response_bytes_is_refused_and_the_rest_goes_on() {
    let apps = feed(Some(
        "grant = [\"stream\", \"timer\", \"log\"]\n\
         [limits]\ndeadline = \"20s\"\nmax_response_bytes = 4096\n",
    ));
    let host = start(&apps, 2);
    let mut reading = Reading::open(host.addr, "/feed/huge");
    let said = reading.until("refused:").to_string();
    assert!(said.contains("max_response_bytes of 4096"), "{said}");
}

#[test]
fn a_client_that_goes_away_is_told_to_the_run() {
    let apps = feed(None);
    let host = start(&apps, 2);
    // Many lines, slowly; the client reads one and leaves.
    let mut reading = Reading::open(host.addr, "/feed/events?n=1000&ms=50");
    reading.until("data: line 0");
    drop(reading);
    // The run finds out at a chunk the client is not taking, says so and
    // ends; the app is not left with a run that never finishes.
    wait_until("the run to end", || count(&host, "feed", "served") == 1);
    let logs = get(host.addr, "/_host/apps/feed/logs?n=20").body;
    assert!(logs.contains("stopped at"), "the run was told: {logs}");
    assert_eq!(get(host.addr, "/hello/").status, 200);
}

#[test]
fn an_app_not_granted_stream_is_refused_at_load() {
    let apps = feed(Some("grant = [\"timer\", \"log\"]\n"));
    let host = start(&apps, 1);
    let answer = get(host.addr, "/feed/late");
    assert_eq!(answer.status, 503);
    assert!(answer.body.contains("requires `stream`"), "{}", answer.body);
    assert_eq!(get(host.addr, "/hello/").status, 200);
}
