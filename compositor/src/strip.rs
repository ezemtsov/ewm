//! Per-output horizontal strip of Emacs frames + view-offset state machine.
//!
//! Frame positions derive from per-frame widths via `column_x`; the viewport's
//! left edge is `column_x(active_idx) + view_offset.current()`.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::animation::{Animation, Clock};
use crate::floating::SizeFracPoint;
use crate::{FocusId, LayoutEntry, LayoutEntryId};

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
        if let Some(&(_, last)) = self.history.back()
            && timestamp < last
        {
            return;
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
    Animation(Animation),
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
            ViewOffset::Animation(a) => a.to(),
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
        if let ViewOffset::Animation(a) = self
            && a.is_done()
        {
            *self = ViewOffset::Static(a.to());
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

    /// Spring from the live offset to `target`, keeping a slide already bound there.
    pub fn animate_to(&mut self, clock: Clock, target: f64, velocity: f64) {
        if !matches!(self, ViewOffset::Gesture(_)) && (self.target() - target).abs() < f64::EPSILON
        {
            return;
        }
        let from = self.current();
        *self = if (from - target).abs() < f64::EPSILON {
            ViewOffset::Static(target)
        } else {
            ViewOffset::Animation(Animation::new(clock, from, target, velocity))
        };
    }
}

/// Frame focus ids occupy the high half of the u64 space (bit 32 set),
/// derived deterministically from `surface_id` so the compositor and lisp can
/// construct the same id at `new_toplevel` / layout-build without coordination.
/// Layout entry ids stay in `[1, 2^32 - 1]` with bit 32 unset.
pub const FRAME_FOCUS_ID_MARKER: u64 = 1u64 << 32;

/// Derive a frame chrome's focus id from its surface id.
pub fn frame_focus_id_for(surface_id: u64) -> FocusId {
    std::num::NonZeroU64::new(FRAME_FOCUS_ID_MARKER | surface_id)
        .expect("frame focus id is non-zero by construction")
}

/// One Emacs frame in the strip.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Frame {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    pub surface_id: u64,
    pub focus_id: FocusId,
    pub width: f64,
    pub height: f64,
    pub entries: Vec<LayoutEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<FullscreenEntry>,
    /// Selected entry's id (keyboard focus target when frame is focused).
    pub selected_entry_id: Option<LayoutEntryId>,
    #[serde(skip)]
    pub move_anim: Option<Animation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub floating_pos: Option<SizeFracPoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct FullscreenEntry {
    pub entry_id: LayoutEntryId,
    pub surface_id: u64,
}

impl Frame {
    /// Construct a default Frame for a newly mapped Emacs frame chrome.
    pub fn new(surface_id: u64) -> Self {
        Self {
            name: String::new(),
            workspace_name: None,
            surface_id,
            focus_id: frame_focus_id_for(surface_id),
            width: 0.0,
            height: 0.0,
            entries: Vec::new(),
            fullscreen: None,
            selected_entry_id: None,
            move_anim: None,
            floating_pos: None,
        }
    }

    pub fn move_offset(&self) -> f64 {
        self.move_anim.as_ref().map_or(0.0, Animation::value)
    }

    pub fn entry_surface_id(&self, entry_id: LayoutEntryId) -> Option<u64> {
        self.entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .and_then(LayoutEntry::surface_id)
    }

    pub fn entry_fullscreen(&self, entry_id: LayoutEntryId) -> bool {
        self.fullscreen
            .is_some_and(|fullscreen| fullscreen.entry_id == entry_id)
    }

    pub fn surface_fullscreen(&self, surface_id: u64) -> bool {
        self.fullscreen
            .is_some_and(|fullscreen| fullscreen.surface_id == surface_id)
    }

    pub fn set_fullscreen_entry(
        &mut self,
        entry_id: LayoutEntryId,
    ) -> Option<(Option<FullscreenEntry>, FullscreenEntry)> {
        let surface_id = self.entry_surface_id(entry_id)?;
        let next = FullscreenEntry {
            entry_id,
            surface_id,
        };
        if self.fullscreen == Some(next) {
            return Some((None, next));
        }
        let previous = self.fullscreen.replace(next);
        Some((previous, next))
    }

    pub fn clear_fullscreen(&mut self) -> Option<FullscreenEntry> {
        self.fullscreen.take()
    }

    pub fn clear_fullscreen_entry(&mut self, entry_id: LayoutEntryId) -> Option<FullscreenEntry> {
        if self.entry_fullscreen(entry_id) {
            self.fullscreen.take()
        } else {
            None
        }
    }

    pub fn validate_fullscreen(&mut self) -> Option<FullscreenEntry> {
        let fullscreen = self.fullscreen?;
        if self.entry_surface_id(fullscreen.entry_id) == Some(fullscreen.surface_id) {
            None
        } else {
            self.fullscreen.take()
        }
    }

    pub fn preserve_compositor_view_state_from(&mut self, previous: &Frame) {
        self.fullscreen = previous.fullscreen.filter(|fullscreen| {
            self.entry_surface_id(fullscreen.entry_id) == Some(fullscreen.surface_id)
        });
    }

    pub fn clear_compositor_view_state(&mut self) {
        self.fullscreen = None;
    }

    /// Unbind a destroyed surface from its entries without removing them.
    pub fn detach_surface(&mut self, surface_id: u64) {
        for entry in &mut self.entries {
            if entry.surface_id() == Some(surface_id) {
                entry.surface = None;
            }
        }
        self.validate_fullscreen();
    }
}

/// Per-output strip of frames laid out left-to-right; `set_active` is the sole mutator of
/// `active_idx`.
#[derive(Debug, serde::Serialize)]
pub struct Strip {
    pub frames: Vec<Frame>,
    pub gap: f64,
    active_idx: usize,
    #[serde(skip)]
    pub view_offset: ViewOffset,
    /// Screen x shift per unit of overview progress after snapping onto a picked frame.
    #[serde(skip)]
    pub overview_shift: f64,
    #[serde(skip)]
    pub clock: Clock,
}

impl Strip {
    pub fn new(clock: Clock) -> Self {
        Self {
            frames: Vec::new(),
            gap: 0.0,
            active_idx: 0,
            view_offset: ViewOffset::default(),
            overview_shift: 0.0,
            clock,
        }
    }

    pub fn active_idx(&self) -> usize {
        self.active_idx
    }

    /// Activate frame `idx` (clamped); re-anchors and animates `view_offset`
    /// so the active frame ends up centered on the viewport.
    pub fn set_active(&mut self, idx: usize) {
        self.set_active_with_velocity(idx, 0.);
    }

    /// `set_active` for a released swipe: the slide starts at `velocity` logical px per second.
    fn set_active_with_velocity(&mut self, idx: usize, velocity: f64) {
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
        self.view_offset
            .animate_to(self.clock.clone(), 0.0, velocity);
    }

    /// X position of frame `idx` relative to the strip origin.
    /// `column_x(0)` is always 0; subsequent columns accumulate `width + gap`.
    pub fn column_x(&self, idx: usize) -> f64 {
        self.frames[..idx.min(self.frames.len())]
            .iter()
            .map(|f| self.stride(f))
            .sum()
    }

    /// Room a frame takes in the strip; an unsized frame takes none.
    fn stride(&self, frame: &Frame) -> f64 {
        if frame.width > 0.0 {
            frame.width + self.gap
        } else {
            0.0
        }
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

    /// Drive the state machine forward; settles finished animations.
    pub fn advance(&mut self) {
        self.view_offset.advance();
        for f in &mut self.frames {
            if f.move_anim.as_ref().is_some_and(Animation::is_done) {
                f.move_anim = None;
            }
        }
    }

    pub fn animations_ongoing(&self) -> bool {
        self.view_offset.is_animation_ongoing() || self.frames.iter().any(|f| f.move_anim.is_some())
    }

    /// `(idx, &Frame, screen_rect)` for frames whose rect intersects the screen-space
    /// `viewport`. Prefix-sums column offsets in a single pass so the cull is O(N).
    pub fn frames_with_render_geo(
        &self,
        view_size: Size<f64, Logical>,
        viewport: Rectangle<f64, Logical>,
    ) -> impl Iterator<Item = (usize, &Frame, Rectangle<f64, Logical>)> {
        let view_pos = self.view_pos();
        let mut col_x = 0.0;
        self.frames.iter().enumerate().filter_map(move |(idx, f)| {
            if f.width <= 0.0 {
                return None;
            }
            let screen_x = col_x - view_pos + f.move_offset();
            col_x += self.stride(f);
            let rect = Rectangle::new(
                Point::from((screen_x, 0.0)),
                Size::from((f.width, view_size.h)),
            );
            viewport.intersection(rect).map(|_| (idx, f, rect))
        })
    }

    /// Flat iterator over all entries across all frames, in declared order.
    pub fn entries(&self) -> impl Iterator<Item = &LayoutEntry> {
        self.frames.iter().flat_map(|f| f.entries.iter())
    }

    /// Mutable flat iterator over all entries across all frames.
    pub fn entries_mut(&mut self) -> impl Iterator<Item = &mut LayoutEntry> {
        self.frames.iter_mut().flat_map(|f| f.entries.iter_mut())
    }

    /// Surface-backed entries across all frames.
    pub fn surface_entries(&self) -> impl Iterator<Item = &LayoutEntry> {
        self.entries().filter(|entry| entry.surface.is_some())
    }

    /// Mutable surface-backed entries across all frames.
    pub fn surface_entries_mut(&mut self) -> impl Iterator<Item = &mut LayoutEntry> {
        self.entries_mut().filter(|entry| entry.surface.is_some())
    }

    pub fn active_frame(&self) -> Option<&Frame> {
        self.frames.get(self.active_idx)
    }

    pub fn active_frame_is_fullscreen(&self) -> bool {
        self.active_frame()
            .is_some_and(|frame| frame.fullscreen.is_some())
    }

    /// Entries on the active (on-screen at rest) frame only.
    pub fn active_entries(&self) -> impl Iterator<Item = &LayoutEntry> {
        self.active_frame()
            .into_iter()
            .flat_map(|f| f.entries.iter())
    }

    pub fn active_entries_mut(&mut self) -> impl Iterator<Item = &mut LayoutEntry> {
        let idx = self.active_idx;
        self.frames
            .get_mut(idx)
            .into_iter()
            .flat_map(|f| f.entries.iter_mut())
    }

    /// Surface-backed entries on the active frame only.
    pub fn active_surface_entries(&self) -> impl Iterator<Item = &LayoutEntry> {
        self.active_entries()
            .filter(|entry| entry.surface.is_some())
    }

    pub fn active_surface_entries_mut(&mut self) -> impl Iterator<Item = &mut LayoutEntry> {
        self.active_entries_mut()
            .filter(|entry| entry.surface.is_some())
    }

    /// Merge a fresh Emacs layout into this strip.
    ///
    /// Existing frames are updated in place so their move animations survive.
    /// Frames omitted by the layout are kept until their Wayland destroy arrives,
    /// but their layout entries are cleared because views belong to the latest
    /// layout declaration.
    /// Pending active-close frames omitted by a racing layout refresh keep their
    /// old slot, so destroy-time refocus still uses the slot the user closed.
    pub fn merge_layout_frames(
        &mut self,
        frames: Vec<Frame>,
        mut is_pending_active_close: impl FnMut(u64) -> bool,
    ) {
        let old_frame_positions: HashMap<u64, usize> = self
            .frames
            .iter()
            .enumerate()
            .map(|(idx, frame)| (frame.surface_id, idx))
            .collect();
        let order: HashMap<u64, usize> = frames
            .iter()
            .enumerate()
            .map(|(idx, frame)| (frame.surface_id, idx))
            .collect();

        for mut new_frame in frames {
            if let Some(existing) = self
                .frames
                .iter_mut()
                .find(|frame| frame.surface_id == new_frame.surface_id)
            {
                let preserved_anim = existing.move_anim.clone();
                let preserved_workspace_name = existing.workspace_name.clone();
                new_frame.clear_compositor_view_state();
                new_frame.preserve_compositor_view_state_from(existing);
                *existing = new_frame;
                if existing.workspace_name.is_none() {
                    existing.workspace_name = preserved_workspace_name;
                }
                existing.move_anim = preserved_anim;
            } else {
                self.frames.push(new_frame);
            }
        }

        for frame in &mut self.frames {
            if !order.contains_key(&frame.surface_id) {
                frame.entries.clear();
                frame.selected_entry_id = None;
                frame.clear_compositor_view_state();
            }
        }

        self.frames
            .sort_by_key(|frame| order.get(&frame.surface_id).copied().unwrap_or(usize::MAX));

        let mut closing_frames = old_frame_positions
            .iter()
            .filter_map(|(&id, &idx)| {
                (!order.contains_key(&id) && is_pending_active_close(id)).then_some((id, idx))
            })
            .collect::<Vec<_>>();
        closing_frames.sort_by_key(|&(_, idx)| idx);
        for (id, old_idx) in closing_frames {
            if let Some(pos) = self.frames.iter().position(|frame| frame.surface_id == id) {
                let frame = self.frames.remove(pos);
                self.frames.insert(old_idx.min(self.frames.len()), frame);
            }
        }
    }

    /// Iterator yielding `(frame_idx, &LayoutEntry)` pairs.
    pub fn entries_with_frame(&self) -> impl Iterator<Item = (usize, &LayoutEntry)> {
        self.frames
            .iter()
            .enumerate()
            .flat_map(|(i, f)| f.entries.iter().map(move |s| (i, s)))
    }

    /// Surface-backed entries with their frame indexes.
    pub fn surface_entries_with_frame(&self) -> impl Iterator<Item = (usize, &LayoutEntry)> {
        self.entries_with_frame()
            .filter(|(_, entry)| entry.surface.is_some())
    }

    /// Seed `move_anim` on neighbours of `frames[idx]` so they slide in to fill the gap.
    fn seed_remove_animation_from(&mut self, idx: usize, anchor_idx: usize) {
        if idx >= self.frames.len() {
            return;
        }
        let clock = self.clock.clone();
        let offset = self.stride(&self.frames[idx]);
        let (slide, from) = if anchor_idx <= idx {
            (&mut self.frames[idx + 1..], offset)
        } else {
            (&mut self.frames[..idx], -offset)
        };
        let slide_in = Animation::new(clock, from, 0.0, 0.);
        for f in slide {
            f.move_anim = Some(slide_in.clone());
        }
    }

    fn active_idx_after_removal(&self, idx: usize, focus: RemovalFocus) -> usize {
        let last_after_idx = self.frames.len().saturating_sub(2);
        match focus {
            RemovalFocus::CurrentActive if idx < self.active_idx => self.active_idx - 1,
            RemovalFocus::CurrentActive if idx > self.active_idx => self.active_idx,
            RemovalFocus::CurrentActive | RemovalFocus::RemovedSlot => idx.min(last_after_idx),
        }
    }

    fn remove_frame_at_with_focus(&mut self, idx: usize, focus: RemovalFocus) {
        if idx >= self.frames.len() {
            return;
        }

        let anchor_idx = match focus {
            RemovalFocus::CurrentActive => self.active_idx,
            RemovalFocus::RemovedSlot => idx,
        };
        let new_active = self.active_idx_after_removal(idx, focus);

        self.seed_remove_animation_from(idx, anchor_idx);
        self.frames.remove(idx);
        self.set_active(new_active);
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

    /// End the gesture: snap to the column nearest the projected stop, keeping the swipe's speed.
    pub fn gesture_end(&mut self, working_w: f64) {
        let ViewOffset::Gesture(g) = &mut self.view_offset else {
            return;
        };
        g.tracker.push(0., self.clock.now());
        let norm = working_w / VIEW_GESTURE_WORKING_AREA_MOVEMENT;
        let velocity = g.tracker.velocity() * norm;
        let projected = g.anchor + g.tracker.projected_end_pos() * norm;
        let target_view = self.column_x(self.active_idx) + projected;
        let snap = (0..self.frames.len())
            .min_by(|&a, &b| {
                let da = (self.column_x(a) - target_view).abs();
                let db = (self.column_x(b) - target_view).abs();
                da.total_cmp(&db)
            })
            .unwrap_or(self.active_idx);
        self.set_active_with_velocity(snap, velocity);
    }

    /// Drop frame `idx`; surviving neighbours slide in to close the gap.
    pub fn remove_frame_at(&mut self, idx: usize) {
        self.remove_frame_at_with_focus(idx, RemovalFocus::CurrentActive);
    }

    /// Drop frame `idx` using active-close semantics, regardless of the
    /// strip's current `active_idx`.
    ///
    /// Used when close intent was captured before a later focus relay changed
    /// `active_idx`: the destroyed slot chooses the right neighbour if present,
    /// otherwise the left neighbour.
    pub fn remove_active_frame_at(&mut self, idx: usize) {
        self.remove_frame_at_with_focus(idx, RemovalFocus::RemovedSlot);
    }
}

#[derive(Debug, Clone, Copy)]
enum RemovalFocus {
    CurrentActive,
    RemovedSlot,
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const MAX_WIDTH: u32 = 2_000;
    const MAX_GAP: u32 = 200;
    const MAX_FRAMES: usize = 10;

    fn strip_with(widths: Vec<f64>, gap: f64, active_idx: usize) -> Strip {
        Strip {
            frames: widths
                .into_iter()
                .map(|w| Frame {
                    name: String::new(),
                    workspace_name: None,
                    surface_id: 0,
                    focus_id: std::num::NonZeroU64::MIN,
                    width: w,
                    height: 0.0,
                    entries: Vec::new(),
                    fullscreen: None,
                    selected_entry_id: None,
                    move_anim: None,
                    floating_pos: None,
                })
                .collect(),
            gap,
            active_idx,
            view_offset: ViewOffset::default(),
            overview_shift: 0.0,
            clock: Clock::default(),
        }
    }

    fn strip_with_ids(ids: &[u64]) -> Strip {
        let mut strip = Strip::new(Clock::default());
        strip.frames = ids.iter().copied().map(Frame::new).collect();
        strip
    }

    fn frame_ids(strip: &Strip) -> Vec<u64> {
        strip.frames.iter().map(|frame| frame.surface_id).collect()
    }

    proptest! {
        #[test]
        fn column_x_matches_prefix_sum(
            widths in prop::collection::vec(0u32..=MAX_WIDTH, 0..MAX_FRAMES),
            gap in 0u32..=MAX_GAP,
            idx in 0usize..(MAX_FRAMES * 2),
        ) {
            let widths = widths.into_iter().map(f64::from).collect::<Vec<_>>();
            let gap = f64::from(gap);
            let s = strip_with(widths.clone(), gap, 0);
            let expected = widths
                .iter()
                .take(idx.min(widths.len()))
                .filter(|w| **w > 0.0)
                .map(|w| *w + gap)
                .sum::<f64>();

            prop_assert_eq!(s.column_x(idx), expected);
        }

        #[test]
        fn screen_positions_are_viewport_relative(
            widths in prop::collection::vec(1u32..=MAX_WIDTH, 1..MAX_FRAMES),
            gap in 0u32..=MAX_GAP,
            active_idx in 0usize..MAX_FRAMES,
            offset in -2_000i32..=2_000,
        ) {
            let widths = widths.into_iter().map(f64::from).collect::<Vec<_>>();
            let active_idx = active_idx % widths.len();
            let offset = f64::from(offset);
            let mut s = strip_with(widths, f64::from(gap), active_idx);
            s.view_offset = ViewOffset::Static(offset);

            prop_assert_eq!(s.view_pos(), s.column_x(active_idx) + offset);
            prop_assert_eq!(s.target_view_pos(), s.column_x(active_idx) + offset);
            for idx in 0..s.frames.len() {
                prop_assert_eq!(
                    s.screen_x_of_frame(idx),
                    s.column_x(idx) - s.view_pos()
                );
            }
        }

        #[test]
        fn render_geo_matches_viewport_intersections(
            widths in prop::collection::vec(0u32..=MAX_WIDTH, 0..MAX_FRAMES),
            gap in 0u32..=MAX_GAP,
            active_idx in 0usize..MAX_FRAMES,
            offset in -4_000i32..=4_000,
            view_w in 1u32..=4_000,
            view_h in 1u32..=2_000,
        ) {
            let widths = widths.into_iter().map(f64::from).collect::<Vec<_>>();
            let active_idx = if widths.is_empty() { 0 } else { active_idx % widths.len() };
            let mut s = strip_with(widths, f64::from(gap), active_idx);
            s.view_offset = ViewOffset::Static(f64::from(offset));
            let view = Size::from((f64::from(view_w), f64::from(view_h)));
            let view_pos = s.view_pos();

            let mut column_x = 0.0;
            let expected = s.frames.iter().enumerate().filter_map(|(idx, frame)| {
                if frame.width <= 0.0 {
                    return None;
                }
                let screen_x = column_x - view_pos;
                column_x += frame.width + s.gap;
                (screen_x < view.w && screen_x + frame.width > 0.0)
                    .then_some((idx, screen_x, frame.width))
            }).collect::<Vec<_>>();

            let rendered = s
                .frames_with_render_geo(view, Rectangle::from_size(view))
                .map(|(idx, _, rect)| (idx, rect.loc.x, rect.size.w, rect.size.h))
                .collect::<Vec<_>>();

            prop_assert_eq!(rendered.len(), expected.len());
            for ((idx, x, w, h), (expected_idx, expected_x, expected_w)) in
                rendered.into_iter().zip(expected)
            {
                prop_assert_eq!(idx, expected_idx);
                prop_assert_eq!(x, expected_x);
                prop_assert_eq!(w, expected_w);
                prop_assert_eq!(h, view.h);
            }
        }

        #[test]
        fn view_offset_offset_translates_static_values(
            value in -1_000i32..=1_000,
            delta in -1_000i32..=1_000,
        ) {
            let mut offset = ViewOffset::Static(f64::from(value));

            offset.offset(f64::from(delta));

            prop_assert_eq!(offset.current(), f64::from(value + delta));
            prop_assert_eq!(offset.target(), f64::from(value + delta));
        }

        #[test]
        fn animate_to_uses_static_only_for_zero_distance(
            current in -2_000i32..=2_000,
            target in -2_000i32..=2_000,
        ) {
            let mut offset = ViewOffset::Static(f64::from(current));

            offset.animate_to(Clock::default(), f64::from(target), 0.);

            prop_assert_eq!(offset.target(), f64::from(target));
            if current == target {
                prop_assert!(offset.is_static());
                prop_assert_eq!(offset.current(), f64::from(current));
            } else {
                prop_assert!(offset.is_animation_ongoing());
                prop_assert!(!offset.is_static());
            }
        }

        #[test]
        fn animate_to_keeps_a_slide_already_bound_there(
            from in -2_000i32..=2_000,
            target in -2_000i32..=2_000,
            elapsed_ms in 1u64..=400,
        ) {
            let start = Duration::from_secs(10);
            let clock = Clock::with_time(start);
            let reference = Animation::new(clock.clone(), f64::from(from), f64::from(target), 0.);
            let mut offset = ViewOffset::Static(f64::from(from));
            offset.animate_to(clock.clone(), f64::from(target), 0.);

            clock.set(start + Duration::from_millis(elapsed_ms));
            offset.animate_to(clock.clone(), f64::from(target), 0.);
            clock.set(start + Duration::from_millis(elapsed_ms * 2));

            prop_assert_eq!(offset.current(), reference.value());
        }

        #[test]
        fn merge_layout_preserves_pending_active_close_slot(
            len in 1usize..=MAX_FRAMES,
            raw_close_idx in 0usize..MAX_FRAMES,
        ) {
            let close_idx = raw_close_idx % len;
            let ids = (0..len).map(|idx| 10_000 + idx as u64).collect::<Vec<_>>();
            let closing_id = ids[close_idx];
            let layout = ids
                .iter()
                .copied()
                .filter(|&id| id != closing_id)
                .map(Frame::new)
                .collect::<Vec<_>>();
            let mut strip = strip_with_ids(&ids);

            strip.merge_layout_frames(layout, |id| id == closing_id);

            prop_assert_eq!(frame_ids(&strip), ids);
        }
    }

    #[test]
    fn gesture_end_tolerates_nan_frame_width() {
        let mut strip = strip_with(vec![100.0, f64::NAN, 100.0], 0.0, 0);

        strip.gesture_begin();
        strip.gesture_update(10.0, Duration::from_millis(1), 1_000.0);
        strip.gesture_end(1_000.0);

        assert!(strip.active_idx() < strip.frames.len());
    }
}
