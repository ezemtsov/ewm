//! Ported from niri's `animation/clock.rs`, without the rate adjustment.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::utils::get_monotonic_time;

const UNSET: u64 = u64::MAX;

/// Shareable lazy clock.
///
/// The clock will fetch the time once and then retain it until explicitly cleared with
/// [`Clock::clear`]. Atomics keep it `Send` for frames crossing the command queue.
#[derive(Debug, Clone)]
pub struct Clock {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    time: AtomicU64,
    complete_instantly: AtomicBool,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new(UNSET)
    }
}

impl Clock {
    fn new(time: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                time: AtomicU64::new(time),
                complete_instantly: AtomicBool::new(false),
            }),
        }
    }

    /// Creates a new clock with the given time.
    pub fn with_time(time: Duration) -> Self {
        Self::new(nanos(time))
    }

    /// Returns the current time.
    pub fn now(&self) -> Duration {
        let time = self.inner.time.load(Ordering::Relaxed);
        if time != UNSET {
            return Duration::from_nanos(time);
        }
        let now = get_monotonic_time();
        self.inner.time.store(nanos(now), Ordering::Relaxed);
        now
    }

    /// Sets the clock time.
    pub fn set(&self, time: Duration) {
        self.inner.time.store(nanos(time), Ordering::Relaxed);
    }

    /// Clears the stored time so it's re-fetched again next.
    pub fn clear(&self) {
        self.inner.time.store(UNSET, Ordering::Relaxed);
    }

    /// Returns whether animations should complete instantly.
    pub fn should_complete_instantly(&self) -> bool {
        self.inner.complete_instantly.load(Ordering::Relaxed)
    }

    /// Sets whether animations should complete instantly.
    pub fn set_complete_instantly(&self, value: bool) {
        self.inner
            .complete_instantly
            .store(value, Ordering::Relaxed);
    }
}

fn nanos(time: Duration) -> u64 {
    u64::try_from(time.as_nanos()).map_or(UNSET - 1, |n| n.min(UNSET - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_clock() {
        let clock = Clock::with_time(Duration::ZERO);
        assert_eq!(clock.now(), Duration::ZERO);

        clock.set(Duration::from_millis(100));
        assert_eq!(clock.now(), Duration::from_millis(100));

        clock.set(Duration::from_millis(200));
        assert_eq!(clock.now(), Duration::from_millis(200));
    }
}
