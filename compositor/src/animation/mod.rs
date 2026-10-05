//! Ported from niri's `animation/mod.rs`, without the config-driven constructors, the
//! deceleration kind and the cubic bezier curve.

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
    /// Time until the animation first reaches `to`.
    ///
    /// Best effort; not always exactly precise.
    clamped_duration: Duration,
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
pub enum Curve {
    Linear,
    EaseOutQuad,
    EaseOutCubic,
    EaseOutExpo,
}

impl Animation {
    pub fn ease(clock: Clock, from: f64, to: f64, duration: Duration, curve: Curve) -> Self {
        let kind = Kind::Easing { curve };

        Self {
            from,
            to,
            duration,
            // Our current curves never overshoot.
            clamped_duration: duration,
            start_time: clock.now(),
            clock,
            kind,
        }
    }

    pub fn spring(clock: Clock, spring: Spring) -> Self {
        let duration = spring.duration();
        let clamped_duration = spring.clamped_duration().unwrap_or(duration);
        let kind = Kind::Spring(spring);

        Self {
            from: spring.from,
            to: spring.to,
            duration,
            clamped_duration,
            start_time: clock.now(),
            clock,
            kind,
        }
    }

    /// Holds `from` for `delay` before the ramp begins.
    pub fn delayed(mut self, delay: Duration) -> Self {
        self.start_time += delay;
        self
    }

    pub fn is_done(&self) -> bool {
        if self.clock.should_complete_instantly() {
            return true;
        }

        self.clock.now() >= self.start_time + self.duration
    }

    pub fn is_clamped_done(&self) -> bool {
        if self.clock.should_complete_instantly() {
            return true;
        }

        self.clock.now() >= self.start_time + self.clamped_duration
    }

    pub fn value_at(&self, at: Duration) -> f64 {
        // Snap even a just-started animation: Emacs reads positions between redraws.
        if self.clock.should_complete_instantly() {
            return self.to;
        }

        if at <= self.start_time {
            return self.from;
        } else if self.start_time + self.duration <= at {
            return self.to;
        }

        let passed = at.saturating_sub(self.start_time);

        match self.kind {
            Kind::Easing { curve } => {
                let passed = passed.as_secs_f64();
                let total = self.duration.as_secs_f64();
                let x = (passed / total).clamp(0., 1.);
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

    /// Returns a value that stops at the target value after first reaching it.
    ///
    /// Best effort; not always exactly precise.
    pub fn clamped_value(&self) -> f64 {
        if self.is_clamped_done() {
            return self.to;
        }

        self.value()
    }

    pub fn to(&self) -> f64 {
        self.to
    }

    pub fn from(&self) -> f64 {
        self.from
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
    pub fn y(self, x: f64) -> f64 {
        match self {
            Curve::Linear => x,
            Curve::EaseOutQuad => 1. - (1. - x).powi(2),
            Curve::EaseOutCubic => 1. - (1. - x).powi(3),
            Curve::EaseOutExpo => 1. - 2f64.powf(-10. * x),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn ease_out_cubic_value_follows_the_curve(
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
            let animation = Animation::ease(clock.clone(), from, to, duration, Curve::EaseOutCubic);
            clock.set(start + Duration::from_millis(elapsed_ms));
            let t = (elapsed_ms as f64 / duration_ms as f64).clamp(0.0, 1.0);
            let eased = 1.0 - (1.0 - t).powi(3);
            let expected = from + (to - from) * eased;

            prop_assert!((animation.value() - expected).abs() < 1e-9);
            prop_assert_eq!(animation.is_done(), elapsed_ms >= duration_ms);
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
            let mut animation = Animation::ease(
                clock.clone(),
                f64::from(from),
                f64::from(to),
                duration,
                Curve::EaseOutCubic,
            );
            clock.set(start + Duration::from_millis(elapsed_ms));
            let before = animation.value();

            animation.offset(delta);

            prop_assert!((animation.value() - (before + delta)).abs() < 1e-9);
            prop_assert_eq!(animation.to(), f64::from(to) + delta);
        }
    }
}
