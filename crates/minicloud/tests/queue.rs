//! Queues (#55): a message that outlives the request that sent it, and the
//! host delivering it until somebody takes it.
//!
//! The delivery loop looks once a second, so these wait on the app's own
//! counters rather than on a duration.

mod common;

use common::*;

fn post(queues: &'static str) -> Apps {
    let config: &'static str = Box::leak(
        format!("grant = [\"queue\", \"kv\", \"log\", \"timer\"]\n{queues}").into_boxed_str(),
    );
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
fn a_message_sent_after_a_delay_is_not_delivered_before_it() {
    let apps = post(TWICE);
    let host = start(&apps, 2);
    let sent = get(host.addr, "/post/send-after?body=later&secs=3");
    assert_eq!(sent.status, 202, "{}", sent.body);
    assert_eq!(count(&host, "post", "queue_sent"), 1);
    // The loop looks once a second, so by now it has looked and let it be:
    // the message is waiting, and it counts as waiting.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert_eq!(count(&host, "post", "queue_delivered"), 0);
    assert_eq!(taken(&host), "");
    assert_eq!(get(host.addr, "/post/depth").body.trim(), "1");

    wait_until("the held message to be delivered", || {
        count(&host, "post", "queue_delivered") == 1
    });
    assert_eq!(taken(&host), "later;");
    assert_eq!(get(host.addr, "/post/depth").body.trim(), "0");
}

#[test]
fn a_delay_below_nothing_is_no_delay_and_one_past_a_week_is_the_apps_err() {
    let apps = post(TWICE);
    let host = start(&apps, 2);
    assert_eq!(
        get(host.addr, "/post/send-after?body=now&secs=-5").status,
        202
    );
    wait_until("the message to be delivered", || {
        count(&host, "post", "queue_delivered") == 1
    });
    assert_eq!(taken(&host), "now;");

    let far = get(host.addr, "/post/send-after?body=far&secs=604801");
    assert_eq!(far.status, 429, "{}", far.body);
    assert!(far.body.contains("604800"), "{}", far.body);
    assert_eq!(count(&host, "post", "queue_refused"), 1);
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
fn a_delivery_says_which_try_it_is_and_how_many_there_will_be() {
    let apps = post(TWICE);
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/post/send?body=count").status, 202);
    // Two attempts, a second apart, and the consumer refuses both: it sees
    // which try each one is, which is how a consumer tells the last
    // failure — the one about to become a dead letter — from the others.
    wait_until("the message to be buried", || {
        count(&host, "post", "queue_buried") == 1
    });
    assert_eq!(get(host.addr, "/post/tries").body.trim(), "1/2;2/2;");
}

/// A first delivery that takes longer than the lease is taken to be lost,
/// and the message goes to a second, which takes it at once. When the
/// first one finishes, the message is already the second's to settle: its
/// answer is said in the log and counted as nothing, so the message is
/// delivered once by the counters though the consumer was asked twice.
#[test]
fn a_delivery_that_outlives_its_lease_does_not_settle_the_message() {
    let apps = post(
        "[limits]\ndeadline = \"10s\"\n\n\
         [[queues]]\nname = \"mail\"\nlease = \"1s\"\nretry = \"3600s\"\n",
    );
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/post/send?body=slow").status, 202);
    wait_until("the first delivery to finish", || {
        get(host.addr, "/_host/apps/post/logs?n=50")
            .body
            .contains("delivery outlived its lease")
    });
    assert_eq!(taken(&host), "slow;slow;");
    assert_eq!(count(&host, "post", "queue_delivered"), 1);
    assert_eq!(count(&host, "post", "queue_retried"), 0);
    assert_eq!(count(&host, "post", "queue_buried"), 0);
    assert_eq!(get(host.addr, "/post/depth").body.trim(), "0");
}

/// A consumer that is always slower than the lease never settles its
/// message, so the message must not go round for ever: once its lease runs
/// out on the last attempt it is a dead letter, and the deliveries that
/// finish after that settle nothing.
#[test]
fn a_message_whose_every_delivery_outlives_its_lease_is_a_dead_letter() {
    let apps = post(
        "[limits]\ndeadline = \"10s\"\n\n\
         [[queues]]\nname = \"mail\"\nlease = \"1s\"\nmax_attempts = 2\nretry = \"3600s\"\n",
    );
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/post/send?body=slower").status, 202);
    let logs = || get(host.addr, "/_host/apps/post/logs?n=50").body;
    wait_until("both deliveries to finish", || {
        logs().matches("delivery outlived its lease").count() == 2
    });
    let said = logs();
    assert!(
        said.contains("is a dead letter after 2 attempt(s): no answer within the lease"),
        "{said}"
    );
    // Asked twice, never a third time, and counted as buried, not taken.
    assert_eq!(taken(&host), "slower;slower;");
    assert_eq!(count(&host, "post", "queue_buried"), 1);
    assert_eq!(count(&host, "post", "queue_delivered"), 0);
    assert_eq!(count(&host, "post", "queue_retried"), 0);
    assert_eq!(get(host.addr, "/post/depth").body.trim(), "0");
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

/// F2 end to end (`docs/queue-properties.md`, S6 and S7): a delivery whose
/// run waits for a worker past its deadline is answered 503 by the
/// scheduler and never started. The host can see that, so the attempt is
/// given back and a message on its last attempt is not buried.
///
/// One worker, no time slice, and four other apps (`crunch`, some 16 ms of
/// CPU a run natively) each with two requests always waiting: the
/// delivery, due a second or two after the send, waits its round-robin
/// turn behind runs of the others for longer than `post`'s 20 ms
/// deadline. The send itself is made before the load starts.
#[test]
#[ignore = "F2: the scheduler's QueueTimeout 503 is taken for the app's 5xx, so the last attempt buries a message the app never saw"]
fn a_delivery_that_timed_out_waiting_for_a_worker_is_given_back() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let config = "grant = [\"queue\", \"kv\", \"log\", \"timer\"]\n\
                  [limits]\ndeadline = \"20ms\"\n\n\
                  [[queues]]\nname = \"mail\"\nmax_attempts = 1\nretry = \"3600s\"\n";
    let apps = apps(&[
        AppSpec {
            name: "post",
            from: fixtures().join("post"),
            config: Some(config),
        },
        sample("crunch"),
        AppSpec {
            name: "crunch2",
            ..sample("crunch")
        },
        AppSpec {
            name: "crunch3",
            ..sample("crunch")
        },
        AppSpec {
            name: "crunch4",
            ..sample("crunch")
        },
    ]);
    let mut options = options(&apps, 1);
    options.slice = None;
    let host = minicloud::Host::start(options).expect("the host starts");
    let addr = host.addr;
    // Whether the delivery's turn comes after the 20 ms is up depends on
    // where the round robin stands when it is admitted, so a trial whose
    // delivery got a worker in time (and was taken) is made again.
    let mut trials = 0;
    while count(&host, "post", "errors.queue_timeout") == 0 {
        trials += 1;
        assert!(trials <= 20, "no delivery waited past its deadline");
        let delivered = count(&host, "post", "queue_delivered");
        assert_eq!(get(addr, "/post/send-after?body=never&secs=2").status, 202);
        let stop = Arc::new(AtomicBool::new(false));
        let load: Vec<_> = ["crunch", "crunch2", "crunch3", "crunch4"]
            .iter()
            .flat_map(|app| [app, app])
            .map(|app| {
                let stop = Arc::clone(&stop);
                let path = format!("/{app}/?n=200000");
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        get(addr, &path);
                    }
                })
            })
            .collect();
        wait_until("the delivery to time out or be taken", || {
            count(&host, "post", "errors.queue_timeout") >= 1
                || count(&host, "post", "queue_delivered") > delivered
        });
        stop.store(true, Ordering::Relaxed);
        for load in load {
            load.join().unwrap();
        }
    }
    let logs = get(addr, "/_host/apps/post/logs?n=50").body;
    assert_eq!(count(&host, "post", "queue_buried"), 0, "{logs}");
    assert_eq!(
        get(addr, "/post/depth").body.trim(),
        "1",
        "given back: {logs}"
    );
}
