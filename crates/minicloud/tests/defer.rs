//! `defer` (#56): work that outlives the request that asked for it.

mod common;

use common::*;

fn errand(config: Option<&'static str>) -> Apps {
    apps(&[
        AppSpec {
            name: "errand",
            from: fixtures().join("errand"),
            config,
        },
        sample("hello"),
    ])
}

fn done(host: &minicloud::Host) -> String {
    get(host.addr, "/errand/count").body.trim().to_string()
}

#[test]
fn the_work_happens_after_the_answer_and_as_a_run_of_its_own() {
    let apps = errand(None);
    let host = start(&apps, 2);
    let asked = get(host.addr, "/errand/ask");
    assert_eq!(asked.status, 202, "the client is answered at once");
    assert_eq!(asked.body, "asked\n");

    wait_until("the deferred run to end", || {
        count(&host, "errand", "deferred_runs") == 1
    });
    assert_eq!(done(&host), "1", "the work was done");
    assert_eq!(count(&host, "errand", "deferred_failed"), 0);
    assert_eq!(count(&host, "errand", "deferred_in_flight"), 0);
    // It is an ordinary run: the ask, the deferred run and the `/count`.
    assert_eq!(count(&host, "errand", "served"), 3);
}

#[test]
fn only_the_host_can_claim_to_be_a_deferred_run() {
    let apps = errand(None);
    let host = start(&apps, 2);
    let claimed = send_raw(
        host.addr,
        b"GET /errand/work HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
          X-Minicloud-Deferred: /work\r\n\r\n"
            .as_slice(),
    );
    assert_eq!(claimed.status, 403, "{}", claimed.body);
    assert_eq!(done(&host), "0");
}

#[test]
fn more_outstanding_work_than_max_in_flight_is_the_apps_err() {
    // One at a time, and the work is held up behind a single worker, so
    // the second ask in the same request finds the first still going.
    let apps = errand(Some(
        "grant = [\"defer\", \"kv\", \"log\"]\n[defer]\nmax_in_flight = 1\n",
    ));
    let host = start(&apps, 1);
    let asked = get(host.addr, "/errand/ask-many?n=4");
    assert_eq!(asked.status, 200);
    assert!(
        asked.body.starts_with("asked 1;"),
        "one was taken and the rest refused: {}",
        asked.body
    );
    assert!(asked.body.contains("max_in_flight"), "{}", asked.body);
    assert!(count(&host, "errand", "deferred_refused") >= 3);
    wait_until("the one that was taken", || {
        count(&host, "errand", "deferred_runs") == 1
    });
    // Once it has finished, there is room again.
    assert_eq!(get(host.addr, "/errand/ask").status, 202);
    wait_until("the next one", || {
        count(&host, "errand", "deferred_runs") == 2
    });
}

#[test]
fn a_deferred_run_that_fails_is_counted_and_said_in_the_log() {
    let apps = errand(None);
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/errand/ask-fail").status, 202);
    wait_until("the failure", || {
        count(&host, "errand", "deferred_failed") == 1
    });
    let logs = get(host.addr, "/_host/apps/errand/logs?n=50").body;
    assert!(
        logs.contains("deferred POST /fail: 500"),
        "the log says which deferred run failed: {logs}"
    );
    // The app that asked is unharmed, and so is its neighbour.
    assert_eq!(get(host.addr, "/errand/count").status, 200);
    assert_eq!(get(host.addr, "/hello/").status, 200);
}

#[test]
fn a_path_that_is_not_a_path_is_refused_without_a_run() {
    let apps = errand(None);
    let host = start(&apps, 2);
    let refused = get(host.addr, "/errand/ask-nowhere");
    assert_eq!(refused.status, 400);
    assert!(refused.body.contains("is not a path"), "{}", refused.body);
    assert_eq!(count(&host, "errand", "deferred_runs"), 0);
}

#[test]
fn an_app_not_granted_defer_is_refused_at_load() {
    let apps = errand(Some("grant = [\"kv\", \"log\"]\n"));
    let host = start(&apps, 1);
    let answer = get(host.addr, "/errand/count");
    assert_eq!(answer.status, 503);
    assert!(
        answer.body.contains("requires `defer`"),
        "the reason names the capability: {}",
        answer.body
    );
    assert_eq!(get(host.addr, "/hello/").status, 200);
}
