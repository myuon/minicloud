//! How often one caller may reach an app: a token bucket per app and caller.
//!
//! Without a limit, one client can fill an app's queue with requests and
//! leave every other client waiting behind them. The host checks a
//! [`Limiter`] at its door, before a request is queued, and answers
//! `429 Too Many Requests` (with a `Retry-After`) when [`Limiter::check`]
//! says [`Allowance::Refused`].
//!
//! # The bucket
//!
//! Each (app, caller) pair has a bucket that holds up to
//! [`RateLimit::burst`] tokens and gains [`RateLimit::requests`] tokens every
//! [`RateLimit::per`]. A request takes one token; with none to take, it is
//! refused. A caller who has been quiet therefore may send a burst, and a
//! caller who never stops is held to the steady rate.
//!
//! The tokens are an `f64`, topped up by the time that has passed since the
//! bucket was last looked at. Whole tokens would need a tick of their own,
//! and a limit of 60 a minute is a token a second, but 5 a minute is one every
//! twelve; counting in fractions needs no tick and has no granularity to
//! choose.
//!
//! # A bucket is made when it is first needed
//!
//! A new bucket starts full, so a caller's first request is never the one
//! that is refused. Nothing is created ahead of time: the callers are not
//! known until they come, and an app that is never called holds nothing.
//!
//! # Bounded
//!
//! The callers are whoever sends a request, so the buckets must not grow with
//! them. Past `max_buckets`, a [`Limiter`] sweeps inside [`Limiter::check`]:
//! first the buckets that have recovered to full, then, if that was not
//! enough, the ones used longest ago. See [`Limiter::new`].

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many buckets a [`Limiter::default`] keeps before it sweeps.
pub const DEFAULT_MAX_BUCKETS: usize = 10_000;

/// How close to a whole token counts as one. Tokens are a float, so a bucket
/// that has waited exactly as long as `Retry-After` said may hold
/// 0.9999999999 of a token; refusing it again for a rounding error would make
/// the promise in `Retry-After` false.
const EPSILON: f64 = 1e-9;

/// How often one caller may reach an app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    /// Tokens added over `per`.
    pub requests: u32,
    /// The window the tokens are added over.
    pub per: Duration,
    /// The most tokens a bucket may hold: how large a burst is allowed.
    pub burst: u32,
}

impl RateLimit {
    /// Tokens gained per second, or `None` for a limit that can never give
    /// one.
    ///
    /// `requests == 0` is a limit the caller should not pass (an app that
    /// takes no requests is not rate limited, it is switched off), and a zero
    /// `per` would divide by zero. Both are answered as a refusal instead of
    /// a panic or an infinite rate: failing closed on a misconfiguration is
    /// the safer way to be wrong at a door.
    fn rate(&self) -> Option<f64> {
        let secs = self.per.as_secs_f64();
        if self.requests == 0 || secs <= 0.0 {
            return None;
        }
        Some(f64::from(self.requests) / secs)
    }

    /// How many tokens a bucket holds when full. A `burst` of 0 would be a
    /// bucket that can never hold the one token a request needs, so it is
    /// read as 1: the smallest bucket that works.
    fn capacity(&self) -> f64 {
        f64::from(self.burst.max(1))
    }
}

/// What a check answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Allowance {
    /// There was a token, and it was taken.
    Allowed,
    /// There was not; this long until the next one, rounded up to a second
    /// (what `Retry-After` can say).
    Refused { retry_after_secs: u64 },
}

/// One caller's tokens for one app.
struct Bucket {
    /// Tokens held as of `last`, between 0 and the limit's capacity.
    tokens: f64,
    /// When the bucket was last looked at; also when it was last used, since
    /// every look is a request.
    last: Instant,
    /// The limit it was last checked against. The sweep has to know when a
    /// bucket is full, and "full" is a property of the limit, which the
    /// sweep is not given.
    limit: RateLimit,
}

impl Bucket {
    /// The tokens held at `now`: those at `last`, plus what has been added
    /// since, up to the capacity. A `now` before `last` adds nothing.
    fn tokens_at(&self, now: Instant, rate: f64) -> f64 {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        (self.tokens + elapsed * rate).min(self.limit.capacity())
    }

    /// Whether the bucket has recovered all the way at `now`.
    fn is_full_at(&self, now: Instant) -> bool {
        match self.limit.rate() {
            Some(rate) => self.tokens_at(now, rate) >= self.limit.capacity() - EPSILON,
            // A limit that never gives tokens is never stored (see `check_at`),
            // so this is unreachable in practice; a bucket like that holds
            // nothing worth keeping.
            None => true,
        }
    }
}

/// The buckets of every app, by app and caller.
pub struct Limiter {
    /// One lock around all the buckets: an update is a few arithmetic
    /// operations that finish in nanoseconds, so splitting the lock finer
    /// would cost more than the waiting it could save.
    buckets: Mutex<HashMap<(String, String), Bucket>>,
    /// The count past which [`Limiter::check`] sweeps.
    max_buckets: usize,
}

impl Default for Limiter {
    fn default() -> Limiter {
        Limiter::new(DEFAULT_MAX_BUCKETS)
    }
}

impl Limiter {
    /// A limiter that keeps at most `max_buckets` buckets.
    ///
    /// When a check leaves more than that, it first drops every bucket that
    /// has recovered to full. That loses nothing: a caller who comes back
    /// finds a new bucket, which starts full, the very state the dropped one
    /// had. Only if the buckets still outnumber the maximum are the ones used
    /// longest ago dropped as well, and those callers do get a fuller bucket
    /// than they had earned, which is the price of a bound. The sweep runs
    /// only when the count is past the maximum, so a check does not pay for
    /// it otherwise.
    pub fn new(max_buckets: usize) -> Limiter {
        Limiter {
            buckets: Mutex::new(HashMap::new()),
            max_buckets,
        }
    }

    /// Takes a token from `who`'s bucket for `app`, if there is one.
    ///
    /// This is `check_at` with the time of the call. The clock is
    /// a parameter of the inner function so that a test can move time by
    /// handing in a later `Instant` instead of sleeping for it.
    pub fn check(&self, app: &str, who: &str, limit: RateLimit) -> Allowance {
        self.check_at(app, who, limit, Instant::now())
    }

    fn check_at(&self, app: &str, who: &str, limit: RateLimit, now: Instant) -> Allowance {
        let Some(rate) = limit.rate() else {
            // No bucket is made: a refusal that costs memory per caller would
            // let a misconfigured app be filled with buckets as well.
            return Allowance::Refused {
                retry_after_secs: 1,
            };
        };

        let mut buckets = self.lock();
        let bucket = buckets
            .entry((app.to_owned(), who.to_owned()))
            .or_insert(Bucket {
                tokens: limit.capacity(),
                last: now,
                limit,
            });

        // The limit may have changed since the bucket was made; what it holds
        // is then cut down to the new capacity, never raised past it.
        bucket.limit = limit;
        bucket.tokens = bucket.tokens_at(now, rate);
        bucket.last = bucket.last.max(now);

        let answer = if bucket.tokens >= 1.0 - EPSILON {
            bucket.tokens = (bucket.tokens - 1.0).max(0.0);
            Allowance::Allowed
        } else {
            let wait = (1.0 - bucket.tokens) / rate;
            Allowance::Refused {
                // Rounded up, but not for a rounding error: 1 / (5 / 60) is
                // 12.000000000000002, and the answer is twelve. A float cast
                // saturates, so even an absurd wait is a number.
                retry_after_secs: ((wait - EPSILON).ceil() as u64).max(1),
            }
        };

        if buckets.len() > self.max_buckets {
            Self::sweep(&mut buckets, self.max_buckets, now);
        }
        answer
    }

    /// Drops the buckets that are full, then the oldest, until at most `max`
    /// are left.
    fn sweep(buckets: &mut HashMap<(String, String), Bucket>, max: usize, now: Instant) {
        buckets.retain(|_, bucket| !bucket.is_full_at(now));
        if buckets.len() <= max {
            return;
        }

        let mut by_age: Vec<(Instant, (String, String))> = buckets
            .iter()
            .map(|(key, bucket)| (bucket.last, key.clone()))
            .collect();
        by_age.sort();
        for (_, key) in by_age.into_iter().take(buckets.len() - max) {
            buckets.remove(&key);
        }
    }

    /// Drops every bucket of `app`, for when the app is deleted.
    pub fn forget(&self, app: &str) {
        self.lock().retain(|(name, _), _| name != app);
    }

    /// How many buckets are held.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no bucket is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The buckets, even if a thread panicked while holding the lock: every
    /// update leaves them whole, so a panic elsewhere is no reason to stop
    /// answering requests.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Bucket>> {
        self.buckets.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token a second, holding at most `burst`.
    fn per_second(burst: u32) -> RateLimit {
        RateLimit {
            requests: 60,
            per: Duration::from_secs(60),
            burst,
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_caller_gets_its_burst_and_the_next_request_is_refused() {
        let limiter = Limiter::default();
        let now = Instant::now();
        let limit = per_second(3);

        for _ in 0..3 {
            assert_eq!(
                limiter.check_at("app", "ann", limit, now),
                Allowance::Allowed
            );
        }
        assert_eq!(
            limiter.check_at("app", "ann", limit, now),
            Allowance::Refused {
                retry_after_secs: 1
            }
        );
    }

    #[test]
    fn tokens_come_back_with_time_and_never_past_the_burst() {
        let limiter = Limiter::default();
        let t0 = Instant::now();
        let limit = per_second(2);

        assert_eq!(
            limiter.check_at("app", "ann", limit, t0),
            Allowance::Allowed
        );
        assert_eq!(
            limiter.check_at("app", "ann", limit, t0),
            Allowance::Allowed
        );
        assert!(matches!(
            limiter.check_at("app", "ann", limit, t0),
            Allowance::Refused { .. }
        ));

        // One second is one token.
        let t1 = t0 + secs(1);
        assert_eq!(
            limiter.check_at("app", "ann", limit, t1),
            Allowance::Allowed
        );
        assert!(matches!(
            limiter.check_at("app", "ann", limit, t1),
            Allowance::Refused { .. }
        ));

        // A long quiet refills the bucket, but only to its burst of two.
        let t2 = t1 + secs(3600);
        assert_eq!(
            limiter.check_at("app", "ann", limit, t2),
            Allowance::Allowed
        );
        assert_eq!(
            limiter.check_at("app", "ann", limit, t2),
            Allowance::Allowed
        );
        assert!(matches!(
            limiter.check_at("app", "ann", limit, t2),
            Allowance::Refused { .. }
        ));
    }

    #[test]
    fn another_app_or_another_caller_has_a_bucket_of_its_own() {
        let limiter = Limiter::default();
        let now = Instant::now();
        let limit = per_second(1);

        assert_eq!(limiter.check_at("a", "ann", limit, now), Allowance::Allowed);
        assert!(matches!(
            limiter.check_at("a", "ann", limit, now),
            Allowance::Refused { .. }
        ));

        assert_eq!(limiter.check_at("b", "ann", limit, now), Allowance::Allowed);
        assert_eq!(limiter.check_at("a", "bob", limit, now), Allowance::Allowed);
        assert_eq!(limiter.len(), 3);
    }

    #[test]
    fn retry_after_is_at_least_a_second_and_waiting_that_long_is_enough() {
        let limiter = Limiter::default();
        let t0 = Instant::now();
        // Five a minute is a token every twelve seconds, which a float holds
        // only approximately.
        let limit = RateLimit {
            requests: 5,
            per: secs(60),
            burst: 1,
        };

        assert_eq!(
            limiter.check_at("app", "ann", limit, t0),
            Allowance::Allowed
        );
        let Allowance::Refused { retry_after_secs } = limiter.check_at("app", "ann", limit, t0)
        else {
            panic!("the bucket should be empty");
        };
        assert_eq!(retry_after_secs, 12);

        let t1 = t0 + secs(retry_after_secs);
        assert_eq!(
            limiter.check_at("app", "ann", limit, t1),
            Allowance::Allowed
        );

        // Half a token short still says a whole second, never zero.
        let fast = per_second(1);
        assert_eq!(limiter.check_at("app", "bob", fast, t0), Allowance::Allowed);
        let almost = t0 + Duration::from_millis(900);
        assert_eq!(
            limiter.check_at("app", "bob", fast, almost),
            Allowance::Refused {
                retry_after_secs: 1
            }
        );
    }

    #[test]
    fn a_limit_that_gives_no_tokens_always_refuses_and_keeps_nothing() {
        let limiter = Limiter::default();
        let now = Instant::now();
        let limit = RateLimit {
            requests: 0,
            per: secs(60),
            burst: 5,
        };

        assert_eq!(
            limiter.check_at("app", "ann", limit, now),
            Allowance::Refused {
                retry_after_secs: 1
            }
        );
        assert!(limiter.is_empty());
    }

    #[test]
    fn past_the_maximum_full_buckets_are_swept_and_a_bucket_in_use_stays() {
        let limiter = Limiter::new(4);
        let t0 = Instant::now();
        // Two tokens at a token every 30 seconds.
        let limit = RateLimit {
            requests: 2,
            per: secs(60),
            burst: 2,
        };

        // Three callers spend a token each, then go quiet.
        for who in ["a", "b", "c"] {
            assert_eq!(limiter.check_at("app", who, limit, t0), Allowance::Allowed);
        }

        // A minute on they have recovered. A busy caller spends both tokens,
        // and the fifth bucket is the one that tips the count over.
        let t1 = t0 + secs(60);
        assert_eq!(
            limiter.check_at("app", "busy", limit, t1),
            Allowance::Allowed
        );
        assert_eq!(
            limiter.check_at("app", "busy", limit, t1),
            Allowance::Allowed
        );
        assert_eq!(limiter.len(), 4);
        assert_eq!(
            limiter.check_at("app", "new", limit, t1),
            Allowance::Allowed
        );

        assert!(limiter.len() <= 4);
        assert_eq!(limiter.len(), 2);
        // Had the busy bucket been dropped, this would be a fresh, full one.
        assert!(matches!(
            limiter.check_at("app", "busy", limit, t1),
            Allowance::Refused { .. }
        ));
    }

    #[test]
    fn when_no_bucket_is_full_the_least_recently_used_are_dropped() {
        let limiter = Limiter::new(2);
        let t0 = Instant::now();
        let limit = per_second(1);

        for (i, who) in ["old", "mid", "new"].into_iter().enumerate() {
            let at = t0 + Duration::from_millis(i as u64);
            assert_eq!(limiter.check_at("app", who, limit, at), Allowance::Allowed);
        }
        assert_eq!(limiter.len(), 2);

        let at = t0 + Duration::from_millis(3);
        // The newest two were kept, with their tokens spent...
        assert!(matches!(
            limiter.check_at("app", "new", limit, at),
            Allowance::Refused { .. }
        ));
        // ...and the oldest starts over.
        assert_eq!(
            limiter.check_at("app", "old", limit, at),
            Allowance::Allowed
        );
    }

    #[test]
    fn forgetting_an_app_drops_its_buckets_and_only_its_buckets() {
        let limiter = Limiter::default();
        let now = Instant::now();
        let limit = per_second(1);

        for (app, who) in [("a", "ann"), ("a", "bob"), ("b", "ann")] {
            assert_eq!(limiter.check_at(app, who, limit, now), Allowance::Allowed);
        }
        assert_eq!(limiter.len(), 3);

        limiter.forget("a");
        assert_eq!(limiter.len(), 1);
        // The other app's bucket still has its token spent.
        assert!(matches!(
            limiter.check_at("b", "ann", limit, now),
            Allowance::Refused { .. }
        ));

        limiter.forget("b");
        assert!(limiter.is_empty());
    }
}
