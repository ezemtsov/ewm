//! Frames on outputs: strips, floating spaces, where frames live and what sits under a point.

use std::collections::{HashMap, HashSet};
use std::mem;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::utils::under_from_surface_tree;
use smithay::desktop::{Window, WindowSurfaceType};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::seat::WaylandFocus;

use crate::closing_frame::Closing;
use crate::resize_fade::{Fit, ViewRect, contents_rect};
use crate::snapshot::Snapshot;
use crate::{
    Event, Ewm, FocusId, LayoutEntry, LayoutEntryHit, LayoutEntryId, SurfaceHit, animation,
    floating, fullscreen_center_offset, module, strip,
};

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

/// Where every Emacs frame is.
#[derive(Debug)]
pub struct Layout {
    /// One per mapped output.
    monitors: Vec<Monitor>,
    /// The output whose active frame holds the keyboard.
    active_output: Option<Output>,
    /// Frames of the last output to go, adopted by the next output to map.
    homeless: Frames,
    /// Home display identity per frame, so a replugged display reclaims its frames.
    home_outputs: HashMap<u64, String>,
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
            home_outputs: HashMap::new(),
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

    /// Surface id of the frame holding the keyboard, 0 without one.
    pub fn focused_frame_id(&self) -> u64 {
        self.focused_frame().map_or(0, |frame| frame.surface_id)
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
    fn follow_focus(&mut self, focused: u64, moved: &Moved) {
        if moved.ids.contains(&focused) {
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
        for id in &retired {
            self.home_outputs.remove(id);
        }
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
        let ids: Vec<u64> = monitor.strip.frame_ids().collect();
        for id in ids {
            self.remember_home_if_missing(id, &home);
        }
        self.settle_focus();
    }

    /// Put new frame `id` on `output` as `placement` says.
    pub fn add_frame(&mut self, id: u64, output: &Output, placement: Placement) {
        let home = crate::utils::output_identity(output);
        let Some(monitor) = self.monitor_of_mut(output) else {
            return;
        };
        let mut frame = strip::Frame::new(id);
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
        self.remember_home_if_missing(id, &home);
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

    /// Frame ids on `output`, tiled then floating.
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

    /// Run `tiled` or `floating` on whichever container holds frame `id`. Returns
    /// its output and the strip frame that was active before.
    fn with_frame_container(
        &mut self,
        id: u64,
        tiled: impl FnOnce(&mut Monitor, usize),
        floating: impl FnOnce(&mut Monitor),
    ) -> Option<(Output, Option<u64>)> {
        let monitor = self.monitor_of_frame_mut(id)?;
        let prev_active = monitor.strip.active_frame_id();
        match monitor.strip.index_of(id) {
            Some(idx) => tiled(monitor, idx),
            None => floating(monitor),
        }
        Some((monitor.output.clone(), prev_active))
    }

    /// Drop frame `id` from wherever it is. For a placed frame, returns its
    /// output and the frame its strip had active.
    pub(crate) fn remove_frame(
        &mut self,
        id: u64,
        active_close: bool,
    ) -> Option<(Output, Option<u64>)> {
        self.home_outputs.remove(&id);
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

    /// Move frame `id` to `dst` on purpose, after its active frame, homing it and its floats.
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
        for id in &moved.ids {
            self.home_outputs.insert(*id, home.clone());
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

    fn remember_home_if_missing(&mut self, frame_id: u64, home: &str) {
        self.home_outputs
            .entry(frame_id)
            .or_insert_with(|| home.to_string());
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
                    .filter(|frame| {
                        frame.pinned_to.is_none()
                            && self.home_outputs.get(&frame.surface_id) == Some(&identity)
                    })
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
    ) -> Option<(Output, Option<u64>)> {
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
        self.active_output = Some(activated.0.clone());
        Some(activated)
    }

    /// Show strip frame `id` active, the keyboard following only a frame that goes off screen.
    /// Returns the output and its old active frame.
    pub(crate) fn show_strip_frame(&mut self, id: u64) -> Option<(Output, Option<u64>)> {
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
    ) -> Option<(Output, Option<u64>)> {
        let monitor = self.monitor_of_mut(output)?;
        let prev_active = monitor.strip.active_frame_id();
        monitor.strip.gesture_end(working_w);
        let output = monitor.output.clone();
        self.settle_focus();
        Some((output, prev_active))
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

    /// Whether entry `entry_id` shows fullscreen.
    pub fn entry_fullscreen(&self, entry_id: LayoutEntryId) -> bool {
        self.entry_location(entry_id)
            .is_some_and(|location| location.entry.fullscreen())
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

    /// Mark one view of each surface primary: its largest among the shown frames,
    /// ties broken by `order`.
    pub(crate) fn assign_primaries(&mut self, order: &[Output]) {
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
            let area = entry_area(location.monitor.output(), location.entry);
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

    /// Panic unless every frame sits in exactly one place and names only entries it holds.
    pub fn verify_invariants(&self) {
        let mut seen = HashSet::new();
        for frame in self.all_frames() {
            assert!(
                seen.insert(frame.surface_id),
                "frame {} sits in two places",
                frame.surface_id
            );
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
        for &id in self.home_outputs.keys() {
            assert!(self.has_frame(id), "home remembered for gone frame {id}");
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

/// Fullscreen entries claim the output's full size, not entry.w/h.
fn entry_area(output: &Output, entry: &LayoutEntry) -> u64 {
    if entry.fullscreen() {
        let size = crate::utils::output_size(output);
        size.w as u64 * size.h as u64
    } else {
        entry.w as u64 * entry.h as u64
    }
}

pub(crate) fn enter_output_for_window(window: &Window, output: &Output) {
    crate::utils::set_preferred_scale_transform(window, output);
    if let Some(surface) = window.wl_surface() {
        output.enter(&surface);
    }
}

fn leave_output_for_window(window: &Window, output: &Output) {
    if let Some(surface) = window.wl_surface() {
        output.leave(&surface);
    }
}

impl Ewm {
    /// Bake frame `id`'s close snapshot.
    /// Bake `id` for its close animation: a frame into its record, a surface into every frame
    /// showing it.
    pub(crate) fn store_close_snapshot(&mut self, renderer: &mut GlesRenderer, id: u64) {
        if self.layout.has_frame(id) {
            if let Some(snapshot) = crate::render::render_frame_snapshot(self, renderer, id)
                && let Some(frame) = self.layout.frame_mut(id)
            {
                frame.close_snapshot = Some(snapshot);
            }
            return;
        }
        // Unmapped already, so its views are fading.
        if !self
            .window(id)
            .and_then(Window::wl_surface)
            .is_some_and(|surface| crate::utils::is_mapped(&surface))
        {
            return;
        }
        let views: Vec<(u64, Snapshot)> = self
            .surface_entry_locations(id)
            .filter_map(|location| {
                let snapshot = crate::render::render_closing_view(self, renderer, location)?;
                Some((location.frame.surface_id, snapshot))
            })
            .collect();
        for (frame_id, snapshot) in views {
            self.layout.close_view(frame_id, snapshot);
        }
    }

    /// Drop Emacs frame `id` from wherever it is. Returns the output it left.
    pub(crate) fn remove_emacs_frame(&mut self, id: u64, active_close: bool) -> Option<Output> {
        // A float off screen or hidden under a fullscreen view leaves without a fade.
        let hidden = self.layout.frame_location(id).is_some_and(|location| {
            location.floating
                && !(location.monitor.float_shown(location.frame)
                    && self.float_visible(location.monitor, location.frame))
        });
        if hidden && let Some(frame) = self.layout.frame_mut(id) {
            frame.close_snapshot = None;
        }
        let removed = self.layout.remove_frame(id, active_close);
        if let Some((output, prev_active)) = &removed {
            self.reconcile_output_entries(output);
            self.reconcile_active_for_output(output, *prev_active);
        }
        removed.map(|(output, _)| output)
    }

    /// Move `output`'s frames to the next output in order, else park them, and unmap it.
    pub(crate) fn detach_output(&mut self, output: &Output) {
        let fallback = self.output_next_of(output).filter(|next| next != output);
        let moved = self.layout.detach_output(output, fallback.as_ref());
        self.settle_moved_frames(&moved);
        for id in moved.retired {
            self.queue_event(Event::Close { id });
        }
        self.space.unmap_output(output);
        self.sync_sorted_outputs();
        self.refresh_cursor_outputs();
        self.recalculate_output_size();
        self.output_management_state.output_heads_changed = true;
    }

    /// Global origin of every floating frame, for Emacs's own geometry.
    pub(crate) fn frame_origins_snapshot(&self) -> Vec<module::FrameOrigin> {
        self.layout
            .monitors()
            .iter()
            .flat_map(Monitor::frame_locations)
            .filter(|location| location.floating)
            .filter_map(|location| {
                let origin = self.frame_origin(location)?;
                Some(module::FrameOrigin {
                    surface_id: location.frame.surface_id,
                    origin: (origin.x, origin.y),
                })
            })
            .collect()
    }

    /// Monitors in output order.
    pub(crate) fn mapped_monitors(&self) -> impl Iterator<Item = &Monitor> {
        self.sorted_outputs
            .iter()
            .filter_map(|output| self.layout.monitor_of(output))
    }

    /// The view of `surface_id` to focus: focused, else in the focused frame, else first.
    pub(crate) fn frame_entry_for_surface(&self, surface_id: u64) -> Option<(u64, LayoutEntryId)> {
        let location = self
            .focused_entry_for_surface(surface_id)
            .or_else(|| self.focused_frame_entry_for_surface(surface_id))
            .or_else(|| self.layout_entry_for_surface(surface_id, |_| true))?;
        Some((location.frame.surface_id, location.entry.id))
    }

    fn focused_frame_entry_for_surface(&self, id: u64) -> Option<LayoutEntryLocation<'_>> {
        self.layout
            .frame_location(self.focused_frame_id())?
            .entries()
            .find(|location| location.entry.surface_id() == Some(id))
    }

    /// Follow a layout change of some output's active frame, `shown` its output and old active.
    pub(crate) fn settle_shown(&mut self, shown: Option<(Output, Option<u64>)>) {
        let Some((output, prev_active)) = shown else {
            return;
        };
        // An active flip may shift a surface's largest visible entry to or from
        // this output, changing its size and scale.
        if self.layout.active_frame_id(&output) != prev_active {
            self.reconcile_active_for_output(&output, prev_active);
            self.refresh_primary_assignments();
            self.queue_redraw_all();
        } else {
            self.queue_redraw(&output);
        }
    }

    /// Global origin of slot `frame_idx` on `strip`.
    pub(crate) fn strip_frame_origin(
        output_geo: Rectangle<i32, Logical>,
        working_area: Rectangle<i32, Logical>,
        strip: &strip::Strip,
        frame_idx: usize,
    ) -> Point<i32, Logical> {
        // Rounded so popups and hits track a sliding frame.
        let frame_dx = strip.screen_x_of_frame(frame_idx).round() as i32;
        Point::from((
            output_geo.loc.x + working_area.loc.x + frame_dx,
            output_geo.loc.y + working_area.loc.y,
        ))
    }

    /// Global origin of the frame at `location`.
    pub(crate) fn frame_origin(&self, location: FrameLocation<'_>) -> Option<Point<i32, Logical>> {
        let output = location.monitor.output();
        let output_geo = self.space.output_geometry(output)?;
        if location.floating {
            let (_, data) = location
                .monitor
                .floating
                .frame_with_data(location.frame.surface_id)?;
            let loc = data.logical_pos();
            let dx = location.slot_x().round() as i32;
            return Some(Point::from((
                output_geo.loc.x + loc.x as i32 + dx,
                output_geo.loc.y + loc.y as i32,
            )));
        }
        let working_area = self.get_working_area(output);
        Some(Self::strip_frame_origin(
            output_geo,
            working_area,
            &location.monitor.strip,
            location.frame_idx,
        ))
    }

    /// Global top-left of an Emacs frame, which is not in `space`.
    pub fn emacs_frame_origin(&self, surface_id: u64) -> Option<Point<i32, Logical>> {
        self.frame_origin(self.layout.frame_location(surface_id)?)
    }

    fn frame_rect(
        output_geo: Rectangle<i32, Logical>,
        working_area: Rectangle<i32, Logical>,
        strip: &strip::Strip,
        frame_idx: usize,
        frame: &strip::Frame,
    ) -> Rectangle<i32, Logical> {
        Rectangle::new(
            Self::strip_frame_origin(output_geo, working_area, strip, frame_idx),
            Size::from((frame.width as i32, working_area.size.h)),
        )
    }

    /// The focused layout entry when it shows surface `id`.
    pub(crate) fn focused_entry_for_surface(&self, id: u64) -> Option<LayoutEntryLocation<'_>> {
        let location = self.layout.entry_location(self.focused_entry_id()?)?;
        (location.entry.surface_id() == Some(id)).then_some(location)
    }

    /// Entries showing `id`: every strip in output order, then every floating space.
    pub(crate) fn surface_entry_locations(
        &self,
        id: u64,
    ) -> impl Iterator<Item = LayoutEntryLocation<'_>> {
        let locations = |floating| {
            self.mapped_monitors()
                .flat_map(move |monitor| monitor.entry_locations_in(floating))
        };
        locations(false)
            .chain(locations(true))
            .filter(move |location| location.entry.surface_id() == Some(id))
    }

    pub(crate) fn layout_entry_for_surface(
        &self,
        id: u64,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryLocation<'_>> {
        self.surface_entry_locations(id)
            .find(|location| predicate(location.entry))
    }

    /// Layout entry used for positioning a multi-view surface.
    pub(crate) fn preferred_entry_for_surface(&self, id: u64) -> Option<LayoutEntryLocation<'_>> {
        self.focused_entry_for_surface(id)
            .or_else(|| self.layout_entry_for_surface(id, LayoutEntry::primary))
    }

    /// Global position of a window: layout entries use the focused-or-primary
    /// entry's screen rect, Emacs frames use `emacs_frame_origin`.
    pub fn window_global_position(
        &self,
        window: &Window,
    ) -> Option<smithay::utils::Point<i32, smithay::utils::Logical>> {
        let surface = window.wl_surface()?;
        let id = self.surface_id(&surface)?;
        if self.layout.has_frame(id) {
            return self.emacs_frame_origin(id);
        }
        let location = self.preferred_entry_for_surface(id)?;
        self.entry_origin(location)
    }

    /// Fullscreen `entry` in `slot`: centered at its own size when native, else across the slot.
    pub(crate) fn fullscreen_entry_rect(
        &self,
        slot: Rectangle<i32, Logical>,
        entry: &LayoutEntry,
    ) -> Rectangle<i32, Logical> {
        let Some(surface) = entry.surface_id().and_then(|id| self.surfaces.get(&id)) else {
            return slot;
        };
        if !surface.fullscreen_native(entry) {
            return slot;
        }
        let window_size = surface.window.geometry().size;
        let (ox, oy) = fullscreen_center_offset(window_size, slot.size);
        Rectangle::new(slot.loc + Point::from((ox, oy)), window_size)
    }

    /// Whether the view at `location` is drawn: its float shows and no sibling covers it.
    pub(crate) fn view_visible(&self, location: LayoutEntryLocation<'_>) -> bool {
        (!location.floating || self.float_visible(location.monitor, location.frame))
            && (location.entry.fullscreen() || location.frame.fullscreen_entry().is_none())
    }

    fn entry_origin(&self, location: LayoutEntryLocation<'_>) -> Option<Point<i32, Logical>> {
        self.drawn_rect(location).map(|rect| rect.loc)
    }

    /// Where the view at `location` is drawn, globally.
    pub(crate) fn drawn_rect(
        &self,
        location: LayoutEntryLocation<'_>,
    ) -> Option<Rectangle<i32, Logical>> {
        let output_geo = self.space.output_geometry(location.monitor.output())?;
        let origin = self.frame_origin(location.frame_location())?;
        Some(self.entry_rect_at(location, output_geo, origin))
    }

    /// Where the view at `location` is drawn with its frame at `origin` on `output_geo`.
    fn entry_rect_at(
        &self,
        location: LayoutEntryLocation<'_>,
        output_geo: Rectangle<i32, Logical>,
        origin: Point<i32, Logical>,
    ) -> Rectangle<i32, Logical> {
        let entry = location.entry;
        if entry.fullscreen() {
            let slot = Self::fullscreen_slot(location, output_geo);
            return self.fullscreen_entry_rect(slot, entry);
        }
        Rectangle::new(origin + Point::from((entry.x, entry.y)), entry.rect().size)
    }

    /// The output-sized slot of `location`'s frame.
    fn fullscreen_slot(
        location: LayoutEntryLocation<'_>,
        output_geo: Rectangle<i32, Logical>,
    ) -> Rectangle<i32, Logical> {
        let dx = location.frame_location().slot_x().round() as i32;
        Rectangle::new(output_geo.loc + Point::from((dx, 0)), output_geo.size)
    }

    /// Where the view at `location` sits, globally, with its frame at `origin`: its slot when
    /// fullscreen, else its window.
    fn view_place(
        location: LayoutEntryLocation<'_>,
        output_geo: Rectangle<i32, Logical>,
        origin: Point<i32, Logical>,
    ) -> Rectangle<i32, Logical> {
        let entry = location.entry;
        if entry.fullscreen() {
            Self::fullscreen_slot(location, output_geo)
        } else {
            Rectangle::new(origin + Point::from((entry.x, entry.y)), entry.rect().size)
        }
    }

    /// `local`, an output-local rect, on `output_geo`.
    fn on_output(
        output_geo: Rectangle<i32, Logical>,
        local: Rectangle<f64, Logical>,
    ) -> Rectangle<i32, Logical> {
        Rectangle::new(
            output_geo.loc + Point::from((local.loc.x as i32, local.loc.y as i32)),
            Size::from((local.size.w as i32, local.size.h as i32)),
        )
    }

    /// On-screen origins of every layout view of `surface_id`, one per entry.
    pub fn surface_view_origins(&self, surface_id: u64) -> Vec<Point<i32, Logical>> {
        if self.layout.has_frame(surface_id) {
            return self.emacs_frame_origin(surface_id).into_iter().collect();
        }
        self.surface_entry_locations(surface_id)
            .filter_map(|location| self.entry_origin(location))
            .collect()
    }

    fn rect_contains_f64(rect: Rectangle<i32, Logical>, pos: Point<f64, Logical>) -> bool {
        pos.x >= rect.loc.x as f64
            && pos.y >= rect.loc.y as f64
            && pos.x < (rect.loc.x + rect.size.w) as f64
            && pos.y < (rect.loc.y + rect.size.h) as f64
    }

    /// Monitor under `pos` with its output geometry, while frames sit in place.
    fn monitor_at(&self, pos: Point<f64, Logical>) -> Option<(&Monitor, Rectangle<i32, Logical>)> {
        // Zoomed or hidden content takes no hits.
        if !self.frames_in_place() {
            return None;
        }
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        Some((self.layout.monitor_of(output)?, output_geo))
    }

    /// The topmost visible float whose slot holds `pos`.
    fn visible_float_under(&self, pos: Point<f64, Logical>) -> Option<&strip::Frame> {
        let (monitor, output_geo) = self.monitor_at(pos)?;
        let pos = pos - output_geo.loc.to_f64();
        self.visible_floats(monitor)
            .find(|(_, rect)| rect.contains(pos))
            .map(|(frame, _)| frame)
    }

    pub(crate) fn floating_frame_under(&self, pos: Point<f64, Logical>) -> Option<u64> {
        self.visible_float_under(pos).map(|frame| frame.surface_id)
    }

    pub(crate) fn floating_resize_edges_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(u64, floating::ResizeEdge)> {
        // A fullscreen float has no edge to take.
        self.visible_float_under(pos)
            .filter(|frame| frame.fullscreen_entry().is_none())
            .map(|frame| (frame.surface_id, floating::ResizeEdge::BOTTOM_RIGHT))
    }

    /// Topmost entry on `monitor` under `pos` that `predicate` accepts.
    fn layout_entry_in<'a>(
        &self,
        monitor: &'a Monitor,
        output_geo: Rectangle<i32, Logical>,
        working_area: Rectangle<i32, Logical>,
        pos: Point<f64, Logical>,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryHit<'a>> {
        // Floats first, topmost first, and a float's slot occludes the strip.
        let floats = monitor
            .frame_locations_in(true)
            .zip(monitor.floats_with_render_geo());
        for (location, (_, slot)) in floats {
            if !self.float_visible(monitor, location.frame) {
                continue;
            }
            let slot = Self::on_output(output_geo, slot);
            let covered = location.frame.fullscreen_entry().is_some();
            for location in location.entries() {
                let entry = location.entry;
                if (covered && !entry.fullscreen()) || !predicate(entry) {
                    continue;
                }
                let rect = self.entry_rect_at(location, output_geo, slot.loc);
                if Self::rect_contains_f64(rect, pos) {
                    return Some(LayoutEntryHit {
                        entry,
                        rect,
                        output_size: output_geo.size,
                    });
                }
            }
            if Self::rect_contains_f64(slot, pos) {
                return None;
            }
        }
        self.tiled_entry_under_matching(monitor, output_geo, working_area, pos, predicate)
    }

    /// Hit-test `pos` against a monitor's tiled entries only (no floating overlay).
    fn tiled_entry_under_matching<'a>(
        &self,
        monitor: &'a Monitor,
        output_geo: Rectangle<i32, Logical>,
        working_area: Rectangle<i32, Logical>,
        pos: Point<f64, Logical>,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryHit<'a>> {
        monitor
            .entry_locations_in(false)
            .filter(|location| predicate(location.entry))
            // Primary clicks miss in the letterbox.
            .filter(|location| self.view_visible(*location))
            .find_map(|location| {
                let origin = Self::strip_frame_origin(
                    output_geo,
                    working_area,
                    &monitor.strip,
                    location.frame_idx,
                );
                let rect = self.entry_rect_at(location, output_geo, origin);
                Self::rect_contains_f64(rect, pos).then_some(LayoutEntryHit {
                    entry: location.entry,
                    rect,
                    output_size: output_geo.size,
                })
            })
    }

    /// The tiled entry whose rect contains a floating frame's center, for spatial reattach.
    pub fn tiled_entry_under_floating_center(
        &self,
        floating_frame_id: u64,
    ) -> Option<LayoutEntryId> {
        let monitor = self
            .layout
            .frame_location(floating_frame_id)
            .filter(|location| location.floating)?
            .monitor;
        let (_, data) = monitor.floating.frame_with_data(floating_frame_id)?;
        let (pos, size) = (data.render_pos(), data.size());
        let center: Point<f64, Logical> = Point::from((pos.x + size.w / 2.0, pos.y + size.h / 2.0));
        let output = monitor.output();
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);
        let center = output_geo.loc.to_f64() + center;
        self.tiled_entry_under_matching(monitor, output_geo, working_area, center, |_| true)
            .map(|hit| hit.entry.id)
    }

    fn pointer_scale_for_entry(
        entry: &LayoutEntry,
        fullscreen: bool,
        window_size: Size<i32, Logical>,
        output_size: Size<i32, Logical>,
    ) -> f64 {
        if fullscreen {
            if entry.primary() {
                return 1.0;
            }

            let uniform = f64::max(
                output_size.w as f64 / window_size.w as f64,
                output_size.h as f64 / window_size.h as f64,
            );
            return 1.0 / uniform;
        }

        if entry.primary() || entry.w == 0 || entry.h == 0 {
            return 1.0;
        }

        f64::min(
            window_size.w as f64 / entry.w as f64,
            window_size.h as f64 / entry.h as f64,
        )
    }

    /// Hit-test layout toplevel/subsurfaces under a global position; falls
    /// back to the frame's own surface when no entry hits. Popups are handled
    /// separately by `popup_surface_under`, so popup placement and z-order stay
    /// identical to rendering.
    pub fn layout_surface_under(&self, pos: Point<f64, Logical>) -> Option<SurfaceHit> {
        let (monitor, output_geo) = self.monitor_at(pos)?;
        let working_area = self.get_working_area(monitor.output());
        self.layout_entry_in(monitor, output_geo, working_area, pos, |entry| {
            entry.surface_id().is_some()
        })
        .and_then(|hit| self.surface_under_layout_entry(pos, hit))
        .or_else(|| self.surface_under_frame(monitor, output_geo, working_area, pos))
    }

    fn surface_under_layout_entry(
        &self,
        pos: Point<f64, Logical>,
        hit: LayoutEntryHit<'_>,
    ) -> Option<SurfaceHit> {
        let entry = hit.entry;
        let window = self.window(entry.surface_id()?)?;
        let window_geo = window.geometry();
        let pointer_scale = Self::pointer_scale_for_entry(
            entry,
            entry.fullscreen(),
            window_geo.size,
            hit.output_size,
        );

        let pos_in_window = Point::from((
            (pos.x - hit.rect.loc.x as f64) * pointer_scale,
            (pos.y - hit.rect.loc.y as f64) * pointer_scale,
        ));
        let window_origin: Point<f64, Logical> =
            Point::from((pos.x - pos_in_window.x, pos.y - pos_in_window.y));

        Self::surface_under_non_popup_window(window, pos_in_window, window_origin)
    }

    /// Find the frame on `monitor` containing `pos`, returning the frame surface itself.
    fn surface_under_frame(
        &self,
        monitor: &Monitor,
        output_geo: Rectangle<i32, Logical>,
        working_area: Rectangle<i32, Logical>,
        pos: Point<f64, Logical>,
    ) -> Option<SurfaceHit> {
        let strip = &monitor.strip;
        // The frame's own surface hides under its fullscreen view.
        let own = |frame: &strip::Frame| {
            frame
                .fullscreen_entry()
                .is_none()
                .then_some(frame.surface_id)
        };
        // Floats sit above the strip.
        let floats = self
            .visible_floats(monitor)
            .map(|(frame, rect)| (own(frame), Self::on_output(output_geo, rect)));
        let tiled = strip.frames().iter().enumerate().map(|(frame_idx, frame)| {
            let rect = Self::frame_rect(output_geo, working_area, strip, frame_idx, frame);
            (own(frame), rect)
        });
        let (id, rect) = floats
            .chain(tiled)
            .find(|&(_, rect)| Self::rect_contains_f64(rect, pos))?;
        let origin = rect.loc.to_f64();
        Self::surface_under_non_popup_window(self.window(id?)?, pos - origin, origin)
    }

    fn surface_under_non_popup_window(
        window: &Window,
        pos_in_window: Point<f64, Logical>,
        window_origin: Point<f64, Logical>,
    ) -> Option<SurfaceHit> {
        let (surface, surface_offset) = window.surface_under(
            pos_in_window,
            WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE,
        )?;
        Some((
            surface,
            Point::from((
                window_origin.x + surface_offset.x as f64,
                window_origin.y + surface_offset.y as f64,
            )),
        ))
    }

    pub(crate) fn popup_surface_under(&self, pos: Point<f64, Logical>) -> Option<SurfaceHit> {
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        for placement in crate::render::collect_popup_placements(self, output_geo, Some(output)) {
            let Some((surface, surface_offset)) = under_from_surface_tree(
                &placement.surface,
                pos,
                placement.location,
                WindowSurfaceType::ALL,
            ) else {
                continue;
            };
            return Some((surface, surface_offset.to_f64()));
        }
        None
    }

    /// The views of `id` in shown frames, with their output.
    pub(crate) fn surface_views(&self, id: u64) -> impl Iterator<Item = (&Output, &LayoutEntry)> {
        self.surface_entry_locations(id)
            .filter(LayoutEntryLocation::shown)
            .map(|location| (location.monitor.output(), location.entry))
    }

    /// Where every shown view sits on its output, as drawn.
    pub fn view_rects(&self) -> Vec<ViewRect> {
        self.layout
            .entry_locations()
            .filter(|location| location.shown() && self.view_visible(*location))
            .filter_map(|location| {
                let entry = location.entry;
                let surface = entry.surface_id()?;
                let state = self.surfaces.get(&surface)?;
                let output = location.monitor.output().clone();
                let output_geo = self.space.output_geometry(&output)?;
                let origin = self.frame_origin(location.frame_location())?;
                let place = Self::view_place(location, output_geo, origin);
                let now = Fit {
                    window: place.size,
                    native: entry.primary(),
                };
                let fit = state.fit_of(entry.id, now);
                let buffer = state.window.geometry().size;
                let mut loc = place.loc;
                if entry.fullscreen() {
                    loc += fit.fullscreen_offset(buffer, place.size);
                }
                let (rect, scale) = contents_rect(fit, buffer, loc.to_f64())?;
                let mut rect = rect.to_i32_round();
                rect.loc -= output_geo.loc;
                Some(ViewRect {
                    surface,
                    entry: entry.id,
                    output,
                    frame: location.frame.surface_id,
                    rect,
                    scale,
                    fit: now,
                    window: entry.rect(),
                })
            })
            .collect()
    }

    fn surface_has_primary_entry(&self, id: u64) -> bool {
        self.surface_views(id).any(|(_, entry)| entry.primary())
    }

    fn configure_surface_fullscreen_state(&self, id: u64, fullscreen: bool) {
        let Some(window) = self.window(id) else {
            return;
        };
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        // Unplaced, the view is sized to the active output.
        let size = self
            .active_output()
            .map(|output| crate::utils::output_size(output).to_i32_round());
        let changed = toplevel.with_pending_state(|state| {
            let was_fullscreen = state.states.contains(XdgToplevelState::Fullscreen);
            let resized = size.is_some_and(|size| state.size != Some(size));
            if let Some(size) = size {
                state.size = Some(size);
            }
            if fullscreen {
                state.states.set(XdgToplevelState::Fullscreen);
                state.states.unset(XdgToplevelState::Maximized);
            } else {
                state.states.unset(XdgToplevelState::Fullscreen);
                state.states.set(XdgToplevelState::Maximized);
            }
            was_fullscreen != fullscreen || resized
        });
        if changed {
            toplevel.send_configure();
        }
    }

    /// Ask surface `id` to go fullscreen or back, and configure it so.
    pub(crate) fn set_surface_fullscreen(&mut self, id: u64, on: bool) {
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        let changed = mem::replace(&mut surface.pending_fullscreen, on) != on;
        if on {
            // One fullscreen per strip frame, the newest request wins, repeated or not.
            for rival in self
                .layout
                .fullscreen_rivals(id, |other| self.pending_fullscreen(other))
            {
                self.set_surface_fullscreen(rival, false);
            }
        }
        if !changed {
            return;
        }
        if self.surface_has_primary_entry(id) {
            self.refresh_primary_assignments();
        } else {
            self.configure_surface_fullscreen_state(id, on);
        }
    }

    /// Whether surface `id` is asked to be fullscreen, shown or not.
    pub fn pending_fullscreen(&self, id: u64) -> bool {
        self.surfaces
            .get(&id)
            .is_some_and(|surface| surface.pending_fullscreen)
    }

    /// Mark the views of committed fullscreen surfaces, reporting floats that hid or came back.
    pub(crate) fn reconcile_fullscreen(&mut self) {
        let surfaces = &self.surfaces;
        let committed = |id| surfaces.get(&id).is_some_and(|surface| surface.fullscreen);
        let shown = self
            .layout
            .shown_floats(|monitor| monitor.strip.active_frame_id());
        self.layout.reconcile_fullscreen(committed);
        self.report_float_visibility(&shown);
        // A float going fullscreen takes the focus from its frame.
        if let Some((_, entry_id)) = self.layout.fullscreen_float_over(self.focused_frame_id())
            && self.focused_entry_id() != Some(entry_id)
        {
            self.set_focus_entry(entry_id, "fullscreen_over", true);
        }
        self.queue_redraw_all();
    }

    /// Tell Emacs which floats went off or on screen since `before`.
    fn report_float_visibility(&mut self, before: &HashSet<u64>) {
        let after = self
            .layout
            .shown_floats(|monitor| monitor.strip.active_frame_id());
        for &id in before.difference(&after) {
            self.queue_event(Event::FrameHidden { id });
        }
        for &id in after.difference(before) {
            self.queue_event(Event::FrameShown { id });
        }
    }

    /// Whether the active frame's fullscreen view covers the Top layer, the strip at rest.
    pub fn render_above_top_layer(&self, output: &Output) -> bool {
        self.frames_in_place()
            && self.layout.monitor_of(output).is_some_and(|monitor| {
                monitor.strip.view_is_static() && monitor.presenting_fullscreen().is_some()
            })
    }

    /// Whether float `frame` on `monitor` is drawn, not hidden under its frame's fullscreen view.
    pub(crate) fn float_visible(&self, monitor: &Monitor, frame: &strip::Frame) -> bool {
        monitor
            .strip
            .frame_of_float(frame)
            .and_then(|id| monitor.fullscreen_on(id))
            .is_none_or(|id| id == frame.surface_id || frame.surface_id == self.focused_frame_id())
    }

    /// `monitor`'s visible floats with their output-local slots, topmost first.
    pub(crate) fn visible_floats<'m>(
        &self,
        monitor: &'m Monitor,
    ) -> impl Iterator<Item = (&'m strip::Frame, Rectangle<f64, Logical>)> {
        monitor
            .floats_with_render_geo()
            .filter(move |(frame, _)| self.float_visible(monitor, frame))
    }

    /// Entry id of the layout entry under a point.
    pub fn layout_entry_id_under(&self, pos: Point<f64, Logical>) -> Option<LayoutEntryId> {
        let (monitor, output_geo) = self.monitor_at(pos)?;
        let working_area = self.get_working_area(monitor.output());
        self.layout_entry_in(monitor, output_geo, working_area, pos, |_| true)
            .map(|hit| hit.entry.id)
    }

    /// Whether `id` shows on output `name`, as a view or as an Emacs frame.
    pub fn surface_on_output(&self, id: u64, name: &str) -> bool {
        self.surfaces
            .get(&id)
            .is_some_and(|surface| surface.outputs.contains(name))
    }

    /// Whether some output shows `id`, as a frame or a view.
    pub fn surface_in_layout(&self, id: u64) -> bool {
        self.surfaces
            .get(&id)
            .is_some_and(|surface| !surface.outputs.is_empty())
    }

    /// The outputs showing each surface.
    pub fn surface_outputs(&self) -> HashMap<u64, HashSet<String>> {
        self.surfaces
            .iter()
            .filter(|(_, surface)| !surface.outputs.is_empty())
            .map(|(&id, surface)| (id, surface.outputs.clone()))
            .collect()
    }

    /// Fire `wl_surface.enter` / `leave` for what `output`'s monitor now shows.
    pub(crate) fn reconcile_output_entries(&mut self, output: &Output) {
        let shown = self.layout.surface_ids_on(output);
        let name = output.name();
        for (id, surface) in &mut self.surfaces {
            match (surface.outputs.contains(&name), shown.contains(id)) {
                (true, false) => {
                    leave_output_for_window(&surface.window, output);
                    surface.outputs.remove(&name);
                }
                (false, true) => {
                    enter_output_for_window(&surface.window, output);
                    surface.outputs.insert(name.clone());
                    surface.placed = true;
                }
                _ => {}
            }
        }
    }

    /// Output membership, scale, events and active frames after `moved` landed.
    fn settle_moved_frames(&mut self, moved: &Moved) {
        if let Some(src) = &moved.src {
            self.reconcile_output_entries(src);
        }
        if let Some(dst) = &moved.dst {
            self.reconcile_output_entries(dst);
        }
        if let Some(dst) = &moved.dst {
            self.update_frames_for_working_area(dst);
        }
        self.refresh_primary_assignments();
        if let Some(src) = &moved.src {
            // The overview keeps a moved frame visible. A parked one has no output to show it.
            let hidden: Vec<u64> = if !self.overview.is_active() {
                moved
                    .src_prev_active
                    .filter(|id| moved.ids.contains(id))
                    .into_iter()
                    .collect()
            } else if moved.dst.is_none() {
                moved.ids.iter().copied().collect()
            } else {
                Vec::new()
            };
            for id in hidden {
                self.queue_event(Event::FrameHidden { id });
            }
            self.reconcile_active_for_output(src, moved.src_prev_active);
        }
        if let Some(dst) = &moved.dst {
            self.reconcile_active_for_output(dst, moved.dst_prev_active);
        }
        // Marks and pins follow the frames to their new strips.
        self.reconcile_fullscreen();
    }

    fn move_frame_ids_between_outputs(&mut self, src: &Output, dst: &Output, ids: &[u64]) {
        let moved = self.layout.move_frames(src, dst, ids, None);
        self.settle_moved_frames(&moved);
    }

    /// Adopt parked frames and reclaim homed ones onto `output`, which just mapped.
    pub(crate) fn attach_output(&mut self, output: &Output) {
        let adopted = self.layout.adopt_homeless(output);
        if !adopted.ids.is_empty() {
            self.settle_moved_frames(&adopted);
        }
        for (src, ids) in self.layout.homed_elsewhere(output) {
            self.move_frame_ids_between_outputs(&src, output, &ids);
        }
        self.layout.forget_last_active(output);
        // Frames coming back may hold a surface whose activation waited for them.
        self.focus_pending_activation_surfaces();
    }

    /// Settle on the focused frame's selected window once a merge changed it.
    fn sync_focus_after_merge(&mut self, before: (u64, Option<LayoutEntryId>)) {
        let after = (self.focused_surface_id(), self.focused_entry_id());
        if after != before {
            self.clear_focused_urgency();
            self.flush_pending_title_event(after.0);
            module::record_focus(after.0, "layout_selected", None);
        }
    }

    /// Replace every frame's contents from Emacs's layout. Nothing moves: frames
    /// keep their output and slot, and a frame the layout leaves out stays empty
    /// until its destroy arrives.
    pub fn apply_layout(&mut self, frames: Vec<strip::Frame>) {
        let before = (self.focused_surface_id(), self.focused_entry_id());
        let outputs: Vec<Output> = self.layout.outputs().cloned().collect();

        self.layout.merge_frames(frames);
        self.reconcile_fullscreen();

        self.sync_focus_after_merge(before);
        for output in &outputs {
            self.reconcile_output_entries(output);
            self.queue_redraw(output);
        }
        self.refresh_primary_assignments();
        self.fit_views(false);
        self.layout.settle_resized();
        self.focus_pending_activation_surfaces();
    }

    /// Move frame `id` by `delta` slots along its strip, stopping at the edge.
    pub fn move_frame(&mut self, id: u64, delta: i32) {
        if let Some(output) = self.layout.move_frame(id, delta).cloned() {
            self.queue_redraw(&output);
        }
    }

    /// Move frame `id` to `dst` and show it there.
    pub(crate) fn move_frame_to_output(&mut self, id: u64, dst: &Output) {
        let moved = self.layout.move_frame_to_output(id, dst);
        if !moved.ids.is_empty() {
            self.settle_moved_frames(&moved);
        }
    }

    /// Run `f` on the floating space owning `id`, redrawing on change; false if none.
    fn with_floating_space(
        &mut self,
        id: u64,
        f: impl FnOnce(&mut floating::FloatingSpace) -> bool,
    ) -> bool {
        let Some(monitor) = self.layout.monitor_of_frame_mut(id) else {
            return false;
        };
        let output = monitor.output().clone();
        let changed = f(&mut monitor.floating);
        if changed {
            self.queue_redraw(&output);
        }
        changed
    }

    pub fn move_floating_frame(&mut self, id: u64, dx: f64, dy: f64) -> bool {
        self.with_floating_space(id, |space| space.move_frame_by(id, Point::from((dx, dy))))
    }

    /// Like `with_floating_space`, but configures the toplevel to the size `f` returns.
    fn with_floating_resize(
        &mut self,
        id: u64,
        f: impl FnOnce(&mut floating::FloatingSpace) -> Option<Size<i32, Logical>>,
    ) -> bool {
        let mut size = None;
        let changed = self.with_floating_space(id, |space| {
            size = f(space);
            size.is_some()
        });
        if let Some(size) = size {
            self.configure_floating_frame_size(id, size);
        }
        changed
    }

    pub fn resize_floating_frame(&mut self, id: u64, dw: f64, dh: f64) -> bool {
        self.with_floating_resize(id, |space| space.resize_frame_by(id, Size::from((dw, dh))))
    }

    fn configure_floating_frame_size(&self, id: u64, size: Size<i32, Logical>) {
        let Some(window) = self.window(id) else {
            return;
        };
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let changed = toplevel.with_pending_state(|state| {
            let was_fullscreen = state.states.contains(XdgToplevelState::Fullscreen);
            let was_maximized = state.states.contains(XdgToplevelState::Maximized);
            let changed = state.size != Some(size) || was_fullscreen || was_maximized;
            state.size = Some(size);
            state.states.unset(XdgToplevelState::Fullscreen);
            state.states.unset(XdgToplevelState::Maximized);
            changed
        });
        if changed {
            toplevel.send_configure();
        }
    }

    /// The output strictly under `pos`, with `pos` made relative to it.
    fn output_local_pos(&self, pos: Point<f64, Logical>) -> Option<(Output, Point<f64, Logical>)> {
        let output = self.output_containing(pos)?;
        let geo = self.space.output_geometry(output)?;
        Some((output.clone(), pos - geo.loc.to_f64()))
    }

    /// Start dragging floating frame `id` grabbed at global position `pos`.
    pub fn interactive_move_floating_frame_begin(
        &mut self,
        id: u64,
        pos: Point<f64, Logical>,
    ) -> bool {
        let Some((output, pos_within_output)) = self.output_local_pos(pos) else {
            return false;
        };
        self.layout
            .interactive_move_begin(id, &output, pos_within_output)
    }

    /// Drag the moving frame to global position `pos`. False once the drag is over.
    pub fn interactive_move_floating_frame_update(&mut self, pos: Point<f64, Logical>) -> bool {
        let Some((output, pos_within_output)) = self.output_local_pos(pos) else {
            // Dead space between outputs leaves the frame where it is.
            return true;
        };
        let Some(moved) = self
            .layout
            .interactive_move_update(&output, pos_within_output)
        else {
            return false;
        };
        if !moved.ids.is_empty() {
            self.settle_moved_frames(&moved);
            if let Some(src) = &moved.src {
                self.queue_redraw(src);
            }
        }
        self.queue_redraw(&output);
        true
    }

    pub fn interactive_move_floating_frame_end(&mut self, id: u64) {
        if let Some(output) = self.layout.interactive_move_end(id) {
            self.queue_redraw(&output);
        }
    }

    pub fn interactive_resize_floating_frame_begin(
        &mut self,
        id: u64,
        edges: floating::ResizeEdge,
    ) -> bool {
        self.with_floating_space(id, |space| space.interactive_resize_begin(id, edges))
    }

    pub fn interactive_resize_floating_frame_update(
        &mut self,
        id: u64,
        delta: Point<f64, Logical>,
    ) -> bool {
        self.with_floating_resize(id, |space| space.interactive_resize_update(id, delta))
    }

    pub fn interactive_resize_floating_frame_end(&mut self, id: u64) {
        self.with_floating_space(id, |space| {
            space.interactive_resize_end(Some(id));
            true
        });
    }

    /// Pick each surface's primary entry (largest active-frame slot) and
    /// configure its toplevel.  Off-screen frames don't bid.
    pub(crate) fn refresh_primary_assignments(&mut self) {
        self.layout.assign_primaries(&self.sorted_outputs);
        self.refresh_mapped_cast_outputs();
        self.configure_primary_entries();
    }

    /// Configure every shown primary entry's surface for its slot, noting the resizes to fade.
    fn configure_primary_entries(&mut self) {
        let shown_primaries = self
            .layout
            .entry_locations()
            .filter(|location| location.shown() && location.entry.primary());
        for location in shown_primaries {
            let Some(id) = location.entry.surface_id() else {
                continue;
            };
            let output = location.monitor.output();
            let fullscreen = self.pending_fullscreen(id);
            let size = if fullscreen {
                crate::utils::output_size(output).to_i32_round()
            } else {
                location.entry.rect().size
            };
            let pending = if location.monitor.settles_at_once(location.frame.surface_id) {
                None
            } else {
                self.pending_resize(id, size)
            };
            if let Some(surface) = self.surfaces.get_mut(&id) {
                surface.configure(output, size, fullscreen, pending);
            }
        }
    }

    /// Emit FrameShown/FrameHidden when an output's strip-active changes.
    /// Caller snapshots `prev_active_id` before the mutation; the helper
    /// compares against the post-mutation state.  No stored state.
    pub(crate) fn reconcile_active_for_output(
        &mut self,
        output: &Output,
        prev_active_id: Option<u64>,
    ) {
        let Some(strip) = self.layout.strip(output) else {
            return;
        };
        let new_active_id = strip.active_frame_id();
        if prev_active_id == new_active_id {
            return;
        }
        if let Some(id) = prev_active_id {
            // Skip if the frame was destroyed -- the lisp side already handled it.
            // The overview keeps every frame visible.
            if !self.overview.is_active() && strip.index_of(id).is_some() {
                self.queue_event(Event::FrameHidden { id });
            }
        }
        if let Some(id) = new_active_id {
            self.queue_event(Event::FrameShown { id });
        }
        // Fullscreen floats go off and on screen with their frame.
        let before = self.layout.shown_floats(|monitor| {
            if monitor.output() == output {
                prev_active_id
            } else {
                monitor.strip.active_frame_id()
            }
        });
        self.report_float_visibility(&before);
    }

    /// Whether the strip on `pos`'s output still moves, so pointer focus waits.
    pub fn is_animating_at(&self, pos: Point<f64, Logical>) -> bool {
        let Some(output) = self.output_containing(pos) else {
            return false;
        };
        self.layout
            .strip(output)
            .is_some_and(|s| s.animations_ongoing())
    }
}

#[cfg(test)]
mod tests;
