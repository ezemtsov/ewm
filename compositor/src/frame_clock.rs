//! Frame clock for accurate VBlank prediction.
//!
//! Ported from niri's `frame_clock.rs`. Tracks last presentation time and refresh
//! interval to predict the next VBlank, enabling accurate estimated VBlank timers
//! instead of fixed-interval timers that drift.

use std::num::NonZeroU64;
use std::time::Duration;

use tracing::error;

use crate::utils::get_monotonic_time;

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
        self.next_presentation_time_at(get_monotonic_time())
    }

    fn next_presentation_time_at(&self, mut now: Duration) -> Duration {
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
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn missing_interval_or_presentation_returns_now(
            now_ns in 1u64..=10_000_000_000,
            refresh_ns in 1u64..=1_000_000_000,
            last_ns in 1u64..=10_000_000_000,
        ) {
            let now = Duration::from_nanos(now_ns);
            let mut no_interval = FrameClock::new(None);
            no_interval.presented(Duration::from_nanos(last_ns));
            prop_assert_eq!(no_interval.next_presentation_time_at(now), now);

            let no_presentation = FrameClock::new(Some(Duration::from_nanos(refresh_ns)));
            prop_assert_eq!(no_presentation.next_presentation_time_at(now), now);
        }

        #[test]
        fn refresh_interval_roundtrips(
            refresh_ns in 1u64..=999_999_999,
        ) {
            let refresh = Duration::from_nanos(refresh_ns);
            let clock = FrameClock::new(Some(refresh));

            prop_assert_eq!(clock.refresh_interval(), Some(refresh));
            prop_assert_eq!(FrameClock::new(None).refresh_interval(), None);
        }

        #[test]
        fn zero_presentation_time_is_ignored(
            now_ns in 1u64..=10_000_000_000,
            refresh_ns in 1u64..=1_000_000_000,
        ) {
            let mut clock = FrameClock::new(Some(Duration::from_nanos(refresh_ns)));
            clock.presented(Duration::ZERO);

            let now = Duration::from_nanos(now_ns);
            prop_assert_eq!(clock.next_presentation_time_at(now), now);
        }

        #[test]
        fn prediction_is_next_refresh_grid_after_now(
            now_ns in 1u64..=10_000_000_000,
            last_ns in 1u64..=10_000_000_000,
            refresh_ns in 1u64..=1_000_000_000,
        ) {
            let now = Duration::from_nanos(now_ns);
            let last = Duration::from_nanos(last_ns);
            let refresh = Duration::from_nanos(refresh_ns);
            let mut clock = FrameClock::new(Some(refresh));
            clock.presented(last);

            let next = clock.next_presentation_time_at(now);
            let adjusted_now = if now <= last {
                let one_refresh_later = now + refresh;
                if one_refresh_later < last {
                    last + refresh
                } else {
                    one_refresh_later
                }
            } else {
                now
            };
            let next_delta = next
                .checked_sub(last)
                .expect("next presentation must be after last presentation");

            prop_assert!(
                next > adjusted_now,
                "next={next:?}, adjusted_now={adjusted_now:?}, last={last:?}, refresh={refresh:?}",
            );
            prop_assert!(
                next <= adjusted_now + refresh,
                "next={next:?}, adjusted_now={adjusted_now:?}, refresh={refresh:?}",
            );
            prop_assert_eq!(next_delta.as_nanos() % u128::from(refresh_ns), 0);
        }
    }
}
