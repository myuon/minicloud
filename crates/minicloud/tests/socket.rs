//! A WebSocket a run holds, both ways (#51).

mod common;

use common::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

fn echo(config: Option<&'static str>) -> Apps {
    apps(&[
        AppSpec {
            name: "echo",
            from: fixtures().join("echo"),
            config,
        },
        sample("hello"),
    ])
}

/// Runs `work` on a runtime of the test's own.
fn block<T>(work: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(work)
}

#[test]
fn a_run_holds_the_socket_and_both_ends_speak() {
    let apps = echo(None);
    let host = start(&apps, 2);
    let url = format!("ws://127.0.0.1:{}/echo/ws", host.addr.port());
    block(async {
        let (mut socket, answer) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("the upgrade");
        assert_eq!(answer.status(), 101);

        // The run speaks first.
        assert_eq!(
            socket.next().await.unwrap().unwrap(),
            Message::Text("hello".to_string())
        );
        // And answers what it hears, more than once.
        for said in ["one", "two", "three"] {
            socket
                .send(Message::Text(said.to_string()))
                .await
                .expect("sent");
            assert_eq!(
                socket.next().await.unwrap().unwrap(),
                Message::Text(format!("you said {said}"))
            );
        }
        // A ping is the host's to answer, not the app's.
        socket
            .send(Message::Ping(vec![1, 2, 3]))
            .await
            .expect("sent");
        socket
            .send(Message::Text("after the ping".to_string()))
            .await
            .expect("sent");
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Pong(payload) => assert_eq!(payload, vec![1, 2, 3]),
                Message::Text(text) => {
                    assert_eq!(text, "you said after the ping");
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }

        // And the run ends the socket when it decides to.
        socket
            .send(Message::Text("bye".to_string()))
            .await
            .expect("sent");
        assert_eq!(
            socket.next().await.unwrap().unwrap(),
            Message::Text("goodbye".to_string())
        );
    });
    wait_until("the run to end", || count(&host, "echo", "served") == 1);
    // Its neighbour was never waiting for any of it.
    assert_eq!(get(host.addr, "/hello/").status, 200);
}

#[test]
fn a_socket_a_run_is_holding_costs_no_worker() {
    // One worker, and a socket held open with nothing being said on it:
    // another app answers throughout, which it could not if the run were
    // holding the worker.
    let apps = echo(None);
    let host = start(&apps, 1);
    let url = format!("ws://127.0.0.1:{}/echo/ws", host.addr.port());
    block(async {
        let (mut socket, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("the upgrade");
        assert_eq!(
            socket.next().await.unwrap().unwrap(),
            Message::Text("hello".to_string())
        );
        for _ in 0..3 {
            let addr = host.addr;
            let answered = tokio::task::spawn_blocking(move || get(addr, "/hello/").status).await;
            assert_eq!(answered.unwrap(), 200);
        }
        socket
            .send(Message::Text("still here".to_string()))
            .await
            .expect("sent");
        assert_eq!(
            socket.next().await.unwrap().unwrap(),
            Message::Text("you said still here".to_string())
        );
    });
}

#[test]
fn a_client_that_goes_away_is_told_to_the_run() {
    let apps = echo(None);
    let host = start(&apps, 2);
    let url = format!("ws://127.0.0.1:{}/echo/ws", host.addr.port());
    block(async {
        let (mut socket, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("the upgrade");
        assert_eq!(
            socket.next().await.unwrap().unwrap(),
            Message::Text("hello".to_string())
        );
        drop(socket);
    });
    wait_until("the run to end", || count(&host, "echo", "served") == 1);
    let logs = get(host.addr, "/_host/apps/echo/logs?n=20").body;
    assert!(logs.contains("the client went"), "{logs}");
}

#[test]
fn an_app_may_refuse_the_upgrade_like_any_other_request() {
    let apps = echo(None);
    let host = start(&apps, 2);
    // The request reaches the app as a request: it sees the headers and
    // may answer an ordinary response instead of taking the socket.
    let url = format!("ws://127.0.0.1:{}/echo/refuse", host.addr.port());
    block(async {
        let refused = tokio_tungstenite::connect_async(&url).await;
        assert!(refused.is_err(), "the upgrade was not taken");
    });
    assert_eq!(get(host.addr, "/echo/refuse").status, 403);
}

#[test]
fn a_call_on_a_run_that_has_no_socket_is_the_apps_err() {
    let apps = echo(None);
    let host = start(&apps, 2);
    let answer = get(host.addr, "/echo/plain");
    assert_eq!(answer.status, 400);
    assert!(
        answer.body.contains("has not accepted a socket"),
        "{}",
        answer.body
    );
}

#[test]
fn an_app_not_granted_socket_is_refused_at_load() {
    let apps = echo(Some("grant = [\"log\"]\n"));
    let host = start(&apps, 1);
    let answer = get(host.addr, "/echo/plain");
    assert_eq!(answer.status, 503);
    assert!(answer.body.contains("requires `socket`"), "{}", answer.body);
    assert_eq!(get(host.addr, "/hello/").status, 200);
}
