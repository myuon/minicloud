//! How long requests took: a histogram of durations, per app or for the
//! server.
//!
//! The counters in [`crate::stats`] say how many requests were served, never
//! how slow they were; an average hides the slow ones, and a p99 cannot be
//! recovered from a sum. A [`Latency`] keeps how many requests fell into each
//! of a fixed set of duration ranges, which is enough to estimate any
//! quantile afterwards, and is the shape Prometheus expects.
//!
//! Like the other counters, everything is a relaxed atomic: a histogram is
//! read by a person or a test, never used to decide anything, so no ordering
//! between its fields is promised. A read made while a request is being
//! recorded may see its bucket and not yet its `count`; [`Snapshot`] is built
//! so that this does not matter.
//!
//! # Fixed buckets
//!
//! The bounds are the same for every histogram and are not configurable.
//! Histograms that share their bounds can be added together and subtracted
//! from one another bucket by bucket, which is what makes [`Snapshot::since`]
//! possible at all. The bounds run from 100 microseconds to 10 seconds in a
//! 1-2.5-5 progression: fine enough to tell a fast handler from a slow one,
//! and wide enough to hold a run that was stopped by its deadline.

use std::sync::atomic::{AtomicU64, Ordering};

/// The upper bound of each bucket, in microseconds. The last bucket is
/// everything above the last bound.
pub const BOUNDS_US: [u64; 16] = [
    100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000,
    1_000_000, 2_500_000, 5_000_000, 10_000_000,
];

/// How many buckets there are: one per bound, and one for everything above
/// the last.
const BUCKETS: usize = BOUNDS_US.len() + 1;

/// How long requests took: a cumulative histogram, as Prometheus means one.
#[derive(Default)]
pub struct Latency {
    /// One counter per bound, and one more for everything above the last.
    /// Each counts only the requests that fell in its own range; the
    /// cumulative form is made on the way out by [`Latency::buckets`].
    buckets: [AtomicU64; BUCKETS],
    /// The durations, summed, in microseconds.
    sum_us: AtomicU64,
    /// The requests observed.
    count: AtomicU64,
}

impl Latency {
    /// Notes a request that took `micros` microseconds.
    ///
    /// A value equal to a bound belongs to that bound's bucket (the bounds
    /// are "less than or equal", as `le` is in Prometheus).
    pub fn observe(&self, micros: u64) {
        // A linear scan: there are only 16 bounds, so it is as quick as a
        // binary search and has nothing to get wrong.
        let index = BOUNDS_US
            .iter()
            .position(|&bound| micros <= bound)
            .unwrap_or(BOUNDS_US.len());
        self.buckets[index].fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(micros, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// The requests observed.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// The durations of every request observed, summed, in microseconds.
    pub fn sum_us(&self) -> u64 {
        self.sum_us.load(Ordering::Relaxed)
    }

    /// The mean duration in microseconds, rounded down, or `None` when
    /// nothing has been observed (there is no mean of nothing, and 0 would
    /// read as "instant").
    pub fn mean_us(&self) -> Option<u64> {
        // A request in the middle of `observe` may be in the sum and not
        // yet the count; the mean is then off by one request, and no more.
        let sum = self.sum_us();
        let count = self.count();
        sum.checked_div(count)
    }

    /// An estimate of the duration below which a fraction `q` of the requests
    /// fell, in microseconds. See [`Snapshot::quantile_us`] for how it is
    /// worked out and how far to trust it.
    pub fn quantile_us(&self, q: f64) -> Option<u64> {
        self.snapshot().quantile_us(q)
    }

    /// The histogram in the form Prometheus uses: for each bound, in
    /// ascending order, the number of requests that took at most that long.
    ///
    /// The counts are **cumulative**: each includes every request counted in
    /// the buckets before it, so they never decrease. The last entry has no
    /// bound (`None`, the `+Inf` of Prometheus) and counts everything.
    pub fn buckets(&self) -> Vec<(Option<u64>, u64)> {
        let snapshot = self.snapshot();
        let mut running = 0u64;
        let mut out = Vec::with_capacity(BUCKETS);
        for (index, n) in snapshot.buckets.iter().enumerate() {
            running += n;
            out.push((BOUNDS_US.get(index).copied(), running));
        }
        out
    }

    /// The values at this moment, as plain numbers that can be kept and
    /// compared later.
    ///
    /// The fields are read one after another, not together, so a request
    /// being recorded at that moment may be in one and not another. The
    /// difference is one request in a field, which a later snapshot makes up.
    pub fn snapshot(&self) -> Snapshot {
        let mut buckets = [0u64; BUCKETS];
        for (slot, counter) in buckets.iter_mut().zip(&self.buckets) {
            *slot = counter.load(Ordering::Relaxed);
        }
        Snapshot {
            count: self.count(),
            sum_us: self.sum_us(),
            buckets,
        }
    }
}

/// A [`Latency`] at one moment, as plain numbers.
///
/// # Why it can be subtracted
///
/// A histogram only ever grows, so on its own it describes all of time since
/// the host started: after a week, a bad minute moves its p99 by nothing. To
/// see how requests are doing *now*, a recorder takes a snapshot every
/// minute and keeps the difference from the one before ([`Snapshot::since`]).
/// That difference is itself a histogram, of the requests of that minute,
/// and answers [`Snapshot::quantile_us`] like any other. A mean or a
/// percentile could not be handled this way: two p99s cannot be subtracted
/// into the p99 of what lies between them, but two sets of bucket counts can.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The requests observed.
    pub count: u64,
    /// The durations, summed, in microseconds.
    pub sum_us: u64,
    /// The requests in each bucket, **not** cumulative: the same layout as
    /// [`BOUNDS_US`], and one more for everything above the last bound.
    pub buckets: [u64; BOUNDS_US.len() + 1],
}

impl Snapshot {
    /// An estimate of the duration below which a fraction `q` of the requests
    /// fell, in microseconds, or `None` when there are none (or `q` is NaN).
    /// `q` is between 0.0 and 1.0, and is held to that range if it is not.
    ///
    /// **This is an estimate, not a measurement.** The histogram does not
    /// remember any request, only which range it fell in, so the answer is
    /// guessed from where in its range the wanted request must be. The way
    /// is the one of Prometheus's `histogram_quantile`: with `rank = q *
    /// total`, find the first bucket whose cumulative count reaches `rank`,
    /// and assume the requests in it are spread evenly between its bounds,
    /// so that
    ///
    /// ```text
    /// lower + (upper - lower) * (rank - before) / in_bucket
    /// ```
    ///
    /// where `before` is the count of the buckets under it and `in_bucket`
    /// is the count in it. The first bucket's `lower` is 0. The answer is
    /// always inside the bucket, so it is wrong by at most the bucket's
    /// width, and the wider the bucket the less it says.
    ///
    /// The last bucket has no upper bound to interpolate towards. A rank that
    /// falls there is answered with the last bound, 10 seconds, which is a
    /// **lower bound**: the true value is at least that, by an amount the
    /// histogram cannot tell.
    ///
    /// The buckets are used and not `count`, so that a snapshot taken in the
    /// middle of a request still agrees with itself.
    pub fn quantile_us(&self, q: f64) -> Option<u64> {
        if q.is_nan() {
            return None;
        }
        let total: u64 = self.buckets.iter().sum();
        if total == 0 {
            return None;
        }
        let rank = q.clamp(0.0, 1.0) * total as f64;
        let mut before = 0u64;
        for (index, &n) in self.buckets.iter().enumerate() {
            // An empty bucket is skipped, even at rank 0, so that the
            // division below never has nothing to divide by.
            if n > 0 && (before + n) as f64 >= rank {
                let Some(&upper) = BOUNDS_US.get(index) else {
                    return Some(BOUNDS_US[BOUNDS_US.len() - 1]);
                };
                let lower = if index == 0 { 0 } else { BOUNDS_US[index - 1] };
                let fraction = (rank - before as f64) / n as f64;
                let estimate = lower as f64 + (upper - lower) as f64 * fraction;
                return Some((estimate.round() as u64).clamp(lower, upper));
            }
            before += n;
        }
        // Only reached if rounding put `rank` a hair above the total.
        Some(BOUNDS_US[BOUNDS_US.len() - 1])
    }

    /// What was observed after `earlier` and up to this snapshot: every
    /// bucket, the count and the sum, subtracted.
    ///
    /// Each subtraction saturates at 0. Two snapshots of the same histogram
    /// never need it, but if `earlier` is the later one, or comes from a
    /// histogram that was started over (the host restarted), a wrapped
    /// subtraction would be a count of billions; an empty difference is the
    /// honest answer for a period that cannot be told.
    pub fn since(&self, earlier: &Snapshot) -> Snapshot {
        let mut buckets = [0u64; BUCKETS];
        for (slot, (now, then)) in buckets
            .iter_mut()
            .zip(self.buckets.iter().zip(&earlier.buckets))
        {
            *slot = now.saturating_sub(*then);
        }
        Snapshot {
            count: self.count.saturating_sub(earlier.count),
            sum_us: self.sum_us.saturating_sub(earlier.sum_us),
            buckets,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_histogram_has_no_requests_and_no_quantile_or_mean() {
        let latency = Latency::default();
        assert_eq!(latency.count(), 0);
        assert_eq!(latency.sum_us(), 0);
        assert_eq!(latency.mean_us(), None);
        assert_eq!(latency.quantile_us(0.5), None);
        assert_eq!(latency.quantile_us(0.99), None);
        assert_eq!(latency.snapshot(), Snapshot::default());
    }

    #[test]
    fn a_value_exactly_on_a_bound_falls_in_that_bounds_bucket() {
        let latency = Latency::default();
        latency.observe(100);
        let snapshot = latency.snapshot();
        assert_eq!(snapshot.buckets[0], 1);
        assert_eq!(snapshot.buckets.iter().sum::<u64>(), 1);

        latency.observe(101);
        let snapshot = latency.snapshot();
        assert_eq!(snapshot.buckets[0], 1);
        assert_eq!(snapshot.buckets[1], 1);
    }

    #[test]
    fn ninety_fast_and_ten_slow_requests_put_p50_near_the_fast_and_p99_near_the_slow() {
        let latency = Latency::default();
        for _ in 0..90 {
            latency.observe(1_000);
        }
        for _ in 0..10 {
            latency.observe(1_000_000);
        }
        // Interpolation puts the estimate somewhere inside the bucket, not
        // on the value, so only the bucket is asserted.
        let p50 = latency.quantile_us(0.5).unwrap();
        assert!((500..=1_000).contains(&p50), "p50 was {p50}");
        let p99 = latency.quantile_us(0.99).unwrap();
        assert!((500_000..=1_000_000).contains(&p99), "p99 was {p99}");
        assert!(p50 < p99);
    }

    #[test]
    fn a_quantile_is_interpolated_inside_its_bucket() {
        let latency = Latency::default();
        // Four requests in (500, 1000]: the median is halfway through it.
        for _ in 0..4 {
            latency.observe(900);
        }
        assert_eq!(latency.quantile_us(0.5), Some(750));
        assert_eq!(latency.quantile_us(1.0), Some(1_000));
        assert_eq!(latency.quantile_us(0.0), Some(500));
        // Out of range is held to the range; NaN is no answer.
        assert_eq!(latency.quantile_us(7.0), Some(1_000));
        assert_eq!(latency.quantile_us(-1.0), Some(500));
        assert_eq!(latency.quantile_us(f64::NAN), None);
    }

    #[test]
    fn a_value_past_the_last_bound_lands_in_the_last_bucket_and_the_quantile_is_the_last_bound() {
        let last = BOUNDS_US[BOUNDS_US.len() - 1];
        let latency = Latency::default();
        latency.observe(last + 1);
        latency.observe(u64::MAX / 4);
        let snapshot = latency.snapshot();
        assert_eq!(snapshot.buckets[BOUNDS_US.len()], 2);
        assert_eq!(snapshot.buckets.iter().sum::<u64>(), 2);
        assert!(latency.quantile_us(0.5).unwrap() >= last);
        assert_eq!(latency.quantile_us(0.99), Some(last));
        assert_eq!(latency.quantile_us(1.0), Some(last));
    }

    #[test]
    fn the_buckets_are_cumulative_never_decrease_and_end_with_the_total() {
        let latency = Latency::default();
        for micros in [50, 100, 300, 300, 5_000, 2_000_000, 99_000_000] {
            latency.observe(micros);
        }
        let buckets = latency.buckets();
        assert_eq!(buckets.len(), BOUNDS_US.len() + 1);
        let bounds: Vec<Option<u64>> = buckets.iter().map(|(bound, _)| *bound).collect();
        let mut expected: Vec<Option<u64>> = BOUNDS_US.iter().map(|&b| Some(b)).collect();
        expected.push(None);
        assert_eq!(bounds, expected);
        assert!(buckets.windows(2).all(|pair| pair[0].1 <= pair[1].1));
        assert_eq!(buckets[0], (Some(100), 2));
        assert_eq!(buckets[2], (Some(500), 4));
        assert_eq!(buckets[BOUNDS_US.len()], (None, 7));
        assert_eq!(buckets[BOUNDS_US.len()].1, latency.count());
    }

    #[test]
    fn since_subtracts_every_bucket_and_never_goes_below_zero() {
        let latency = Latency::default();
        latency.observe(100);
        latency.observe(1_000);
        let earlier = latency.snapshot();
        latency.observe(1_000);
        latency.observe(2_000_000);
        let later = latency.snapshot();

        let delta = later.since(&earlier);
        assert_eq!(delta.count, 2);
        assert_eq!(delta.sum_us, 2_001_000);
        assert_eq!(delta.buckets[0], 0);
        assert_eq!(delta.buckets[3], 1);
        assert_eq!(delta.buckets[13], 1);
        assert_eq!(delta.buckets.iter().sum::<u64>(), 2);
        // The difference answers quantiles for the period alone.
        assert!(delta.quantile_us(0.99).unwrap() > 1_000_000);

        // The wrong way round, nothing wraps.
        let backwards = earlier.since(&later);
        assert_eq!(backwards, Snapshot::default());
        assert_eq!(backwards.quantile_us(0.5), None);
    }

    #[test]
    fn the_mean_is_the_sum_over_the_count() {
        let latency = Latency::default();
        latency.observe(100);
        latency.observe(200);
        latency.observe(600);
        assert_eq!(latency.count(), 3);
        assert_eq!(latency.sum_us(), 900);
        assert_eq!(latency.mean_us(), Some(300));
    }
}
