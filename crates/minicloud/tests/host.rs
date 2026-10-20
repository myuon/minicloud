//! The host, started in-process on a free port and asked over TCP.
//!
//! Nothing here asserts a duration. Where a test needs the host in some state
//! — workers saturated, runs parked, a queue deep — it waits for the host's
//! own stats to say so, and what it asserts afterwards is counted: requests
//! answered, statuses, parks, yields, turns.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use common::*;

const HELLO: &str = "/hello/?name=Cove";

#[test]
fn two_or_more_apps_are_served_concurrently() {
    let apps = apps(&[sample("hello"), sample("crunch"), sample("slow")]);
    let host = start(&apps, 2);
    let addr = host.addr;
    let clients: Vec<_> = (0..12)
        .map(|i| {
            thread::spawn(move || match i % 3 {
                0 => get(addr, HELLO),
                1 => get(addr, "/crunch/?n=20000"),
                _ => get(addr, "/slow/?ms=20"),
            })
        })
        .collect();
    let answers: Vec<_> = clients.into_iter().map(|c| c.join().unwrap()).collect();
    for answer in &answers {
        assert_eq!(answer.status, 200, "{answer:?}");
    }
    assert!(answers.iter().any(|a| a.body == "Hello, Cove! (GET /)\n"));
    assert!(answers
        .iter()
        .any(|a| a.body == "2262 primes up to 20000, the largest 19997\n"));
    assert!(answers.iter().any(|a| a.body == "waited 1 x 20 ms\n"));
    for app in ["hello", "crunch", "slow"] {
        assert_eq!(count(&host, app, "served"), 4, "{app}");
    }
}

#[test]
fn hello_answers_while_crunch_saturates_the_workers_and_slow_is_parked() {
    let apps = apps(&[sample("hello"), sample("crunch"), sample("slow")]);
    let host = start(&apps, 2);
    let addr = host.addr;
    // Twice as many clients asking for long crunches as there are workers,
    // each asking again as soon as it is answered, so that both workers stay
    // busy and more crunches wait for as long as the test needs; and slow
    // requests that sit parked for most of a second.
    let stop = Arc::new(AtomicBool::new(false));
    let crunches: Vec<_> = (0..4)
        .map(|_| {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut answers = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    answers.push(get(addr, "/crunch/?n=200000"));
                }
                answers
            })
        })
        .collect();
    let sleepers: Vec<_> = (0..6)
        .map(|_| thread::spawn(move || get(addr, "/slow/?ms=400&times=2")))
        .collect();
    wait_until("crunch to hold both workers and slow to be parked", || {
        count(&host, "crunch", "in_flight") >= 2 && count(&host, "slow", "parked") >= 1
    });

    let hello = get(addr, HELLO);
    assert_eq!(hello.status, 200);
    assert_eq!(hello.body, "Hello, Cove! (GET /)\n");
    stop.store(true, Ordering::Relaxed);

    for crunch in crunches {
        for answer in crunch.join().unwrap() {
            assert_eq!(answer.status, 200, "{answer:?}");
            assert_eq!(
                answer.body,
                "17984 primes up to 200000, the largest 199999\n"
            );
        }
    }
    for sleeper in sleepers {
        assert_eq!(sleeper.join().unwrap().status, 200);
    }
    // The slow runs parked at every sleep and held no worker; the crunches
    // were sliced, since something was always waiting while they ran.
    assert_eq!(count(&host, "slow", "parks"), 12);
    assert_eq!(count(&host, "slow", "blocking_host_calls"), 0);
    assert!(count(&host, "crunch", "yields") > 0);
    assert_eq!(count(&host, "hello", "ok"), 1);
}

#[test]
fn overload_is_rejected_explicitly_and_other_apps_still_answer() {
    // One slow request in flight, two waiting, and the rest turned away.
    let apps = apps(&[
        sample("hello"),
        sample_with(
            "slow",
            "grant = [\"timer\", \"log\"]\n[limits]\nmax_in_flight = 1\nmax_queued = 2\n",
        ),
    ]);
    let host = start(&apps, 2);
    let addr = host.addr;
    let first = thread::spawn(move || get(addr, "/slow/?ms=1000"));
    wait_until("the first slow request to park", || {
        count(&host, "slow", "parked") == 1
    });
    let more: Vec<_> = (0..6)
        .map(|_| thread::spawn(move || get(addr, "/slow/?ms=10")))
        .collect();
    wait_until("the queue to fill and the rest to be rejected", || {
        count(&host, "slow", "queued") == 2 && count(&host, "slow", "rejected.queue_full") == 4
    });

    // Another app is not behind slow's queue.
    assert_eq!(get(addr, HELLO).status, 200);

    let answers: Vec<_> = more.into_iter().map(|t| t.join().unwrap()).collect();
    let rejected: Vec<_> = answers.iter().filter(|a| a.status == 429).collect();
    assert_eq!(rejected.len(), 4, "{answers:?}");
    assert_eq!(rejected[0].header("retry-after"), Some("1"));
    assert!(rejected[0].body.contains("requests waiting already"));
    assert_eq!(answers.iter().filter(|a| a.status == 200).count(), 2);
    assert_eq!(first.join().unwrap().status, 200);
}

#[test]
fn the_server_wide_limit_answers_503_with_retry_after() {
    let apps = apps(&[sample("hello"), sample("slow")]);
    let mut options = options(&apps, 2);
    options.max_in_flight = 1;
    let host = minicloud::Host::start(options).unwrap();
    let addr = host.addr;
    let first = thread::spawn(move || get(addr, "/slow/?ms=500"));
    wait_until("the slow request to park", || {
        count(&host, "slow", "parked") == 1
    });
    let busy = get(addr, HELLO);
    assert_eq!(busy.status, 503);
    assert_eq!(busy.header("retry-after"), Some("1"));
    assert_eq!(count(&host, "hello", "rejected.server_busy"), 1);
    assert_eq!(first.join().unwrap().status, 200);
    // Once it has answered, there is room again.
    assert_eq!(get(addr, HELLO).status, 200);
}

#[test]
fn a_budget_overrun_ends_that_request_only() {
    let apps = apps(&[
        sample("hello"),
        sample_with(
            "slow",
            "grant = [\"timer\", \"log\"]\n[limits]\ndeadline = \"300ms\"\nmax_host_calls = 3\n",
        ),
    ]);
    let host = start(&apps, 2);
    let addr = host.addr;

    // Deadline, while running: `/spin` loops until its two seconds pass.
    let spun = get(addr, "/hello/spin");
    assert_eq!(spun.status, 504);
    assert_eq!(spun.header("x-cove-stop"), Some("deadline"));
    assert!(spun.body.contains("deadline of"), "{}", spun.body);
    assert!(spun.body.contains("hello.cove"), "{}", spun.body);

    // Deadline, while parked: the timer would answer in five seconds.
    let late = get(addr, "/slow/?ms=5000");
    assert_eq!(late.status, 504);
    assert!(
        late.body.contains("deadline of 300ms exceeded"),
        "{}",
        late.body
    );

    // Host calls: five sleeps against a limit of three.
    let chatty = get(addr, "/slow/?ms=0&times=5");
    assert_eq!(chatty.status, 500);
    assert!(
        chatty.body.contains("host-call limit of 3"),
        "{}",
        chatty.body
    );

    // Each ended its own request; the same apps answer the next one.
    assert_eq!(get(addr, HELLO).status, 200);
    assert_eq!(get(addr, "/slow/?ms=1").status, 200);
    assert_eq!(count(&host, "hello", "errors.deadline"), 1);
    assert_eq!(count(&host, "slow", "errors.deadline"), 1);
    assert_eq!(count(&host, "slow", "errors.host_calls"), 1);
    assert_eq!(count(&host, "hello", "ok"), 1);
    assert_eq!(count(&host, "slow", "ok"), 1);
}

#[test]
fn an_over_reaching_app_is_refused_at_load_and_the_others_serve() {
    let apps = apps(&[sample("hello"), fixture("greedy"), fixture("spawner")]);
    let host = start(&apps, 1);
    let addr = host.addr;

    let greedy = get(addr, "/greedy/");
    assert_eq!(greedy.status, 503);
    assert!(
        greedy
            .body
            .contains("`greedy.handle` requires `log`, which app.toml does not grant"),
        "{}",
        greedy.body
    );
    let spawner = get(addr, "/spawner/");
    assert_eq!(spawner.status, 503);
    assert!(
        spawner.body.contains("can spawn a task"),
        "{}",
        spawner.body
    );
    assert_eq!(get(addr, HELLO).status, 200);
    assert_eq!(get(addr, "/nobody/").status, 404);

    let stats = host.stats();
    assert_eq!(stats["apps"]["greedy"]["state"], "refused");
    assert_eq!(stats["apps"]["greedy"]["required"], "log");
    assert_eq!(stats["apps"]["hello"]["state"], "ready");

    // `check` says the same, and fails.
    let report =
        minicloud::toolchain::check(&apps.root, &[], &minicloud::HostModules::standard()).unwrap();
    assert!(!report.ok);
    assert!(report
        .out
        .contains("greedy     requires [log]  granted [-]  REFUSED"));
    assert!(report
        .out
        .contains("hello      requires [-]  granted [-]  ok"));
    assert!(report.err.contains("minicloud::spawn"), "{}", report.err);
}

#[test]
fn size_limits_are_enforced_both_ways() {
    let apps = apps(&[sample_with(
        "hello",
        "[limits]\nmax_request_bytes = 16\nmax_response_bytes = 100\n",
    )]);
    let host = start(&apps, 1);
    let addr = host.addr;

    assert_eq!(post(addr, "/hello/echo", "sixteen bytes!!!").status, 200);
    let large = post(addr, "/hello/echo", "seventeen bytes!!");
    assert_eq!(large.status, 413);
    assert!(large.body.contains("at most 16 bytes"));

    // A chunked body declares no length; it is cut off as it arrives.
    let chunked = send_raw(
        addr,
        b"POST /hello/echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
          Transfer-Encoding: chunked\r\n\r\n10\r\n0123456789abcdef\r\n10\r\n0123456789abcdef\r\n0\r\n\r\n",
    );
    assert_eq!(chunked.status, 413);

    assert_eq!(get(addr, "/hello/big?bytes=100").status, 200);
    let big = get(addr, "/hello/big?bytes=101");
    assert_eq!(big.status, 500);
    assert!(
        big.body.contains("above its max_response_bytes of 100"),
        "{}",
        big.body
    );

    assert_eq!(count(&host, "hello", "rejected.too_large"), 2);
    assert_eq!(count(&host, "hello", "errors.response_too_large"), 1);
}

#[test]
fn an_app_flooding_its_queue_does_not_starve_another() {
    // One worker, so every turn is the scheduler's choice. `flood` is crunch
    // under another name, one run at a time and a deep queue.
    let apps = apps(&[
        sample("hello"),
        AppSpec {
            name: "flood",
            from: examples().join("crunch"),
            config: Some("[limits]\nmax_in_flight = 1\nmax_queued = 64\n"),
        },
    ]);
    let host = start(&apps, 1);
    let addr = host.addr;
    let flood: Vec<_> = (0..20)
        .map(|_| thread::spawn(move || get(addr, "/flood/?n=200000")))
        .collect();
    wait_until("flood's queue to be deep", || {
        count(&host, "flood", "queued") >= 10
    });

    // Each hello is admitted behind a deep queue of flood's, and is served
    // at the next turn rather than after it.
    for _ in 0..5 {
        assert_eq!(get(addr, HELLO).status, 200);
    }
    let flood_served = count(&host, "flood", "served");
    assert!(
        flood_served < 20,
        "every flood request was served before five hellos: {flood_served}"
    );
    assert!(count(&host, "flood", "queued") > 0);
    assert_eq!(count(&host, "hello", "served"), 5);

    for request in flood {
        assert_eq!(request.join().unwrap().status, 200);
    }
    assert_eq!(count(&host, "flood", "served"), 20);
}

#[test]
fn the_connection_limit_answers_503() {
    let apps = apps(&[sample("hello")]);
    let mut options = options(&apps, 1);
    options.max_connections = 1;
    let host = minicloud::Host::start(options).unwrap();
    // Held open and idle: it is the one connection allowed.
    let _held = std::net::TcpStream::connect(host.addr).unwrap();
    wait_until("the held connection to be accepted", || {
        host.stats()["connections"].as_u64() == Some(1)
    });
    let refused = get(host.addr, HELLO);
    assert_eq!(refused.status, 503);
    assert_eq!(refused.header("retry-after"), Some("1"));
    assert_eq!(host.stats()["rejected_connections"], 1);
}

#[test]
fn the_request_reaches_the_app_as_a_web_request() {
    let apps = apps(&[sample("hello")]);
    let host = start(&apps, 1);
    let answer = get(host.addr, "/hello/a/b?name=two%20words&x=1");
    assert_eq!(answer.status, 200);
    assert_eq!(answer.body, "Hello, two words! (GET /a/b)\n");
    assert_eq!(
        answer.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    let stats = host.stats();
    assert_eq!(stats["totals"]["served"], 1);
}

/// The native tier, where this machine has one: the apps are compiled, and a
/// long compiled run is still sliced (ADR 0085).
#[cfg(all(target_arch = "x86_64", unix))]
#[test]
fn on_x86_64_unix_the_apps_run_on_the_native_tier_and_still_yield() {
    if std::env::var("COVE_HOST_TEST_BACKEND").is_ok_and(|backend| backend == "vm") {
        return;
    }
    let apps = apps(&[sample("hello"), sample("crunch")]);
    let host = start(&apps, 1);
    let addr = host.addr;
    assert_eq!(app_stats(&host, "crunch")["tier"], "native");
    assert_eq!(app_stats(&host, "hello")["tier"], "native");
    let stop = Arc::new(AtomicBool::new(false));
    let crunches: Vec<_> = (0..3)
        .map(|_| {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    assert_eq!(get(addr, "/crunch/?n=200000").status, 200);
                }
            })
        })
        .collect();
    wait_until("crunches to wait for the one worker", || {
        count(&host, "crunch", "yields") > 0
    });
    assert_eq!(get(addr, HELLO).status, 200);
    stop.store(true, Ordering::Relaxed);
    for crunch in crunches {
        crunch.join().unwrap();
    }
}

#[test]
fn the_bundled_and_example_apps_check_and_their_tests_pass() {
    // The webhook lab's admin secret, the ledger's post secret and the
    // admin UI's secret come from the environment.
    std::env::set_var("WEBHOOKS_ADMIN_TOKEN", "test");
    std::env::set_var("LEDGER_TOKEN", "test");
    std::env::set_var("ADMIN_UI_TOKEN", "test");
    let modules = minicloud::HostModules::standard();
    for dir in [bundled(), examples()] {
        let checked = minicloud::toolchain::check(&dir, &[], &modules).unwrap();
        assert!(
            checked.ok,
            "{}: {}{}",
            dir.display(),
            checked.out,
            checked.err
        );
        let tested = minicloud::toolchain::test(&dir, &[], None, &modules).unwrap();
        assert!(tested.ok, "{}: {}{}", dir.display(), tested.out, tested.err);
    }
}

/// What a park costs end to end, printed, against which `kv`'s
/// answer-at-once is weighed: `cargo t --test host -- --ignored park_cost
/// --nocapture`. A request of sixteen zero-length sleeps against one, the
/// difference per park.
#[test]
#[ignore]
fn park_cost() {
    let apps = apps(&[sample_with(
        "slow",
        "grant = [\"timer\", \"log\"]\n[limits]\nmax_host_calls = 32\n",
    )]);
    let host = start(&apps, 2);
    let time = |path: &str| {
        let started = std::time::Instant::now();
        for _ in 0..300 {
            assert_eq!(get(host.addr, path).status, 200);
        }
        started.elapsed() / 300
    };
    time("/slow/?ms=0&times=1");
    let one = time("/slow/?ms=0&times=1");
    let sixteen = time("/slow/?ms=0&times=16");
    println!(
        "a request with 1 park {one:?}, with 16 {sixteen:?}: {:?} per park",
        (sixteen - one) / 15
    );
}

#[test]
fn a_run_that_outgrows_its_heap_fails_while_it_allocates() {
    // `hoarder` has a heap of 65,536 words. A run that keeps a few numbers
    // fits, parks once and answers.
    let apps = apps(&[fixture("hoarder")]);
    let host = start(&apps, 1);
    let addr = host.addr;
    let fits = get(addr, "/hoarder/?n=100&ms=0");
    assert_eq!(fits.status, 200, "{}", fits.body);
    assert_eq!(fits.body, "kept 100\n");
    assert_eq!(count(&host, "hoarder", "parks"), 1);
    let peak = count(&host, "hoarder", "heap_peak_words");
    assert!(peak > 0 && peak <= 65536, "{peak}");

    // One that keeps a million fails its allocation where it makes it: it
    // never reaches the minute's wait after it (which its deadline would
    // have ended), so it never parks.
    let grows = get(addr, "/hoarder/?n=1000000&ms=60000");
    assert_eq!(grows.status, 500, "{}", grows.body);
    assert_eq!(grows.header("x-cove-stop"), Some("heap"));
    assert!(
        grows.body.contains("this run has no memory left"),
        "{}",
        grows.body
    );
    assert_eq!(count(&host, "hoarder", "errors.heap"), 1);
    assert_eq!(
        count(&host, "hoarder", "parks"),
        1,
        "the second run never parked"
    );
    assert!(count(&host, "hoarder", "heap_peak_words") <= 65536);
    // A run is collected before it is refused more heap, and what it
    // allocated before it failed is counted all the same.
    assert_eq!(count(&host, "hoarder", "gc_runs"), 1, "the second run");
    assert!(count(&host, "hoarder", "gc_collections") >= 1);
    assert!(count(&host, "hoarder", "allocated_words") > peak);
}

#[test]
fn a_spawn_reached_through_a_module_is_refused_where_it_is() {
    let apps = apps(&[fixture("delegator")]);
    let host = start(&apps, 1);
    let refused = get(host.addr, "/delegator/");
    assert_eq!(refused.status, 503);
    assert!(
        refused
            .body
            .contains("`delegator.handle` can spawn a task — `work.double` spawns one"),
        "{}",
        refused.body
    );
    let report =
        minicloud::toolchain::check(&apps.root, &[], &minicloud::HostModules::standard()).unwrap();
    assert!(!report.ok);
    assert!(report.err.contains("minicloud::spawn"), "{}", report.err);
    assert!(
        report.err.contains("delegator/work/work.cove:"),
        "at the spawn, in the module: {}",
        report.err
    );
}

/// `check` refuses an `app.toml` that still says `limits.fuel` (Cove ADR
/// 0091), naming it; `check --deployed`, which `install.sh` runs over the
/// installed apps before switching to a new release, warns instead, as the
/// host does when it starts.
#[test]
fn check_refuses_the_removed_fuel_limit_unless_the_apps_are_deployed() {
    let apps = apps(&[
        sample_with("hello", "[limits]\nfuel = 2000000\n"),
        sample("slow"),
    ]);
    let modules = minicloud::HostModules::standard();
    let strict = minicloud::toolchain::check(&apps.root, &[], &modules).unwrap();
    assert!(!strict.ok);
    assert!(
        strict.out.contains("REFUSED: `")
            && strict
                .out
                .contains("`limits.fuel` was removed (Cove ADR 0091"),
        "{}",
        strict.out
    );
    let deployed = minicloud::toolchain::check_as(
        &apps.root,
        &[],
        &modules,
        None,
        minicloud::config::RemovedKeys::Ignore,
    )
    .unwrap();
    assert!(deployed.ok, "{}\n{}", deployed.out, deployed.err);
    assert!(
        deployed
            .err
            .contains("warning: [hello] `limits.fuel` in app.toml is ignored"),
        "{}",
        deployed.err
    );
    assert!(deployed
        .out
        .contains("hello      requires [-]  granted [-]  ok"));
}
