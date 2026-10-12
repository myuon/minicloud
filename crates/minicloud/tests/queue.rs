//! Queues (#55): a message that outlives the request that sent it, and the
//! host delivering it until somebody takes it.
//!
//! The delivery loop looks once a second, so these wait on the app's own
//! counters rather than on a duration.

mod common;

use common::*;

fn post(queues: &'static str) -> Apps {
    let config: &'static str =
        Box::leak(format!("grant = [\"queue\", \"kv\", \"log\"]\n{queues}").into_boxed_str());
    apps(&[
        AppSpec {
            name: "post",
            from: fixtures().join("post"),
            config: Some(config),
        },
        sample("hello"),
    ])
}

/// One queue, tried twice, retried after a second.
const TWICE: &str = "[[queues]]\nname = \"mail\"\nmax_attempts = 2\nretry = \"1s\"\n";

fn taken(host: &minicloud::Host) -> String {
    get(host.addr, "/post/taken").body.trim().to_string()
}

#[test]
fn a_message_outlives_the_request_that_sent_it_and_is_delivered() {
    let apps = post(TWICE);
    let host = start(&apps, 2);
    let sent = get(host.addr, "/post/send?body=one");
    assert_eq!(sent.status, 202, "{}", sent.body);
    assert!(
        sent.body.trim().parse::<i64>().is_ok(),
        "the message's id: {}",
        sent.body
    );
    assert_eq!(count(&host, "post", "queue_sent"), 1);

    wait_until("the message to be delivered", || {
        count(&host, "post", "queue_delivered") == 1
    });
    assert_eq!(taken(&host), "one;");
    assert_eq!(count(&host, "post", "queue_retried"), 0);
    assert_eq!(count(&host, "post", "queue_buried"), 0);
    // It is on disk: the store is the app's own file.
    assert!(apps.data.join("post/queue.sqlite3").is_file());
    // And it is off the queue once it is taken.
    assert_eq!(get(host.addr, "/post/depth").body.trim(), "0");
}

#[test]
fn only_the_host_delivers() {
    let apps = post(TWICE);
    let host = start(&apps, 2);
    let claimed = send_raw(
        host.addr,
        b"POST /post/queue/mail HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
          X-Minicloud-Queue: mail\r\nContent-Length: 4\r\n\r\nsham"
            .as_slice(),
    );
    assert_eq!(claimed.status, 403, "{}", claimed.body);
    assert_eq!(taken(&host), "");
}

#[test]
fn a_delivery_that_is_refused_is_tried_again_and_then_buried() {
    let apps = post(TWICE);
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/post/send?body=fail").status, 202);

    wait_until("the retry", || count(&host, "post", "queue_retried") >= 1);
    wait_until("the dead letter", || {
        count(&host, "post", "queue_buried") == 1
    });
    assert_eq!(count(&host, "post", "queue_delivered"), 0);
    // The reason is in the app's log, with the message's id.
    let logs = get(host.addr, "/_host/apps/post/logs?n=50").body;
    assert!(logs.contains("not taken (500"), "{logs}");
    assert!(
        logs.contains("is a dead letter after 2 attempt(s)"),
        "{logs}"
    );
    // It is off the queue: a dead letter is kept, not retried for ever.
    assert_eq!(get(host.addr, "/post/depth").body.trim(), "0");
    // And the app that refused it is still serving, as is its neighbour.
    assert_eq!(get(host.addr, "/hello/").status, 200);
}

#[test]
fn a_queue_the_app_does_not_declare_is_the_apps_err() {
    let apps = post(TWICE);
    let host = start(&apps, 2);
    let refused = get(host.addr, "/post/send-nowhere");
    assert_eq!(refused.status, 400);
    assert!(refused.body.contains("`nowhere`"), "{}", refused.body);
    assert!(
        refused.body.contains("`mail`"),
        "it says which it has: {}",
        refused.body
    );
}

#[test]
fn a_queue_past_its_depth_refuses_the_send() {
    let apps = post("[[queues]]\nname = \"mail\"\nmax_depth = 2\nretry = \"3600s\"\n");
    let host = start(&apps, 1);
    // The consumer refuses them, so they stay on the queue and fill it.
    for _ in 0..2 {
        assert_eq!(get(host.addr, "/post/send?body=fail").status, 202);
    }
    let full = get(host.addr, "/post/send?body=fail");
    assert_eq!(full.status, 429, "{}", full.body);
    assert!(full.body.contains("max_depth"), "{}", full.body);
    assert_eq!(count(&host, "post", "queue_refused"), 1);
}

#[test]
fn the_messages_survive_a_restart() {
    let apps = post("[[queues]]\nname = \"mail\"\nretry = \"3600s\"\n");
    {
        let host = start(&apps, 1);
        // Refused, so it stays on the queue, and the retry is an hour off.
        assert_eq!(get(host.addr, "/post/send?body=fail").status, 202);
        wait_until("the first attempt", || {
            count(&host, "post", "queue_retried") == 1
        });
    }
    let host = start(&apps, 1);
    assert_eq!(
        get(host.addr, "/post/depth").body.trim(),
        "1",
        "the message is where the host left it"
    );
}

#[test]
fn a_queue_that_cannot_be_read_refuses_that_app_alone() {
    for bad in [
        "[[queues]]\nname = \"Mail\"\n",
        "[[queues]]\nname = \"mail\"\nmax_attempts = 0\n",
        "[[queues]]\nname = \"mail\"\nretry = \"soon\"\n",
        "[[queues]]\nname = \"mail\"\npath = \"queue\"\n",
        "[[queues]]\nname = \"mail\"\n[[queues]]\nname = \"mail\"\n",
    ] {
        let apps = post(bad);
        let host = start(&apps, 1);
        assert_eq!(get(host.addr, "/post/depth").status, 503, "{bad}");
        assert_eq!(get(host.addr, "/hello/").status, 200, "{bad}");
    }
}
