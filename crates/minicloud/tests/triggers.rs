//! `[[triggers]]` (#56): the host running an app on a schedule.
//!
//! The tests drive the clock rather than wait for it: `Host::fire_due` is
//! what the host's own timer calls a moment after each minute begins, and
//! it takes the time, so a schedule's behaviour is tested in milliseconds
//! and without a sleep.

mod common;

use common::*;

/// 2027-01-15 08:00:00 UTC: minute 0 of an hour, so `0 * * * *` and
/// `*/5 * * * *` are both due in it.
const DUE: i64 = 1_800_000_000;
/// The minute after it, which neither names.
const NOT_DUE: i64 = DUE + 60;
/// The next hour, which both name again.
const AGAIN: i64 = DUE + 3600;

fn ticker(triggers: &'static str) -> Apps {
    let config: &'static str = Box::leak(
        format!("grant = [\"kv\", \"log\", \"timer\"]\n[limits]\ndeadline = \"10s\"\n{triggers}")
            .into_boxed_str(),
    );
    apps(&[
        AppSpec {
            name: "ticker",
            from: fixtures().join("ticker"),
            config: Some(config),
        },
        sample("hello"),
    ])
}

fn count(host: &minicloud::Host) -> String {
    get(host.addr, "/ticker/count").body.trim().to_string()
}

#[test]
fn a_schedule_runs_the_app_in_the_minute_it_names() {
    let apps = ticker("[[triggers]]\ncron = \"0 * * * *\"\npath = \"/tick\"\n");
    let host = start(&apps, 2);
    assert_eq!(host.fire_due(DUE), 1, "the schedule is due in that minute");
    wait_until("the tick to be counted", || {
        common::count(&host, "ticker", "trigger_runs") == 1
    });
    // The app ran and wrote to its store, which is the point.
    assert_eq!(count(&host), "1");
    // It is an ordinary run of the app: it and the `/count` just asked are
    // the two the app has served, both answered by the app itself.
    assert_eq!(common::count(&host, "ticker", "served"), 2);
    assert_eq!(common::count(&host, "ticker", "ok"), 2);
    assert_eq!(common::count(&host, "ticker", "trigger_failed"), 0);
    assert_eq!(common::count(&host, "ticker", "trigger_skipped"), 0);
    // `/count` is one request of its own, and the app that has no schedule
    // ran nothing.
    assert_eq!(common::count(&host, "hello", "served"), 0);
}

#[test]
fn a_minute_the_schedule_does_not_name_runs_nothing_and_a_minute_fires_once() {
    let apps = ticker("[[triggers]]\ncron = \"0 * * * *\"\npath = \"/tick\"\n");
    let host = start(&apps, 2);
    assert_eq!(host.fire_due(NOT_DUE), 0);
    assert_eq!(host.fire_due(DUE), 1);
    // The same minute again — a timer that wakes twice — fires nothing.
    assert_eq!(host.fire_due(DUE + 1), 0);
    assert_eq!(host.fire_due(DUE + 59), 0);
    wait_until("the first tick", || count(&host) == "1");
    assert_eq!(host.fire_due(AGAIN), 1);
    wait_until("the second tick", || count(&host) == "2");
    assert_eq!(common::count(&host, "ticker", "trigger_runs"), 2);
}

#[test]
fn only_the_host_can_claim_to_be_the_clock() {
    let apps = ticker("[[triggers]]\ncron = \"0 * * * *\"\npath = \"/tick\"\n");
    let host = start(&apps, 2);
    // The app answers `/tick` to the host alone, and a client that sends
    // the header itself is still not the host: the header is stripped from
    // everything that comes off a connection.
    let claimed = send_raw(
        host.addr,
        b"GET /ticker/tick HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
          X-Minicloud-Trigger: /tick\r\n\r\n"
            .as_slice(),
    );
    assert_eq!(claimed.status, 403, "{}", claimed.body);
    assert_eq!(count(&host), "0");
}

#[test]
fn a_tick_whose_run_has_not_finished_is_let_go_by() {
    let apps = ticker("[[triggers]]\ncron = \"0 * * * *\"\npath = \"/slow\"\n");
    let host = start(&apps, 2);
    assert_eq!(host.fire_due(DUE), 1);
    // The run sleeps, so the next hour's tick finds it still going: it is
    // let go by rather than run beside it.
    assert_eq!(host.fire_due(AGAIN), 0);
    wait_until("the slow tick to finish", || count(&host) == "1");
    assert_eq!(common::count(&host, "ticker", "trigger_skipped"), 1);
    assert_eq!(common::count(&host, "ticker", "trigger_runs"), 1);
    // Once it has finished, the schedule runs again.
    assert_eq!(host.fire_due(AGAIN + 3600), 1);
    wait_until("the tick after it", || count(&host) == "2");
}

#[test]
fn a_tick_that_fails_is_counted_and_said_in_the_apps_log() {
    let apps = ticker("[[triggers]]\ncron = \"0 * * * *\"\npath = \"/boom\"\n");
    let host = start(&apps, 2);
    assert_eq!(host.fire_due(DUE), 1);
    wait_until("the failure to be counted", || {
        common::count(&host, "ticker", "trigger_failed") == 1
    });
    assert_eq!(common::count(&host, "ticker", "trigger_runs"), 1);
    let logs = get(host.addr, "/_host/apps/ticker/logs?n=50").body;
    assert!(
        logs.contains("trigger POST /boom: 500"),
        "the log says which schedule failed and how: {logs}"
    );
}

#[test]
fn a_schedule_that_does_not_parse_refuses_that_app_alone() {
    let apps = ticker("[[triggers]]\ncron = \"every thursday\"\npath = \"/tick\"\n");
    let host = start(&apps, 2);
    let answer = get(host.addr, "/ticker/count");
    assert_eq!(answer.status, 503);
    assert!(
        answer.body.contains("[[triggers]] cron"),
        "the reason names the key: {}",
        answer.body
    );
    assert_eq!(app_stats(&host, "ticker")["state"], "refused");
    assert_eq!(host.fire_due(DUE), 0, "a refused app has nothing to run");
    assert_eq!(get(host.addr, "/hello/").status, 200);
}

#[test]
fn the_schedules_and_what_they_have_done_are_in_the_stats() {
    let apps = ticker(
        "[[triggers]]\ncron = \"0 * * * *\"\npath = \"/tick\"\n\
         [[triggers]]\ncron = \"@daily\"\npath = \"/tick\"\nmethod = \"GET\"\n",
    );
    let host = start(&apps, 2);
    let stats = app_stats(&host, "ticker");
    let listed = stats["triggers"].as_array().expect("the schedules");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0]["cron"], "0 * * * *");
    assert_eq!(listed[0]["method"], "POST");
    assert_eq!(listed[0]["path"], "/tick");
    assert_eq!(listed[1]["cron"], "@daily");
    assert_eq!(listed[1]["method"], "GET");
    assert!(
        listed[0]["next_unix_ms"].as_i64().is_some_and(|at| at > 0),
        "when it is next due"
    );
    // Both are due at midnight; an app with no schedule says nothing.
    assert_eq!(host.fire_due(1_799_971_200), 2);
    wait_until("both ticks", || count(&host) == "2");
    assert!(app_stats(&host, "hello")["triggers"].is_null());
}
