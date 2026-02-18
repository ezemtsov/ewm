//! Frame clock for accurate VBlank prediction.
//!
//! Ported from niri's `frame_clock.rs`. Tracks last presentation time and refresh
//! interval to predict the next VBlank, enabling accurate estimated VBlank timers
//! instead of fixed-interval timers that drift.

use std::num::NonZeroU64;
use std::time::Duration;

use crate::utils::get_monotonic_time;
use tracing::error;

#[derive(Debug)]
pub struct FrameClock {
    last_presentation_time: Option<Duration>,
    refresh_interval_ns: Option<NonZeroU64>,
}

impl FrameClock {
    pub fn new(refresh_interval: Option<Duration>) -> Self {
        let refresh_interval_ns = refresh_interval.and_then(|interval| {
            assert_eq!(interval.as_secs(), 0);
            NonZeroU64::new(interval.subsec_nanos().into())
        });

        Self {
            last_presentation_time: None,
            refresh_interval_ns,
        }
    }

    pub fn refresh_interval(&self) -> Option<Duration> {
        self.refresh_interval_ns
            .map(|r| Duration::from_nanos(r.get()))
    }

    /// Record that a frame was presented at the given time.
    pub fn presented(&mut self, presentation_time: Duration) {
        if presentation_time.is_zero() {
            return;
        }
        self.last_presentation_time = Some(presentation_time);
    }

    /// Predict the next presentation time based on the last VBlank and refresh interval.
    pub fn next_presentation_time(&self) -> Duration {
        let mut now = get_monotonic_time();

        let Some(refresh_interval_ns) = self.refresh_interval_ns else {
            return now;
        };
        let Some(last_presentation_time) = self.last_presentation_time else {
            return now;
        };

        let refresh_interval_ns = refresh_interval_ns.get();

        if now <= last_presentation_time {
            // Got an early VBlank.
            let orig_now = now;
            now += Duration::from_nanos(refresh_interval_ns);

            if now < last_presentation_time {
                error!(
                    now = ?orig_now,
                    ?last_presentation_time,
                    "got a 2+ early VBlank, {:?} until presentation",
                    last_presentation_time - now,
                );
                now = last_presentation_time + Duration::from_nanos(refresh_interval_ns);
            }
        }

        let since_last = now - last_presentation_time;
        let since_last_ns =
            since_last.as_secs() * 1_000_000_000 + u64::from(since_last.subsec_nanos());
        let to_next_ns = (since_last_ns / refresh_interval_ns + 1) * refresh_interval_ns;

        last_presentation_time + Duration::from_nanos(to_next_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_clock_no_interval() {
        let clock = FrameClock::new(None);
        // Without interval, returns approximately now
        let t = clock.next_presentation_time();
        assert!(!t.is_zero());
    }

    #[test]
    fn test_frame_clock_no_presentation() {
        let clock = FrameClock::new(Some(Duration::from_micros(16_667)));
        // Without presentation time, returns approximately now
        let t = clock.next_presentation_time();
        assert!(!t.is_zero());
    }

    #[test]
    fn test_frame_clock_predicts_next() {
        let mut clock = FrameClock::new(Some(Duration::from_millis(16)));
        let now = get_monotonic_time();
        // Simulate a recent presentation
        clock.presented(now - Duration::from_millis(5));
        let next = clock.next_presentation_time();
        // Next should be after now
        assert!(next >= now);
        // And within one refresh interval from now
        assert!(next <= now + Duration::from_millis(16));
    }

    #[test]
    fn test_refresh_interval() {
        let clock = FrameClock::new(Some(Duration::from_micros(16_667)));
        let interval = clock.refresh_interval().unwrap();
        assert_eq!(interval.as_micros(), 16_667);

        let clock = FrameClock::new(None);
        assert!(clock.refresh_interval().is_none());
    }
}
