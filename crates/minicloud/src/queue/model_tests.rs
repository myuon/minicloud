//! The queue store held to its contract (`docs/queue-properties.md`).
//!
//! The regressions below are the cases a model of the contract found, each
//! cut down to the calls that show it. One that fails on the code as it is,
//! by a finding the doc already lists, is ignored with that finding's id
//! until its fix lands.

use super::*;

const T: i64 = 1_000_000;
const BODY: usize = 1024;
const DEPTH: i64 = 100;
/// The reason the delivery loop gives `bury_spent` (`queue_host::OUTLIVED`).
const OUTLIVED: &str = "no answer within the lease";

fn ids(messages: &[Message]) -> Vec<i64> {
    messages.iter().map(|m| m.id).collect()
}

// ------------------------------------------------------------ regressions

/// S5, F1: a message waiting for its retry when `max_attempts` is lowered to
/// its `attempts` is handed out again, past the new bound.
///
/// Sent; leased under `M = 2` (attempts 1); the delivery fails and retries,
/// which ends the lease; a redeploy lowers `M` to 1; the retry comes due.
/// The tick's `bury_spent(M = 1)` leaves it alone, since it holds no lease,
/// and `lease` hands it out with attempts 2 > 1. The contract: no lease is
/// issued at `attempts >= M`, and the message is a dead letter at the next
/// tick, with a reason that is not "no answer within the lease".
#[test]
#[ignore = "F1: lease does not look at max_attempts; fixed when lease takes M"]
fn lowering_max_attempts_stops_a_message_waiting_for_its_retry() {
    let q = Queues::in_memory().unwrap();
    let id = q.send("a", "x", T, T, BODY, DEPTH).unwrap();
    // M = 2: the tick, then the lease.
    assert!(q.bury_spent("a", 2, T, OUTLIVED).unwrap().is_empty());
    let taken = q.lease(Some("a"), T, 1, 30).unwrap();
    assert_eq!((taken[0].id, taken[0].attempts), (id, 1));
    assert!(q.retry(id, taken[0].lease_id, T + 1, 10).unwrap());
    // Redeployed with M = 1; the retry comes due.
    let spent = q.bury_spent("a", 1, T + 11, OUTLIVED).unwrap();
    let leased = q.lease(Some("a"), T + 11, 1, 30).unwrap();
    assert!(
        leased.is_empty(),
        "leased at attempts {} with max_attempts 1",
        leased[0].attempts
    );
    assert_eq!(ids(&spent), [id], "a dead letter at the next tick");
    let dead = q.dead(None, 10).unwrap();
    assert_eq!(dead[0].0.id, id);
    assert_ne!(dead[0].1, OUTLIVED, "its lease did not run out");
}
