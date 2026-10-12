//! Following an app's log (#51): an answer the host writes as it goes.
//!
//! The other tests read a whole answer; these read one that never ends, so
//! they keep the connection and wait for a line to appear on it.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use common::*;

/// 2027-01-15 08:00:00 UTC, the minute `0 * * * *` is due in.
const DUE: i64 = 1_800_000_000;

/// A connection held open while the host writes to it.
struct Following {
    stream: TcpStream,
    seen: String,
}

impl Following {
    fn open(addr: SocketAddr, request: &str) -> Following {
        let mut stream = TcpStream::connect(addr).expect("connects");
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        Following {
            stream,
            seen: String::new(),
        }
    }

    /// Reads until `needle` has been written, and answers everything read
    /// so far. Panics after a generous bound rather than hanging.
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
                Ok(0) => panic!("the host closed the connection before {needle:?}"),
                Ok(n) => self.seen.push_str(&String::from_utf8_lossy(&buffer[..n])),
                Err(_) => continue,
            }
        }
        &self.seen
    }
}

fn ticker() -> Apps {
    apps(&[AppSpec {
        name: "ticker",
        from: fixtures().join("ticker"),
        config: Some(
            "grant = [\"kv\", \"log\", \"timer\"]\n\
             [[triggers]]\ncron = \"0 * * * *\"\npath = \"/tick\"\n",
        ),
    }])
}

#[test]
fn following_a_log_sends_what_is_there_and_then_what_happens() {
    let apps = ticker();
    let host = start(&apps, 2);
    // One line to be the backlog.
    assert_eq!(host.fire_due(DUE), 1);
    wait_until("the first tick", || {
        count(&host, "ticker", "trigger_runs") == 1
    });

    let mut following = Following::open(
        host.addr,
        "GET /_host/apps/ticker/logs?follow=1&n=10 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    let head = following.until("tick 1").to_string();
    assert!(head.contains("200 OK"), "{head}");
    assert!(head.contains("content-type: text/event-stream"), "{head}");
    assert!(head.contains("data: "), "the lines are events: {head}");
    assert!(
        head.contains(": following `ticker`; the last 2 line(s) first"),
        "it says what it sent first: {head}"
    );

    // A line written while the connection is open reaches it.
    assert_eq!(host.fire_due(DUE + 3600), 1);
    let more = following.until("tick 2");
    assert!(more.contains("data: ") && more.contains("tick 2"), "{more}");
}

#[test]
fn following_an_app_that_is_not_there_is_a_404() {
    let apps = ticker();
    let host = start(&apps, 1);
    let answer = get(host.addr, "/_host/apps/nobody/logs?follow=1");
    assert_eq!(answer.status, 404);
}

#[test]
fn the_admin_listener_answers_the_lines_to_the_token_and_to_nobody_else() {
    let apps = ticker();
    let host = start(&apps, 2);
    let admin = host.admin_addr.expect("an admin listener");
    assert_eq!(host.fire_due(DUE), 1);
    wait_until("the tick", || count(&host, "ticker", "trigger_runs") == 1);

    // Without the token, nothing — neither the lines nor the following.
    for path in ["/apps/ticker/logs", "/apps/ticker/logs?follow=1"] {
        let answer = send_raw(
            admin,
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        );
        assert_eq!(answer.status, 401, "{path}");
    }

    let answer = send_raw(
        admin,
        format!(
            "GET /apps/ticker/logs?n=5 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Authorization: Bearer {ADMIN_TOKEN}\r\n\r\n"
        )
        .as_bytes(),
    );
    assert_eq!(answer.status, 200);
    assert!(answer.body.contains("tick 1"), "{}", answer.body);

    // And it follows, with the token, wherever the operations views are.
    let mut following = Following::open(
        admin,
        &format!(
            "GET /apps/ticker/logs?follow=1&n=5 HTTP/1.1\r\nHost: localhost\r\n\
             Authorization: Bearer {ADMIN_TOKEN}\r\n\r\n"
        ),
    );
    let head = following.until("tick 1").to_string();
    assert!(head.contains("text/event-stream"), "{head}");
}
