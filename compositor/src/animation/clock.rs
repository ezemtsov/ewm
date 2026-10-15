//! Ported from niri's `animation/clock.rs`, without the rate adjustment, with a test freeze.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::utils::get_monotonic_time;

const UNSET: u64 = u64::MAX;

/// Shareable lazy clock.
///
/// The clock will fetch the time once and then retain it until explicitly cleared with
/// [`Clock::clear`].
#[derive(Debug, Clone)]
pub struct Clock {
    inner: Arc<Inner>,
}

/// Atomics keep the clock `Send` for frames crossing the command queue.
#[derive(Debug)]
struct Inner {
    time: AtomicU64,
    complete_instantly: AtomicBool,
    frozen: AtomicBool,
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
                frozen: AtomicBool::new(false),
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

    /// Moves the clock to the frame being rendered, unless frozen.
    pub fn set_presentation_time(&self, time: Duration) {
        if self.inner.frozen.load(Ordering::Relaxed) {
            return;
        }
        self.set(time);
    }

    /// Clears the stored time so it's re-fetched again next, unless frozen.
    pub fn clear(&self) {
        if self.inner.frozen.load(Ordering::Relaxed) {
            return;
        }
        self.inner.time.store(UNSET, Ordering::Relaxed);
    }

    /// Holds the clock at `time` against the loop, only `set` moves it then.
    pub fn freeze(&self, time: Duration) {
        self.set(time);
        self.inner.frozen.store(true, Ordering::Relaxed);
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
    time.as_nanos().min(u128::from(UNSET - 1)) as u64
}
