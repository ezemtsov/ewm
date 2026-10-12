//! Per-output horizontal strip of Emacs frames + view-offset state machine.
//!
//! Frame positions derive from per-frame widths via `column_x`; the viewport's
//! left edge is `column_x(active_idx) + view_offset.current()`.

use std::collections::{HashSet, VecDeque};
use std::mem;
use std::time::Duration;

use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::animation::{Animation, Clock};
use crate::closing_frame::Closing;
use crate::floating::SizeFracPoint;
use crate::snapshot::Snapshot;
use crate::{FocusId, LayoutEntry, LayoutEntryId, slide_spring};

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

    /// Spring from the live offset to `target`, keeping a slide already bound within `pixel` of it.
    pub fn animate_to(&mut self, clock: Clock, target: f64, velocity: f64, pixel: f64) {
        if clock.should_complete_instantly() {
            *self = ViewOffset::Static(target);
            return;
        }
        if !matches!(self, ViewOffset::Gesture(_)) {
            // Already there or bound there: only absorb the rounding.
            let to_diff = target - self.target();
            if to_diff.abs() < pixel {
                self.offset(to_diff);
                return;
            }
        }
        let from = self.current();
        *self = ViewOffset::Animation(Animation::new(
            clock,
            from,
            target,
            velocity,
            slide_spring(),
        ));
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

/// Who a frame belongs to: the user, or the compositor when EWM made it to fill an output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Owner {
    #[default]
    User,
    Compositor,
}

/// One Emacs frame in the strip.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Frame {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    pub surface_id: u64,
    pub owner: Owner,
    pub focus_id: FocusId,
    pub width: f64,
    pub height: f64,
    pub entries: Vec<LayoutEntry>,
    /// Selected entry's id (keyboard focus target when frame is focused).
    pub selected_entry_id: Option<LayoutEntryId>,
    #[serde(skip)]
    pub move_anim: Option<Animation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub floating_pos: Option<SizeFracPoint>,
    /// The strip frame a float is pinned to, showing and sliding with it.
    #[serde(skip)]
    pub pinned_to: Option<u64>,
    #[serde(skip)]
    pub close_snapshot: Option<Snapshot>,
    /// Views fading out where they were drawn.
    #[serde(skip)]
    pub closing_views: Vec<Closing>,
}

impl Frame {
    /// EWM made this frame to fill an output and nobody has used or named it.
    pub fn is_filler(&self) -> bool {
        self.owner == Owner::Compositor && self.workspace_name.is_none()
    }

    /// Construct a default Frame for a newly mapped Emacs frame chrome.
    pub fn new(surface_id: u64) -> Self {
        Self {
            name: String::new(),
            workspace_name: None,
            surface_id,
            owner: Owner::User,
            focus_id: frame_focus_id_for(surface_id),
            width: 0.0,
            height: 0.0,
            entries: Vec::new(),
            selected_entry_id: None,
            move_anim: None,
            floating_pos: None,
            pinned_to: None,
            close_snapshot: None,
            closing_views: Vec::new(),
        }
    }

    pub fn move_offset(&self) -> f64 {
        self.move_anim.as_ref().map_or(0.0, Animation::value)
    }

    /// Drop finished animations.
    pub fn advance(&mut self) {
        if self.move_anim.as_ref().is_some_and(Animation::is_done) {
            self.move_anim = None;
        }
        self.closing_views.retain(Closing::are_animations_ongoing);
    }

    pub(crate) fn animations_ongoing(&self) -> bool {
        self.move_anim.is_some() || !self.closing_views.is_empty()
    }

    pub fn entry_surface_id(&self, entry_id: LayoutEntryId) -> Option<u64> {
        self.entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .and_then(LayoutEntry::surface_id)
    }

    /// The view shown fullscreen, if any.
    pub fn fullscreen_entry(&self) -> Option<&LayoutEntry> {
        self.entries.iter().find(|entry| entry.fullscreen())
    }

    /// Whether any entry shows a surface.
    pub(crate) fn has_views(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.surface_id().is_some())
    }

    /// Mark the view showing a `committed` surface, the selected entry first.
    pub(crate) fn mark_fullscreen(&mut self, committed: &impl Fn(u64) -> bool) {
        let shows = |entry: &LayoutEntry| entry.surface_id().is_some_and(committed);
        let selected = self.selected_entry_id;
        let pick = self
            .entries
            .iter()
            .position(|entry| Some(entry.id) == selected && shows(entry))
            .or_else(|| self.entries.iter().position(shows));
        for (idx, entry) in self.entries.iter_mut().enumerate() {
            entry.set_fullscreen(Some(idx) == pick);
        }
    }

    /// Drop a selection that names none of the entries.
    pub fn validate_selection(&mut self) {
        if self
            .selected_entry_id
            .is_some_and(|sel| !self.entries.iter().any(|entry| entry.id == sel))
        {
            self.selected_entry_id = None;
        }
    }

    /// Take `new`'s contents, keeping the name, animations and floating state.
    pub fn refill(&mut self, mut new: Frame) {
        new.validate_selection();
        if new.workspace_name.is_none() {
            new.workspace_name = self.workspace_name.take();
        }
        new.move_anim = self.move_anim.take();
        new.closing_views = mem::take(&mut self.closing_views);
        new.floating_pos = self.floating_pos;
        new.pinned_to = self.pinned_to;
        *self = new;
    }

    /// Empty the frame as a layout that leaves it out does.
    pub fn clear_contents(&mut self) {
        self.entries.clear();
        self.selected_entry_id = None;
    }

    /// Unbind a destroyed surface from its entries without removing them.
    pub fn detach_surface(&mut self, surface_id: u64) {
        for entry in &mut self.entries {
            if entry.surface_id() == Some(surface_id) {
                entry.surface = None;
            }
        }
    }
}

/// Frames in `current` that `layout` leaves out.
pub fn dropped_frame_ids<'a>(
    current: impl Iterator<Item = &'a Frame>,
    layout: &[Frame],
) -> Vec<u64> {
    current
        .map(|frame| frame.surface_id)
        .filter(|&id| !layout.iter().any(|frame| frame.surface_id == id))
        .collect()
}

/// Per-output strip of frames laid out left-to-right; `set_active` is the sole mutator of
/// `active_idx`.
#[derive(Debug, serde::Serialize)]
pub struct Strip {
    frames: Vec<Frame>,
    gap: f64,
    /// The output in strip space, which a frame showing fullscreen spans.
    #[serde(skip)]
    fullscreen_slot: Rectangle<f64, Logical>,
    active_idx: usize,
    #[serde(skip)]
    view_offset: ViewOffset,
    /// Screen x shift per unit of overview progress after snapping onto a picked frame.
    #[serde(skip)]
    overview_shift: f64,
    #[serde(skip)]
    scale: f64,
    #[serde(skip)]
    clock: Clock,
    /// Closing frames at their strip positions.
    #[serde(skip)]
    closing_frames: Vec<(Point<f64, Logical>, Closing)>,
}

impl Strip {
    pub fn new(clock: Clock) -> Self {
        Self {
            frames: Vec::new(),
            gap: 0.0,
            fullscreen_slot: Rectangle::default(),
            active_idx: 0,
            view_offset: ViewOffset::default(),
            overview_shift: 0.0,
            scale: 1.0,
            clock,
            closing_frames: Vec::new(),
        }
    }

    pub fn update_config(&mut self, scale: f64) {
        self.scale = scale;
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
        self.set_active_from_view_pos(idx, self.view_pos(), velocity);
    }

    /// Activate frame `idx` with the viewport still at `view_pos`, then slide it into place.
    fn set_active_from_view_pos(&mut self, idx: usize, view_pos: f64, velocity: f64) {
        if self.frames.is_empty() {
            self.active_idx = 0;
            self.view_offset = ViewOffset::Static(0.0);
            return;
        }
        self.active_idx = idx.min(self.frames.len() - 1);
        let delta = view_pos - self.view_pos();
        if delta != 0.0 {
            self.view_offset.offset(delta);
        }
        self.view_offset
            .animate_to(self.clock.clone(), 0.0, velocity, 1. / self.scale);
    }

    /// X position of frame `idx` relative to the strip origin.
    /// `column_x(0)` is always 0; subsequent columns accumulate `width + gap`.
    pub fn column_x(&self, idx: usize) -> f64 {
        self.frames[..idx.min(self.frames.len())]
            .iter()
            .map(|f| self.stride(f))
            .sum()
    }

    /// The `working_area` the strip lays out in and the `output` a fullscreen view fills.
    pub(crate) fn set_view(
        &mut self,
        working_area: Rectangle<i32, Logical>,
        output: Size<f64, Logical>,
    ) {
        let inset = working_area.loc.to_f64();
        self.fullscreen_slot = Rectangle::new(Point::from((-inset.x, -inset.y)), output);
    }

    /// Whether the view is at rest, neither sliding nor under a swipe.
    pub fn view_is_static(&self) -> bool {
        self.view_offset.is_static()
    }

    /// `frame`'s slot at x 0: the output when it shows fullscreen, else its width over the view.
    fn slot(&self, frame: &Frame, view_h: f64) -> Rectangle<f64, Logical> {
        if frame.fullscreen_entry().is_some() {
            self.fullscreen_slot
        } else {
            Rectangle::from_size(Size::from((frame.width, view_h)))
        }
    }

    /// Room a frame takes in the strip; an unsized frame takes none.
    fn stride(&self, frame: &Frame) -> f64 {
        let width = if frame.fullscreen_entry().is_some() {
            self.fullscreen_slot.size.w
        } else {
            frame.width
        };
        if width > 0.0 { width + self.gap } else { 0.0 }
    }

    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    pub fn frames_mut(&mut self) -> &mut [Frame] {
        &mut self.frames
    }

    pub fn index_of(&self, surface_id: u64) -> Option<usize> {
        self.frames.iter().position(|f| f.surface_id == surface_id)
    }

    pub fn frame_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.frames.iter().map(|frame| frame.surface_id)
    }

    pub fn active_frame_id(&self) -> Option<u64> {
        self.active_frame().map(|frame| frame.surface_id)
    }

    pub fn push_frame(&mut self, frame: Frame) {
        self.frames.push(frame);
    }

    pub fn append_frames(&mut self, frames: impl IntoIterator<Item = Frame>) {
        self.frames.extend(frames);
    }

    /// Insert at `idx`, keeping the active frame active.
    pub fn insert_frame(&mut self, idx: usize, frame: Frame) {
        let was_empty = self.frames.is_empty();
        self.frames.insert(idx, frame);
        if !was_empty && idx <= self.active_idx {
            self.set_active(self.active_idx + 1);
        }
    }

    /// Move frame `from` to slot `to`. It and the frames it passes slide into place.
    pub fn move_frame(&mut self, from: usize, to: usize) {
        if from == to || from >= self.frames.len() || to >= self.frames.len() {
            return;
        }
        let view_pos = self.view_pos();
        let active_id = self.active_frame_id();
        let from_x = self.column_x(from);
        let stride = self.stride(&self.frames[from]);
        let frame = self.frames.remove(from);
        self.frames.insert(to, frame);
        if !self.clock.should_complete_instantly() {
            let clock = self.clock.clone();
            let slide = |f: &Frame, from: f64| {
                Animation::new(
                    clock.clone(),
                    from + f.move_offset(),
                    0.0,
                    0.,
                    slide_spring(),
                )
            };
            let to_x = self.column_x(to);
            let moved = &mut self.frames[to];
            moved.move_anim = Some(slide(moved, from_x - to_x));
            let (passed, offset) = if from < to {
                (from..to, stride)
            } else {
                (to + 1..from + 1, -stride)
            };
            for f in &mut self.frames[passed] {
                f.move_anim = Some(slide(f, offset));
            }
        }
        let active = active_id.and_then(|id| self.index_of(id)).unwrap_or(0);
        self.set_active_from_view_pos(active, view_pos, 0.);
    }

    /// Take every frame out, leaving an empty strip.
    pub fn take_frames(&mut self) -> Vec<Frame> {
        let frames = mem::take(&mut self.frames);
        self.set_active(0);
        frames
    }

    /// Pull out the frames `take` selects; the active frame stays active when it stays.
    pub fn drain_frames(&mut self, take: impl FnMut(&Frame) -> bool) -> Vec<Frame> {
        let active_id = self.active_frame_id();
        let (taken, kept): (Vec<_>, Vec<_>) =
            mem::take(&mut self.frames).into_iter().partition(take);
        self.frames = kept;
        let idx = active_id
            .and_then(|id| self.index_of(id))
            .unwrap_or(self.active_idx);
        self.set_active(idx);
        taken
    }

    pub fn view_offset(&self) -> &ViewOffset {
        &self.view_offset
    }

    pub fn overview_shift(&self) -> f64 {
        self.overview_shift
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

    /// The strip frame `float` belongs to: the one it is pinned to, else the active one.
    pub(crate) fn frame_of_float(&self, float: &Frame) -> Option<u64> {
        float
            .pinned_to
            .filter(|&id| self.index_of(id).is_some())
            .or_else(|| self.active_frame_id())
    }

    /// Strip-relative viewport left edge once any in-flight animation settles.
    pub fn target_view_pos(&self) -> f64 {
        self.column_x(self.active_idx) + self.view_offset.target()
    }

    /// Drop the slide a pick started and let the zoom carry the jump from `prev_view_pos`.
    pub fn carry_slide_as_overview_shift(&mut self, prev_view_pos: f64, zoom: f64, progress: f64) {
        self.view_offset = ViewOffset::Static(0.0);
        self.overview_shift = (self.view_pos() - prev_view_pos) * zoom / progress;
    }

    /// Turn a leftover overview shift back into a slide before the zoom changes course.
    pub fn bake_overview_shift(&mut self, progress: Option<f64>, zoom: f64) {
        let shift = mem::take(&mut self.overview_shift);
        // A closed overview showed no shift, so there is nothing to carry over.
        if let Some(progress) = progress.filter(|_| shift != 0.0) {
            self.view_offset.offset(-shift * progress / zoom);
            self.view_offset
                .animate_to(self.clock.clone(), 0.0, 0., 1. / self.scale);
        }
    }

    /// Drive the state machine forward; settles finished animations.
    pub fn advance(&mut self) {
        self.view_offset.advance();
        for f in &mut self.frames {
            f.advance();
        }
        self.closing_frames
            .retain(|(_, closing)| closing.are_animations_ongoing());
    }

    pub fn animations_ongoing(&self) -> bool {
        self.view_offset.is_animation_ongoing()
            || self.frames.iter().any(Frame::animations_ongoing)
            || !self.closing_frames.is_empty()
    }

    /// Closing frames with their screen-space origin, newest first.
    pub fn closing_frames_with_render_pos(
        &self,
    ) -> impl Iterator<Item = (&Closing, Point<f64, Logical>)> {
        self.closing_frames
            .iter()
            .rev()
            .map(|(pos, closing)| (closing, *pos - Point::from((self.view_pos(), 0.))))
    }

    /// `(idx, frame, slot, screen_x)` for frames whose slot meets the screen-space `viewport`.
    pub fn frames_with_render_geo(
        &self,
        view_size: Size<f64, Logical>,
        viewport: Rectangle<f64, Logical>,
    ) -> impl Iterator<Item = (usize, &Frame, Rectangle<f64, Logical>, f64)> {
        let view_pos = self.view_pos();
        let mut col_x = 0.0;
        self.frames.iter().enumerate().filter_map(move |(idx, f)| {
            let mut rect = self.slot(f, view_size.h);
            if rect.size.w <= 0.0 {
                return None;
            }
            let screen_x = col_x - view_pos + f.move_offset();
            col_x += self.stride(f);
            rect.loc.x += screen_x;
            viewport
                .intersection(rect)
                .map(|_| (idx, f, rect, screen_x))
        })
    }

    /// Flat iterator over all entries across all frames, in declared order.
    pub fn entries(&self) -> impl Iterator<Item = &LayoutEntry> {
        self.frames.iter().flat_map(|f| f.entries.iter())
    }

    /// Surface-backed entries across all frames.
    pub fn surface_entries(&self) -> impl Iterator<Item = &LayoutEntry> {
        self.entries().filter(|entry| entry.surface.is_some())
    }

    pub fn active_frame(&self) -> Option<&Frame> {
        self.frames.get(self.active_idx)
    }

    /// Refill the frames this strip holds from `frames`, emptying the ones it leaves out.
    pub fn merge_layout_frames(&mut self, frames: Vec<Frame>) {
        let named: HashSet<u64> = frames.iter().map(|frame| frame.surface_id).collect();

        for new_frame in frames {
            if let Some(existing) = self
                .frames
                .iter_mut()
                .find(|frame| frame.surface_id == new_frame.surface_id)
            {
                existing.refill(new_frame);
            }
        }

        for frame in &mut self.frames {
            if !named.contains(&frame.surface_id) {
                frame.clear_contents();
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
    fn seed_remove_animation_from(&mut self, idx: usize, anchor_idx: usize) -> Option<Animation> {
        if idx >= self.frames.len() || self.clock.should_complete_instantly() {
            return None;
        }
        let clock = self.clock.clone();
        let offset = self.stride(&self.frames[idx]);
        let (slide, from) = if anchor_idx <= idx {
            (&mut self.frames[idx + 1..], offset)
        } else {
            (&mut self.frames[..idx], -offset)
        };
        if slide.is_empty() {
            return None;
        }
        let slide_in = Animation::new(clock, from, 0.0, 0., slide_spring());
        for f in slide {
            f.move_anim = Some(slide_in.clone());
        }
        Some(slide_in)
    }

    fn active_idx_after_removal(&self, idx: usize, focus: RemovalFocus) -> usize {
        let last_after_idx = self.frames.len().saturating_sub(2);
        match focus {
            RemovalFocus::CurrentActive if idx < self.active_idx => self.active_idx - 1,
            RemovalFocus::CurrentActive if idx > self.active_idx => self.active_idx,
            RemovalFocus::CurrentActive | RemovalFocus::RemovedSlot => idx.min(last_after_idx),
        }
    }

    /// Remove frame `idx`, returning it with the slide its neighbours make into the gap.
    fn remove_frame_at_with_focus(
        &mut self,
        idx: usize,
        focus: RemovalFocus,
    ) -> Option<(Frame, Option<Animation>)> {
        if idx >= self.frames.len() {
            return None;
        }

        let anchor_idx = match focus {
            RemovalFocus::CurrentActive => self.active_idx,
            RemovalFocus::RemovedSlot => idx,
        };
        let new_active = self.active_idx_after_removal(idx, focus);

        // The anchor's side stays still, so past the gap the view moves with it.
        let mut view_pos = self.view_pos();
        if anchor_idx > idx {
            view_pos -= self.stride(&self.frames[idx]);
        }
        let slide = self.seed_remove_animation_from(idx, anchor_idx);
        let frame = self.frames.remove(idx);
        self.set_active_from_view_pos(new_active, view_pos, 0.);
        Some((frame, slide))
    }

    /// Remove a closed frame and fade its snapshot out where it stood.
    fn close_frame_at_with_focus(&mut self, idx: usize, focus: RemovalFocus) {
        let screen_x = self.screen_x_of_frame(idx);
        let Some((frame, slide)) = self.remove_frame_at_with_focus(idx, focus) else {
            return;
        };
        let Some(snapshot) = frame.close_snapshot else {
            return;
        };
        // In strip coordinates as they are after the removal.
        let pos = Point::from((screen_x + self.view_pos(), 0.));
        // Fade only after the neighbours have slid in over it.
        let delay = slide.map_or(Duration::ZERO, |slide| slide.duration());
        let closing = Closing::new(snapshot, self.clock.clone()).delayed(delay);
        self.closing_frames.push((pos, closing));
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

    /// `remove_frame_at` for a frame that closed, fading it out.
    pub fn close_frame_at(&mut self, idx: usize) {
        self.close_frame_at_with_focus(idx, RemovalFocus::CurrentActive);
    }

    /// Close frame `idx` using active-close semantics, regardless of the
    /// strip's current `active_idx`.
    ///
    /// Used when close intent was captured before a later focus relay changed
    /// `active_idx`: the destroyed slot chooses the right neighbour if present,
    /// otherwise the left neighbour.
    pub fn close_active_frame_at(&mut self, idx: usize) {
        self.close_frame_at_with_focus(idx, RemovalFocus::RemovedSlot);
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
                    focus_id: std::num::NonZeroU64::MIN,
                    width: w,
                    ..Frame::new(0)
                })
                .collect(),
            gap,
            fullscreen_slot: Rectangle::default(),
            active_idx,
            view_offset: ViewOffset::default(),
            overview_shift: 0.0,
            scale: 1.0,
            clock: Clock::default(),
            closing_frames: Vec::new(),
        }
    }

    fn strip_with_ids(ids: &[u64]) -> Strip {
        let mut strip = Strip::new(Clock::default());
        strip.frames = ids.iter().copied().map(Frame::new).collect();
        strip
    }

    fn frame_ids(strip: &Strip) -> Vec<u64> {
        strip.frame_ids().collect()
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
                .map(|(idx, _, rect, _)| (idx, rect.loc.x, rect.size.w, rect.size.h))
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

            offset.animate_to(Clock::default(), f64::from(target), 0., 1.);

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
            let reference = Animation::new(clock.clone(), f64::from(from), f64::from(target), 0., slide_spring());
            let mut offset = ViewOffset::Static(f64::from(from));
            offset.animate_to(clock.clone(), f64::from(target), 0., 1.);

            clock.set(start + Duration::from_millis(elapsed_ms));
            offset.animate_to(clock.clone(), f64::from(target), 0., 1.);
            clock.set(start + Duration::from_millis(elapsed_ms * 2));

            prop_assert_eq!(offset.current(), reference.value());
        }

        #[test]
        fn removal_keeps_the_active_frame_still(
            widths in prop::collection::vec(1.0f64..MAX_WIDTH as f64, 2..=MAX_FRAMES),
            gap in 0.0f64..MAX_GAP as f64,
            raw_active in 0usize..MAX_FRAMES,
            raw_remove in 0usize..MAX_FRAMES,
        ) {
            let len = widths.len();
            let active = raw_active % len;
            let idx = raw_remove % len;
            prop_assume!(idx != active);
            let mut strip = strip_with(widths, gap, active);
            let before = strip.screen_x_of_frame(active);

            strip.remove_frame_at(idx);

            let after = strip.screen_x_of_frame(if idx < active { active - 1 } else { active });
            prop_assert!((after - before).abs() <= 1. / strip.scale);
        }

        #[test]
        fn move_frame_keeps_every_frame_still_at_the_instant(
            widths in prop::collection::vec(1.0f64..MAX_WIDTH as f64, 2..=MAX_FRAMES),
            gap in 0.0f64..MAX_GAP as f64,
            raw_active in 0usize..MAX_FRAMES,
            raw_from in 0usize..MAX_FRAMES,
            raw_to in 0usize..MAX_FRAMES,
        ) {
            let len = widths.len();
            let mut strip = strip_with(widths, gap, raw_active % len);
            for (idx, frame) in strip.frames.iter_mut().enumerate() {
                frame.surface_id = 100 + idx as u64;
            }
            let (from, to) = (raw_from % len, raw_to % len);
            let active_id = strip.active_frame_id().unwrap();
            let before: Vec<(u64, f64)> = (0..len)
                .map(|idx| (strip.frames[idx].surface_id, strip.screen_x_of_frame(idx)))
                .collect();

            strip.move_frame(from, to);

            prop_assert_eq!(strip.active_frame_id().unwrap(), active_id);
            for (id, x) in before {
                let idx = strip.index_of(id).unwrap();
                prop_assert!((strip.screen_x_of_frame(idx) - x).abs() <= 1. / strip.scale);
            }
        }

        #[test]
        fn merge_layout_changes_neither_frame_set_nor_order(
            len in 1usize..=MAX_FRAMES,
            named in prop::collection::vec(any::<bool>(), MAX_FRAMES),
            shuffle in prop::collection::vec(0usize..MAX_FRAMES, MAX_FRAMES),
            strangers in prop::collection::vec(20_000u64..20_010, 0..3),
        ) {
            let ids = (0..len).map(|idx| 10_000 + idx as u64).collect::<Vec<_>>();
            let mut layout_ids = ids
                .iter()
                .copied()
                .zip(&named)
                .filter_map(|(id, &named)| named.then_some(id))
                .chain(strangers)
                .collect::<Vec<_>>();
            let len = layout_ids.len();
            for (idx, &target) in shuffle.iter().enumerate().take(len) {
                layout_ids.swap(idx, target % len);
            }
            let mut strip = strip_with_ids(&ids);

            strip.merge_layout_frames(layout_ids.into_iter().map(Frame::new).collect());

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

    proptest! {
        /// A frame showing fullscreen spans the output in strip space, the others their width.
        #[test]
        fn fullscreen_frames_span_the_output(
            widths in prop::collection::vec(1u32..=MAX_WIDTH, 1..=MAX_FRAMES),
            fullscreen in prop::collection::vec(any::<bool>(), MAX_FRAMES),
            inset_x in 0i32..200,
            inset_y in 0i32..200,
            out_w in 800u32..=4000,
            out_h in 600u32..=3000,
        ) {
            let mut s = strip_with(widths.iter().map(|w| f64::from(*w)).collect(), 0.0, 0);
            let wa = Rectangle::new(
                Point::from((inset_x, inset_y)),
                Size::from((out_w as i32 - inset_x, out_h as i32 - inset_y)),
            );
            let output = Size::from((f64::from(out_w), f64::from(out_h)));
            s.set_view(wa, output);
            for (frame, &fills) in s.frames.iter_mut().zip(&fullscreen) {
                if fills {
                    let mut entry =
                        LayoutEntry::surface(std::num::NonZeroU64::MIN, String::new(), 1, 0, 0, 1, 1);
                    entry.set_fullscreen(true);
                    frame.entries.push(entry);
                }
            }
            let view = Size::from((f64::from(wa.size.w), f64::from(wa.size.h)));
            let viewport = Rectangle::new(Point::from((-1e6, -1e6)), Size::from((2e6, 2e6)));
            let mut x = 0.0;
            for (idx, frame, rect, _) in s.frames_with_render_geo(view, viewport) {
                let expected = if frame.fullscreen_entry().is_some() {
                    Rectangle::new(
                        Point::from((x - f64::from(inset_x), -f64::from(inset_y))),
                        output,
                    )
                } else {
                    Rectangle::new(Point::from((x, 0.0)), Size::from((frame.width, view.h)))
                };
                prop_assert_eq!(rect, expected, "frame {}", idx);
                x += if frame.fullscreen_entry().is_some() { output.w } else { frame.width };
            }
        }
    }
}
