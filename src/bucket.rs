//! Token buckets for rate limits: whole tokens that refill at a fixed rate up to a capacity, on a
//! monotonic time the caller measures from a start of its own.

use std::time::Duration;

/// A bucket's capacity and refill rate. The caller keeps them, so a table of buckets holds only
/// their tokens and times.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rate {
    /// Tokens a full bucket holds.
    pub(crate) capacity: usize,
    /// Tokens added per second.
    pub(crate) per_second: usize,
}

/// A bucket's tokens and the time its refills have paid for.
#[derive(Debug)]
pub(crate) struct Bucket {
    tokens: usize,
    refilled: Duration,
}

impl Bucket {
    /// A full bucket at `now`.
    pub(crate) fn full(rate: Rate, now: Duration) -> Self {
        Self {
            tokens: rate.capacity,
            refilled: now,
        }
    }

    /// Takes `amount` tokens at `now`; `false`, taking none, when the bucket holds fewer. The
    /// time a refill pays for is moved forward exactly, so frequent calls lose no fraction of
    /// the rate.
    pub(crate) fn take(&mut self, amount: usize, rate: Rate, now: Duration) -> bool {
        const NANOS: u128 = 1_000_000_000;
        let per_second = u128::try_from(rate.per_second).unwrap_or(u128::MAX).max(1);
        let elapsed = now.saturating_sub(self.refilled).as_nanos();
        let earned = per_second.saturating_mul(elapsed) / NANOS;
        if self.tokens >= rate.capacity
            || earned >= u128::try_from(rate.capacity).unwrap_or(u128::MAX)
        {
            self.tokens = rate.capacity;
            self.refilled = now;
        } else if earned > 0 {
            let paid = u64::try_from(earned * NANOS / per_second).unwrap_or(u64::MAX);
            self.tokens = self
                .tokens
                .saturating_add(usize::try_from(earned).unwrap_or(usize::MAX))
                .min(rate.capacity);
            self.refilled = self
                .refilled
                .checked_add(Duration::from_nanos(paid))
                .unwrap_or(now);
        }
        match self.tokens.checked_sub(amount) {
            Some(rest) => {
                self.tokens = rest;
                true
            }
            None => false,
        }
    }

    /// The time the refills have paid for: at most one token's time before the last
    /// [`take`](Self::take), so a table can drop buckets left alone for long.
    pub(crate) fn refilled(&self) -> Duration {
        self.refilled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: Rate = Rate {
        capacity: 1000,
        per_second: 250,
    };

    #[test]
    fn a_bucket_refills_at_its_rate_up_to_its_capacity() {
        let start = Duration::from_secs(5);
        let mut bucket = Bucket::full(RATE, start);
        assert!(bucket.take(1000, RATE, start));
        assert!(!bucket.take(1, RATE, start));
        let second = start + Duration::from_secs(1);
        assert!(bucket.take(250, RATE, second));
        assert!(!bucket.take(1, RATE, second));
        let later = start + Duration::from_secs(100);
        assert!(bucket.take(1000, RATE, later));
        assert!(!bucket.take(1, RATE, later));
    }

    #[test]
    fn takes_spaced_finer_than_a_token_lose_no_fraction() {
        let mut bucket = Bucket::full(RATE, Duration::ZERO);
        assert!(bucket.take(1000, RATE, Duration::ZERO));
        // A token every 4 ms; a take every 1 ms earns one every fourth call.
        let taken = (1..=1000_u64)
            .filter(|&ms| bucket.take(1, RATE, Duration::from_millis(ms)))
            .count();
        assert_eq!(taken, 250);
    }
}
