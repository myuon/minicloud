//! `[rate]` (#54): one caller turned away at the door, the rest unaffected.

mod common;

use common::*;

/// `hello`, with a `[rate]` of the test's own.
fn limited(rate: &str) -> Apps {
    let config: &'static str = Box::leak(format!("grant = []\n{rate}").into_boxed_str());
    apps(&[sample_with("hello", config), sample("crunch")])
}

#[test]
fn a_caller_past_its_allowance_is_429_with_retry_after() {
    let apps = limited("[rate]\nrequests = 3\nper = \"1m\"\n");
    let host = start(&apps, 2);
    for i in 1..=3 {
        assert_eq!(get(host.addr, "/hello/").status, 200, "request {i}");
    }
    let refused = get(host.addr, "/hello/");
    assert_eq!(refused.status, 429);
    assert!(
        refused.header("retry-after").is_some_and(|after| after
            .parse::<u64>()
            .is_ok_and(|secs| (1..=60).contains(&secs))),
        "it says how long to wait: {:?}",
        refused.header("retry-after")
    );
    assert!(refused.body.contains("3 request(s)"), "{}", refused.body);
    assert_eq!(count(&host, "hello", "rejected.rate_limited"), 1);
    // The app ran three times and not a fourth: the refusal is at the door.
    assert_eq!(count(&host, "hello", "served"), 3);
    // Its neighbour is untouched, as it is by a queue that is full.
    assert_eq!(get(host.addr, "/crunch/?n=10").status, 200);
}

#[test]
fn an_app_without_a_rate_takes_whatever_comes() {
    let apps = limited("");
    let host = start(&apps, 2);
    for _ in 0..20 {
        assert_eq!(get(host.addr, "/hello/").status, 200);
    }
    assert_eq!(count(&host, "hello", "rejected.rate_limited"), 0);
}

#[test]
fn a_burst_may_be_larger_than_the_rate() {
    let apps = limited("[rate]\nrequests = 1\nper = \"1m\"\nburst = 5\n");
    let host = start(&apps, 2);
    for i in 1..=5 {
        assert_eq!(get(host.addr, "/hello/").status, 200, "request {i}");
    }
    assert_eq!(get(host.addr, "/hello/").status, 429);
}

#[test]
fn a_header_may_be_the_caller_instead_of_the_address() {
    // Every request here comes from the same address, so a key of
    // `address` would be one bucket; the header is what tells them apart.
    let apps = limited("[rate]\nrequests = 2\nper = \"1m\"\nkey = \"header:x-api-key\"\n");
    let host = start(&apps, 2);
    let ask = |key: &str| {
        send_raw(
            host.addr,
            format!(
                "GET /hello/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
                 X-Api-Key: {key}\r\n\r\n"
            )
            .as_bytes(),
        )
        .status
    };
    assert_eq!(ask("one"), 200);
    assert_eq!(ask("one"), 200);
    assert_eq!(ask("one"), 429);
    // Another key, another allowance.
    assert_eq!(ask("two"), 200);
    assert_eq!(ask("two"), 200);
    assert_eq!(ask("two"), 429);
}

#[test]
fn one_bucket_for_the_whole_app_counts_every_caller_together() {
    let apps = limited("[rate]\nrequests = 2\nper = \"1m\"\nkey = \"app\"\n");
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/hello/").status, 200);
    assert_eq!(get(host.addr, "/hello/").status, 200);
    assert_eq!(get(host.addr, "/hello/").status, 429);
}

#[test]
fn a_rate_that_cannot_be_read_refuses_that_app_alone() {
    for bad in [
        "[rate]\nper = \"1m\"\n",
        "[rate]\nrequests = 0\n",
        "[rate]\nrequests = 1\nper = \"soon\"\n",
        "[rate]\nrequests = 1\nkey = \"whoever\"\n",
        "[rate]\nrequests = 1\nkeys = 2\n",
    ] {
        let apps = limited(bad);
        let host = start(&apps, 1);
        let answer = get(host.addr, "/hello/");
        assert_eq!(answer.status, 503, "{bad}");
        assert_eq!(get(host.addr, "/crunch/?n=10").status, 200, "{bad}");
    }
}

#[test]
fn the_files_of_an_app_are_behind_its_rate_too() {
    let apps = limited("[rate]\nrequests = 1\nper = \"1m\"\n");
    std::fs::create_dir_all(apps.root.join("hello/assets")).unwrap();
    std::fs::write(apps.root.join("hello/assets/style.css"), "body{}\n").unwrap();
    let host = start(&apps, 2);
    assert_eq!(get(host.addr, "/hello/style.css").status, 200);
    let refused = get(host.addr, "/hello/style.css");
    assert_eq!(
        refused.status, 429,
        "a file is cheap, but a flood of them is not"
    );
}
