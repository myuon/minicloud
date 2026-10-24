//! The delivery loop held to `docs/queue-properties.md`, a step at a time
//! ([`super::harness`]).
//!
//! What each test expects is the contract's, not the code's. A test the
//! code does not pass yet because of a finding the document names is
//! `#[ignore]`d with that finding's number, so that `cargo test -- --ignored`
//! shows each gap and the suite stays green until it is closed.

use super::harness::{policies, policy, Harness, Op};
use super::Store;

/// A message, the only one on `mail`, leased by one tick.
fn leased(h: &mut Harness) -> i64 {
    let id = h.send("mail", "hello");
    h.tick();
    assert_eq!(h.facts(id).leases.len(), 1, "the tick leased it");
    id
}

// ------------------------------------------------------------ L3a

/// With nothing going wrong — every delivery admitted, every run
/// answering 2xx at once, no crash, no clock jump, no redeploy — each
/// message is invoked exactly once, however many ticks go by.
#[test]
fn l3a_with_no_fault_each_message_is_invoked_exactly_once() {
    let mut h = Harness::new("l3a", vec![policy("mail", |p| p.batch = 3)]);
    let mut sent = Vec::new();
    for tick in 0..300 {
        if tick % 2 == 0 && sent.len() < 100 {
            sent.push(h.send("mail", &format!("m{tick}")));
            sent.push(h.send("mail", &format!("n{tick}")));
        }
        h.tick();
        let ids: Vec<i64> = h.pending.iter().map(|p| p.message.id).collect();
        for id in ids {
            h.take(id, 200);
        }
        h.advance_ms(1000);
    }
    assert_eq!(sent.len(), 100);
    for id in sent {
        let facts = h.facts(id);
        assert_eq!(facts.invocations, 1, "message {id}: {facts:?}");
        assert_eq!(facts.leases.len(), 1, "message {id}: {facts:?}");
        assert_eq!(facts.written, vec![(Op::Done, true)], "message {id}");
        assert!(h.row(id).is_none(), "message {id} is off the queue");
    }
    assert_eq!(h.delivered(), 100);
}

// ------------------------------------------------------------ F1 / S5

/// S5: a lease is issued only while `attempts` is below the current
/// `max_attempts`. A message waiting for its retry when `max_attempts` is
/// lowered to its `attempts` is not handed out again; it is a dead letter
/// at the next tick, saying why.
#[test]
fn f1_lowering_max_attempts_stops_a_waiting_retry() {
    let mut h = Harness::new(
        "f1",
        vec![policy("mail", |p| {
            p.max_attempts = 3;
            p.retry = std::time::Duration::from_secs(10);
        })],
    );
    let id = leased(&mut h);
    h.take(id, 500);
    let row = h.row(id).unwrap();
    assert_eq!(
        (row.attempts, row.state),
        (1, "held"),
        "waiting for its retry"
    );

    // A redeploy lowers `max_attempts` to the attempts it has had.
    h.queues = vec![policy("mail", |p| {
        p.max_attempts = 1;
        p.retry = std::time::Duration::from_secs(10);
    })];
    h.advance(10);
    h.tick();

    let facts = h.facts(id);
    assert_eq!(facts.leases.len(), 1, "not leased again: {facts:?}");
    assert!(h.pending.is_empty(), "no delivery in flight");
    if !h.pending.is_empty() {
        h.take(id, 200);
    }
    assert_eq!(h.facts(id).invocations, 1, "the app is not asked again");
    let row = h.row(id).expect("a dead letter, not gone");
    assert_eq!(row.state, "dead", "{row:?}");
    assert!(
        row.why.contains("max_attempts"),
        "the reason says max_attempts was lowered: {:?}",
        row.why
    );
}

/// S5, N2: a tick reads the clock once. With one attempt allowed, a lease
/// that runs out while the tick is between `bury_spent` and `lease` (the
/// tick waited on the store, say) is neither buried as spent by the first
/// nor handed out by the second: the message has had its one delivery. It
/// is buried at the next tick, as having had no answer within its lease.
#[test]
fn n2_a_lease_running_out_in_the_middle_of_a_tick_is_not_handed_out_again() {
    use std::sync::atomic::Ordering;
    let mut h = Harness::new(
        "n2",
        vec![policy("mail", |p| {
            p.max_attempts = 1;
            p.lease = std::time::Duration::from_secs(30);
        })],
    );
    let id = leased(&mut h);
    h.start_run(id);
    // Half a second before the lease runs out, a tick whose `bury_spent`
    // takes a whole second.
    h.advance_ms(29_500);
    h.slow_bury_spent.store(1000, Ordering::Relaxed);
    h.tick();
    h.slow_bury_spent.store(0, Ordering::Relaxed);
    assert_eq!(h.facts(id).leases.len(), 1, "not leased past max_attempts");
    h.tick();
    let row = h.row(id).expect("a dead letter");
    assert_eq!(
        (row.state, row.why.as_str()),
        ("dead", "no answer within the lease")
    );
}

// ------------------------------------------------------------ F2 / S6, S7

/// S6.3, S7: a run that waited for a worker past its deadline was never
/// started; the scheduler answered 503 itself. The host can see that, so
/// the attempt is given back — and on the last attempt the message is not
/// buried as if its consumer had failed.
#[test]
fn f2_a_run_that_timed_out_in_the_scheduler_is_given_back() {
    let mut h = Harness::new("f2-timeout", vec![policy("mail", |p| p.max_attempts = 1)]);
    let id = leased(&mut h);
    h.time_out_in_scheduler(id);
    h.poll();
    assert_eq!(h.facts(id).invocations, 0);
    let row = h.row(id).expect("still on the queue");
    assert_ne!(row.state, "dead", "not buried on its last attempt: {row:?}");
    assert_eq!(row.attempts, 0, "the attempt is given back");
    h.check_attempts(id);
}

/// The same for a run cancelled while it waited: the scheduler answers 499
/// without starting it. Below 500, so the delivery once took it for the app's
/// answer and deleted the message — S1 as well as S6: nothing of the app
/// ran. (No path cancels a queue delivery's run today — its `Cancel` is
/// the delivery's own and nobody fires it — so this waits for one.)
#[test]
fn f2_a_run_cancelled_before_it_started_is_given_back() {
    let mut h = Harness::new("f2-cancel", vec![policy("mail", |p| p.max_attempts = 1)]);
    let id = leased(&mut h);
    h.cancel_before_start(id);
    h.poll();
    assert_eq!(h.facts(id).invocations, 0);
    let row = h.row(id).expect("S1: not lost; the app never saw it");
    assert_ne!(row.state, "dead", "not buried: {row:?}");
    assert_eq!(row.attempts, 0, "the attempt is given back");
    assert_eq!(h.delivered(), 0, "not counted as delivered");
    h.check_attempts(id);
}

/// S1, S7: a run cancelled after its entry started is answered 499 by the
/// host, not by the app. Below 500, but it is no answer: the message is not
/// taken, the attempt counts, and on the last one the dead letter says the
/// run did not answer.
#[test]
fn f2_a_run_cancelled_after_it_started_is_no_answer() {
    let mut h = Harness::new(
        "f2-cancel-mid",
        vec![policy("mail", |p| p.max_attempts = 2)],
    );
    let id = leased(&mut h);
    h.start_run(id);
    h.cancel_mid_run(id);
    h.poll();
    let row = h.row(id).expect("S1: not taken");
    assert_eq!((row.state, row.attempts), ("held", 1), "retried: {row:?}");
    assert_eq!(h.delivered(), 0);
    h.check_attempts(id);

    h.advance(3600);
    h.tick();
    h.start_run(id);
    h.cancel_mid_run(id);
    h.poll();
    let row = h.row(id).expect("a dead letter");
    assert_eq!(row.state, "dead");
    assert!(row.why.starts_with("no answer: 499"), "{}", row.why);
}

// ------------------------------------------------------------ F3 / S7

/// S7, with the proposed fix's reading of a failed write: a `done` that
/// could not be written is not a delivery counted as made, and it is kept
/// and tried again — so when the lease of the last attempt runs out, the
/// message is not a dead letter "with no answer within the lease": the app
/// answered.
#[test]
fn f3_a_done_that_fails_to_write_is_kept_not_counted_and_not_buried() {
    let mut h = Harness::new(
        "f3",
        vec![policy("mail", |p| {
            p.max_attempts = 1;
            p.lease = std::time::Duration::from_secs(30);
        })],
    );
    let id = leased(&mut h);
    h.fail_settles(id, 1);
    h.take(id, 200);
    assert_eq!(h.facts(id).failed, vec![Op::Done]);
    assert_eq!(h.delivered(), 0, "only a written done is counted");

    h.advance(31);
    h.tick();
    h.poll();
    let row = h.row(id);
    assert!(
        row.as_ref().is_none_or(|row| row.state != "dead"),
        "not buried as no answer: {row:?}"
    );
    assert!(
        h.facts(id).written.contains(&(Op::Done, true)),
        "the done is written in the end: {:?}",
        h.facts(id)
    );
    assert_eq!(h.delivered(), 1);
}

/// The other settles fail the same way: a `retry` that is not written
/// leaves the lease in place, and the lease of the last attempt then
/// buries the message as having had no answer, though it had a 500.
#[test]
fn f3_a_retry_that_fails_to_write_is_kept() {
    let mut h = Harness::new(
        "f3-retry",
        vec![policy("mail", |p| {
            p.max_attempts = 2;
            p.lease = std::time::Duration::from_secs(30);
            p.retry = std::time::Duration::from_secs(5);
        })],
    );
    let id = leased(&mut h);
    h.fail_settles(id, 1);
    h.take(id, 500);
    assert_eq!(
        h.counters()
            .queue_retried
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    // The retry, kept and written at the next tick, makes it due in 5 s.
    h.advance(5);
    h.tick();
    assert_eq!(
        h.facts(id).leases.len(),
        2,
        "tried again after `retry`, not after the lease"
    );
}

/// S7, N3: a kept settle is the delivery's answer for as long as it is
/// kept. The `done` fails, and fails again at the next tick, after the
/// lease of the last attempt has run out: the message is neither buried as
/// having had no answer nor handed out again, and the tick after that
/// writes the `done`.
#[test]
fn n3_a_message_whose_settle_is_kept_is_not_buried_nor_leased_when_its_lease_runs_out() {
    let mut h = Harness::new(
        "n3",
        vec![policy("mail", |p| {
            p.max_attempts = 1;
            p.lease = std::time::Duration::from_secs(30);
        })],
    );
    let id = leased(&mut h);
    h.fail_settles(id, 2);
    h.take(id, 200);
    h.advance(31);
    h.tick();
    assert_eq!(h.facts(id).failed, vec![Op::Done, Op::Done]);
    let row = h.row(id).expect("still on the queue");
    assert_eq!(row.state, "due", "{row:?}");
    assert_eq!(h.facts(id).leases.len(), 1, "not handed out again");
    assert_eq!((h.delivered(), h.dead_count()), (0, 0));
    assert!(!h.logs().contains("dead letter"), "{}", h.logs());

    h.tick();
    assert!(h.row(id).is_none(), "the done is written");
    assert_eq!(h.delivered(), 1);
}

/// S6, F3 on `give_back`: a refusal at admission whose give-back fails to
/// write is kept and written at the next tick, so the attempt the app never
/// had is taken back after all.
#[test]
fn f3_a_give_back_that_fails_to_write_is_kept() {
    let mut h = Harness::new("f3-give-back", vec![policy("mail", |p| p.max_attempts = 1)]);
    h.admits = Box::new(|_| false);
    let id = h.send("mail", "x");
    h.fail_settles(id, 1);
    h.tick();
    assert_eq!(h.facts(id).failed, vec![Op::GiveBack]);
    assert_eq!(h.row(id).unwrap().attempts, 1, "not given back yet");
    h.tick();
    assert_eq!(h.facts(id).written, vec![(Op::GiveBack, true)]);
    assert_eq!(h.row(id).unwrap().attempts, 0, "the attempt is taken back");
    h.check_attempts(id);

    h.admits = Box::new(|_| true);
    h.advance(30);
    h.tick();
    h.take(id, 200);
    assert!(h.row(id).is_none(), "delivered on its one attempt");
}

// ------------------------------------------------------------ F6 / L2

const TWO_QUEUES: &str = "[[queues]]\nname = \"first\"\nbatch = 4\n\n\
                          [[queues]]\nname = \"second\"\nbatch = 2\n";

/// Runs the busy-first-queue pattern for `ticks` ticks: before each tick
/// `first` is filled to its `batch` of due messages, and every delivery
/// answers 200 before the next tick. Answers `second`'s one message.
fn busy_first_queue(h: &mut Harness, ticks: usize) -> i64 {
    let second = h.send("second", "lonely");
    for _ in 0..ticks {
        for n in 0..4 {
            h.send("first", &format!("busy {n}"));
        }
        h.tick();
        let ids: Vec<i64> = h.pending.iter().map(|p| p.message.id).collect();
        for id in ids {
            h.take(id, 200);
        }
        h.advance(1);
    }
    second
}

/// L2: a due message is eventually leased, per queue: a busy queue does not
/// starve another of the same app.
#[test]
fn f6_a_busy_queue_does_not_starve_another() {
    let mut h = Harness::new("f6", policies(TWO_QUEUES));
    let second = busy_first_queue(&mut h, 200);
    assert!(
        !h.facts(second).leases.is_empty(),
        "`second`'s message is leased within 200 ticks"
    );
}

/// What closes the counterexample to L2. It was a cycle: on every tick
/// `first` was leased its whole room of 4, which raised the app's in-flight
/// count to 4 before `second` was looked at, and `second`'s room, `2 - 4`,
/// saturated to 0, so `lease` was never asked for it. Now each queue's room
/// is its own `batch` less its own deliveries: on every tick `first` is
/// asked with 4 and `second` with 2, `second`'s message is leased on the
/// first, and the app has both queues' deliveries in flight at once.
#[test]
fn f6_each_queue_has_room_of_its_own() {
    let mut h = Harness::new("f6-room", policies(TWO_QUEUES));
    let second = busy_first_queue(&mut h, 50);
    let asked = h.leases_asked();
    assert_eq!(
        asked.iter().filter(|(t, _, _)| *t == 1).collect::<Vec<_>>(),
        vec![&(1, "first".to_string(), 4), &(1, "second".to_string(), 2)],
    );
    for tick in 1..=50u32 {
        let first: Vec<_> = asked
            .iter()
            .filter(|(t, queue, _)| *t == tick && queue == "first")
            .collect();
        assert_eq!(first, vec![&(tick, "first".to_string(), 4)], "tick {tick}");
    }
    assert_eq!(h.in_flight_after_tick[0], 5, "4 of `first`, 1 of `second`");
    assert_eq!(h.facts(second).leases.len(), 1);
    assert!(h.row(second).is_none(), "delivered");
}

// ------------------------------------------------------------ F7 / L1

/// L1 under E1 as it now reads: a redeploy that drops a queue leaves its
/// messages on the store, and the next tick makes them dead letters that
/// say why — counted, logged and shown — rather than leaving them where
/// nothing delivers them. A delivery running when the queue went is left
/// to settle.
#[test]
fn f7_the_messages_of_a_queue_no_longer_declared_are_dead_letters() {
    let mut h = Harness::new("f7", policies(TWO_QUEUES));
    let running = h.send("second", "running");
    h.tick();
    h.start_run(running);
    let waiting = h.send("second", "waiting");
    let kept = h.send("first", "kept");

    h.queues = policies("[[queues]]\nname = \"first\"\nbatch = 4\n");
    h.tick();
    let row = h.row(waiting).expect("a dead letter");
    assert_eq!(
        (row.state, row.why.as_str()),
        ("dead", "queue `second` is no longer declared")
    );
    assert_eq!(h.row(running).unwrap().state, "held", "its delivery runs");
    assert!(h.logs().contains(&format!(
        "queue `second`: message {waiting} is a dead letter after 0 attempt(s): \
         queue `second` is no longer declared"
    )));
    assert_eq!(h.facts(kept).leases.len(), 1, "a declared queue goes on");

    // The running delivery's answer still settles its message.
    h.answer(running, 200);
    h.poll();
    assert!(h.row(running).is_none());
    assert_eq!(
        h.counters()
            .queue_buried
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

// ------------------------------------------------------------ in flight

/// Every way a delivery can end gives back the in-flight count it took,
/// once: the harness checks after each step that `queue_in_flight` is
/// exactly the deliveries in flight, and never below nothing.
#[test]
fn in_flight_returns_after_every_way_a_delivery_ends() {
    type Ending = fn(&mut Harness, i64);
    let endings: [(&str, Ending); 10] = [
        ("answered 200", |h, id| h.take(id, 200)),
        ("answered 404", |h, id| h.take(id, 404)),
        ("answered 500", |h, id| h.take(id, 500)),
        ("no answer after it started", |h, id| {
            h.start_run(id);
            h.drop_answer(id);
            h.poll();
        }),
        ("no answer before it started", |h, id| {
            h.drop_answer(id);
            h.poll();
        }),
        ("timed out in the scheduler", |h, id| {
            h.time_out_in_scheduler(id);
            h.poll();
        }),
        ("cancelled before it started", |h, id| {
            h.cancel_before_start(id);
            h.poll();
        }),
        ("cancelled after it started", |h, id| {
            h.start_run(id);
            h.cancel_mid_run(id);
            h.poll();
        }),
        ("settle write failed", |h, id| {
            h.fail_settles(id, 1);
            h.take(id, 200);
        }),
        ("crash", |h, _| {
            h.crash();
        }),
    ];
    for (what, end) in endings {
        for max_attempts in [1, 3] {
            let mut h = Harness::new(
                "in-flight",
                vec![policy("mail", |p| p.max_attempts = max_attempts)],
            );
            let id = leased(&mut h);
            assert_eq!(h.in_flight(), 1, "{what}");
            end(&mut h, id);
            assert_eq!(h.in_flight(), 0, "{what}, max_attempts {max_attempts}");
        }
    }

    // Refused at admission: never counted as in flight past the tick.
    let mut h = Harness::new("in-flight-refused", vec![policy("mail", |_| {})]);
    h.admits = Box::new(|_| false);
    let id = h.send("mail", "x");
    h.tick();
    assert_eq!((h.in_flight(), h.facts(id).refused), (0, 1));
}

/// Many messages, every ending mixed, ticks interleaved with answers in a
/// fixed pseudo-random order: the count never leaks and never goes below
/// nothing (both checked by the harness after every step).
#[test]
fn in_flight_never_leaks_nor_underflows_over_a_mixed_run() {
    let mut h = Harness::new(
        "in-flight-mixed",
        vec![policy("mail", |p| {
            p.batch = 5;
            p.max_attempts = 3;
            p.lease = std::time::Duration::from_secs(4);
            p.retry = std::time::Duration::from_secs(1);
        })],
    );
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let mut refuse = false;
    for step in 0..2000 {
        if step % 7 == 0 {
            h.send("mail", "m");
        }
        refuse = !refuse && next(4) == 0;
        let refusing = refuse;
        h.admits = Box::new(move |_| !refusing);
        h.tick();
        let ids: Vec<i64> = h.pending.iter().map(|p| p.message.id).collect();
        for id in ids {
            match next(9) {
                0 => h.take(id, 200),
                1 => h.take(id, 500),
                2 => {
                    h.start_run(id);
                    h.drop_answer(id);
                    h.poll();
                }
                3 => {
                    h.time_out_in_scheduler(id);
                    h.poll();
                }
                4 => {
                    h.fail_settles(id, 1);
                    h.take(id, 200);
                }
                // Left running: it answers later, or outlives its lease.
                _ => {}
            }
        }
        if next(50) == 0 {
            h.crash();
        }
        h.advance_ms(700);
    }
}

/// A host that turns every delivery away: lease, refusal, give-back, round
/// and round. Nothing leaks, no attempt is spent, and once the host admits
/// again the message is delivered.
#[test]
fn a_lease_refuse_give_back_cycle_does_not_leak() {
    let mut h = Harness::new("refuse-cycle", vec![policy("mail", |p| p.max_attempts = 1)]);
    h.admits = Box::new(|_| false);
    let id = h.send("mail", "x");
    for _ in 0..100 {
        h.tick();
        assert_eq!(h.in_flight(), 0);
        h.advance(30);
    }
    let facts = h.facts(id);
    assert_eq!(
        (facts.leases.len(), facts.refused, facts.given_back()),
        (100, 100, 100)
    );
    assert_eq!(h.row(id).unwrap().attempts, 0);
    h.check_attempts(id);

    h.admits = Box::new(|_| true);
    h.tick();
    h.take(id, 200);
    assert!(h.row(id).is_none());
    assert_eq!((h.in_flight(), h.delivered()), (0, 1));
}

// ------------------------------------------------------------ F4 / S8, L3a

/// S8: a message sent with `sendAfter(d)` at host time `t` is not handed
/// out before `t + d`. Sent at x.9 s with a delay of 1 s, ticks every
/// 100 ms: the first lease must come at x+1.9 s or later.
#[test]
fn f4_send_after_is_not_handed_out_before_its_time() {
    let mut h = Harness::new("f4-early", vec![policy("mail", |_| {})]);
    h.set_ms(1_000_010_900);
    let sent_ms = h.now_ms();
    let id = h.send_after("mail", "later", 1);
    let mut leased_ms = None;
    for _ in 0..30 {
        h.advance_ms(100);
        h.tick();
        if !h.facts(id).leases.is_empty() {
            leased_ms = Some(h.now_ms());
            break;
        }
    }
    let leased_ms = leased_ms.expect("leased in the end");
    assert!(
        leased_ms >= sent_ms + 1000,
        "leased {} ms after a sendAfter(1)",
        leased_ms - sent_ms
    );
}

/// L3a: a delivery that answers within the configured `lease` is not
/// handed out again. Leased at x.9 s with `lease = "2s"`, it answers at
/// x+2.5 s, 1.6 s later; ticks every 100 ms in between.
#[test]
fn f4_a_delivery_answering_within_its_lease_is_not_leased_again() {
    let mut h = Harness::new(
        "f4-lease",
        vec![policy("mail", |p| {
            p.lease = std::time::Duration::from_secs(2)
        })],
    );
    h.set_ms(1_000_010_800);
    let id = h.send("mail", "x");
    h.advance_ms(100);
    h.tick();
    assert_eq!(h.facts(id).leases.len(), 1);
    h.start_run(id);
    for _ in 0..16 {
        h.advance_ms(100);
        h.tick();
    }
    assert_eq!(h.now_ms(), 1_000_012_500);
    assert_eq!(
        h.facts(id).leases.len(),
        1,
        "not handed out again within its 2 s lease"
    );
    h.answer(id, 200);
    h.poll();
    assert!(h.row(id).is_none(), "its own answer settles it");
}

/// A `retry` under a second, which `app.toml` takes, waits that long: not
/// nothing, and not a second.
#[test]
fn f4_a_retry_under_a_second_waits_as_long_as_it_says() {
    let mut h = Harness::new(
        "f4-retry",
        policies("[[queues]]\nname = \"mail\"\nretry = \"500ms\"\n"),
    );
    h.set_ms(1_000_010_000);
    let id = leased(&mut h);
    h.take(id, 500);
    h.advance_ms(400);
    h.tick();
    assert_eq!(h.facts(id).leases.len(), 1, "not before 500 ms");
    h.advance_ms(100);
    h.tick();
    assert_eq!(h.facts(id).leases.len(), 2, "at 500 ms");
}

/// The same with a lease that is not whole seconds, which `app.toml` takes:
/// `lease = "1500ms"` (over a deadline of 500 ms, as a lease must be) holds
/// the message for a second and a half, not for one.
#[test]
fn f4_a_lease_of_a_fraction_of_a_second_is_not_cut() {
    let mut h = Harness::new(
        "f4-sub-second",
        policies(
            "[limits]\ndeadline = \"500ms\"\n\n\
             [[queues]]\nname = \"mail\"\nlease = \"1500ms\"\n",
        ),
    );
    h.set_ms(1_000_010_000);
    let id = leased(&mut h);
    h.start_run(id);
    for _ in 0..14 {
        h.advance_ms(100);
        h.tick();
    }
    assert_eq!(
        h.facts(id).leases.len(),
        1,
        "not handed out again within 1500 ms"
    );
}

// ------------------------------------------------------------ crashes

/// Where a crash falls in a delivery.
#[derive(Clone, Copy, Debug)]
enum CrashAt {
    /// After `lease` committed, before the run was admitted.
    Leased,
    /// After admission, before the app's entry started.
    Admitted,
    /// After the entry started, before it answered.
    Started,
    /// After the app answered 200, before the settle was written.
    Answered,
    /// After the settle was written.
    Settled,
}

/// For a crash at each point of a delivery: S1 (the message is not lost:
/// it is done only by a delivery that had a 2xx answer and held the
/// current lease), S4 (no delivery from before the crash settles anything
/// after it) and S6.1-S6.3 (the gap between attempts and invocations is
/// the crash's alone), and the message is delivered in the end.
#[test]
fn a_crash_between_any_two_steps_loses_nothing_and_leaves_no_stale_settle() {
    for at in [
        CrashAt::Leased,
        CrashAt::Admitted,
        CrashAt::Started,
        CrashAt::Answered,
        CrashAt::Settled,
    ] {
        let mut h = Harness::new(
            "crash",
            vec![policy("mail", |p| {
                p.max_attempts = 3;
                p.lease = std::time::Duration::from_secs(30);
            })],
        );
        let id = h.send("mail", "x");
        // Lease and admission are two steps: the tick leases, and the
        // admission is held back until the test lets it happen.
        h.hold = true;
        h.tick();
        h.hold = false;
        let first_lease = h.facts(id).leases[0];
        if !matches!(at, CrashAt::Leased) {
            h.admit_held();
        }
        match at {
            CrashAt::Leased | CrashAt::Admitted => {}
            CrashAt::Started => h.start_run(id),
            CrashAt::Answered => {
                h.start_run(id);
                h.answer(id, 200);
            }
            CrashAt::Settled => h.take(id, 200),
        }
        let taken = h.crash();
        assert_eq!(
            h.in_flight(),
            0,
            "{at:?}: a fresh process has nothing in flight"
        );
        match at {
            CrashAt::Settled => assert!(taken.is_empty()),
            _ => assert_eq!(taken, vec![(id, first_lease)], "{at:?}"),
        }

        // S1: still there unless a written done took it.
        let done = h.facts(id).written.contains(&(Op::Done, true));
        assert_eq!(h.row(id).is_none(), done, "{at:?}: S1");
        h.check_attempts(id);

        // The lease runs out and a new process delivers it.
        h.advance(31);
        h.tick();
        if !done {
            assert_eq!(h.facts(id).leases.len(), 2, "{at:?}: handed out again");
            h.check_attempts(id);
        }
        // S4: the delivery the crash took cannot settle it now. (In the
        // host it is gone with its process; this asks the store whether
        // its token would still be taken.)
        for (lost, lease) in &taken {
            let store = h.store();
            assert_eq!(store.done(*lost, *lease), Ok(false), "{at:?}: S4");
            assert_eq!(store.retry(*lost, *lease, 0, 0), Ok(false), "{at:?}: S4");
            assert_eq!(
                store.bury(*lost, *lease, 0, "stale"),
                Ok(false),
                "{at:?}: S4"
            );
        }
        if !done {
            h.take(id, 200);
        }
        assert!(h.row(id).is_none(), "{at:?}: delivered in the end");
        let facts = h.facts(id);
        let expected_invocations = match at {
            CrashAt::Leased | CrashAt::Admitted | CrashAt::Settled => 1,
            // At least once: the run that started before the crash, and
            // the one after.
            CrashAt::Started | CrashAt::Answered => 2,
        };
        assert_eq!(facts.invocations, expected_invocations, "{at:?}: {facts:?}");
        assert_eq!(
            facts
                .written
                .iter()
                .filter(|w| **w == (Op::Done, true))
                .count(),
            1,
            "{at:?}: done once"
        );
        assert_eq!(h.dead_count(), 0, "{at:?}: nothing buried");
    }
}

/// N6: at shutdown the engine closes before the runtime stops, dropping the
/// runs still waiting, and a delivery's task may run in between and find
/// its answer never came. That is the host stopping, not the run failing:
/// nothing is written, as in a crash: the message keeps its lease, the
/// lease runs out, and the next process delivers it. (Settled as "no
/// answer", it was retried at once with the attempt spent, and on the last
/// attempt buried.)
#[test]
fn n6_a_run_dropped_by_the_shutdown_is_not_an_answer() {
    for started in [false, true] {
        let mut h = Harness::new(
            "n6",
            vec![policy("mail", |p| {
                p.max_attempts = 2;
                p.lease = std::time::Duration::from_secs(30);
            })],
        );
        let id = leased(&mut h);
        if started {
            h.start_run(id);
        }
        assert_eq!(h.shut_down(), vec![], "the shutdown let go of it");
        let facts = h.facts(id);
        assert!(
            facts.written.is_empty() && facts.failed.is_empty(),
            "nothing written: {facts:?}"
        );
        let row = h.row(id).expect("not settled");
        assert_eq!((row.state, row.attempts), ("held", 1), "started {started}");
        h.check_attempts(id);
        assert_eq!(h.logs(), "", "nothing said: a fresh process");

        h.advance(31);
        h.tick();
        assert_eq!(h.facts(id).leases.len(), 2, "started {started}");
        h.take(id, 200);
        assert!(h.row(id).is_none(), "started {started}: delivered");
    }
}

// ------------------------------------------------------------ the clock

/// A clock set back between the lease and the settle: the settle does not
/// read the time to decide whether it holds the lease, so it still lands;
/// and a run that never answers is held longer in real time, not shorter.
#[test]
fn a_clock_set_back_between_lease_and_settle() {
    let lease = |p: &mut crate::config::QueuePolicy| p.lease = std::time::Duration::from_secs(30);
    let mut h = Harness::new("clock-back-settle", vec![policy("mail", lease)]);
    let start = h.now();
    let id = leased(&mut h);
    h.advance(-100);
    h.take(id, 200);
    assert!(h.row(id).is_none());
    assert_eq!(h.delivered(), 1);

    let id = leased(&mut h);
    h.start_run(id);
    h.advance(-100);
    // 31 s of real time later the host's clock is still before the lease's end.
    h.advance(31);
    h.tick();
    assert_eq!(
        h.facts(id).leases.len(),
        1,
        "held longer, not handed out again"
    );
    h.set_ms((start + 30) * 1000);
    h.tick();
    assert_eq!(
        h.facts(id).leases.len(),
        2,
        "handed out at host time t + lease"
    );
}

/// A clock set back between `sendAfter` and the tick: S8 is in host time,
/// so the message waits until the host's clock reaches `t + d`.
#[test]
fn a_clock_set_back_between_send_after_and_the_tick() {
    let mut h = Harness::new("clock-back-send", vec![policy("mail", |_| {})]);
    let t = h.now();
    let id = h.send_after("mail", "later", 10);
    h.advance(-50);
    for _ in 0..59 {
        h.tick();
        assert!(h.facts(id).leases.is_empty(), "not before host time t + 10");
        h.advance(1);
    }
    assert_eq!(h.now(), t + 9);
    h.tick();
    assert!(h.facts(id).leases.is_empty());
    h.advance(1);
    h.tick();
    assert_eq!(h.facts(id).leases.len(), 1);
}

// ------------------------------------------------------------ redeploy

/// A redeploy during a delivery: the running delivery settles under the
/// policy it was started with — its retry delay, its `max_attempts` — and
/// the next tick leases under the new one.
#[test]
fn a_running_delivery_keeps_its_policy_and_the_next_tick_takes_the_new_one() {
    use std::time::Duration;
    let old = policy("mail", |p| {
        p.max_attempts = 3;
        p.retry = Duration::from_secs(10);
        p.lease = Duration::from_secs(30);
    });
    let new = policy("mail", |p| {
        p.max_attempts = 5;
        p.retry = Duration::from_secs(100);
        p.lease = Duration::from_secs(300);
    });
    let mut h = Harness::new("redeploy", vec![old.clone()]);
    let id = leased(&mut h);
    h.queues = vec![new];
    h.take(id, 500);
    let row = h.row(id).unwrap();
    assert_eq!(row.due_unix, h.now() + 10, "the old policy's retry");

    h.advance(10);
    h.tick();
    let row = h.row(id).unwrap();
    assert_eq!(row.due_unix, h.now() + 300, "the new policy's lease");
    assert_eq!(h.facts(id).told_attempts, vec!["3", "5"]);

    // And a delivery started under `max_attempts = 1` buries on its 500
    // though the new version would retry it.
    let mut h = Harness::new(
        "redeploy-last",
        vec![policy("mail", |p| p.max_attempts = 1)],
    );
    let id = leased(&mut h);
    h.queues = vec![old];
    h.take(id, 500);
    assert_eq!(h.row(id).unwrap().state, "dead");
}
