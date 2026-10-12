//! The engine clock: the one place that reads the system clocks, and the tick pacer.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::warn;

/// The current monotonic time.
#[expect(
    clippy::disallowed_methods,
    reason = "consistency: the engine clock is the one reader of the monotonic clock"
)]
pub(crate) fn now() -> Instant {
    Instant::now()
}

/// Whole seconds since the Unix epoch by the wall clock, for checks of signed times off the tick
/// (JWT `exp`, `nbf`, `iat`). `None` if the system clock is set before 1970.
pub(crate) fn unix_now() -> Option<u64> {
    since_epoch().map(|since| since.as_secs())
}

/// Milliseconds since the Unix epoch by the wall clock, for times a session hands to the tick as
/// logged inputs (a player's join); 0 if the system clock is set before 1970.
pub(crate) fn unix_millis() -> i64 {
    since_epoch().map_or(0, |since| {
        i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
    })
}

/// The time since the Unix epoch by the wall clock; `None` before 1970.
#[expect(
    clippy::disallowed_methods,
    reason = "consistency: the engine clock is the one reader of the wall clock"
)]
fn since_epoch() -> Option<Duration> {
    SystemTime::now().duration_since(UNIX_EPOCH).ok()
}

/// Wall-clock Unix seconds for signed-token checks (JWT and offer identity times); before 1970
/// reads as 0, which fails every check.
pub(crate) fn token_now() -> i64 {
    unix_now().and_then(|s| i64::try_from(s).ok()).unwrap_or(0)
}

/// Blocks the calling thread until `deadline`; returns at once if it has passed. Only the tick
/// thread calls this, between ticks.
#[expect(
    clippy::disallowed_methods,
    reason = "performance: the tick thread idles until the next tick boundary"
)]
pub(crate) fn sleep_until(deadline: Instant) {
    let wait = deadline.saturating_duration_since(now());
    if !wait.is_zero() {
        std::thread::sleep(wait);
    }
}

/// Keeps ticks on a fixed schedule of one tick per `period`.
///
/// A tick that ends late starts the next one at once, which catches the schedule up. When the
/// schedule falls a whole period or more behind, the missed ticks are dropped and the schedule
/// restarts at the current time, so at most one tick is ever caught up.
#[derive(Debug)]
pub(crate) struct Pacer {
    period: Duration,
    next: Instant,
}

impl Pacer {
    /// A schedule whose first tick starts at `start`.
    pub(crate) fn new(period: Duration, start: Instant) -> Self {
        Self {
            period,
            next: start,
        }
    }

    /// Schedules the tick after the one that ended at `now` and returns when it starts.
    pub(crate) fn schedule(&mut self, now: Instant) -> Instant {
        let next = self.next.checked_add(self.period).unwrap_or(now);
        let behind = now.saturating_duration_since(next);
        self.next = if behind >= self.period {
            let dropped = behind.as_nanos() / self.period.as_nanos().max(1);
            warn!(?behind, dropped_ticks = %dropped, "ticks overran; dropping missed ticks");
            now
        } else {
            next
        };
        self.next
    }
}

/// The input passes (`input-pass-us`): periods of a fixed length from an epoch, numbered from 0.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Passes {
    epoch: Instant,
    period_us: std::num::NonZeroU64,
}

impl Passes {
    /// Passes of `period_us` microseconds from `epoch`.
    pub(crate) fn new(epoch: Instant, period_us: std::num::NonZeroU32) -> Self {
        Self {
            epoch,
            period_us: period_us.into(),
        }
    }

    /// When the passes start.
    pub(crate) fn epoch(&self) -> Instant {
        self.epoch
    }

    /// Microseconds from the epoch to `at`; 0 before it.
    pub(crate) fn micros(&self, at: Instant) -> u64 {
        let since = at.saturating_duration_since(self.epoch).as_micros();
        u64::try_from(since).unwrap_or(u64::MAX)
    }

    /// The pass `at` falls in; also how many passes have ended by `at`.
    pub(crate) fn pass_of(&self, at: Instant) -> u64 {
        self.pass_at(self.micros(at))
    }

    /// The pass the instant `micros` microseconds from the epoch falls in.
    pub(crate) fn pass_at(&self, micros: u64) -> u64 {
        micros / self.period_us
    }

    /// When pass `pass` ends: the instant the next one starts.
    pub(crate) fn end(&self, pass: u64) -> Instant {
        self.at(pass.saturating_add(1).saturating_mul(self.period_us.get()))
    }

    /// The instant `micros` microseconds from the epoch.
    pub(crate) fn at(&self, micros: u64) -> Instant {
        self.epoch
            .checked_add(Duration::from_micros(micros))
            .unwrap_or(self.epoch)
    }

    /// The first pass end at or after `micros` microseconds from the epoch, in microseconds
    /// from the epoch.
    pub(crate) fn end_from(&self, micros: u64) -> u64 {
        micros
            .div_ceil(self.period_us.get())
            .saturating_mul(self.period_us.get())
    }

    /// How many passes ended before `micros` microseconds from the epoch: a pass that ends right
    /// then is not counted.
    pub(crate) fn ended_before(&self, micros: u64) -> u64 {
        micros.div_ceil(self.period_us.get()).saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: Duration = Duration::from_millis(50);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_tick_that_ends_early_waits_for_the_next_boundary() {
        let base = now();
        let mut pacer = Pacer::new(PERIOD, base);
        assert_eq!(pacer.schedule(base + ms(10)), base + ms(50));
        assert_eq!(pacer.schedule(base + ms(70)), base + ms(100));
    }

    #[test]
    fn a_tick_late_by_less_than_a_period_is_caught_up_on_schedule() {
        let base = now();
        let mut pacer = Pacer::new(PERIOD, base);
        // The first tick takes 80 ms: the second starts at once, on its old slot.
        assert_eq!(pacer.schedule(base + ms(80)), base + ms(50));
        // The second ends at 85 ms: the third keeps its 100 ms slot.
        assert_eq!(pacer.schedule(base + ms(85)), base + ms(100));
    }

    #[test]
    fn a_tick_late_by_a_period_or_more_drops_missed_ticks() {
        let base = now();
        let mut pacer = Pacer::new(PERIOD, base);
        // The first tick takes 230 ms: slots 50..200 are missed; one tick runs at once.
        assert_eq!(pacer.schedule(base + ms(230)), base + ms(230));
        // The schedule restarts from there instead of running the missed ticks back to back.
        assert_eq!(pacer.schedule(base + ms(240)), base + ms(280));
    }

    #[test]
    fn slow_ticks_never_build_a_backlog_beyond_one_tick() {
        let base = now();
        let mut pacer = Pacer::new(PERIOD, base);
        let mut start = base;
        for _ in 0..1_000 {
            let end = start + ms(70);
            start = pacer.schedule(end);
            assert!(start <= end, "a late tick starts at once");
            assert!(end.saturating_duration_since(start) < PERIOD);
        }
    }

    #[test]
    fn a_pass_ends_where_the_next_begins() {
        let base = now();
        let passes = Passes::new(base, std::num::NonZeroU32::new(15_625).unwrap());
        let us = Duration::from_micros;
        assert_eq!(passes.pass_of(base), 0);
        assert_eq!(passes.pass_of(base + us(15_624)), 0);
        assert_eq!(passes.pass_of(base + us(15_625)), 1);
        assert_eq!(passes.end(0), base + us(15_625));
        assert_eq!(passes.end(3), base + us(62_500));
        assert_eq!(passes.pass_of(passes.end(3)), 4);
    }

    #[test]
    fn the_wall_clock_is_after_2020() {
        assert!(unix_now().is_some_and(|secs| secs > 1_577_836_800));
    }
}
