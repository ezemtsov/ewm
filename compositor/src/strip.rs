//! Per-output horizontal strip of Emacs frames + view-offset state machine.
//!
//! Frame positions derive from per-frame widths via `column_x`; the viewport's
//! left edge is `column_x(active_idx) + view_offset.current()`.

use std::collections::VecDeque;
use std::time::Duration;

use crate::utils::get_monotonic_time;
use crate::{LayoutEntry, ViewId};

/// Default duration for view-offset slides between frames.
pub const DEFAULT_ANIM_DURATION: Duration = Duration::from_millis(250);

/// Fade-in duration for a surface's first buffer-bearing commit.
pub const OPEN_ANIM_DURATION: Duration = Duration::from_millis(150);

/// Touchpad swipe distance mapping to one working-area-width of strip movement.
pub const VIEW_GESTURE_WORKING_AREA_MOVEMENT: f64 = 1200.0;

/// Velocity history window for swipe end-position projection.
const SWIPE_HISTORY_LIMIT: Duration = Duration::from_millis(150);

/// Touchpad scroll deceleration (matches GNOME Shell).
const SWIPE_DECELERATION: f64 = 0.997;

/// Rolling (delta, timestamp) history backing `velocity` and `projected_end_pos`.
#[derive(Debug, Clone, Default)]
pub struct SwipeTracker {
    history: VecDeque<(f64, Duration)>,
    pos: f64,
}

impl SwipeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, delta: f64, timestamp: Duration) {
        if let Some(&(_, last)) = self.history.back() {
            if timestamp < last {
                return;
            }
        }
        self.history.push_back((delta, timestamp));
        self.pos += delta;
        while let Some(&(_, first)) = self.history.front() {
            let &(_, last) = self.history.back().unwrap();
            if last <= first + SWIPE_HISTORY_LIMIT {
                break;
            }
            self.history.pop_front();
        }
    }

    pub fn pos(&self) -> f64 {
        self.pos
    }

    /// Average velocity over the retained history window.
    pub fn velocity(&self) -> f64 {
        let (Some(&(_, first)), Some(&(_, last))) = (self.history.front(), self.history.back())
        else {
            return 0.0;
        };
        let total_time = (last - first).as_secs_f64();
        if total_time == 0.0 {
            return 0.0;
        }
        self.history.iter().map(|&(d, _)| d).sum::<f64>() / total_time
    }

    /// Position the gesture would settle at after exponential deceleration.
    pub fn projected_end_pos(&self) -> f64 {
        self.pos - self.velocity() / (1000.0 * SWIPE_DECELERATION.ln())
    }
}

/// Ease-out-cubic interpolation between two scalar view offsets.
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

    /// Animation whose ramp begins after `delay`; `value()` stays at `from` until then.
    pub fn delayed(delay: Duration, from: f64, to: f64, duration: Duration) -> Self {
        Self::new_at(get_monotonic_time() + delay, from, to, duration)
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

    /// Shift both endpoints by `delta` so the running animation re-anchors without jumping.
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

/// In-progress touchpad swipe; `current = anchor + tracker.pos()` to avoid drift.
#[derive(Debug, Clone)]
pub struct ViewGesture {
    pub current: f64,
    pub anchor: f64,
    pub tracker: SwipeTracker,
}

/// View-offset state machine: `Static` at rest, `Animation` mid-slide, `Gesture` during swipe.
#[derive(Debug, Clone)]
pub enum ViewOffset {
    Static(f64),
    Animation(ViewAnimation),
    Gesture(ViewGesture),
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
            ViewOffset::Gesture(g) => g.current,
        }
    }

    /// Resting offset (target of an in-flight animation, or current if static).
    pub fn target(&self) -> f64 {
        match self {
            ViewOffset::Static(o) => *o,
            ViewOffset::Animation(a) => a.target(),
            ViewOffset::Gesture(g) => g.current,
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

    /// Apply `delta` by re-anchoring any in-flight animation or gesture (no clobbering).
    pub fn offset(&mut self, delta: f64) {
        match self {
            ViewOffset::Static(x) => *x += delta,
            ViewOffset::Animation(a) => a.offset(delta),
            ViewOffset::Gesture(g) => {
                g.current += delta;
                g.anchor += delta;
            }
        }
    }
}

/// One Emacs frame in the strip.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Frame {
    pub name: String,
    pub surface_id: u64,
    pub view_id: ViewId,
    pub width: f64,
    pub surfaces: Vec<LayoutEntry>,
    #[serde(skip)]
    pub move_anim: Option<ViewAnimation>,
}

impl Frame {
    pub fn move_offset(&self) -> f64 {
        self.move_anim.as_ref().map_or(0.0, ViewAnimation::value)
    }
}

/// Per-output strip of frames laid out left-to-right; `set_active` is the sole mutator of `active_idx`.
#[derive(Debug, Default, serde::Serialize)]
pub struct Strip {
    pub frames: Vec<Frame>,
    pub gap: f64,
    active_idx: usize,
    #[serde(skip)]
    pub view_offset: ViewOffset,
}

impl Strip {
    pub fn active_idx(&self) -> usize {
        self.active_idx
    }

    /// Activate frame `idx` (clamped); re-anchors and animates `view_offset`
    /// so the active frame ends up centered on the viewport.
    pub fn set_active(&mut self, idx: usize) {
        if self.frames.is_empty() {
            self.active_idx = 0;
            self.view_offset = ViewOffset::Static(0.0);
            return;
        }
        let idx = idx.min(self.frames.len() - 1);
        let delta = self.column_x(self.active_idx) - self.column_x(idx);
        if delta != 0.0 {
            self.view_offset.offset(delta);
        }
        self.active_idx = idx;
        let cur = self.view_offset.current();
        self.view_offset = if cur.abs() < f64::EPSILON {
            ViewOffset::Static(0.0)
        } else {
            ViewOffset::Animation(ViewAnimation::new(cur, 0.0, DEFAULT_ANIM_DURATION))
        };
    }

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
    /// area's left edge), including any in-flight slide.
    pub fn screen_x_of_frame(&self, idx: usize) -> f64 {
        let move_offset = self.frames.get(idx).map_or(0.0, Frame::move_offset);
        self.column_x(idx) - self.view_pos() + move_offset
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

    /// Drive the state machine forward; settles finished animations.
    pub fn advance(&mut self) {
        self.view_offset.advance();
        for f in &mut self.frames {
            if f.move_anim.as_ref().is_some_and(ViewAnimation::is_done) {
                f.move_anim = None;
            }
        }
    }

    pub fn animations_ongoing(&self) -> bool {
        self.view_offset.is_animation_ongoing()
            || self.frames.iter().any(|f| f.move_anim.is_some())
    }

    /// Flat iterator over all surfaces across all frames, in declared order.
    pub fn surfaces(&self) -> impl Iterator<Item = &LayoutEntry> {
        self.frames.iter().flat_map(|f| f.surfaces.iter())
    }

    /// Mutable flat iterator over all surfaces across all frames.
    pub fn surfaces_mut(&mut self) -> impl Iterator<Item = &mut LayoutEntry> {
        self.frames.iter_mut().flat_map(|f| f.surfaces.iter_mut())
    }

    pub fn active_frame(&self) -> Option<&Frame> {
        self.frames.get(self.active_idx)
    }

    /// Entries on the active (on-screen at rest) frame only.
    pub fn active_surfaces(&self) -> impl Iterator<Item = &LayoutEntry> {
        self.active_frame().into_iter().flat_map(|f| f.surfaces.iter())
    }

    pub fn active_surfaces_mut(&mut self) -> impl Iterator<Item = &mut LayoutEntry> {
        let idx = self.active_idx;
        self.frames
            .get_mut(idx)
            .into_iter()
            .flat_map(|f| f.surfaces.iter_mut())
    }

    /// Iterator yielding `(frame_idx, &LayoutEntry)` pairs.
    pub fn surfaces_with_frame(&self) -> impl Iterator<Item = (usize, &LayoutEntry)> {
        self.frames
            .iter()
            .enumerate()
            .flat_map(|(i, f)| f.surfaces.iter().map(move |s| (i, s)))
    }

    /// Seed `move_anim` on neighbours of `frames[idx]` so they slide in to fill the gap.
    pub fn seed_remove_animation(&mut self, idx: usize) {
        if idx >= self.frames.len() {
            return;
        }
        let offset = self.frames[idx].width + self.gap;
        let (slide, from) = if self.active_idx <= idx {
            (&mut self.frames[idx + 1..], offset)
        } else {
            (&mut self.frames[..idx], -offset)
        };
        for f in slide {
            f.move_anim = Some(ViewAnimation::new(from, 0.0, DEFAULT_ANIM_DURATION));
        }
    }

    /// Begin a touchpad-driven view-offset gesture.
    pub fn gesture_begin(&mut self) {
        if self.frames.is_empty() {
            return;
        }
        let current = self.view_offset.current();
        self.view_offset = ViewOffset::Gesture(ViewGesture {
            current,
            anchor: current,
            tracker: SwipeTracker::new(),
        });
    }

    /// Push a swipe delta in unaccelerated touchpad units.  `working_w` is
    /// the active output's working-area width, used to normalise the swipe.
    pub fn gesture_update(&mut self, delta_x: f64, timestamp: Duration, working_w: f64) {
        let ViewOffset::Gesture(g) = &mut self.view_offset else {
            return;
        };
        g.tracker.push(delta_x, timestamp);
        let norm = working_w / VIEW_GESTURE_WORKING_AREA_MOVEMENT;
        g.current = g.anchor + g.tracker.pos() * norm;
    }

    /// End the gesture: project velocity to a settled position and snap to the nearest column.
    pub fn gesture_end(&mut self, working_w: f64) {
        let ViewOffset::Gesture(g) = &self.view_offset else {
            return;
        };
        let norm = working_w / VIEW_GESTURE_WORKING_AREA_MOVEMENT;
        let projected = g.anchor + g.tracker.projected_end_pos() * norm;
        let target_view = self.column_x(self.active_idx) + projected;
        let snap = (0..self.frames.len())
            .min_by(|&a, &b| {
                let da = (self.column_x(a) - target_view).abs();
                let db = (self.column_x(b) - target_view).abs();
                da.partial_cmp(&db).unwrap()
            })
            .unwrap_or(self.active_idx);
        self.set_active(snap);
    }

    /// Drop frame `idx`; surviving neighbours slide in to close the gap.
    pub fn remove_frame_at(&mut self, idx: usize) {
        if idx >= self.frames.len() {
            return;
        }
        self.seed_remove_animation(idx);
        let prev_active = self.active_idx;
        self.frames.remove(idx);

        let new_active = if idx < prev_active {
            prev_active - 1
        } else if idx == prev_active {
            prev_active.min(self.frames.len().saturating_sub(1))
        } else {
            prev_active
        };
        self.set_active(new_active);
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
                    name: String::new(),
                    surface_id: 0,
                    view_id: std::num::NonZeroU64::MIN,
                    width: w,
                    surfaces: Vec::new(),
                    move_anim: None,
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
