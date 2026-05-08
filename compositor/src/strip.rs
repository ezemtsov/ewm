//! Per-output horizontal strip of Emacs frames + view-offset state machine.
//!
//! Data model adapted from niri's `src/layout/scrolling.rs`.
//!
//! Frame positions are not stored; they are derived from per-frame widths via
//! `column_x`. The viewport's left edge is `column_x(active_idx) +
//! view_offset.current()` -- focusing a frame moves the strip under the
//! viewport, not the other way around.

use std::time::Duration;

use crate::utils::get_monotonic_time;
use crate::LayoutEntry;

/// Default duration for view-offset slides between frames.
pub const DEFAULT_ANIM_DURATION: Duration = Duration::from_millis(250);

/// Ease-out-cubic interpolation between two scalar view offsets
/// (`y = 1 - (1 - t)^3` -- fast start, gentle settle).
#[derive(Debug, Clone)]
pub struct ViewAnimation {
    from: f64,
    to: f64,
    start: Duration,
    duration: Duration,
}

impl ViewAnimation {
    pub fn new(from: f64, to: f64, duration: Duration) -> Self {
        Self::new_at(get_monotonic_time(), from, to, duration)
    }

    fn new_at(start: Duration, from: f64, to: f64, duration: Duration) -> Self {
        Self {
            from,
            to,
            start,
            duration,
        }
    }

    pub fn target(&self) -> f64 {
        self.to
    }

    /// Shift both endpoints by `delta`.  The interpolated value at any time
    /// also shifts by `delta`, so the running animation continues smoothly
    /// in the new coordinate system instead of jumping.
    pub fn offset(&mut self, delta: f64) {
        self.from += delta;
        self.to += delta;
    }

    pub fn value(&self) -> f64 {
        self.value_at(get_monotonic_time())
    }

    pub fn is_done(&self) -> bool {
        self.is_done_at(get_monotonic_time())
    }

    fn value_at(&self, now: Duration) -> f64 {
        let elapsed = now.saturating_sub(self.start);
        let t = (elapsed.as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        self.from + (self.to - self.from) * eased
    }

    fn is_done_at(&self, now: Duration) -> bool {
        now.saturating_sub(self.start) >= self.duration
    }
}

/// View-offset state machine for one output's strip: `Static` at rest,
/// `Animation` mid-slide.
#[derive(Debug, Clone)]
pub enum ViewOffset {
    Static(f64),
    Animation(ViewAnimation),
}

impl Default for ViewOffset {
    fn default() -> Self {
        ViewOffset::Static(0.0)
    }
}

impl ViewOffset {
    /// Live offset for the current frame.
    pub fn current(&self) -> f64 {
        match self {
            ViewOffset::Static(o) => *o,
            ViewOffset::Animation(a) => a.value(),
        }
    }

    /// Resting offset (target of an in-flight animation, or current if static).
    pub fn target(&self) -> f64 {
        match self {
            ViewOffset::Static(o) => *o,
            ViewOffset::Animation(a) => a.target(),
        }
    }

    pub fn is_static(&self) -> bool {
        matches!(self, ViewOffset::Static(_))
    }

    pub fn is_animation_ongoing(&self) -> bool {
        matches!(self, ViewOffset::Animation(_))
    }

    /// Settle Animation -> Static once `is_done`. Call once per tick.
    pub fn advance(&mut self) {
        if let ViewOffset::Animation(a) = self {
            if a.is_done() {
                *self = ViewOffset::Static(a.target());
            }
        }
    }

    /// Apply a constant `delta` to the current state, preserving any running
    /// animation by re-anchoring its endpoints rather than clobbering them.
    /// Used when the coordinate system shifts (e.g. active column changes).
    pub fn offset(&mut self, delta: f64) {
        match self {
            ViewOffset::Static(x) => *x += delta,
            ViewOffset::Animation(a) => a.offset(delta),
        }
    }
}

/// One Emacs frame in the strip.
///
/// `surface_id` identifies the frame's own xdg_toplevel (the Emacs frame
/// chrome); `name` is the workspace label exposed to ext-workspace-v1.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Frame {
    pub name: String,
    pub surface_id: u64,
    pub width: f64,
    pub active: bool,
    pub surfaces: Vec<LayoutEntry>,
}

/// Per-output strip of frames laid out left-to-right.
///
/// Frame positions are derived from prefix-sum of widths via `column_x`.
/// The viewport's left edge is `column_x(active_idx) + view_offset.current()`
/// -- focusing a frame moves the strip under the viewport.
#[derive(Debug, Default, serde::Serialize)]
pub struct Strip {
    pub frames: Vec<Frame>,
    pub gap: f64,
    pub active_idx: usize,
    #[serde(skip)]
    pub view_offset: ViewOffset,
}

impl Strip {
    /// X position of frame `idx` relative to the strip origin.
    /// `column_x(0)` is always 0; subsequent columns accumulate `width + gap`.
    pub fn column_x(&self, idx: usize) -> f64 {
        self.frames[..idx.min(self.frames.len())]
            .iter()
            .map(|f| f.width + self.gap)
            .sum()
    }

    /// Strip-relative viewport left edge.
    pub fn view_pos(&self) -> f64 {
        self.column_x(self.active_idx) + self.view_offset.current()
    }

    /// Screen-relative x of frame `idx` (0 means flush with the working
    /// area's left edge). Equals `column_x(idx) - view_pos()`, which is the
    /// translation a renderer must apply to surfaces inside that frame.
    pub fn screen_x_of_frame(&self, idx: usize) -> f64 {
        self.column_x(idx) - self.view_pos()
    }

    /// Strip-relative viewport left edge once any in-flight animation settles.
    pub fn target_view_pos(&self) -> f64 {
        self.column_x(self.active_idx) + self.view_offset.target()
    }

    /// Animate `view_offset` from its current value to `target`. Collapses to
    /// `Static(target)` if the delta is already zero.
    pub fn animate_view_offset(&mut self, target: f64, duration: Duration) {
        let from = self.view_offset.current();
        if (from - target).abs() < f64::EPSILON {
            self.view_offset = ViewOffset::Static(target);
        } else {
            self.view_offset = ViewOffset::Animation(ViewAnimation::new(from, target, duration));
        }
    }

    /// Drive the state machine forward; settles a finished animation.
    pub fn advance(&mut self) {
        self.view_offset.advance();
    }

    /// Flat iterator over all surfaces across all frames, in declared order.
    pub fn surfaces(&self) -> impl Iterator<Item = &LayoutEntry> {
        self.frames.iter().flat_map(|f| f.surfaces.iter())
    }

    /// Mutable flat iterator over all surfaces across all frames.
    pub fn surfaces_mut(&mut self) -> impl Iterator<Item = &mut LayoutEntry> {
        self.frames.iter_mut().flat_map(|f| f.surfaces.iter_mut())
    }

    /// Iterator yielding `(frame_idx, &LayoutEntry)` pairs.
    pub fn surfaces_with_frame(&self) -> impl Iterator<Item = (usize, &LayoutEntry)> {
        self.frames
            .iter()
            .enumerate()
            .flat_map(|(i, f)| f.surfaces.iter().map(move |s| (i, s)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_with(widths: Vec<f64>, gap: f64, active_idx: usize) -> Strip {
        Strip {
            frames: widths
                .into_iter()
                .map(|w| Frame {
                    width: w,
                    ..Default::default()
                })
                .collect(),
            gap,
            active_idx,
            view_offset: ViewOffset::default(),
        }
    }

    #[test]
    fn column_x_uniform_no_gap() {
        let s = strip_with(vec![100.0, 100.0, 100.0], 0.0, 0);
        assert_eq!(s.column_x(0), 0.0);
        assert_eq!(s.column_x(1), 100.0);
        assert_eq!(s.column_x(2), 200.0);
    }

    #[test]
    fn column_x_with_gap_and_mixed_widths() {
        let s = strip_with(vec![100.0, 200.0, 50.0], 10.0, 0);
        assert_eq!(s.column_x(0), 0.0);
        assert_eq!(s.column_x(1), 110.0);
        assert_eq!(s.column_x(2), 320.0);
    }

    #[test]
    fn column_x_past_end_clamps() {
        let s = strip_with(vec![100.0], 0.0, 0);
        assert_eq!(s.column_x(5), 100.0);
    }

    #[test]
    fn view_pos_uses_active_column_plus_offset() {
        let mut s = strip_with(vec![100.0, 100.0], 0.0, 1);
        s.view_offset = ViewOffset::Static(15.0);
        assert_eq!(s.view_pos(), 115.0);
        assert_eq!(s.target_view_pos(), 115.0);
    }

    #[test]
    fn screen_x_of_active_frame_at_rest_is_zero() {
        let s = strip_with(vec![100.0, 200.0, 50.0], 10.0, 1);
        assert_eq!(s.screen_x_of_frame(s.active_idx), 0.0);
    }

    #[test]
    fn screen_x_of_neighbor_at_rest_is_relative_offset() {
        // active=1 (column_x=110), so frame 0 is at -110, frame 2 is at +210
        let s = strip_with(vec![100.0, 200.0, 50.0], 10.0, 1);
        assert_eq!(s.screen_x_of_frame(0), -110.0);
        assert_eq!(s.screen_x_of_frame(2), 210.0);
    }

    #[test]
    fn offset_shifts_static_value() {
        let mut vo = ViewOffset::Static(10.0);
        vo.offset(5.0);
        assert_eq!(vo.current(), 15.0);
        assert_eq!(vo.target(), 15.0);
    }

    #[test]
    fn offset_preserves_animation_value_at_each_instant() {
        // Build an animation, sample its midpoint, then offset by delta.
        // Both `from` and `to` shift by delta, so value_at(now) shifts by
        // exactly delta (no jump).
        let now = Duration::from_secs(0);
        let mut a = ViewAnimation::new_at(now, -100.0, 0.0, Duration::from_millis(100));
        let mid_before = a.value_at(now + Duration::from_millis(50));
        a.offset(40.0);
        let mid_after = a.value_at(now + Duration::from_millis(50));
        assert!((mid_after - (mid_before + 40.0)).abs() < 1e-9);
        assert_eq!(a.target(), 40.0);
    }

    #[test]
    fn animate_zero_distance_stays_static() {
        let mut s = strip_with(vec![100.0], 0.0, 0);
        s.view_offset = ViewOffset::Static(50.0);
        s.animate_view_offset(50.0, Duration::from_millis(100));
        assert!(s.view_offset.is_static());
        assert_eq!(s.view_offset.current(), 50.0);
    }

    #[test]
    fn animate_nonzero_starts_animation() {
        let mut s = strip_with(vec![100.0, 100.0], 0.0, 0);
        s.animate_view_offset(50.0, Duration::from_millis(100));
        assert!(s.view_offset.is_animation_ongoing());
        assert_eq!(s.view_offset.target(), 50.0);
    }

    #[test]
    fn view_animation_endpoints() {
        let now = Duration::from_secs(10);
        let a = ViewAnimation::new_at(now, 0.0, 100.0, Duration::from_millis(100));
        assert_eq!(a.value_at(now), 0.0);
        assert_eq!(a.value_at(now + Duration::from_millis(100)), 100.0);
        assert!(!a.is_done_at(now));
        assert!(a.is_done_at(now + Duration::from_millis(100)));
    }

    #[test]
    fn view_animation_eases_out_cubic_at_midpoint() {
        let now = Duration::from_secs(0);
        let a = ViewAnimation::new_at(now, 0.0, 100.0, Duration::from_millis(100));
        // y = 1 - (1 - 0.5)^3 = 0.875 -> 87.5
        let mid = a.value_at(now + Duration::from_millis(50));
        assert!((mid - 87.5).abs() < 1e-9);
    }

    #[test]
    fn view_offset_target_matches_animation_target() {
        let a = ViewAnimation::new_at(
            Duration::from_secs(0),
            10.0,
            70.0,
            Duration::from_millis(100),
        );
        let vo = ViewOffset::Animation(a);
        assert_eq!(vo.target(), 70.0);
        assert!(vo.is_animation_ongoing());
        assert!(!vo.is_static());
    }
}
