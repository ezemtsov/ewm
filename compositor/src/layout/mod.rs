//! Frames on outputs: strips, floating spaces, where frames live and what sits under a point.

pub mod closing_frame;
pub(crate) mod entry;
pub(crate) mod ewm;
pub mod floating;
pub mod overview;
pub mod resize_fade;
pub mod snapshot;
pub mod strip;
pub mod transition;

pub(crate) use ewm::enter_output_for_window;

use std::collections::{HashMap, HashSet};
use std::mem;

use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::layout::closing_frame::Closing;
use crate::layout::snapshot::Snapshot;
use crate::{FocusId, LayoutEntry, LayoutEntryId, animation};

#[derive(Debug, Clone, Copy)]
pub(crate) struct LayoutEntryLocation<'a> {
    pub(crate) monitor: &'a Monitor,
    pub(crate) frame_idx: usize,
    pub(crate) floating: bool,
    pub(crate) frame: &'a strip::Frame,
    pub(crate) entry: &'a LayoutEntry,
}

impl<'a> LayoutEntryLocation<'a> {
    /// Whether the entry is on screen: in a shown float, or in the strip's active frame.
    pub(crate) fn shown(&self) -> bool {
        self.frame_location().shown()
    }

    pub(crate) fn frame_location(&self) -> FrameLocation<'a> {
        FrameLocation {
            monitor: self.monitor,
            frame_idx: self.frame_idx,
            floating: self.floating,
            frame: self.frame,
        }
    }

    /// The strip frame this view shows on, a float's being the one it belongs to.
    fn strip_frame(&self) -> Option<u64> {
        if self.floating {
            self.monitor.strip.frame_of_float(self.frame)
        } else {
            Some(self.frame.surface_id)
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FrameLocation<'a> {
    pub monitor: &'a Monitor,
    pub frame_idx: usize,
    pub floating: bool,
    pub frame: &'a strip::Frame,
}

impl<'a> FrameLocation<'a> {
    /// Whether the frame is on screen: a shown float, or the strip's active frame.
    pub(crate) fn shown(&self) -> bool {
        if self.floating {
            self.monitor.float_shown(self.frame)
        } else {
            self.monitor.strip.active_idx() == self.frame_idx
        }
    }

    /// Screen x of the strip slot the frame rides in.
    pub(crate) fn slot_x(&self) -> f64 {
        if self.floating {
            self.monitor.pin_offset(self.frame)
        } else {
            self.monitor.strip.screen_x_of_frame(self.frame_idx)
        }
    }

    /// The frame's entries at this slot.
    fn entries(self) -> impl Iterator<Item = LayoutEntryLocation<'a>> {
        self.frame
            .entries
            .iter()
            .map(move |entry| LayoutEntryLocation {
                monitor: self.monitor,
                frame_idx: self.frame_idx,
                floating: self.floating,
                frame: self.frame,
                entry,
            })
    }
}

/// A floating frame following the pointer.
#[derive(Debug, Clone, Copy)]
struct InteractiveMove {
    frame_id: u64,
    /// Where the pointer grabbed the frame, as a fraction of its size.
    pointer_ratio: (f64, f64),
}

/// Where a new frame goes on its output.
#[derive(Debug, Clone, Copy)]
pub enum Placement {
    /// On the strip right of `after`, else at the end.
    Tiled { after: Option<u64> },
    /// In the floating space at `pos`, else centered.
    Floating { pos: Option<Point<f64, Logical>> },
}

/// One output's frames: the strip and the floating space above it.
#[derive(Debug)]
pub struct Monitor {
    output: Output,
    name: String,
    pub strip: strip::Strip,
    pub floating: floating::FloatingSpace,
    /// Working area last reported to Emacs.
    pub working_area: Rectangle<i32, Logical>,
    /// The working area changed since the last layout, whose views then jump into place.
    pub resized: bool,
    /// The frame this display showed last, active again as it returns.
    last_active: Option<u64>,
}

impl Monitor {
    /// Whether the views of `frame` jump: under a resize grab, or after the working area changed.
    pub(crate) fn settles_at_once(&self, frame: u64) -> bool {
        self.resized || self.floating.is_resizing(frame)
    }

    pub fn output(&self) -> &Output {
        &self.output
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The output's rectangle in the global space, none before it has a mode.
    pub fn rect(&self) -> Option<Rectangle<i32, Logical>> {
        let mode = self.output.current_mode()?;
        let size = self
            .output
            .current_transform()
            .transform_size(mode.size)
            .to_f64()
            .to_logical(self.output.current_scale().fractional_scale())
            .to_i32_ceil();
        Some(Rectangle::new(self.output.current_location(), size))
    }

    fn has_frame(&self, id: u64) -> bool {
        self.strip.index_of(id).is_some() || self.floating.has_frame(id)
    }

    /// Frames on this monitor, tiled then floating.
    pub fn frames(&self) -> impl Iterator<Item = &strip::Frame> {
        self.strip.frames().iter().chain(self.floating.frames())
    }

    fn frames_mut(&mut self) -> impl Iterator<Item = &mut strip::Frame> {
        self.strip
            .frames_mut()
            .iter_mut()
            .chain(self.floating.frames_mut().iter_mut())
    }

    fn frame_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.frames().map(|frame| frame.surface_id)
    }

    /// The frame holding the keyboard while this is the active output.
    fn focused_frame(&self) -> Option<&strip::Frame> {
        if self.floating.is_focused() {
            self.floating.active_frame()
        } else {
            self.strip.active_frame()
        }
    }

    /// Whether the active float is on screen.
    fn active_float_shown(&self) -> bool {
        self.floating
            .active_frame()
            .is_some_and(|float| self.float_shown(float))
    }

    /// The floats hold the keyboard only while their active one is shown, and take it from an
    /// empty strip.
    fn settle_focus_layer(&mut self) {
        let keep = self.floating.is_focused() || self.strip.frames().is_empty();
        self.floating.set_focused(self.active_float_shown() && keep);
    }

    /// Keep each float pinned while its frame lasts, a fullscreen float falling back to the active
    /// frame and a float with plain views dropping its pin.
    fn settle_pins(&mut self) {
        let strip = &self.strip;
        for float in self.floating.frames_mut() {
            float.pinned_to = if float.fullscreen_entry().is_some() {
                strip.frame_of_float(float)
            } else if float.has_views() {
                None
            } else {
                float.pinned_to.filter(|&id| strip.index_of(id).is_some())
            };
        }
    }

    /// Pull out the frames `take` selects, a strip frame with the floats pinned to it.
    fn drain_frames(&mut self, mut take: impl FnMut(&strip::Frame) -> bool) -> Frames {
        let tiled = self.strip.drain_frames(&mut take);
        let floating = self.floating.drain_frames(|frame| {
            take(frame)
                || frame
                    .pinned_to
                    .is_some_and(|pin| tiled.iter().any(|tile| tile.surface_id == pin))
        });
        Frames { tiled, floating }
    }

    /// Whether `frame`, a float, is on screen: unpinned or pinned to the active frame.
    pub fn float_shown(&self, frame: &strip::Frame) -> bool {
        frame
            .pinned_to
            .is_none_or(|id| Some(id) == self.strip.active_frame_id())
    }

    /// How far a pinned float has slid with its strip frame.
    pub(crate) fn pin_offset(&self, float: &strip::Frame) -> f64 {
        float
            .pinned_to
            .and_then(|id| self.strip.index_of(id))
            .map_or(0., |idx| self.strip.screen_x_of_frame(idx))
    }

    /// Floats with their slots, pinned ones sliding with their frame, topmost first.
    pub(crate) fn floats_with_render_geo(
        &self,
    ) -> impl Iterator<Item = (&strip::Frame, Rectangle<f64, Logical>)> {
        self.floating
            .frames_with_render_geo()
            .map(|(frame, mut slot)| {
                slot.loc.x += self.pin_offset(frame);
                (frame, slot)
            })
    }

    /// Span the fullscreen slots over the output, or over `working_area` while it has no mode.
    pub(crate) fn set_view(&mut self, working_area: Rectangle<i32, Logical>) {
        let size = self
            .output
            .current_mode()
            .map_or(working_area.size.to_f64(), |_| {
                crate::utils::output_size(&self.output)
            });
        self.strip.set_view(working_area, size);
        self.floating.set_view(size);
    }

    /// The topmost float pinned to strip frame `id` showing fullscreen.
    pub(crate) fn fullscreen_float_on(&self, id: u64) -> Option<&strip::Frame> {
        self.floating
            .frames()
            .iter()
            .find(|float| float.pinned_to == Some(id) && float.fullscreen_entry().is_some())
    }

    /// The frame covering the output while strip frame `id` is active.
    pub(crate) fn fullscreen_on(&self, id: u64) -> Option<u64> {
        self.fullscreen_float_on(id)
            .or_else(|| {
                self.strip
                    .frames()
                    .iter()
                    .find(|frame| frame.surface_id == id && frame.fullscreen_entry().is_some())
            })
            .map(|frame| frame.surface_id)
    }

    /// The frame covering the output.
    pub fn presenting_fullscreen(&self) -> Option<u64> {
        self.fullscreen_on(self.strip.active_frame_id()?)
    }

    pub(crate) fn advance(&mut self) {
        self.strip.advance();
        self.floating.advance();
    }

    pub(crate) fn animations_ongoing(&self) -> bool {
        self.strip.animations_ongoing() || self.floating.animations_ongoing()
    }

    /// The tiled or the floating frames with their slots.
    fn frame_locations_in(&self, floating: bool) -> impl Iterator<Item = FrameLocation<'_>> {
        let frames = if floating {
            self.floating.frames()
        } else {
            self.strip.frames()
        };
        frames
            .iter()
            .enumerate()
            .map(move |(frame_idx, frame)| FrameLocation {
                monitor: self,
                frame_idx,
                floating,
                frame,
            })
    }

    /// Every frame with its slot, tiled then floating.
    fn frame_locations(&self) -> impl Iterator<Item = FrameLocation<'_>> {
        self.frame_locations_in(false)
            .chain(self.frame_locations_in(true))
    }

    /// The tiled or the floating entries with their frames.
    fn entry_locations_in(&self, floating: bool) -> impl Iterator<Item = LayoutEntryLocation<'_>> {
        self.frame_locations_in(floating)
            .flat_map(FrameLocation::entries)
    }

    /// Every entry with its frame, tiled then floating.
    fn entry_locations(&self) -> impl Iterator<Item = LayoutEntryLocation<'_>> {
        self.frame_locations().flat_map(FrameLocation::entries)
    }
}

/// Tiled and floating frames together.
#[derive(Debug, Default)]
pub struct Frames {
    pub tiled: Vec<strip::Frame>,
    pub floating: Vec<strip::Frame>,
}

impl Frames {
    fn iter(&self) -> impl Iterator<Item = &strip::Frame> {
        self.tiled.iter().chain(&self.floating)
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut strip::Frame> {
        self.tiled.iter_mut().chain(&mut self.floating)
    }

    pub(crate) fn ids(&self) -> Vec<u64> {
        self.iter().map(|frame| frame.surface_id).collect()
    }
}

/// Frames that moved, with where from, where to and the frames active before on either side.
#[derive(Debug, Default)]
pub(crate) struct Moved {
    pub ids: Vec<u64>,
    pub src: Option<Output>,
    pub dst: Option<Output>,
    pub src_prev_active: Option<u64>,
    pub dst_prev_active: Option<u64>,
    /// Compositor-owned frames dropped for Emacs to delete.
    pub retired: Vec<u64>,
}

/// An output whose active frame may have changed, with the frame active before.
#[derive(Debug)]
pub(crate) struct ActiveChange {
    pub output: Output,
    pub prev_active: Option<u64>,
}

/// Where every Emacs frame is.
#[derive(Debug)]
pub struct Layout {
    /// One per mapped output.
    monitors: Vec<Monitor>,
    /// The output whose active frame holds the keyboard.
    active_output: Option<Output>,
    /// Frames of the last output to go, adopted by the next output to map.
    homeless: Frames,
    /// Last active frame per unplugged display, handed to its monitor when it returns.
    last_active: HashMap<String, u64>,
    /// Floating frame the pointer is dragging.
    interactive_move: Option<InteractiveMove>,
    /// Clock for driving animations.
    clock: animation::Clock,
}

impl Layout {
    pub fn new(clock: animation::Clock) -> Self {
        Self {
            monitors: Vec::new(),
            active_output: None,
            homeless: Frames::default(),
            last_active: HashMap::new(),
            interactive_move: None,
            clock,
        }
    }

    pub fn monitors(&self) -> &[Monitor] {
        &self.monitors
    }

    /// Order the monitors as `outputs` are, unknown ones last.
    pub(crate) fn sort_monitors(&mut self, outputs: &[Output]) {
        self.monitors.sort_by_key(|monitor| {
            outputs
                .iter()
                .position(|output| output == &monitor.output)
                .unwrap_or(usize::MAX)
        });
    }

    /// The frame holding the keyboard: the focused frame of the active output.
    pub fn focused_frame(&self) -> Option<&strip::Frame> {
        self.active_output
            .as_ref()
            .and_then(|output| self.monitor_of(output))
            .and_then(Monitor::focused_frame)
    }

    /// Surface id of the frame holding the keyboard.
    pub fn focused_frame_id(&self) -> Option<u64> {
        self.focused_frame().map(|frame| frame.surface_id)
    }

    /// The output whose frame holds the keyboard.
    pub fn focused_output(&self) -> Option<&Output> {
        self.active_output.as_ref()
    }

    /// The frame holding the keyboard on `output`, or that would once it is the active output.
    pub(crate) fn focused_frame_on(&self, output: &Output) -> Option<u64> {
        self.monitor_of(output)?
            .focused_frame()
            .map(|frame| frame.surface_id)
    }

    /// Settle each floating space's hold on the keyboard, then keep it on a frame while a monitor
    /// has one, the first in output order otherwise.
    fn settle_focus(&mut self) {
        for monitor in &mut self.monitors {
            monitor.settle_focus_layer();
        }
        if self.focused_frame().is_none() {
            self.active_output = self
                .monitors
                .iter()
                .find(|monitor| monitor.focused_frame().is_some())
                .map(|monitor| monitor.output.clone());
        }
    }

    /// A frame that moved keeps the keyboard.
    fn follow_focus(&mut self, focused: Option<u64>, moved: &Moved) {
        if let Some(focused) = focused.filter(|id| moved.ids.contains(id)) {
            self.activate_frame(focused, None);
        }
    }

    fn monitor_idx(&self, output: &Output) -> Option<usize> {
        self.monitors
            .iter()
            .position(|monitor| &monitor.output == output)
    }

    /// The mapped output called `name`.
    pub fn output(&self, name: &str) -> Option<&Output> {
        self.monitors
            .iter()
            .find(|monitor| monitor.name == name)
            .map(Monitor::output)
    }

    /// The rectangle of `output` in the global space, none without a monitor or a mode.
    pub fn output_rect(&self, output: &Output) -> Option<Rectangle<i32, Logical>> {
        self.monitor_of(output)?.rect()
    }

    pub fn monitor_of(&self, output: &Output) -> Option<&Monitor> {
        self.monitors
            .iter()
            .find(|monitor| &monitor.output == output)
    }

    pub fn monitor_of_mut(&mut self, output: &Output) -> Option<&mut Monitor> {
        self.monitors
            .iter_mut()
            .find(|monitor| &monitor.output == output)
    }

    pub fn strip(&self, output: &Output) -> Option<&strip::Strip> {
        self.monitor_of(output).map(|monitor| &monitor.strip)
    }

    pub fn strip_mut(&mut self, output: &Output) -> Option<&mut strip::Strip> {
        self.monitor_of_mut(output)
            .map(|monitor| &mut monitor.strip)
    }

    pub fn floating(&self, output: &Output) -> Option<&floating::FloatingSpace> {
        self.monitor_of(output).map(|monitor| &monitor.floating)
    }

    pub fn floating_mut(&mut self, output: &Output) -> Option<&mut floating::FloatingSpace> {
        self.monitor_of_mut(output)
            .map(|monitor| &mut monitor.floating)
    }

    pub fn outputs(&self) -> impl Iterator<Item = &Output> {
        self.monitors.iter().map(Monitor::output)
    }

    /// Every monitor's strip with its output name.
    pub fn strips(&self) -> impl Iterator<Item = (&str, &strip::Strip)> {
        self.monitors
            .iter()
            .map(|monitor| (monitor.name.as_str(), &monitor.strip))
    }

    pub fn strips_mut(&mut self) -> impl Iterator<Item = &mut strip::Strip> {
        self.monitors.iter_mut().map(|monitor| &mut monitor.strip)
    }

    pub fn homeless(&self) -> &Frames {
        &self.homeless
    }

    /// Add a monitor for a newly mapped output.
    pub fn add_output(&mut self, output: Output, working_area: Rectangle<i32, Logical>) {
        let name = output.name();
        if self.monitor_of(&output).is_some() {
            return;
        }
        let last_active = self
            .last_active
            .remove(&crate::utils::output_identity(&output));
        let mut strip = strip::Strip::new(self.clock.clone());
        strip.update_config(output.current_scale().fractional_scale());
        let floating = floating::FloatingSpace::new(working_area.to_f64(), self.clock.clone());
        let mut monitor = Monitor {
            output,
            name,
            strip,
            floating,
            working_area,
            resized: false,
            last_active,
        };
        monitor.set_view(working_area);
        self.monitors.push(monitor);
    }

    /// Drop `output`'s monitor, moving its frames to `fallback` or parking them,
    /// and remember its active frame for the display's return.
    pub(crate) fn detach_output(&mut self, output: &Output, fallback: Option<&Output>) -> Moved {
        let Some(idx) = self.monitor_idx(output) else {
            return Moved::default();
        };
        let focused = self.focused_frame_id();
        let mut monitor = self.monitors.remove(idx);
        let src_prev_active = monitor.strip.active_frame_id();
        let identity = crate::utils::output_identity(output);
        match src_prev_active {
            Some(id) => {
                self.last_active.insert(identity, id);
            }
            None => {
                self.last_active.remove(&identity);
            }
        }
        let mut frames = monitor.drain_frames(|_| true);
        // A filler nobody used or named dies with its output, except Emacs's last frame.
        let last_frames =
            self.all_frames().next().is_none() && frames.iter().all(strip::Frame::is_filler);
        let retired: Vec<u64> = if last_frames {
            Vec::new()
        } else {
            frames
                .tiled
                .extract_if(.., |frame| frame.is_filler())
                .chain(frames.floating.extract_if(.., |frame| frame.is_filler()))
                .map(|frame| frame.surface_id)
                .collect()
        };
        self.interactive_move = self
            .interactive_move
            .filter(|move_| !retired.contains(&move_.frame_id));
        let placed = match fallback.and_then(|dst| self.monitor_idx(dst)) {
            Some(dst_idx) => self.place_frames(dst_idx, None, frames),
            None => {
                let ids = frames.ids();
                self.park(frames);
                Moved {
                    ids,
                    ..Moved::default()
                }
            }
        };
        self.follow_focus(focused, &placed);
        Moved {
            src: Some(monitor.output),
            src_prev_active,
            retired,
            ..placed
        }
    }

    /// Hold `frames` until an output maps.
    fn park(&mut self, frames: Frames) {
        // A parked frame has no floating space, so its drag is over.
        self.interactive_move = self.interactive_move.filter(|move_| {
            !frames
                .floating
                .iter()
                .any(|frame| frame.surface_id == move_.frame_id)
        });
        self.homeless.tiled.extend(frames.tiled);
        self.homeless.floating.extend(frames.floating);
        self.settle_focus();
    }

    /// Replace `output`'s strip wholesale, homing its frames there.
    pub fn install_strip(&mut self, output: &Output, strip: strip::Strip) {
        let home = crate::utils::output_identity(output);
        let Some(monitor) = self.monitor_of_mut(output) else {
            return;
        };
        monitor.strip = strip;
        for frame in monitor.strip.frames_mut() {
            frame.home = home.clone();
        }
        self.settle_focus();
    }

    /// Put new frame `id` on `output` as `placement` says.
    pub fn add_frame(&mut self, id: u64, output: &Output, placement: Placement) {
        let Some(monitor) = self.monitor_of_mut(output) else {
            return;
        };
        let mut frame = strip::Frame::new(id);
        frame.home = crate::utils::output_identity(output);
        match placement {
            Placement::Tiled { after } => {
                let at = monitor.strip.slot_after(after);
                monitor.strip.insert_frame(at, frame);
            }
            Placement::Floating { pos } => {
                frame.floating_pos = pos.map(|pos| {
                    floating::Data::logical_to_size_frac_in_working_area(
                        monitor.floating.working_area(),
                        pos,
                    )
                });
                monitor.floating.add_frame(frame);
            }
        }
        // A fullscreen float pins to the first strip frame as it appears.
        monitor.settle_pins();
        self.settle_focus();
    }

    /// Frames on monitors, each strip then its floating space.
    pub(crate) fn placed_frames(&self) -> impl Iterator<Item = &strip::Frame> {
        self.monitors.iter().flat_map(Monitor::frames)
    }

    /// Placed frames, then the parked ones.
    pub fn all_frames(&self) -> impl Iterator<Item = &strip::Frame> {
        self.placed_frames().chain(self.homeless.iter())
    }

    fn all_frames_mut(&mut self) -> impl Iterator<Item = &mut strip::Frame> {
        self.monitors
            .iter_mut()
            .flat_map(Monitor::frames_mut)
            .chain(self.homeless.iter_mut())
    }

    pub(crate) fn frame_mut(&mut self, frame_surface_id: u64) -> Option<&mut strip::Frame> {
        self.all_frames_mut()
            .find(|frame| frame.surface_id == frame_surface_id)
    }

    /// Whether frame `frame_surface_id` is parked.
    pub fn is_homeless(&self, frame_surface_id: u64) -> bool {
        self.homeless
            .iter()
            .any(|frame| frame.surface_id == frame_surface_id)
    }

    fn monitor_of_frame_mut(&mut self, id: u64) -> Option<&mut Monitor> {
        self.monitors
            .iter_mut()
            .find(|monitor| monitor.has_frame(id))
    }

    /// Run `tiled` or `floating` on whichever container holds frame `id`. Returns its output with
    /// the strip frame active before.
    fn with_frame_container(
        &mut self,
        id: u64,
        tiled: impl FnOnce(&mut Monitor, usize),
        floating: impl FnOnce(&mut Monitor),
    ) -> Option<ActiveChange> {
        let monitor = self.monitor_of_frame_mut(id)?;
        let prev_active = monitor.strip.active_frame_id();
        match monitor.strip.index_of(id) {
            Some(idx) => tiled(monitor, idx),
            None => floating(monitor),
        }
        Some(ActiveChange {
            output: monitor.output.clone(),
            prev_active,
        })
    }

    /// Drop frame `id` from wherever it is. For a placed frame, returns its
    /// output and the frame its strip had active.
    pub(crate) fn remove_frame(&mut self, id: u64, active_close: bool) -> Option<ActiveChange> {
        self.interactive_move = self.interactive_move.filter(|move_| move_.frame_id != id);
        self.homeless.tiled.retain(|frame| frame.surface_id != id);
        self.homeless
            .floating
            .retain(|frame| frame.surface_id != id);
        let removed = self.with_frame_container(
            id,
            |monitor, idx| {
                if active_close {
                    monitor.strip.close_active_frame_at(idx);
                } else {
                    monitor.strip.close_frame_at(idx);
                }
                monitor.settle_pins();
            },
            |monitor| {
                monitor.floating.close_frame(id);
            },
        );
        self.settle_focus();
        removed
    }

    /// Put `frames` on monitor `idx`, tiled after `after` or last. A returning display wakes its
    /// remembered frame unless the keyboard is on this strip, an empty strip wakes its first.
    fn place_frames(&mut self, idx: usize, after: Option<u64>, frames: Frames) -> Moved {
        let ids = frames.ids();
        let keyboard_here = self.active_output.as_ref() == Some(&self.monitors[idx].output);
        let monitor = &mut self.monitors[idx];
        let dst_prev_active = monitor.strip.active_frame_id();
        let at = monitor.strip.slot_after(after);
        monitor.strip.insert_frames(at, frames.tiled);
        let remembered = monitor.last_active.take_if(|id| ids.contains(id));
        let strip = &mut monitor.strip;
        let wake = remembered
            .filter(|_| !keyboard_here || monitor.floating.is_focused())
            .and_then(|id| strip.index_of(id));
        if let Some(idx) = wake {
            strip.set_active(idx);
        } else if dst_prev_active.is_none() && !strip.frames().is_empty() {
            strip.set_active(0);
        }
        // `add_frame` stacks on top, so the topmost goes last.
        for frame in frames.floating.into_iter().rev() {
            monitor.floating.add_frame(frame);
        }
        monitor.settle_pins();
        let moved = Moved {
            ids,
            dst: Some(monitor.output.clone()),
            dst_prev_active,
            ..Moved::default()
        };
        self.settle_focus();
        moved
    }

    /// Move frames `ids` from `src` to `dst`, each kind to its own container, tiled after `after`.
    pub(crate) fn move_frames(
        &mut self,
        src: &Output,
        dst: &Output,
        ids: &[u64],
        after: Option<u64>,
    ) -> Moved {
        let (Some(src_idx), Some(dst_idx)) = (self.monitor_idx(src), self.monitor_idx(dst)) else {
            return Moved::default();
        };
        if src_idx == dst_idx {
            return Moved::default();
        }
        let focused = self.focused_frame_id();
        let src = &mut self.monitors[src_idx];
        let src_prev_active = src.strip.active_frame_id();
        let frames = src.drain_frames(|frame| ids.contains(&frame.surface_id));
        let moved = Moved {
            src: Some(src.output.clone()),
            src_prev_active,
            ..self.place_frames(dst_idx, after, frames)
        };
        self.follow_focus(focused, &moved);
        moved
    }

    /// Move frame `id` to `dst` on purpose, after its active frame, homing it and its floats
    /// there even when it already sits on `dst`.
    pub(crate) fn move_frame_to_output(&mut self, id: u64, dst: &Output) -> Moved {
        let src = self
            .frame_location(id)
            .map(|location| location.monitor.output().clone());
        let Some(src) = src else {
            return Moved::default();
        };
        let after = self.strip(dst).and_then(strip::Strip::active_frame_id);
        let moved = self.move_frames(&src, dst, &[id], after);
        let home = crate::utils::output_identity(dst);
        if let Some(monitor) = self.monitor_of_mut(dst) {
            for frame in monitor
                .frames_mut()
                .filter(|frame| frame.surface_id == id || moved.ids.contains(&frame.surface_id))
            {
                frame.home = home.clone();
            }
        }
        moved
    }

    /// Put every parked frame on `dst`.
    pub(crate) fn adopt_homeless(&mut self, dst: &Output) -> Moved {
        let Some(idx) = self.monitor_idx(dst) else {
            return Moved::default();
        };
        let frames = mem::take(&mut self.homeless);
        self.place_frames(idx, None, frames)
    }

    /// Unpinned frames homed at `dst`'s display, grouped by the other output holding them.
    pub(crate) fn homed_elsewhere(&self, dst: &Output) -> Vec<(Output, Vec<u64>)> {
        let identity = crate::utils::output_identity(dst);
        self.monitors
            .iter()
            .filter(|monitor| &monitor.output != dst)
            .filter_map(|monitor| {
                let ids: Vec<u64> = monitor
                    .frames()
                    .filter(|frame| frame.pinned_to.is_none() && frame.home == identity)
                    .map(|frame| frame.surface_id)
                    .collect();
                (!ids.is_empty()).then(|| (monitor.output.clone(), ids))
            })
            .collect()
    }

    /// Forget the remembered frame of `output` once every frame it gets back has landed.
    pub(crate) fn forget_last_active(&mut self, output: &Output) {
        if let Some(monitor) = self.monitor_of_mut(output) {
            monitor.last_active = None;
        }
    }

    /// Give `id` the keyboard, on view `entry` when given: its output becomes the active one and
    /// it the active frame of its container, a pinned float its strip frame too. Returns the
    /// output and its old active frame.
    pub(crate) fn activate_frame(
        &mut self,
        id: u64,
        entry: Option<LayoutEntryId>,
    ) -> Option<ActiveChange> {
        // The view is the frame's selection until the next layout confirms it.
        if let Some(entry) = entry
            && let Some(frame) = self.frame_mut(id)
        {
            frame.selected_entry_id = Some(entry);
        }
        let activated = self.with_frame_container(
            id,
            |monitor, idx| {
                monitor.strip.set_active(idx);
                monitor.floating.set_focused(false);
            },
            |monitor| {
                monitor.floating.activate_frame(id);
                if let Some(idx) = monitor
                    .floating
                    .frame(id)
                    .and_then(|float| float.pinned_to)
                    .and_then(|pin| monitor.strip.index_of(pin))
                {
                    monitor.strip.set_active(idx);
                }
            },
        )?;
        self.active_output = Some(activated.output.clone());
        Some(activated)
    }

    /// Show strip frame `id` active, the keyboard following only a frame that goes off screen.
    /// Returns the output and its old active frame.
    pub(crate) fn show_strip_frame(&mut self, id: u64) -> Option<ActiveChange> {
        let shown =
            self.with_frame_container(id, |monitor, idx| monitor.strip.set_active(idx), |_| {});
        self.settle_focus();
        shown
    }

    /// End the strip swipe on `output`, snapping to the nearest frame. Returns the output and its
    /// old active frame.
    pub(crate) fn strip_gesture_end(
        &mut self,
        output: &Output,
        working_w: f64,
    ) -> Option<ActiveChange> {
        let monitor = self.monitor_of_mut(output)?;
        let prev_active = monitor.strip.active_frame_id();
        monitor.strip.gesture_end(working_w);
        let output = monitor.output.clone();
        self.settle_focus();
        Some(ActiveChange {
            output,
            prev_active,
        })
    }

    /// A layout has laid every output out anew.
    pub(crate) fn settle_resized(&mut self) {
        for monitor in &mut self.monitors {
            monitor.resized = false;
        }
    }

    /// Whether surface `id` shows fullscreen on some output.
    pub fn is_surface_fullscreen(&self, id: u64) -> bool {
        self.entry_locations()
            .any(|location| location.entry.fullscreen() && location.entry.surface_id() == Some(id))
    }

    /// The surface entry `entry_id` shows.
    pub fn entry_surface_id(&self, entry_id: LayoutEntryId) -> Option<u64> {
        self.entry_location(entry_id)?.entry.surface_id()
    }

    /// Mark the views of `committed` surfaces and settle every float on its frame.
    pub(crate) fn reconcile_fullscreen(&mut self, committed: impl Fn(u64) -> bool) {
        for frame in self.all_frames_mut() {
            frame.mark_fullscreen(&committed);
        }
        for monitor in &mut self.monitors {
            monitor.settle_pins();
        }
        self.settle_focus();
    }

    /// The view a fullscreen float pinned over strip frame `id` shows, with the float.
    pub(crate) fn fullscreen_float_over(&self, id: u64) -> Option<(u64, LayoutEntryId)> {
        let float = self
            .monitors
            .iter()
            .find(|monitor| monitor.strip.index_of(id).is_some())?
            .fullscreen_float_on(id)?;
        Some((float.surface_id, float.fullscreen_entry()?.id))
    }

    /// Other `requested` surfaces on the strip frames `id` shows on, floats counting for theirs.
    pub(crate) fn fullscreen_rivals(
        &self,
        id: u64,
        requested: impl Fn(u64) -> bool,
    ) -> HashSet<u64> {
        let frames: HashSet<u64> = self
            .entry_locations()
            .filter(|location| location.entry.surface_id() == Some(id))
            .filter_map(|location| location.strip_frame())
            .collect();
        self.entry_locations()
            .filter(|location| {
                location
                    .strip_frame()
                    .is_some_and(|frame| frames.contains(&frame))
            })
            .filter_map(|location| location.entry.surface_id())
            .filter(|&other| other != id && requested(other))
            .collect()
    }

    /// Each monitor's active frame, or every frame when `every_frame`, its floats on screen,
    /// and the surfaces their views show.
    pub(crate) fn shown_surface_ids(&self, every_frame: bool) -> HashSet<u64> {
        self.monitors
            .iter()
            .flat_map(Monitor::frame_locations)
            .filter(|location| every_frame || location.shown())
            .flat_map(|location| location.frame.surface_ids())
            .collect()
    }

    /// Floats on screen, `active` being each monitor's active strip frame.
    pub(crate) fn shown_floats(&self, active: impl Fn(&Monitor) -> Option<u64>) -> HashSet<u64> {
        self.monitors
            .iter()
            .flat_map(|monitor| {
                let active = active(monitor);
                monitor
                    .floating
                    .frames()
                    .iter()
                    .filter(move |frame| frame.pinned_to.is_none_or(|id| Some(id) == active))
                    .map(|frame| frame.surface_id)
            })
            .collect()
    }

    /// Whether some output shows a view fullscreen.
    pub fn any_active_fullscreen(&self) -> bool {
        self.monitors
            .iter()
            .any(|monitor| monitor.presenting_fullscreen().is_some())
    }

    /// Mark one view of each surface primary: its largest among the shown frames, a view shown
    /// fullscreen counting the output while `requested` of its surface, ties broken by `order`.
    pub(crate) fn assign_primaries(&mut self, order: &[Output], requested: impl Fn(u64) -> bool) {
        let mut best: HashMap<u64, (u64, LayoutEntryId)> = HashMap::new();
        let shown = order
            .iter()
            .filter_map(|output| self.monitor_of(output))
            .flat_map(Monitor::entry_locations)
            .filter(LayoutEntryLocation::shown);
        for location in shown {
            let Some(surface_id) = location.entry.surface_id() else {
                continue;
            };
            let fullscreen = location.entry.fullscreen() && requested(surface_id);
            let size = entry_size(location.monitor.output(), location.entry, fullscreen);
            let area = size.w as u64 * size.h as u64;
            let entry = best.entry(surface_id).or_insert((area, location.entry.id));
            // A tie keeps the primary.
            if area > entry.0 || (area == entry.0 && location.entry.primary()) {
                *entry = (area, location.entry.id);
            }
        }
        let primaries: HashSet<LayoutEntryId> = best.values().map(|&(_, id)| id).collect();
        for frame in self.all_frames_mut() {
            for entry in &mut frame.entries {
                entry.set_primary(primaries.contains(&entry.id));
            }
        }
    }

    pub fn frame_location(&self, id: u64) -> Option<FrameLocation<'_>> {
        self.monitors
            .iter()
            .flat_map(Monitor::frame_locations)
            .find(|location| location.frame.surface_id == id)
    }

    /// Whether `id` is an Emacs frame we place, parked ones included.
    pub fn has_frame(&self, id: u64) -> bool {
        self.monitors.iter().any(|monitor| monitor.has_frame(id)) || self.is_homeless(id)
    }

    pub fn frame_output(&self, id: u64) -> Option<&str> {
        self.frame_location(id)
            .map(|location| location.monitor.name())
    }

    /// Surface ids of `output`'s strip frames, left to right.
    pub fn strip_frame_ids(&self, output: &Output) -> impl Iterator<Item = u64> + '_ {
        self.strip(output)
            .into_iter()
            .flat_map(strip::Strip::frame_ids)
    }

    /// Frame surface_id whose focus id matches.
    pub fn frame_with_focus_id(&self, focus_id: FocusId) -> Option<u64> {
        self.placed_frames()
            .find(|frame| frame.focus_id == focus_id)
            .map(|frame| frame.surface_id)
    }

    /// Focus id assigned to the frame with this surface id.
    pub fn focus_id_of_frame(&self, frame_surface_id: u64) -> Option<FocusId> {
        self.frame_location(frame_surface_id)
            .map(|location| location.frame.focus_id)
    }

    /// Surface id of the active frame on `output`'s strip.
    pub fn active_frame_id(&self, output: &Output) -> Option<u64> {
        self.strip(output)?.active_frame_id()
    }

    /// Strip frames that are not their strip's active one.
    pub(crate) fn inactive_frame_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.strips().flat_map(|(_, strip)| {
            let active = strip.active_frame_id();
            strip.frame_ids().filter(move |id| Some(*id) != active)
        })
    }

    /// Every entry with the output showing it.
    pub fn entry_outputs(&self) -> impl Iterator<Item = (LayoutEntryId, &Output)> {
        self.entry_locations()
            .map(|location| (location.entry.id, location.monitor.output()))
    }

    /// Strip frame surface ids per monitor, left to right.
    pub(crate) fn strip_frames_snapshot(&self) -> HashMap<String, Vec<u64>> {
        self.strips()
            .map(|(name, strip)| (name.to_string(), strip.frame_ids().collect()))
            .collect()
    }

    /// Output name per placed frame.
    pub(crate) fn frame_outputs_snapshot(&self) -> HashMap<u64, String> {
        self.monitors
            .iter()
            .flat_map(|monitor| monitor.frame_ids().map(|id| (id, monitor.name.clone())))
            .collect()
    }

    /// Active frame surface id per monitor.
    pub(crate) fn active_frames_snapshot(&self) -> HashMap<String, u64> {
        self.strips()
            .filter_map(|(name, strip)| Some((name.to_string(), strip.active_frame_id()?)))
            .collect()
    }

    /// Surfaces `output` shows: its frames and their entries' surfaces.
    pub(crate) fn surface_ids_on(&self, output: &Output) -> HashSet<u64> {
        self.monitor_of(output)
            .into_iter()
            .flat_map(Monitor::frames)
            .flat_map(strip::Frame::surface_ids)
            .collect()
    }

    /// Every entry with its frame, per monitor tiled then floating.
    pub(crate) fn entry_locations(&self) -> impl Iterator<Item = LayoutEntryLocation<'_>> {
        self.monitors.iter().flat_map(Monitor::entry_locations)
    }

    pub(crate) fn entry_location(
        &self,
        entry_id: LayoutEntryId,
    ) -> Option<LayoutEntryLocation<'_>> {
        self.entry_locations()
            .find(|location| location.entry.id == entry_id)
    }

    /// Detach a surface from every layout entry.
    pub(crate) fn detach_surface(&mut self, id: u64) {
        for frame in self.all_frames_mut() {
            frame.detach_surface(id);
        }
    }

    /// Fade `snapshot` out where frame `id` drew the view it holds.
    pub(crate) fn close_view(&mut self, id: u64, snapshot: Snapshot) {
        let closing = Closing::new(snapshot, self.clock.clone());
        if let Some(frame) = self.frame_mut(id) {
            frame.closing_views.push(closing);
        }
    }

    /// Refill every held frame's contents from `frames`, emptying the ones it leaves out.
    pub fn merge_frames(&mut self, mut frames: Vec<strip::Frame>) {
        for monitor in &mut self.monitors {
            let (tiled, floating): (Vec<_>, Vec<_>) = frames
                .extract_if(.., |frame| monitor.has_frame(frame.surface_id))
                .partition(|frame| monitor.strip.index_of(frame.surface_id).is_some());
            monitor.strip.merge_layout_frames(tiled);
            monitor.floating.merge_layout_frames(floating);
        }
    }

    /// Panic unless every frame and entry sits in exactly one place and a frame names only
    /// entries it holds.
    pub fn verify_invariants(&self) {
        let mut seen = HashSet::new();
        let mut entries = HashSet::new();
        for frame in self.all_frames() {
            assert!(
                seen.insert(frame.surface_id),
                "frame {} sits in two places",
                frame.surface_id
            );
            for entry in &frame.entries {
                assert!(
                    entries.insert(entry.id),
                    "entry {} sits in two frames",
                    entry.id
                );
            }
            if let Some(sel) = frame.selected_entry_id {
                assert!(
                    frame.entries.iter().any(|entry| entry.id == sel),
                    "frame {} selects an entry it lacks",
                    frame.surface_id
                );
            }
            assert!(
                frame
                    .entries
                    .iter()
                    .filter(|entry| entry.fullscreen())
                    .count()
                    <= 1,
                "frame {} shows two views fullscreen",
                frame.surface_id
            );
        }
        let mut names = HashSet::new();
        for monitor in &self.monitors {
            assert!(
                names.insert(monitor.name.as_str()),
                "two monitors for {}",
                monitor.name
            );
            assert_eq!(
                monitor.strip.active_frame().is_some(),
                !monitor.strip.frames().is_empty(),
                "strip on {} has frames but no active one",
                monitor.name
            );
            assert_eq!(
                monitor.floating.frames_with_render_geo().count(),
                monitor.floating.frames().len(),
                "floating space on {} lost a frame's data",
                monitor.name
            );
            for frame in monitor.floating.frames() {
                assert!(
                    frame.pinned_to.is_none()
                        || frame.fullscreen_entry().is_some()
                        || !frame.has_views(),
                    "float {} on {} is pinned while showing views, none fullscreen",
                    frame.surface_id,
                    monitor.name
                );
                assert!(
                    frame.fullscreen_entry().is_none()
                        || frame.pinned_to.is_some()
                        || monitor.strip.frames().is_empty(),
                    "fullscreen float {} on {} is pinned to no frame",
                    frame.surface_id,
                    monitor.name
                );
                assert!(
                    frame
                        .pinned_to
                        .is_none_or(|id| monitor.strip.index_of(id).is_some()),
                    "float {} on {} is pinned to a frame not in its strip",
                    frame.surface_id,
                    monitor.name
                );
            }
            for frame in monitor.strip.frames() {
                assert!(
                    frame.pinned_to.is_none(),
                    "strip frame {} on {} is pinned",
                    frame.surface_id,
                    monitor.name
                );
            }
        }
        assert_eq!(
            self.active_output.is_some(),
            self.focused_frame().is_some(),
            "the active output holds no frame"
        );
        assert_eq!(
            self.focused_frame().is_some(),
            self.monitors
                .iter()
                .any(|monitor| monitor.focused_frame().is_some()),
            "the keyboard is off every frame while a monitor has one"
        );
        for monitor in &self.monitors {
            assert!(
                !monitor.floating.is_focused() || monitor.active_float_shown(),
                "floating space on {} holds the keyboard with no shown float",
                monitor.name
            );
            assert!(
                monitor.floating.frames().is_empty()
                    || !monitor.strip.frames().is_empty()
                    || monitor.floating.is_focused(),
                "floats on {} beside an empty strip hold no keyboard",
                monitor.name
            );
        }
        if let Some(move_) = self.interactive_move {
            assert!(
                self.frame_location(move_.frame_id)
                    .is_some_and(|location| location.floating),
                "dragged frame {} is not in a floating space",
                move_.frame_id
            );
            let (x, y) = move_.pointer_ratio;
            assert!(
                (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y),
                "pointer grabbed frame {} outside it at {:?}",
                move_.frame_id,
                move_.pointer_ratio
            );
        }
    }

    /// Start dragging floating frame `id`, remembering where the pointer grabbed it.
    pub(crate) fn interactive_move_begin(
        &mut self,
        id: u64,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> bool {
        if self.interactive_move.is_some() {
            return false;
        }
        let Some(monitor) = self.monitor_of(output) else {
            return false;
        };
        // Measured against the drawn slot, so a sliding frame grabs without a jump.
        let Some((_, rect)) = monitor
            .floats_with_render_geo()
            .find(|(frame, _)| frame.surface_id == id)
        else {
            return false;
        };
        let offset = pos_within_output - rect.loc;
        let size = rect.size;
        self.interactive_move = Some(InteractiveMove {
            frame_id: id,
            pointer_ratio: (
                (offset.x / size.w.max(1.0)).clamp(0.0, 1.0),
                (offset.y / size.h.max(1.0)).clamp(0.0, 1.0),
            ),
        });
        true
    }

    /// Carry the dragged frame to `pos_within_output` on `output`, crossing outputs with
    /// the pointer. Returns the crossing, empty when it stayed, `None` when no drag is live.
    pub(crate) fn interactive_move_update(
        &mut self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<Moved> {
        let move_ = self.interactive_move?;
        let id = move_.frame_id;
        if !self
            .frame_location(id)
            .is_some_and(|location| location.floating)
        {
            // The frame left its floating space, so the drag is over.
            self.interactive_move = None;
            return None;
        }
        if self.monitor_idx(output).is_none() {
            // Nothing to drag onto, so the frame waits where it is.
            return Some(Moved::default());
        }
        let moved = self.move_frame_to_output(id, output);
        if let Some(monitor) = self.monitor_of_mut(output) {
            let size = monitor
                .floating
                .frame_with_data(id)
                .map(|(_, data)| data.size());
            if let Some(size) = size {
                let pos = pos_within_output
                    - Point::from((
                        size.w * move_.pointer_ratio.0,
                        size.h * move_.pointer_ratio.1,
                    ));
                monitor.floating.drag_frame_to(id, pos);
            }
        }
        Some(moved)
    }

    /// Drop the dragged frame, clamping it back on screen. Returns the output it landed on.
    pub(crate) fn interactive_move_end(&mut self, id: u64) -> Option<Output> {
        if self
            .interactive_move
            .is_none_or(|move_| move_.frame_id != id)
        {
            return None;
        }
        self.interactive_move = None;
        let output = self.frame_location(id)?.monitor.output().clone();
        self.monitor_of_mut(&output)?
            .floating
            .settle_dragged_frame(id);
        Some(output)
    }

    /// Move frame `id` by `delta` slots along its strip, stopping at the edge.
    /// Returns the output it moved on.
    pub fn move_frame(&mut self, id: u64, delta: i32) -> Option<&Output> {
        let (monitor, from) = self.monitors.iter_mut().find_map(|monitor| {
            let from = monitor.strip.index_of(id)?;
            Some((monitor, from))
        })?;
        let to = from
            .checked_add_signed(delta as isize)
            .filter(|&to| to < monitor.strip.frames().len())?;
        monitor.strip.move_frame(from, to);
        Some(&monitor.output)
    }
}

/// The entry's size, the output's when `fullscreen`.
pub(crate) fn entry_size(
    output: &Output,
    entry: &LayoutEntry,
    fullscreen: bool,
) -> Size<i32, Logical> {
    if fullscreen {
        crate::utils::output_size(output).to_i32_round()
    } else {
        entry.rect().size
    }
}

#[cfg(test)]
mod tests;
