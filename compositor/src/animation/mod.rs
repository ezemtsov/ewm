//! Ported from niri's `animation/mod.rs`, without the config-driven constructors, the
//! deceleration kind, the clamped duration and every curve but linear and ease-out-cubic.

use std::time::Duration;

mod clock;
pub use clock::Clock;

mod spring;
pub use spring::{Spring, SpringParams};

#[derive(Debug, Clone)]
pub struct Animation {
    from: f64,
    to: f64,
    duration: Duration,
    start_time: Duration,
    clock: Clock,
    kind: Kind,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Easing { curve: Curve },
    Spring(Spring),
}

#[derive(Debug, Clone, Copy)]
enum Curve {
    Linear,
    EaseOutCubic,
}

impl Animation {
    pub fn new(
        clock: Clock,
        from: f64,
        to: f64,
        initial_velocity: f64,
        params: SpringParams,
    ) -> Self {
        let spring = Spring {
            from,
            to,
            initial_velocity,
            params,
        };
        Self::spring(clock, spring)
    }

    pub fn ease(clock: Clock, from: f64, to: f64, duration: Duration) -> Self {
        Self::easing(clock, from, to, duration, Curve::EaseOutCubic)
    }

    pub fn linear(clock: Clock, from: f64, to: f64, duration: Duration) -> Self {
        Self::easing(clock, from, to, duration, Curve::Linear)
    }

    fn easing(clock: Clock, from: f64, to: f64, duration: Duration, curve: Curve) -> Self {
        Self {
            from,
            to,
            duration,
            start_time: clock.now(),
            clock,
            kind: Kind::Easing { curve },
        }
    }

    pub fn spring(clock: Clock, spring: Spring) -> Self {
        Self {
            from: spring.from,
            to: spring.to,
            duration: spring.duration(),
            start_time: clock.now(),
            clock,
            kind: Kind::Spring(spring),
        }
    }

    /// Holds `from` for `delay` before the ramp begins.
    pub fn delayed(mut self, delay: Duration) -> Self {
        self.start_time += delay;
        self
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn is_done(&self) -> bool {
        if self.clock.should_complete_instantly() {
            return true;
        }

        self.clock.now() >= self.start_time + self.duration
    }

    pub fn value_at(&self, at: Duration) -> f64 {
        if at <= self.start_time {
            // Return from when at == start_time so that when the animations are off, the behavior
            // within a single event loop cycle (i.e. no time had passed since the start of an
            // animation) matches the behavior when the animations are on.
            return self.from;
        } else if self.start_time + self.duration <= at {
            return self.to;
        }

        if self.clock.should_complete_instantly() {
            return self.to;
        }

        let passed = at.saturating_sub(self.start_time);

        match self.kind {
            Kind::Easing { curve } => {
                let x = (passed.as_secs_f64() / self.duration.as_secs_f64()).clamp(0., 1.);
                curve.y(x) * (self.to - self.from) + self.from
            }
            Kind::Spring(spring) => {
                let value = spring.value_at(passed);

                // Protect against numerical instability.
                let range = (self.to - self.from) * 10.;
                let a = self.from - range;
                let b = self.to + range;
                if self.from <= self.to {
                    value.clamp(a, b)
                } else {
                    value.clamp(b, a)
                }
            }
        }
    }

    pub fn value(&self) -> f64 {
        self.value_at(self.clock.now())
    }

    pub fn to(&self) -> f64 {
        self.to
    }

    pub fn offset(&mut self, offset: f64) {
        self.from += offset;
        self.to += offset;

        if let Kind::Spring(spring) = &mut self.kind {
            spring.from += offset;
            spring.to += offset;
        }
    }
}

impl Curve {
    fn y(self, x: f64) -> f64 {
        match self {
            Curve::Linear => x,
            Curve::EaseOutCubic => 1. - (1. - x).powi(3),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn value_follows_the_curve(
            from in -1_000i32..=1_000,
            to in -1_000i32..=1_000,
            duration_ms in 1u64..=10_000,
            elapsed_ms in 0u64..=20_000,
        ) {
            let start = Duration::from_secs(10);
            let duration = Duration::from_millis(duration_ms);
            let from = f64::from(from);
            let to = f64::from(to);
            let clock = Clock::with_time(start);
            let eased = Animation::ease(clock.clone(), from, to, duration);
            let linear = Animation::linear(clock.clone(), from, to, duration);
            clock.set(start + Duration::from_millis(elapsed_ms));
            let t = (elapsed_ms as f64 / duration_ms as f64).clamp(0.0, 1.0);
            let expected_eased = from + (to - from) * (1.0 - (1.0 - t).powi(3));
            let expected_linear = from + (to - from) * t;

            prop_assert!((eased.value() - expected_eased).abs() < 1e-9);
            prop_assert!((linear.value() - expected_linear).abs() < 1e-9);
            prop_assert_eq!(eased.is_done(), elapsed_ms >= duration_ms);
        }

        #[test]
        fn offset_translates_samples(
            from in -1_000i32..=1_000,
            to in -1_000i32..=1_000,
            delta in -1_000i32..=1_000,
            duration_ms in 1u64..=10_000,
            elapsed_ms in 0u64..=20_000,
        ) {
            let start = Duration::from_secs(10);
            let duration = Duration::from_millis(duration_ms);
            let delta = f64::from(delta);
            let clock = Clock::with_time(start);
            let mut animation = Animation::ease(clock.clone(), f64::from(from), f64::from(to), duration);
            clock.set(start + Duration::from_millis(elapsed_ms));
            let before = animation.value();

            animation.offset(delta);

            prop_assert!((animation.value() - (before + delta)).abs() < 1e-9);
            prop_assert_eq!(animation.to(), f64::from(to) + delta);
        }
    }
}
