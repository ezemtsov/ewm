//! A two-ended transition, 0 to 1, run by a spring or driven by the fingers.
//!
//! Ported from niri's layout/mod.rs, the overview progress and its gesture functions.

use std::time::Duration;

use crate::animation::{Animation, Clock};
use crate::layout::strip::SwipeTracker;
use crate::rubber_band::RubberBand;
use crate::slide_spring;

/// Touchpad travel in unaccelerated units for a full transition.
const GESTURE_MOVEMENT: f64 = 300.;
const GESTURE_RUBBER_BAND: RubberBand = RubberBand {
    stiffness: 0.5,
    limit: 0.05,
};

#[derive(Debug)]
enum Progress {
    Animation(Animation),
    Gesture(Gesture),
    On,
}

#[derive(Debug)]
struct Gesture {
    tracker: SwipeTracker,
    start: f64,
    value: f64,
}

/// Off at 0, on at 1, and in between while a spring or the fingers move it.
#[derive(Debug)]
pub struct Transition {
    on: bool,
    progress: Option<Progress>,
    clock: Clock,
}

impl Transition {
    pub fn new(clock: Clock) -> Self {
        Self {
            on: false,
            progress: None,
            clock,
        }
    }

    /// The resting end, the one a swipe or spring in flight heads for.
    pub fn is_on(&self) -> bool {
        self.on
    }

    /// Away from off: on, mid-gesture or mid-animation.
    pub fn is_active(&self) -> bool {
        self.progress.is_some()
    }

    pub fn is_animating(&self) -> bool {
        matches!(self.progress, Some(Progress::Animation(_)))
    }

    pub fn progress(&self) -> Option<f64> {
        self.progress.as_ref().map(|p| match p {
            Progress::Animation(anim) => anim.value(),
            Progress::Gesture(gesture) => gesture.value,
            Progress::On => 1.,
        })
    }

    /// Where the transition is now, 0 when off.
    pub fn value(&self) -> f64 {
        self.progress().unwrap_or(0.)
    }

    pub fn toggle(&mut self) {
        self.on = !self.on;
        let from = self.value();
        let to = if self.on { 1. } else { 0. };
        self.progress = Some(Progress::Animation(Animation::new(
            self.clock.clone(),
            from,
            to,
            0.,
            slide_spring(),
        )));
    }

    /// Head for `on`, false when already there.
    pub fn set(&mut self, on: bool) -> bool {
        if self.on == on {
            return false;
        }
        self.toggle();
        true
    }

    pub fn gesture_begin(&mut self) {
        self.on = true;
        let value = self.value();
        self.progress = Some(Progress::Gesture(Gesture {
            tracker: SwipeTracker::new(),
            start: value,
            value,
        }));
    }

    /// Push a swipe delta. Positive heads for on. `Some(true)` when the value changed.
    pub fn gesture_update(&mut self, delta: f64, timestamp: Duration) -> Option<bool> {
        let Some(Progress::Gesture(gesture)) = &mut self.progress else {
            return None;
        };
        gesture.tracker.push(delta, timestamp);
        let pos = gesture.tracker.pos() / GESTURE_MOVEMENT;
        let value = GESTURE_RUBBER_BAND.clamp(0., 1., gesture.start + pos);
        if gesture.value == value {
            return Some(false);
        }
        gesture.value = value;
        Some(true)
    }

    /// Settle to whichever end the projected swipe is closer to, keeping its speed.
    pub fn gesture_end(&mut self) -> bool {
        let Some(Progress::Gesture(gesture)) = &mut self.progress else {
            return false;
        };
        gesture.tracker.push(0., self.clock.now());
        let mut velocity = gesture.tracker.velocity() / GESTURE_MOVEMENT;
        let current_pos = gesture.tracker.pos() / GESTURE_MOVEMENT;
        let pos = gesture.tracker.projected_end_pos() / GESTURE_MOVEMENT;
        let target = (gesture.start + pos).clamp(0., 1.).round();
        // Inside the rubber band only the speed that showed on screen carries over.
        velocity *= GESTURE_RUBBER_BAND.clamp_derivative(0., 1., gesture.start + current_pos);
        self.on = target == 1.;
        self.progress = Some(Progress::Animation(Animation::new(
            self.clock.clone(),
            gesture.value,
            target,
            velocity,
            slide_spring(),
        )));
        true
    }

    /// Settle a finished animation. Call once per tick.
    pub fn advance(&mut self) {
        if let Some(Progress::Animation(anim)) = &self.progress
            && anim.is_done()
        {
            self.progress = self.on.then_some(Progress::On);
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn rubber_band_clamp_is_monotonic_and_bounded(
            a in -3f64..=4.,
            b in -3f64..=4.,
        ) {
            let band = GESTURE_RUBBER_BAND;
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            let (clo, chi) = (band.clamp(0., 1., lo), band.clamp(0., 1., hi));
            prop_assert!(clo <= chi);
            prop_assert!(-band.limit < clo && chi < 1. + band.limit);
        }
    }
}
