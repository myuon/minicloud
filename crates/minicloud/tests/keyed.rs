//! `[keyed]` (#55): one run at a time for one key, and an alarm per key.

mod common;

use std::sync::mpsc;
use std::time::Duration;

use common::*;

fn rooms(config: Option<&'static str>) -> Apps {
    apps(&[
        AppSpec {
            name: "room",
            from: fixtures().join("room"),
            config,
        },
        sample("hello"),
    ])
}

/// `n` requests to `path`, at once, answered.
fn all_at_once(addr: std::net::SocketAddr, path: &'static str, n: usize) -> Vec<Answer> {
    let (done, answers) = mpsc::channel();
    for _ in 0..n {
        let done = done.clone();
        std::thread::spawn(move || {
            let _ = done.send(get(addr, path));
        });
    }
    drop(done);
    answers.into_iter().collect()
}

#[test]
fn one_key_is_one_run_at_a_time_so_a_read_and_a_write_do_not_interleave() {
    let apps = rooms(None);
    let host = start(&apps, 4);
    // Five at once, each reading, waiting 30 ms, and writing back. Without
    // a turn of its own every one of them would read 0 and write 1.
    let answers = all_at_once(host.addr, "/room/bump?room=a", 5);
    assert_eq!(answers.len(), 5);
    assert!(answers.iter().all(|answer| answer.status == 200));
    assert_eq!(
        get(host.addr, "/room/count?room=a").body.trim(),
        "5",
        "every one of the five saw the one before it"
    );
}

#[test]
fn another_key_is_not_waiting_for_this_one() {
    let apps = rooms(None);
    let host = start(&apps, 4);
    // Four about one room and four about another: if a key's turn stopped
    // every room, the second room's would come after the first's.
    let (done, answers) = mpsc::channel();
    for room in ["a", "b"] {
        for _ in 0..4 {
            let (done, addr) = (done.clone(), host.addr);
            std::thread::spawn(move || {
                let _ = done.send(get(addr, &format!("/room/bump?room={room}")).status);
            });
        }
    }
    drop(done);
    let answers: Vec<u16> = answers.into_iter().collect();
    assert_eq!(answers.len(), 8);
    assert!(answers.iter().all(|status| *status == 200));
    assert_eq!(get(host.addr, "/room/count?room=a").body.trim(), "4");
    assert_eq!(get(host.addr, "/room/count?room=b").body.trim(), "4");
}

#[test]
fn a_request_with_no_key_is_not_keyed() {
    let apps = rooms(None);
    let host = start(&apps, 4);
    // No `?room=`, so nothing to wait for: these run together, and the
    // interleaving they were written to show is what they show.
    let answers = all_at_once(host.addr, "/room/bump", 4);
    assert!(answers.iter().all(|answer| answer.status == 200));
    let count: i64 = get(host.addr, "/room/count").body.trim().parse().unwrap();
    assert!(
        (1..=4).contains(&count),
        "an unkeyed request has no turn: {count}"
    );
}

#[test]
fn more_waiting_for_one_key_than_max_waiting_is_429() {
    let apps = rooms(Some(
        "grant = [\"keyed\", \"kv\", \"log\", \"time\", \"timer\"]\n\
         [limits]\ndeadline = \"20s\"\n\
         [keyed]\nby = \"query:room\"\nmax_waiting = 2\n",
    ));
    let host = start(&apps, 4);
    let answers = all_at_once(host.addr, "/room/bump?room=a", 6);
    let refused = answers.iter().filter(|a| a.status == 429).count();
    assert!(refused > 0, "some are turned away rather than queued");
    assert!(
        answers.iter().any(|a| a.status == 200),
        "and some are served"
    );
    let turned_away = answers.iter().find(|a| a.status == 429).unwrap();
    assert!(
        turned_away.body.contains("waiting for this key"),
        "{}",
        turned_away.body
    );
    assert_eq!(turned_away.header("retry-after"), Some("1"));
    // Another app is untouched by one key's crowd.
    assert_eq!(get(host.addr, "/hello/").status, 200);
}

#[test]
fn an_alarm_wakes_the_app_once_at_its_key() {
    let apps = rooms(None);
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/room/set-alarm?room=a&in=50").status, 202);
    wait_until("the alarm to go off", || {
        count(&host, "room", "alarms_run") == 1
    });
    assert_eq!(get(host.addr, "/room/woke?room=a").body.trim(), "1");
    assert_eq!(count(&host, "room", "alarms_failed"), 0);
    // It goes off once: an app that wants another sets another.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(count(&host, "room", "alarms_run"), 1);
    assert_eq!(get(host.addr, "/room/woke?room=a").body.trim(), "1");
    // And the app can see that it has none now.
    assert_eq!(get(host.addr, "/room/alarm-at?room=a").body.trim(), "-1");
}

#[test]
fn an_alarm_is_one_per_key_and_can_be_taken_back() {
    let apps = rooms(None);
    let host = start(&apps, 2);
    // Far enough off that it will not go off during the test.
    assert_eq!(
        get(host.addr, "/room/set-alarm?room=a&in=60000").status,
        202
    );
    let first: i64 = get(host.addr, "/room/alarm-at?room=a")
        .body
        .trim()
        .parse()
        .unwrap();
    assert!(first > 0);
    // Setting it again moves it rather than adding one.
    assert_eq!(
        get(host.addr, "/room/set-alarm?room=a&in=90000").status,
        202
    );
    let second: i64 = get(host.addr, "/room/alarm-at?room=a")
        .body
        .trim()
        .parse()
        .unwrap();
    assert!(second > first, "{first} then {second}");
    // Another key has its own, and clearing says whether there was one.
    assert_eq!(get(host.addr, "/room/alarm-at?room=b").body.trim(), "-1");
    assert_eq!(
        get(host.addr, "/room/clear-alarm?room=a").body.trim(),
        "true"
    );
    assert_eq!(
        get(host.addr, "/room/clear-alarm?room=a").body.trim(),
        "false"
    );
    assert_eq!(count(&host, "room", "alarms_run"), 0);
}

#[test]
fn only_the_host_rings_an_alarm() {
    let apps = rooms(None);
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/room/alarm?room=a").status, 403);
    assert_eq!(get(host.addr, "/room/woke?room=a").body.trim(), "0");
}

#[test]
fn an_alarm_survives_a_restart() {
    let apps = rooms(None);
    {
        let host = start(&apps, 1);
        assert_eq!(
            get(host.addr, "/room/set-alarm?room=a&in=60000").status,
            202
        );
    }
    let host = start(&apps, 1);
    assert!(
        get(host.addr, "/room/alarm-at?room=a")
            .body
            .trim()
            .parse::<i64>()
            .is_ok_and(|at| at > 0),
        "the alarm is where the host left it"
    );
}

#[test]
fn a_keyed_section_that_cannot_be_read_refuses_that_app_alone() {
    for bad in [
        "[keyed]\nmax_waiting = 4\n",
        "[keyed]\nby = \"whatever\"\n",
        "[keyed]\nby = \"query:room\"\nmax_waiting = 0\n",
        "[keyed]\nby = \"query:room\"\nalarm_path = \"alarm\"\n",
    ] {
        let config: &'static str = Box::leak(
            format!("grant = [\"keyed\", \"kv\", \"log\", \"time\", \"timer\"]\n{bad}")
                .into_boxed_str(),
        );
        let apps = rooms(Some(config));
        let host = start(&apps, 1);
        assert_eq!(get(host.addr, "/room/count?room=a").status, 503, "{bad}");
        assert_eq!(get(host.addr, "/hello/").status, 200, "{bad}");
    }
}
