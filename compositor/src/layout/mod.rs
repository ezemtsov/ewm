//! Frames on outputs: strips, floating spaces, where frames live and what sits under a point.

use std::collections::{HashMap, HashSet};
use std::mem;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::utils::under_from_surface_tree;
use smithay::desktop::{Window, WindowSurfaceType};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState;
use smithay::utils::{Logical, Point, Rectangle, Serial, Size};
use smithay::wayland::seat::WaylandFocus;
use tracing::{debug, warn};

use crate::{
    Event, Ewm, FocusId, LayoutEntry, LayoutEntryHit, LayoutEntryId, SurfaceHit, animation,
    floating, fullscreen_center_offset, module, strip,
};

#[derive(Debug)]
struct FrameMigration {
    id: u64,
    prev_output: Option<String>,
    previous_frame: Option<strip::Frame>,
}

#[derive(Debug)]
struct OutputLayoutSnapshot {
    output_name: String,
    active_frame_id: Option<u64>,
    entry_surface_ids: HashSet<u64>,
}

#[derive(Debug)]
pub(crate) struct EntryLocation {
    pub(crate) output: String,
    pub(crate) frame_idx: usize,
    pub(crate) floating: bool,
    pub(crate) frame_surface_id: u64,
    pub(crate) surface_id: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LayoutEntryLocation<'a> {
    pub(crate) output_name: &'a str,
    pub(crate) frame_idx: usize,
    pub(crate) floating: bool,
    pub(crate) entry: &'a LayoutEntry,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameLocation<'a> {
    pub(crate) output_name: &'a str,
    pub(crate) frame_idx: usize,
    pub(crate) strip: Option<&'a strip::Strip>,
    pub(crate) frame: &'a strip::Frame,
}

#[derive(Debug, Default)]
pub struct FrameSet {
    mapped: HashMap<String, strip::Strip>,
    /// Frames with no mapped output; the next output to map adopts them all.
    homeless: Vec<strip::Frame>,
    /// Frame surface id -> home output identity (`output_identity`, not the
    /// connector name), so a frame returns to its origin even when the display
    /// re-enumerates on a different connector after a replug.
    home_outputs: HashMap<u64, String>,
    /// Last active frame per output identity (`output_identity`, matching
    /// `home_outputs`), restored when the display maps again -- across a
    /// connector rename, not just the same connector.
    last_active: HashMap<String, u64>,
}

impl FrameSet {
    pub fn mapped_strips(&self) -> &HashMap<String, strip::Strip> {
        &self.mapped
    }

    pub fn homeless_frames(&self) -> &[strip::Frame] {
        &self.homeless
    }

    pub fn mapped_strip(&self, output_name: &str) -> Option<&strip::Strip> {
        self.mapped.get(output_name)
    }

    pub fn mapped_strip_mut(&mut self, output_name: &str) -> Option<&mut strip::Strip> {
        self.mapped.get_mut(output_name)
    }

    pub fn ensure_mapped_strip(
        &mut self,
        output_name: &str,
        clock: animation::Clock,
        scale: f64,
    ) -> &mut strip::Strip {
        let strip = self
            .mapped
            .entry(output_name.to_string())
            .or_insert_with(|| strip::Strip::new(clock));
        strip.update_config(scale);
        strip
    }

    pub fn insert_mapped_strip(
        &mut self,
        output_name: String,
        strip: strip::Strip,
    ) -> Option<strip::Strip> {
        self.mapped.insert(output_name, strip)
    }

    pub fn mapped_strips_mut(&mut self) -> impl Iterator<Item = &mut strip::Strip> {
        self.mapped.values_mut()
    }

    pub fn all_frames(&self) -> impl Iterator<Item = &strip::Frame> {
        self.mapped
            .values()
            .flat_map(|strip| strip.frames().iter())
            .chain(self.homeless.iter())
    }

    fn all_frames_mut(&mut self) -> impl Iterator<Item = &mut strip::Frame> {
        self.mapped
            .values_mut()
            .flat_map(|strip| strip.frames_mut().iter_mut())
            .chain(self.homeless.iter_mut())
    }

    pub(crate) fn frame_mut(&mut self, frame_surface_id: u64) -> Option<&mut strip::Frame> {
        self.all_frames_mut()
            .find(|frame| frame.surface_id == frame_surface_id)
    }

    fn is_homeless(&self, frame_surface_id: u64) -> bool {
        self.homeless
            .iter()
            .any(|frame| frame.surface_id == frame_surface_id)
    }

    /// Take every homeless frame for adoption by a newly mapped output.
    fn take_homeless_frames(&mut self) -> Vec<strip::Frame> {
        mem::take(&mut self.homeless)
    }

    fn remove_homeless_frame(&mut self, frame_surface_id: u64) -> bool {
        let len = self.homeless.len();
        self.homeless
            .retain(|frame| frame.surface_id != frame_surface_id);
        self.homeless.len() != len
    }

    /// Park a mapped strip's frames in the homeless pool.
    fn pool_mapped_strip(&mut self, output_name: &str) {
        let Some(mut strip) = self.mapped.remove(output_name) else {
            return;
        };
        self.homeless.extend(strip.take_frames());
    }

    fn remember_last_active(&mut self, identity: &str, frame_id: Option<u64>) {
        match frame_id {
            Some(id) => {
                self.last_active.insert(identity.to_string(), id);
            }
            None => {
                self.last_active.remove(identity);
            }
        }
    }

    fn take_last_active(&mut self, identity: &str) -> Option<u64> {
        self.last_active.remove(identity)
    }

    fn remember_frame_homes(&mut self, home: &str, frame_ids: impl IntoIterator<Item = u64>) {
        for id in frame_ids {
            if id != 0 {
                self.remember_home_if_missing(id, home);
            }
        }
    }

    fn remember_home_if_missing(&mut self, frame_id: u64, home: &str) {
        self.home_outputs
            .entry(frame_id)
            .or_insert_with(|| home.to_string());
    }

    fn forget_frame_home(&mut self, frame_id: u64) {
        self.home_outputs.remove(&frame_id);
    }

    fn home_output(&self, frame_id: u64) -> Option<&str> {
        self.home_outputs.get(&frame_id).map(String::as_str)
    }
}

impl Ewm {
    /// Detach a surface from every layout entry and the reverse index.
    pub(crate) fn detach_surface_from_layouts(&mut self, id: u64) {
        self.surface_outputs.remove(&id);
        for frame in self.frame_set.all_frames_mut() {
            frame.detach_surface(id);
        }
        for space in self.floating_spaces.values_mut() {
            for frame in space.frames_mut() {
                frame.detach_surface(id);
            }
        }
    }

    /// Restore the focus invariants after a layout swap or frame removal:
    /// drop stale `selected_entry_id` / `focused_entry_id`, and zero
    /// `focused_frame_id` when its frame no longer exists.
    pub(crate) fn prune_dangling_focus(&mut self) {
        for frame in self.frame_set.all_frames_mut() {
            frame.validate_fullscreen();
            if let Some(sel) = frame.selected_entry_id
                && !frame.entries.iter().any(|entry| entry.id == sel)
            {
                frame.selected_entry_id = None;
            }
        }
        for space in self.floating_spaces.values_mut() {
            for frame in space.frames_mut() {
                frame.validate_fullscreen();
                if let Some(sel) = frame.selected_entry_id
                    && !frame.entries.iter().any(|entry| entry.id == sel)
                {
                    frame.selected_entry_id = None;
                }
            }
        }
        if let Some(v) = self.focused_entry_id {
            let in_focused_frame = self
                .frame_location(self.focused_frame_id)
                .map(|location| location.frame)
                .is_some_and(|f| f.entries.iter().any(|entry| entry.id == v));
            if !in_focused_frame {
                self.focused_entry_id = None;
            }
        }
        if self.focused_frame_id != 0 && self.lookup_frame(self.focused_frame_id).is_none() {
            self.focused_frame_id = 0;
            self.focused_entry_id = None;
        }
    }

    /// If no frame is focused but some frame exists, use canonical output order.
    pub(crate) fn focus_any_frame_if_unfocused(&mut self, reason: &'static str) {
        if self.focused_frame_id == 0 {
            for output in &self.sorted_outputs {
                let name = output.name();
                if let Some(id) = self
                    .frame_set
                    .mapped_strip(&name)
                    .and_then(|strip| strip.frames().first())
                    .map(|frame| frame.surface_id)
                {
                    self.set_focus_frame(id, reason, false);
                    return;
                }
            }
        }
    }

    pub(crate) fn focus_mapped_frame_if_focused_output_unmapped(&mut self, output_name: &str) {
        let focused_output = self
            .frame_position(self.focused_surface_id())
            .map(|(focused_output, _)| focused_output);
        let focused_homeless = self.frame_set.is_homeless(self.focused_frame_id);
        if focused_output.as_deref() != Some(output_name) && !focused_homeless {
            return;
        }

        if let Some(id) = self
            .sorted_outputs
            .iter()
            .find_map(|output| self.active_frame_surface_id(&output.name()))
        {
            self.set_focus_frame(id, "output_disabled", false);
        } else {
            self.set_focus_state(0, None);
        }
    }

    /// Bake frame `id`'s close snapshot.
    pub(crate) fn store_frame_close_snapshot(&mut self, renderer: &mut GlesRenderer, id: u64) {
        let Some(snapshot) = crate::render::render_frame_snapshot(self, renderer, id) else {
            return;
        };
        if let Some(frame) = self.frame_mut(id) {
            frame.close_snapshot = Some(snapshot);
        }
    }

    /// Drop the Emacs frame `id` from whichever strip holds it.
    pub(crate) fn remove_emacs_frame_from_strip(&mut self, id: u64, active_close: bool) {
        self.frame_set.forget_frame_home(id);
        if self.frame_set.remove_homeless_frame(id) {
            return;
        }
        let Some((out, idx)) = self.lookup_frame(id) else {
            return;
        };
        let pre_entries = self.strip_entries(&out);
        let removed_floating = self.floating_spaces.get_mut(&out).and_then(|space| {
            space
                .close_frame(id)
                .map(|_| space.frames().is_empty() && space.closing_frames().is_empty())
        });
        if let Some(empty) = removed_floating {
            if empty {
                self.floating_spaces.remove(&out);
            }
            self.reconcile_output_entries(&out, &pre_entries);
            return;
        }
        let mut prev_active_id = None;
        if let Some(strip) = self.frame_set.mapped_strip_mut(&out) {
            prev_active_id = strip.active_frame().map(|f| f.surface_id);
            if active_close {
                strip.close_active_frame_at(idx);
            } else {
                strip.close_frame_at(idx);
            }
        }
        self.reconcile_output_entries(&out, &pre_entries);
        self.reconcile_active_for_output(&out, prev_active_id);
    }

    pub(crate) fn migrate_or_pool_output_frames(&mut self, output_name: &str, output: &Output) {
        let active_id = self.active_frame_surface_id(output_name);
        let home_identity = crate::utils::output_identity(output);
        self.frame_set
            .remember_last_active(&home_identity, active_id);

        if !self.migrate_output_frames_to_fallback(output_name, &home_identity) {
            let pre = self.surface_outputs_for(output_name);
            self.reconcile_output_membership(output_name, Some(output), &pre, &HashSet::new());
            self.leave_emacs_frames_on_output(output);
            if let Some(id) = active_id {
                self.queue_event(Event::FrameHidden { id });
            }
        }
        self.frame_set.pool_mapped_strip(output_name);
    }

    /// Resolve an Emacs window entry id to its strip location and owning frame.
    fn frame_at_location(&self, location: &LayoutEntryLocation) -> Option<&strip::Frame> {
        if location.floating {
            self.floating_spaces
                .get(location.output_name)?
                .frames()
                .get(location.frame_idx)
        } else {
            self.frame_set
                .mapped_strip(location.output_name)?
                .frames()
                .get(location.frame_idx)
        }
    }

    pub(crate) fn lookup_entry(&self, entry_id: LayoutEntryId) -> Option<EntryLocation> {
        let location = self.layout_entry_for_id(entry_id)?;
        let frame = self.frame_at_location(&location)?;
        Some(EntryLocation {
            output: location.output_name.to_string(),
            frame_idx: location.frame_idx,
            floating: location.floating,
            frame_surface_id: frame.surface_id,
            surface_id: location.entry.surface_id(),
        })
    }

    pub(crate) fn frame_location(&self, frame_surface_id: u64) -> Option<FrameLocation<'_>> {
        for (output_name, strip) in self.frame_set.mapped_strips() {
            if let Some((frame_idx, frame)) = strip
                .frames()
                .iter()
                .enumerate()
                .find(|(_, frame)| frame.surface_id == frame_surface_id)
            {
                return Some(FrameLocation {
                    output_name,
                    frame_idx,
                    strip: Some(strip),
                    frame,
                });
            }
        }
        for (output_name, space) in &self.floating_spaces {
            if let Some((frame_idx, frame)) = space
                .frames()
                .iter()
                .enumerate()
                .find(|(_, frame)| frame.surface_id == frame_surface_id)
            {
                return Some(FrameLocation {
                    output_name,
                    frame_idx,
                    strip: None,
                    frame,
                });
            }
        }
        None
    }

    /// Mutable lookup across strips, the homeless pool and floating spaces.
    pub(crate) fn frame_mut(&mut self, frame_surface_id: u64) -> Option<&mut strip::Frame> {
        self.frame_set.frame_mut(frame_surface_id).or_else(|| {
            self.floating_spaces
                .values_mut()
                .find_map(|space| space.frame_mut(frame_surface_id))
        })
    }

    pub(crate) fn mapped_output_strips(&self) -> impl Iterator<Item = (&str, &strip::Strip)> {
        self.sorted_outputs.iter().filter_map(|output| {
            self.frame_set
                .mapped_strips()
                .get_key_value(&output.name())
                .map(|(name, strip)| (name.as_str(), strip))
        })
    }

    pub fn is_emacs_frame(&self, id: u64) -> bool {
        self.frame_location(id).is_some() || self.frame_set.is_homeless(id)
    }

    pub fn emacs_frame_output(&self, id: u64) -> Option<&str> {
        self.frame_location(id).map(|location| location.output_name)
    }

    pub fn emacs_frames_on<'a>(&'a self, output_name: &'a str) -> impl Iterator<Item = u64> + 'a {
        self.frame_set
            .mapped_strip(output_name)
            .into_iter()
            .flat_map(|s| s.frames().iter().map(|f| f.surface_id))
            .filter(|&id| id != 0)
    }

    /// Resolve a frame's xdg_toplevel `surface_id` to `(output_name, frame_idx)`.
    pub fn lookup_frame(&self, frame_surface_id: u64) -> Option<(String, usize)> {
        self.frame_location(frame_surface_id)
            .map(|location| (location.output_name.to_string(), location.frame_idx))
    }

    /// Frame surface_id whose focus id matches.
    pub fn frame_with_focus_id(&self, focus_id: FocusId) -> Option<u64> {
        self.mapped_output_strips()
            .flat_map(|(_, strip)| strip.frames().iter())
            .chain(
                self.floating_spaces
                    .values()
                    .flat_map(|space| space.frames().iter()),
            )
            .find(|f| f.focus_id == focus_id)
            .map(|f| f.surface_id)
    }

    /// Focus id assigned to the frame with this surface id.
    pub fn focus_id_of_frame(&self, frame_surface_id: u64) -> Option<FocusId> {
        self.frame_location(frame_surface_id)
            .map(|location| location.frame.focus_id)
    }

    /// Surface id of the Emacs frame whose strip-frame currently hosts
    /// `surface_id`, or `None` if the surface is not under any frame.
    pub fn frame_for_surface(&self, surface_id: u64) -> Option<u64> {
        self.frame_entry_for_surface(surface_id)
            .map(|(frame_id, _)| frame_id)
    }

    pub(crate) fn frame_entry_for_surface(&self, surface_id: u64) -> Option<(u64, LayoutEntryId)> {
        let location = self
            .focused_entry_for_surface(surface_id)
            .or_else(|| self.layout_entry_for_surface(surface_id, |_| true))?;
        let frame = self.frame_at_location(&location)?;
        Some((frame.surface_id, location.entry.id))
    }

    /// `(output_name, frame_idx)` of the strip frame containing `surface_id`,
    /// either as the frame's own xdg_toplevel or in its layout entries.
    pub fn frame_position(&self, surface_id: u64) -> Option<(String, usize)> {
        // Frame surface match is unique.
        if let Some(location) = self.lookup_frame(surface_id) {
            return Some(location);
        }
        // For multi-view surfaces, prefer the focused output's active frame so remote
        // focus requests (xdg_activation, foreign_toplevel, unlock) don't jump outputs.
        if let Some(out) = self.get_focused_output()
            && let Some(strip) = self.frame_set.mapped_strip(&out)
        {
            let idx = strip.active_idx();
            if strip.frames().get(idx).is_some_and(|frame| {
                frame
                    .entries
                    .iter()
                    .any(|entry| entry.surface_id() == Some(surface_id))
            }) {
                return Some((out, idx));
            }
        }
        // Fall back to visible frames in canonical output order.
        self.mapped_output_strips()
            .find_map(|(name, strip)| {
                strip
                    .frames()
                    .iter()
                    .position(|frame| {
                        frame
                            .entries
                            .iter()
                            .any(|entry| entry.surface_id() == Some(surface_id))
                    })
                    .map(|i| (name.to_string(), i))
            })
            .or_else(|| {
                self.floating_spaces.iter().find_map(|(name, space)| {
                    space
                        .frames()
                        .iter()
                        .position(|frame| {
                            frame
                                .entries
                                .iter()
                                .any(|entry| entry.surface_id() == Some(surface_id))
                        })
                        .map(|i| (name.to_string(), i))
                })
            })
    }

    /// Surface id of the active Emacs frame on `output_name`.
    pub fn active_frame_surface_id(&self, output_name: &str) -> Option<u64> {
        let strip = self.frame_set.mapped_strip(output_name)?;
        strip.active_frame().map(|f| f.surface_id)
    }

    /// Active Emacs frame surface id per mapped output.
    pub(crate) fn active_frames_snapshot(&self) -> HashMap<String, u64> {
        self.sorted_outputs
            .iter()
            .filter_map(|o| {
                let name = o.name();
                self.active_frame_surface_id(&name).map(|id| (name, id))
            })
            .collect()
    }

    pub(crate) fn first_active_frame_surface_id(&self) -> Option<u64> {
        self.sorted_outputs
            .iter()
            .find_map(|output| self.active_frame_surface_id(&output.name()))
    }

    /// Activate a frame on OUTPUT, dispatching by placement kind.
    pub(crate) fn activate_frame_at(
        &mut self,
        output: &str,
        floating: bool,
        frame_surface_id: u64,
        frame_idx: usize,
    ) {
        if floating {
            self.activate_floating_frame(output, frame_surface_id);
        } else {
            self.activate_frame(output, frame_idx);
        }
    }

    /// Mark `new_idx` as active on `output_name`.
    fn activate_floating_frame(&mut self, output_name: &str, frame_surface_id: u64) {
        if let Some(space) = self.floating_spaces.get_mut(output_name) {
            space.activate_frame(frame_surface_id);
            if let Some(output) = self.find_mapped_output(output_name) {
                self.queue_redraw(&output);
            }
        }
    }

    pub fn activate_frame(&mut self, output_name: &str, new_idx: usize) {
        let Some(strip) = self.frame_set.mapped_strip_mut(output_name) else {
            return;
        };
        let prev_idx = strip.active_idx();
        let prev_active_id = strip.active_frame().map(|f| f.surface_id);
        let frames_len = strip.frames().len();
        let prev_offset = strip.view_offset().current();
        strip.set_active(new_idx);
        let active_changed = strip.active_idx() != prev_idx;
        if module::DEBUG_MODE.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::debug!(
                "activate_frame: {} {} -> {} (frames={}) view_offset {:.1} -> {:.1}",
                output_name,
                prev_idx,
                strip.active_idx(),
                frames_len,
                prev_offset,
                strip.view_offset().current(),
            );
        }

        // Active-idx flip may shift a surface's largest visible entry to/from this
        // output, changing its size and scale.
        if active_changed {
            self.reconcile_active_for_output(output_name, prev_active_id);
            self.refresh_primary_assignments();
            self.queue_redraw_all();
            return;
        }
        let output = self.find_mapped_output(output_name);
        if let Some(output) = output {
            self.queue_redraw(&output);
        }
    }

    /// Global logical top-left of an Emacs frame's working area,
    /// driven by the strip column (frames aren't placed in `space`).
    pub fn emacs_frame_origin(
        &self,
        surface_id: u64,
    ) -> Option<smithay::utils::Point<i32, smithay::utils::Logical>> {
        let location = self.frame_location(surface_id)?;
        let output = self.mapped_output_by_name(location.output_name)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);

        if location.strip.is_none() {
            let (_, data) = self
                .floating_spaces
                .get(location.output_name)?
                .frame_with_data(surface_id)?;
            let loc = data.logical_pos();
            return Some(Point::from((
                output_geo.loc.x + loc.x as i32,
                output_geo.loc.y + loc.y as i32,
            )));
        }

        Some(
            Self::frame_rect(
                output_geo,
                working_area,
                location.strip?,
                location.frame_idx,
                location.frame,
            )
            .loc,
        )
    }

    fn frame_rect(
        output_geo: Rectangle<i32, Logical>,
        working_area: Rectangle<i32, Logical>,
        strip: &strip::Strip,
        frame_idx: usize,
        frame: &strip::Frame,
    ) -> Rectangle<i32, Logical> {
        // Rounded so popups and hits track a sliding frame.
        let frame_dx = strip.screen_x_of_frame(frame_idx).round() as i32;
        Rectangle::new(
            Point::from((
                output_geo.loc.x + working_area.loc.x + frame_dx,
                output_geo.loc.y + working_area.loc.y,
            )),
            Size::from((frame.width as i32, working_area.size.h)),
        )
    }

    /// Find the focused layout entry for a surface.
    ///
    /// The frame index feeds `Strip::screen_x_of_frame` so callers can apply
    /// the per-frame slide offset when computing global positions.
    pub(crate) fn focused_entry_for_surface(&self, id: u64) -> Option<LayoutEntryLocation<'_>> {
        let focused = self.focused_entry_id?;
        self.layout_entry_for_surface(id, |entry| entry.id == focused)
    }

    pub(crate) fn layout_entry_for_id(
        &self,
        entry_id: LayoutEntryId,
    ) -> Option<LayoutEntryLocation<'_>> {
        for (output_name, strip) in self.mapped_output_strips() {
            if let Some((frame_idx, entry)) = strip
                .entries_with_frame()
                .find(|(_, entry)| entry.id == entry_id)
            {
                return Some(LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    floating: false,
                    entry,
                });
            }
        }
        for (output_name, space) in &self.floating_spaces {
            if let Some((frame_idx, entry)) = space
                .entries_with_frame()
                .find(|(_, entry)| entry.id == entry_id)
            {
                return Some(LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    floating: true,
                    entry,
                });
            }
        }
        None
    }

    pub(crate) fn layout_entry_for_surface(
        &self,
        id: u64,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryLocation<'_>> {
        for (output_name, strip) in self.mapped_output_strips() {
            if let Some((frame_idx, entry)) = strip
                .surface_entries_with_frame()
                .find(|(_, entry)| entry.surface_id() == Some(id) && predicate(entry))
            {
                return Some(LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    floating: false,
                    entry,
                });
            }
        }
        for (output_name, space) in &self.floating_spaces {
            if let Some((frame_idx, entry)) = space
                .surface_entries_with_frame()
                .find(|(_, entry)| entry.surface_id() == Some(id) && predicate(entry))
            {
                return Some(LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    floating: true,
                    entry,
                });
            }
        }
        None
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
        let id = self.window_ids.get(window).copied()?;
        if !self.surface_outputs.contains_key(&id) {
            return self.emacs_frame_origin(id);
        }
        let location = self.preferred_entry_for_surface(id)?;
        self.entry_origin(location)
    }

    /// On-screen rectangle of one layout view. Fullscreen-primary centers on
    /// the toplevel's geometry.
    fn layout_entry_is_fullscreen(
        strip: &strip::Strip,
        frame_idx: usize,
        entry: &LayoutEntry,
    ) -> bool {
        strip
            .frames()
            .get(frame_idx)
            .is_some_and(|frame| frame.entry_fullscreen(entry.id))
    }

    pub(crate) fn layout_entry_rect(
        &self,
        output_geo: Rectangle<i32, Logical>,
        working_area: Rectangle<i32, Logical>,
        strip: &strip::Strip,
        frame_idx: usize,
        entry: &LayoutEntry,
    ) -> Rectangle<i32, Logical> {
        if Self::layout_entry_is_fullscreen(strip, frame_idx, entry) {
            if entry.primary() {
                let window_size = entry
                    .surface_id()
                    .and_then(|surface_id| self.id_windows.get(&surface_id))
                    .map(|window| window.geometry().size)
                    .unwrap_or_default();
                let (ox, oy) = fullscreen_center_offset(window_size, output_geo.size);
                return Rectangle::new(
                    Point::from((output_geo.loc.x + ox, output_geo.loc.y + oy)),
                    window_size,
                );
            }
            return Rectangle::new(output_geo.loc, entry.rect().size);
        }

        let frame_dx = strip.screen_x_of_frame(frame_idx).round() as i32;
        Rectangle::new(
            Point::from((
                output_geo.loc.x + working_area.loc.x + frame_dx + entry.x,
                output_geo.loc.y + working_area.loc.y + entry.y,
            )),
            entry.rect().size,
        )
    }

    fn entry_origin(&self, location: LayoutEntryLocation<'_>) -> Option<Point<i32, Logical>> {
        let output = self.mapped_output_by_name(location.output_name)?;
        let output_geo = self.space.output_geometry(output)?;
        if location.floating {
            let space = self.floating_spaces.get(location.output_name)?;
            let frame = space.frames().get(location.frame_idx)?;
            let (_, data) = space.frame_with_data(frame.surface_id)?;
            let loc = data.logical_pos();
            return Some(Point::from((
                output_geo.loc.x + loc.x as i32 + location.entry.x,
                output_geo.loc.y + loc.y as i32 + location.entry.y,
            )));
        }
        let working_area = self.get_working_area(output);
        let strip = self.frame_set.mapped_strip(location.output_name)?;
        Some(
            self.layout_entry_rect(
                output_geo,
                working_area,
                strip,
                location.frame_idx,
                location.entry,
            )
            .loc,
        )
    }

    pub(crate) fn layout_entry_location_fullscreen(
        &self,
        location: LayoutEntryLocation<'_>,
    ) -> bool {
        !location.floating
            && self
                .frame_set
                .mapped_strip(location.output_name)
                .is_some_and(|strip| {
                    Self::layout_entry_is_fullscreen(strip, location.frame_idx, location.entry)
                })
    }

    /// On-screen origins of every layout view of `surface_id`, one per entry.
    pub fn surface_view_origins(&self, surface_id: u64) -> Vec<Point<i32, Logical>> {
        if !self.surface_outputs.contains_key(&surface_id) {
            return self.emacs_frame_origin(surface_id).into_iter().collect();
        }
        let mut origins = Vec::new();
        for (output_name, strip) in self.mapped_output_strips() {
            for (frame_idx, entry) in strip.surface_entries_with_frame() {
                if entry.surface_id() != Some(surface_id) {
                    continue;
                }
                let location = LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    floating: false,
                    entry,
                };
                if let Some(p) = self.entry_origin(location) {
                    origins.push(p);
                }
            }
        }
        for (output_name, space) in &self.floating_spaces {
            for (frame_idx, entry) in space.surface_entries_with_frame() {
                if entry.surface_id() != Some(surface_id) {
                    continue;
                }
                let location = LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    floating: true,
                    entry,
                };
                if let Some(p) = self.entry_origin(location) {
                    origins.push(p);
                }
            }
        }
        origins
    }

    fn rect_contains_f64(rect: Rectangle<i32, Logical>, pos: Point<f64, Logical>) -> bool {
        pos.x >= rect.loc.x as f64
            && pos.y >= rect.loc.y as f64
            && pos.x < (rect.loc.x + rect.size.w) as f64
            && pos.y < (rect.loc.y + rect.size.h) as f64
    }

    /// Floating space under `pos`: its output name, the space, and output-local `pos`.
    fn floating_space_at(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(String, &floating::FloatingSpace, Point<f64, Logical>)> {
        if !self.frames_in_place() {
            return None;
        }
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let output_name = output.name();
        let pos_within_output = pos - output_geo.loc.to_f64();
        let space = self.floating_spaces.get(&output_name)?;
        Some((output_name, space, pos_within_output))
    }

    pub(crate) fn floating_frame_under(&self, pos: Point<f64, Logical>) -> Option<(String, u64)> {
        let (output_name, space, pos_within_output) = self.floating_space_at(pos)?;
        let (frame_id, _) = space.frame_under(pos_within_output)?;
        Some((output_name, frame_id))
    }

    pub(crate) fn floating_resize_edges_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(String, u64, floating::ResizeEdge)> {
        let (output_name, space, pos_within_output) = self.floating_space_at(pos)?;
        let (frame_id, edges) = space.resize_edges_under(pos_within_output)?;
        Some((output_name, frame_id, edges))
    }

    fn layout_entry_under_matching(
        &self,
        pos: Point<f64, Logical>,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryHit<'_>> {
        // Zoomed or hidden content takes no surface hits.
        if !self.frames_in_place() {
            return None;
        }
        if let Some(hit) = self.floating_entry_under_matching(pos, &mut predicate) {
            return Some(hit);
        }
        // A floating frame occludes the strip; don't match a tiled entry beneath it.
        if self.floating_frame_under(pos).is_some() {
            return None;
        }
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);
        let strip = self.frame_set.mapped_strip(&output.name())?;
        self.tiled_entry_under_matching(strip, output_geo, working_area, pos, predicate)
    }

    /// Hit-test `pos` against floating frames' entries only (the overlay above the strip).
    fn floating_entry_under_matching(
        &self,
        pos: Point<f64, Logical>,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryHit<'_>> {
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let space = self.floating_spaces.get(&output.name())?;
        for (frame, frame_rect) in space.frames_with_render_geo() {
            for entry in frame.entries.iter() {
                if !predicate(entry) {
                    continue;
                }
                let rect = Rectangle::new(
                    Point::from((
                        output_geo.loc.x + frame_rect.loc.x as i32 + entry.x,
                        output_geo.loc.y + frame_rect.loc.y as i32 + entry.y,
                    )),
                    entry.rect().size,
                );
                if Self::rect_contains_f64(rect, pos) {
                    return Some(LayoutEntryHit {
                        entry,
                        fullscreen: false,
                        rect,
                        output_size: output_geo.size,
                    });
                }
            }
        }
        None
    }

    /// Hit-test `pos` against a tiled strip's entries only (no floating overlay).
    fn tiled_entry_under_matching<'a>(
        &self,
        strip: &'a strip::Strip,
        output_geo: Rectangle<i32, Logical>,
        working_area: Rectangle<i32, Logical>,
        pos: Point<f64, Logical>,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryHit<'a>> {
        let active_idx = strip.active_idx();
        // Fullscreen primary clicks miss in the letterbox; inactive fullscreens render off-screen.
        for (frame_idx, entry) in strip.entries_with_frame() {
            if !predicate(entry) {
                continue;
            }
            let fullscreen = Self::layout_entry_is_fullscreen(strip, frame_idx, entry);
            if fullscreen && frame_idx != active_idx {
                continue;
            }
            let rect = self.layout_entry_rect(output_geo, working_area, strip, frame_idx, entry);
            if Self::rect_contains_f64(rect, pos) {
                return Some(LayoutEntryHit {
                    entry,
                    fullscreen,
                    rect,
                    output_size: output_geo.size,
                });
            }
        }
        None
    }

    /// Find the topmost layout entry containing `pos`, returning the entry
    /// and its global rectangle.
    fn layout_entry_under(&self, pos: Point<f64, Logical>) -> Option<LayoutEntryHit<'_>> {
        self.layout_entry_under_matching(pos, |_| true)
    }

    /// Find the topmost surface-backed layout entry containing `pos`.
    fn surface_layout_entry_under(&self, pos: Point<f64, Logical>) -> Option<LayoutEntryHit<'_>> {
        self.layout_entry_under_matching(pos, |entry| entry.surface_id().is_some())
    }

    /// The tiled entry whose rect contains a floating frame's center, for spatial reattach.
    pub fn tiled_entry_under_floating_center(
        &self,
        floating_frame_id: u64,
    ) -> Option<LayoutEntryId> {
        let (output_name, center) = self.floating_spaces.iter().find_map(|(name, space)| {
            space.frames_with_render_geo().find_map(|(frame, rect)| {
                (frame.surface_id == floating_frame_id).then(|| {
                    let c: Point<f64, Logical> = Point::from((
                        rect.loc.x + rect.size.w / 2.0,
                        rect.loc.y + rect.size.h / 2.0,
                    ));
                    (name.clone(), c)
                })
            })
        })?;
        let output = self.mapped_output_by_name(&output_name)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);
        let center = Point::from((
            output_geo.loc.x as f64 + center.x,
            output_geo.loc.y as f64 + center.y,
        ));
        let strip = self.frame_set.mapped_strip(&output_name)?;
        self.tiled_entry_under_matching(strip, output_geo, working_area, center, |_| true)
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
        self.surface_layout_entry_under(pos)
            .and_then(|hit| self.surface_under_layout_entry(pos, hit))
            .or_else(|| self.surface_under_frame(pos))
    }

    fn surface_under_layout_entry(
        &self,
        pos: Point<f64, Logical>,
        hit: LayoutEntryHit<'_>,
    ) -> Option<SurfaceHit> {
        let entry = hit.entry;
        let window = self.id_windows.get(&entry.surface_id()?)?;
        let window_geo = window.geometry();
        let pointer_scale =
            Self::pointer_scale_for_entry(entry, hit.fullscreen, window_geo.size, hit.output_size);

        let pos_in_window = Point::from((
            (pos.x - hit.rect.loc.x as f64) * pointer_scale,
            (pos.y - hit.rect.loc.y as f64) * pointer_scale,
        ));
        let window_origin: Point<f64, Logical> =
            Point::from((pos.x - pos_in_window.x, pos.y - pos_in_window.y));

        Self::surface_under_non_popup_window(window, pos_in_window, window_origin)
    }

    /// Find the frame containing `pos`, returning the frame surface itself.
    fn surface_under_frame(&self, pos: Point<f64, Logical>) -> Option<SurfaceHit> {
        if !self.frames_in_place() {
            return None;
        }
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);
        if let Some(space) = self.floating_spaces.get(&output.name()) {
            for (frame, frame_rect) in space.frames_with_render_geo() {
                let rect = Rectangle::new(
                    Point::from((
                        output_geo.loc.x + frame_rect.loc.x as i32,
                        output_geo.loc.y + frame_rect.loc.y as i32,
                    )),
                    Size::from((frame_rect.size.w as i32, frame_rect.size.h as i32)),
                );
                if !Self::rect_contains_f64(rect, pos) {
                    continue;
                }
                let Some(window) = self.id_windows.get(&frame.surface_id) else {
                    continue;
                };
                let window_origin: Point<f64, Logical> =
                    Point::from((rect.loc.x as f64, rect.loc.y as f64));
                let pos_in_window = Point::from((pos.x - window_origin.x, pos.y - window_origin.y));
                if let Some(hit) =
                    Self::surface_under_non_popup_window(window, pos_in_window, window_origin)
                {
                    return Some(hit);
                }
            }
        }
        let strip = self.frame_set.mapped_strip(&output.name())?;

        for (frame_idx, frame) in strip.frames().iter().enumerate() {
            let rect = Self::frame_rect(output_geo, working_area, strip, frame_idx, frame);
            if !Self::rect_contains_f64(rect, pos) {
                continue;
            }
            let Some(window) = self.id_windows.get(&frame.surface_id) else {
                continue;
            };
            let window_origin: Point<f64, Logical> =
                Point::from((rect.loc.x as f64, rect.loc.y as f64));
            let pos_in_window = Point::from((pos.x - window_origin.x, pos.y - window_origin.y));
            if let Some(hit) =
                Self::surface_under_non_popup_window(window, pos_in_window, window_origin)
            {
                return Some(hit);
            }
        }

        None
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

    /// Check if a surface is currently displaying as fullscreen on any output.
    pub fn is_surface_fullscreen(&self, id: u64) -> bool {
        self.mapped_output_strips()
            .map(|(_, strip)| strip)
            .any(|strip| {
                strip
                    .frames()
                    .iter()
                    .any(|frame| frame.surface_fullscreen(id))
            })
    }

    /// The views of `id` in active frames and floating frames, with their output.
    pub(crate) fn surface_views(&self, id: u64) -> impl Iterator<Item = (&str, &LayoutEntry)> {
        self.mapped_output_strips()
            .flat_map(|(name, strip)| {
                strip
                    .active_surface_entries()
                    .map(move |entry| (name, entry))
            })
            .chain(self.floating_spaces.iter().flat_map(|(name, space)| {
                space
                    .surface_entries()
                    .map(move |entry| (name.as_str(), entry))
            }))
            .filter(move |(_, entry)| entry.surface_id() == Some(id))
    }

    fn surface_has_primary_entry(&self, id: u64) -> bool {
        self.surface_views(id).any(|(_, entry)| entry.primary())
    }

    fn configure_surface_fullscreen_state(&self, id: u64, fullscreen: bool) {
        let Some(window) = self.id_windows.get(&id) else {
            return;
        };
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let changed = toplevel.with_pending_state(|state| {
            let was_fullscreen = state.states.contains(XdgToplevelState::Fullscreen);
            if fullscreen {
                state.states.set(XdgToplevelState::Fullscreen);
                state.states.unset(XdgToplevelState::Maximized);
            } else {
                state.states.unset(XdgToplevelState::Fullscreen);
            }
            was_fullscreen != fullscreen
        });
        if changed {
            toplevel.send_configure();
        }
    }

    pub fn set_layout_entry_fullscreen(
        &mut self,
        entry_id: LayoutEntryId,
        fullscreen: bool,
    ) -> Option<u64> {
        let frame = self
            .frame_set
            .all_frames_mut()
            .find(|frame| frame.entry_surface_id(entry_id).is_some())?;
        let target_surface = frame.entry_surface_id(entry_id)?;
        let cleared = if fullscreen {
            frame
                .set_fullscreen_entry(entry_id)
                .and_then(|(previous, _)| previous)
        } else {
            frame.clear_fullscreen_entry(entry_id)
        };
        self.refresh_primary_assignments();
        if let Some(cleared) = cleared
            && !self.surface_has_primary_entry(cleared.surface_id)
        {
            self.configure_surface_fullscreen_state(cleared.surface_id, false);
        }
        if !self.surface_has_primary_entry(target_surface) {
            self.configure_surface_fullscreen_state(target_surface, fullscreen);
        }
        Some(target_surface)
    }

    pub fn toggle_layout_entry_fullscreen(&mut self, entry_id: LayoutEntryId) {
        let fullscreen = self
            .layout_entry_for_id(entry_id)
            .is_some_and(|location| self.layout_entry_location_fullscreen(location));
        self.set_layout_entry_fullscreen(entry_id, !fullscreen);
    }

    /// True iff the active frame has a fullscreen entry covering the Top layer.
    pub fn render_above_top_layer(&self, output: &Output) -> bool {
        self.frames_in_place()
            && self
                .frame_set
                .mapped_strip(&output.name())
                .is_some_and(strip::Strip::active_frame_is_fullscreen)
    }

    /// Entry id of the layout entry under a point.
    pub fn layout_entry_id_under(&self, pos: Point<f64, Logical>) -> Option<LayoutEntryId> {
        self.layout_entry_under(pos).map(|hit| hit.entry.id)
    }

    /// Entry surface ids on `output_name`'s strip; empty if the strip is gone.
    fn strip_entries(&self, output_name: &str) -> HashSet<u64> {
        let mut entries: HashSet<u64> = self
            .frame_set
            .mapped_strip(output_name)
            .map(|strip| {
                strip
                    .surface_entries()
                    .filter_map(LayoutEntry::surface_id)
                    .collect()
            })
            .unwrap_or_default();
        if let Some(space) = self.floating_spaces.get(output_name) {
            entries.extend(space.surface_entries().filter_map(LayoutEntry::surface_id));
        }
        entries
    }

    /// Whether `id` shows on output `name`, as a view or as an Emacs frame.
    pub(crate) fn surface_on_output(&self, id: u64, name: &str) -> bool {
        self.surface_outputs
            .get(&id)
            .is_some_and(|names| names.contains(name))
            || self.emacs_frame_output(id) == Some(name)
    }

    fn surface_outputs_for(&self, output_name: &str) -> HashSet<u64> {
        self.surface_outputs
            .iter()
            .filter_map(|(&id, outputs)| outputs.contains(output_name).then_some(id))
            .collect()
    }

    pub(crate) fn set_preferred_scale_transform(&self, window: &Window, output: &Output) {
        let scale = output.current_scale();
        let transform = output.current_transform();
        window.with_surfaces(|surface, data| {
            crate::utils::send_scale_transform(surface, data, scale, transform);
        });
    }

    pub(crate) fn enter_output_for_window(&self, window: &Window, output: &Output) {
        self.set_preferred_scale_transform(window, output);
        if let Some(surface) = window.wl_surface() {
            output.enter(&surface);
        }
    }

    fn leave_output_for_window(&self, window: &Window, output: &Output) {
        if let Some(surface) = window.wl_surface() {
            output.leave(&surface);
        }
    }

    fn leave_emacs_frames_on_output(&self, output: &Output) {
        let output_name = output.name();
        for id in self.emacs_frames_on(&output_name) {
            if let Some(window) = self.id_windows.get(&id) {
                self.leave_output_for_window(window, output);
            }
        }
    }

    fn reconcile_output_membership(
        &mut self,
        output_name: &str,
        output: Option<&Output>,
        pre: &HashSet<u64>,
        post: &HashSet<u64>,
    ) {
        for &id in pre.difference(post) {
            if let (Some(out), Some(window)) = (output, self.id_windows.get(&id)) {
                self.leave_output_for_window(window, out);
            }
            if let Some(outputs) = self.surface_outputs.get_mut(&id) {
                outputs.remove(output_name);
                if outputs.is_empty() {
                    self.surface_outputs.remove(&id);
                }
            }
        }
        for &id in post.difference(pre) {
            let Some(window) = self.id_windows.get(&id) else {
                warn!("reconcile_output_entries: surface {} not in id_windows", id);
                continue;
            };
            if let Some(out) = output {
                self.enter_output_for_window(window, out);
            }
        }
        for &id in post {
            self.surface_outputs
                .entry(id)
                .or_default()
                .insert(output_name.to_string());
        }
    }

    /// Diff `pre` (snapshot before strip mutation) against the current strip;
    /// fire `wl_surface.enter` / `leave` and sync `surface_outputs`.
    fn reconcile_output_entries(&mut self, output_name: &str, pre: &HashSet<u64>) {
        let output = self.find_mapped_output(output_name);
        let post = output
            .as_ref()
            .map(|_| self.strip_entries(output_name))
            .unwrap_or_default();
        self.reconcile_output_membership(output_name, output.as_ref(), pre, &post);
    }

    pub(crate) fn reconcile_mapped_output_entries(&mut self, output: &Output) {
        let output_name = output.name();
        let pre = self.surface_outputs_for(&output_name);
        let post = self.strip_entries(&output_name);
        self.reconcile_output_membership(&output_name, Some(output), &pre, &post);
    }

    fn collect_frame_migrations(
        &self,
        output_name: &str,
        frames: &[strip::Frame],
    ) -> Vec<FrameMigration> {
        frames
            .iter()
            .filter_map(|frame| {
                let id = frame.surface_id;
                if id == 0 {
                    return None;
                }
                let previous = self.frame_location(id);
                let prev_output = previous.map(|location| location.output_name.to_string());
                if prev_output.as_deref() == Some(output_name) {
                    return None;
                }
                Some(FrameMigration {
                    id,
                    prev_output,
                    previous_frame: previous.map(|location| location.frame.clone()),
                })
            })
            .collect()
    }

    fn preserve_migrated_frame_state(frames: &mut [strip::Frame], migrations: &[FrameMigration]) {
        for frame in frames {
            let Some(previous_frame) = migrations
                .iter()
                .find(|migration| migration.id == frame.surface_id)
                .and_then(|migration| migration.previous_frame.as_ref())
            else {
                continue;
            };
            if frame.workspace_name.is_none() {
                frame.workspace_name = previous_frame.workspace_name.clone();
            }
            frame.preserve_compositor_view_state_from(previous_frame);
        }
    }

    fn output_layout_snapshot(&self, output_name: String) -> OutputLayoutSnapshot {
        let active_frame_id = self
            .frame_set
            .mapped_strip(&output_name)
            .and_then(|strip| strip.active_frame().map(|frame| frame.surface_id));
        let entry_surface_ids = self.strip_entries(&output_name);
        OutputLayoutSnapshot {
            output_name,
            active_frame_id,
            entry_surface_ids,
        }
    }

    fn output_layout_snapshots(
        &self,
        output_name: &str,
        migrations: &[FrameMigration],
    ) -> Vec<OutputLayoutSnapshot> {
        let mut output_names = vec![output_name.to_string()];
        for migration in migrations {
            if let Some(prev_name) = &migration.prev_output
                && !output_names.iter().any(|name| name == prev_name)
            {
                output_names.push(prev_name.clone());
            }
        }
        output_names
            .into_iter()
            .map(|name| self.output_layout_snapshot(name))
            .collect()
    }

    fn remove_migrated_frames_from_sources(
        &mut self,
        migrations: &[FrameMigration],
        snapshots: &[OutputLayoutSnapshot],
    ) {
        for migration in migrations {
            let Some(prev_name) = migration.prev_output.as_deref() else {
                continue;
            };
            let was_active_in_prev = snapshots.iter().any(|snapshot| {
                snapshot.output_name == prev_name && snapshot.active_frame_id == Some(migration.id)
            });
            if let Some(prev_strip) = self.frame_set.mapped_strip_mut(prev_name) {
                // `remove_frame_at` adjusts `active_idx` and seeds the
                // slide-in animation on surviving neighbours; plain
                // `retain` would leave `active_idx` stale.
                if let Some(idx) = prev_strip.index_of(migration.id) {
                    prev_strip.remove_frame_at(idx);
                }
            }
            // Source reconcile skips Hidden for a frame already gone from its
            // strip; freeze it here.
            if was_active_in_prev && self.mapped_output_by_name(prev_name).is_some() {
                self.queue_event(Event::FrameHidden { id: migration.id });
            }
        }
    }

    fn reconcile_entry_snapshots(&mut self, snapshots: &[OutputLayoutSnapshot]) {
        for snapshot in snapshots {
            self.reconcile_output_entries(&snapshot.output_name, &snapshot.entry_surface_ids);
        }
    }

    fn send_frame_migration_output_events(
        &self,
        target_output: &Output,
        migrations: &[FrameMigration],
    ) {
        for migration in migrations {
            let Some(window) = self.id_windows.get(&migration.id) else {
                continue;
            };
            if let Some(prev_name) = migration.prev_output.as_deref()
                && let Some(prev_output) = self.mapped_output_by_name(prev_name)
            {
                self.leave_output_for_window(window, prev_output);
            }
            self.enter_output_for_window(window, target_output);
        }
    }

    fn move_frame_ids_between_outputs(
        &mut self,
        src_name: &str,
        dst_name: &str,
        frame_ids: &[u64],
    ) -> bool {
        if src_name == dst_name || frame_ids.is_empty() {
            return false;
        }

        let frame_ids: HashSet<u64> = frame_ids.iter().copied().collect();
        let pre_src = self.strip_entries(src_name);
        let pre_dst = self.strip_entries(dst_name);
        let prev_src_active = self.active_frame_surface_id(src_name);
        let prev_dst_active = self.active_frame_surface_id(dst_name);

        let moved = self
            .frame_set
            .mapped_strip_mut(src_name)
            .map_or_else(Vec::new, |src_strip| {
                src_strip.drain_frames(|frame| frame_ids.contains(&frame.surface_id))
            });
        if moved.is_empty() {
            return false;
        }

        let moved_ids = moved
            .iter()
            .map(|frame| frame.surface_id)
            .collect::<Vec<_>>();

        let clock = self.animations_clock.clone();
        let scale = self.output_scale(dst_name);
        let dst_strip = self.frame_set.ensure_mapped_strip(dst_name, clock, scale);
        let insert_at = dst_strip.frames().len();
        dst_strip.append_frames(moved);
        if let Some(focused_idx) = moved_ids
            .iter()
            .position(|id| *id == self.focused_frame_id)
            .map(|idx| insert_at + idx)
        {
            dst_strip.set_active(focused_idx);
        } else if insert_at == 0 {
            dst_strip.set_active(0);
        }

        self.reconcile_output_entries(src_name, &pre_src);
        self.reconcile_output_entries(dst_name, &pre_dst);
        let dst_output = self.find_mapped_output(dst_name);
        if let Some(dst_output) = &dst_output {
            let migrations: Vec<FrameMigration> = moved_ids
                .iter()
                .map(|&id| FrameMigration {
                    id,
                    prev_output: Some(src_name.to_string()),
                    previous_frame: None,
                })
                .collect();
            self.send_frame_migration_output_events(dst_output, &migrations);
        }

        self.refresh_primary_assignments();
        if let Some(dst_output) = &dst_output {
            self.update_frames_for_working_area(dst_output);
        }
        self.prune_dangling_focus();
        self.focus_any_frame_if_unfocused("frame_set_move");
        if let Some(id) = prev_src_active
            && frame_ids.contains(&id)
        {
            self.queue_event(Event::FrameHidden { id });
        }
        self.reconcile_active_for_output(src_name, prev_src_active);
        self.reconcile_active_for_output(dst_name, prev_dst_active);
        for id in moved_ids {
            let home_output = self
                .frame_set
                .home_output(id)
                .unwrap_or(dst_name)
                .to_string();
            self.queue_event(Event::FrameMoved {
                id,
                output: dst_name.to_string(),
                home_output,
            });
        }

        true
    }

    fn migrate_output_frames_to_fallback(
        &mut self,
        output_name: &str,
        home_identity: &str,
    ) -> bool {
        let frame_ids = self
            .frame_set
            .mapped_strip(output_name)
            .map(|strip| {
                strip
                    .frames()
                    .iter()
                    .map(|frame| frame.surface_id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.frame_set
            .remember_frame_homes(home_identity, frame_ids.iter().copied());
        let fallback = self
            .sorted_outputs
            .iter()
            .map(Output::name)
            .find(|name| name != output_name);
        fallback.is_some_and(|fallback| {
            self.move_frame_ids_between_outputs(output_name, &fallback, &frame_ids)
        })
    }

    pub(crate) fn restore_home_output_frames(&mut self, output_name: &str) -> bool {
        // Match frames by the reconnecting output's stable identity, not its
        // connector name, so a display that re-enumerated on a different
        // connector still reclaims its frames.
        let Some(home_identity) = self
            .mapped_output_by_name(output_name)
            .map(crate::utils::output_identity)
        else {
            return false;
        };
        let source_moves = self
            .frame_set
            .mapped_strips()
            .iter()
            .filter(|(src_name, _)| src_name.as_str() != output_name)
            .filter_map(|(src_name, strip)| {
                let frame_ids = strip
                    .frames()
                    .iter()
                    .filter(|frame| {
                        self.frame_set
                            .home_output(frame.surface_id)
                            .is_some_and(|home| home == home_identity)
                    })
                    .map(|frame| frame.surface_id)
                    .collect::<Vec<_>>();
                (!frame_ids.is_empty()).then(|| (src_name.to_string(), frame_ids))
            })
            .collect::<Vec<_>>();

        let mut moved = false;
        for (src_name, frame_ids) in source_moves {
            moved |= self.move_frame_ids_between_outputs(&src_name, output_name, &frame_ids);
        }
        moved
    }

    /// Adopt every homeless frame onto a newly mapped output.
    fn adopt_homeless_frames(&mut self, dst_name: &str) -> bool {
        let moved = self.frame_set.take_homeless_frames();
        if moved.is_empty() {
            return false;
        }

        let pre_dst = self.strip_entries(dst_name);
        let prev_dst_active = self.active_frame_surface_id(dst_name);
        let moved_ids = moved
            .iter()
            .map(|frame| frame.surface_id)
            .collect::<Vec<_>>();

        let clock = self.animations_clock.clone();
        let scale = self.output_scale(dst_name);
        let dst_strip = self.frame_set.ensure_mapped_strip(dst_name, clock, scale);
        let insert_at = dst_strip.frames().len();
        dst_strip.append_frames(moved);
        if insert_at == 0 {
            dst_strip.set_active(0);
        }

        self.reconcile_output_entries(dst_name, &pre_dst);
        if let Some(dst_output) = self.find_mapped_output(dst_name) {
            let migrations: Vec<FrameMigration> = moved_ids
                .iter()
                .map(|&id| FrameMigration {
                    id,
                    prev_output: None,
                    previous_frame: None,
                })
                .collect();
            self.send_frame_migration_output_events(&dst_output, &migrations);
            self.update_frames_for_working_area(&dst_output);
        }
        self.refresh_primary_assignments();
        self.reconcile_active_for_output(dst_name, prev_dst_active);
        for id in moved_ids {
            let home_output = self
                .frame_set
                .home_output(id)
                .unwrap_or(dst_name)
                .to_string();
            self.queue_event(Event::FrameMoved {
                id,
                output: dst_name.to_string(),
                home_output,
            });
        }
        true
    }

    /// Activate the output's remembered active frame if it is present again.
    fn restore_last_active_frame(&mut self, output_name: &str) {
        let Some(identity) = self
            .mapped_output_by_name(output_name)
            .map(crate::utils::output_identity)
        else {
            return;
        };
        let Some(frame_id) = self.frame_set.take_last_active(&identity) else {
            return;
        };
        let prev_active_id = self.active_frame_surface_id(output_name);
        let focused_frame_id = self.focused_frame_id;
        let Some(strip) = self.frame_set.mapped_strip_mut(output_name) else {
            return;
        };
        // The focused frame stays active; focus continuity wins.
        if strip
            .frames()
            .iter()
            .any(|frame| frame.surface_id == focused_frame_id)
        {
            return;
        }
        let Some(idx) = strip.index_of(frame_id) else {
            return;
        };
        strip.set_active(idx);
        self.reconcile_active_for_output(output_name, prev_active_id);
    }

    /// Adopt homeless frames and pull homed frames back onto an output that
    /// just became mapped.
    pub(crate) fn attach_frames_for_mapped_output(
        &mut self,
        output: &Output,
        focus_reason: &'static str,
    ) {
        let output_name = output.name();
        let adopted = self.adopt_homeless_frames(&output_name);
        let restored = self.restore_home_output_frames(&output_name);
        if !adopted && !restored {
            self.reconcile_mapped_output_entries(output);
            self.reconcile_active_for_output(&output_name, None);
        }
        self.restore_last_active_frame(&output_name);
        // Frames coming back may hold a surface whose activation waited for them.
        self.focus_pending_activation_surfaces();
        if self.focused_frame_id == 0
            && let Some(id) = self.active_frame_surface_id(&output_name)
        {
            self.set_focus_frame(id, focus_reason, false);
        }
        self.focus_any_frame_if_unfocused(focus_reason);
    }

    /// Follow the focused frame's selected window after a merge, even if only its surface changed.
    fn sync_focus_after_layout_merge(&mut self, output_name: &str, focused_surface: u64) {
        let frame_id = self.focused_frame_id;
        let mut selected = None;

        let Some(strip) = self.frame_set.mapped_strip_mut(output_name) else {
            return;
        };
        if let Some(new_idx) = strip.index_of(frame_id) {
            strip.set_active(new_idx);
            selected = Some(strip.frames()[new_idx].selected_entry_id);
        } else if !strip.frames().is_empty() {
            let clamped = strip.active_idx().min(strip.frames().len() - 1);
            strip.set_active(clamped);
        }

        let Some(new_entry) = selected else {
            return;
        };
        let entry_changed = new_entry != self.focused_entry_id;
        self.set_focus_state(frame_id, new_entry);
        if entry_changed || self.focused_surface_id() != focused_surface {
            self.clear_focused_urgency();
            self.flush_pending_title_event(self.focused_surface_id());
            module::record_focus(self.focused_surface_id(), "layout_selected", None);
        }
    }

    fn sync_focus_after_floating_layout_merge(&mut self, output_name: &str, focused_surface: u64) {
        let frame_id = self.focused_frame_id;
        let mut selected = None;

        let Some(space) = self.floating_spaces.get_mut(output_name) else {
            return;
        };
        if let Some(idx) = space.idx_of(frame_id) {
            selected = Some(space.frames()[idx].selected_entry_id);
            space.activate_frame(frame_id);
        }

        let Some(new_entry) = selected else {
            return;
        };
        let entry_changed = new_entry != self.focused_entry_id;
        self.set_focus_state(frame_id, new_entry);
        if entry_changed || self.focused_surface_id() != focused_surface {
            self.clear_focused_urgency();
            self.flush_pending_title_event(self.focused_surface_id());
            module::record_focus(self.focused_surface_id(), "floating_layout_selected", None);
        }
    }

    /// Apply a declarative per-output layout.
    ///
    /// Replaces the previous layout for the given output. Surfaces removed from
    /// this output get `wl_surface.leave`; new surfaces get `wl_surface.enter`
    /// and scale notification.
    pub fn apply_output_layout(&mut self, output_name: &str, mut frames: Vec<strip::Frame>) {
        // 1. Find the output
        let output = match self.find_mapped_output(output_name) {
            Some(o) => o,
            None => {
                warn!("apply_output_layout: output '{}' not found", output_name);
                return;
            }
        };
        let current = self
            .frame_set
            .mapped_strip(output_name)
            .map(strip::Strip::frames);
        let moved = strip::moved_entry_ids(current.unwrap_or_default(), &frames);

        // Only frames seeing their first home need the identity string; skip
        // building it in the steady state where every frame already has one.
        if frames
            .iter()
            .any(|f| f.surface_id != 0 && self.frame_set.home_output(f.surface_id).is_none())
        {
            let home_identity = crate::utils::output_identity(&output);
            self.frame_set
                .remember_frame_homes(&home_identity, frames.iter().map(|f| f.surface_id));
        }
        let migrations = self.collect_frame_migrations(output_name, &frames);
        for frame in &mut frames {
            frame.clear_compositor_view_state();
        }
        Self::preserve_migrated_frame_state(&mut frames, &migrations);
        // Capture affected outputs before the swap; reconciliation runs after
        // the merge against the post-merge strips so kept frames don't leak
        // `surface_outputs`, and active-frame event balance has a stable base.
        let snapshots = self.output_layout_snapshots(output_name, &migrations);
        self.remove_migrated_frames_from_sources(&migrations, &snapshots);

        // Omitted frames keep their slot until Wayland destroy arrives, but
        // their layout entries are cleared by `merge_layout_frames`.
        let clock = self.animations_clock.clone();
        let focused_surface = self.focused_surface_id();
        let scale = self.output_scale(output_name);
        let strip_entry = self
            .frame_set
            .ensure_mapped_strip(output_name, clock, scale);
        strip_entry.merge_layout_frames(frames);
        self.sync_focus_after_layout_merge(output_name, focused_surface);

        self.reconcile_entry_snapshots(&snapshots);
        self.send_frame_migration_output_events(&output, &migrations);

        self.refresh_primary_assignments_after_layout(&moved);
        self.update_frames_for_working_area(&output);
        self.prune_dangling_focus();
        // Covers the race where `apply_output_layout` runs before
        // `new_toplevel`'s auto-focus.
        self.focus_any_frame_if_unfocused("auto_focus_on_layout");
        self.focus_pending_activation_surfaces();

        debug!(
            "apply_output_layout: output '{}' with {} surface entries",
            output_name,
            self.strip_entries(output_name).len(),
        );

        for snapshot in snapshots {
            self.reconcile_active_for_output(&snapshot.output_name, snapshot.active_frame_id);
        }
    }

    /// Apply floating frame contents for an output; Rust keeps positions and stacking.
    pub fn apply_floating_layout(&mut self, output_name: &str, frames: Vec<strip::Frame>) {
        let pre = self.strip_entries(output_name);
        let focused_surface = self.focused_surface_id();
        let current = self
            .floating_spaces
            .get(output_name)
            .map(floating::FloatingSpace::frames);
        let moved = strip::moved_entry_ids(current.unwrap_or_default(), &frames);
        let Some(space) = self.ensure_floating_space_for_output(output_name) else {
            warn!("apply_floating_layout: output '{}' not found", output_name);
            return;
        };
        space.merge_layout_frames(frames);
        self.sync_focus_after_floating_layout_merge(output_name, focused_surface);
        let post = self.strip_entries(output_name);
        let output = self.find_mapped_output(output_name);
        self.reconcile_output_membership(output_name, output.as_ref(), &pre, &post);
        self.refresh_primary_assignments_after_layout(&moved);
        self.prune_dangling_focus();
        self.focus_pending_activation_surfaces();
        if let Some(output) = output {
            self.queue_redraw(&output);
        }
    }

    /// Run `f` on the floating space owning `id`, redrawing on change; false if none.
    fn with_floating_space(
        &mut self,
        id: u64,
        f: impl FnOnce(&mut floating::FloatingSpace) -> bool,
    ) -> bool {
        let Some(output_name) = self.emacs_frame_output(id).map(str::to_string) else {
            return false;
        };
        let Some(space) = self.floating_spaces.get_mut(&output_name) else {
            return false;
        };
        let changed = f(space);
        if changed && let Some(output) = self.find_mapped_output(&output_name) {
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
        let Some(output_name) = self.emacs_frame_output(id).map(str::to_string) else {
            return false;
        };
        let Some(space) = self.floating_spaces.get_mut(&output_name) else {
            return false;
        };
        let Some(size) = f(space) else {
            return false;
        };
        self.configure_floating_frame_size(id, size);
        if let Some(output) = self.find_mapped_output(&output_name) {
            self.queue_redraw(&output);
        }
        true
    }

    pub fn resize_floating_frame(&mut self, id: u64, dw: f64, dh: f64) -> bool {
        self.with_floating_resize(id, |space| space.resize_frame_by(id, Size::from((dw, dh))))
    }

    fn configure_floating_frame_size(&self, id: u64, size: Size<i32, Logical>) {
        let Some(window) = self.id_windows.get(&id) else {
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

    pub fn interactive_move_floating_frame_begin(&mut self, id: u64) -> bool {
        self.with_floating_space(id, |space| space.interactive_move_begin(id))
    }

    pub fn interactive_move_floating_frame_update(
        &mut self,
        id: u64,
        delta: Point<f64, Logical>,
    ) -> bool {
        self.with_floating_space(id, |space| space.interactive_move_update(id, delta))
    }

    pub fn interactive_move_floating_frame_end(&mut self, id: u64) {
        self.with_floating_space(id, |space| {
            space.interactive_move_end(Some(id));
            true
        });
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
        self.refresh_primary_assignments_after_layout(&HashSet::new());
    }

    /// Like `refresh_primary_assignments`, after a layout that `moved` entries.
    fn refresh_primary_assignments_after_layout(&mut self, moved: &HashSet<LayoutEntryId>) {
        let output_sizes = self.output_sizes_by_name();
        let best_area = self.primary_entry_areas(&output_sizes);
        self.assign_primary_entries(&output_sizes, &best_area);
        self.refresh_mapped_cast_outputs();
        for (id, size, serial) in self.configure_primary_entries() {
            if let Some(pending) = self.pending_resize(id, size, moved)
                && let Some(surface) = self.surfaces.get_mut(&id)
            {
                surface.pending_resizes.push((serial, pending));
            }
        }
    }

    /// Configure every primary entry, returning the configures that changed a size.
    fn configure_primary_entries(&self) -> Vec<(u64, Size<i32, Logical>, Serial)> {
        let mut resized = Vec::new();
        for output in &self.sorted_outputs {
            let Some(strip) = self.frame_set.mapped_strip(&output.name()) else {
                continue;
            };
            let active_idx = strip.active_idx();
            let Some(frame) = strip.frames().get(active_idx) else {
                continue;
            };
            self.configure_frame_primaries(output, frame, &mut resized);
        }
        for output in &self.sorted_outputs {
            let oname = output.name();
            let Some(space) = self.floating_spaces.get(&oname) else {
                continue;
            };
            for frame in space.frames() {
                self.configure_frame_primaries(output, frame, &mut resized);
            }
        }
        resized
    }

    fn configure_frame_primaries(
        &self,
        output: &Output,
        frame: &strip::Frame,
        resized: &mut Vec<(u64, Size<i32, Logical>, Serial)>,
    ) {
        for entry in frame.entries.iter().filter(|entry| entry.primary()) {
            if let Some(id) = entry.surface_id()
                && let Some((size, serial)) =
                    self.configure_primary(output, frame.entry_fullscreen(entry.id), entry)
            {
                resized.push((id, size, serial));
            }
        }
    }

    /// Configure `entry`'s surface, returning the size and serial when the size changed.
    fn configure_primary(
        &self,
        output: &Output,
        fullscreen: bool,
        entry: &LayoutEntry,
    ) -> Option<(Size<i32, Logical>, Serial)> {
        let surface_id = entry.surface_id()?;
        let window = self.id_windows.get(&surface_id)?;
        window.with_surfaces(|s, data| {
            crate::utils::send_scale_transform(
                s,
                data,
                output.current_scale(),
                output.current_transform(),
            );
        });
        let toplevel = window.toplevel()?;
        let new_size = if fullscreen {
            let s = crate::utils::output_size(output);
            (s.w as i32, s.h as i32).into()
        } else {
            entry.rect().size
        };
        let (changed, size_changed) = toplevel.with_pending_state(|state| {
            let was_fullscreen = state.states.contains(XdgToplevelState::Fullscreen);
            let was_maximized = state.states.contains(XdgToplevelState::Maximized);
            let size_changed = state.size != Some(new_size);
            let changed =
                size_changed || was_fullscreen != fullscreen || was_maximized == fullscreen;
            state.size = Some(new_size);
            if fullscreen {
                state.states.set(XdgToplevelState::Fullscreen);
                state.states.unset(XdgToplevelState::Maximized);
            } else {
                state.states.set(XdgToplevelState::Maximized);
                state.states.unset(XdgToplevelState::Fullscreen);
            }
            (changed, size_changed)
        });
        if !changed {
            return None;
        }
        let serial = toplevel.send_configure();
        size_changed.then_some((new_size, serial))
    }

    fn output_sizes_by_name(&self) -> HashMap<String, (u64, u64)> {
        self.space
            .outputs()
            .map(|o| {
                let size = crate::utils::output_size(o);
                (o.name(), (size.w as u64, size.h as u64))
            })
            .collect()
    }

    /// Fullscreen entries claim the output's full size, not entry.w/h.
    fn layout_entry_area(
        output_sizes: &HashMap<String, (u64, u64)>,
        output_name: &str,
        entry: &LayoutEntry,
        fullscreen: bool,
    ) -> u64 {
        if fullscreen {
            let (w, h) = output_sizes
                .get(output_name)
                .copied()
                .unwrap_or((entry.w as u64, entry.h as u64));
            w * h
        } else {
            entry.w as u64 * entry.h as u64
        }
    }

    pub(crate) fn primary_entry_areas(
        &self,
        output_sizes: &HashMap<String, (u64, u64)>,
    ) -> HashMap<u64, u64> {
        let mut best_area: HashMap<u64, u64> = HashMap::new();
        for (oname, strip) in self.mapped_output_strips() {
            let active_idx = strip.active_idx();
            let Some(frame) = strip.frames().get(active_idx) else {
                continue;
            };
            for entry in frame.entries.iter().filter(|entry| entry.surface.is_some()) {
                let fullscreen = frame.entry_fullscreen(entry.id);
                let area = Self::layout_entry_area(output_sizes, oname, entry, fullscreen);
                let Some(surface_id) = entry.surface_id() else {
                    continue;
                };
                best_area
                    .entry(surface_id)
                    .and_modify(|a| *a = (*a).max(area))
                    .or_insert(area);
            }
        }
        for (oname, space) in &self.floating_spaces {
            for frame in space.frames() {
                for entry in frame.entries.iter().filter(|entry| entry.surface.is_some()) {
                    let area = Self::layout_entry_area(output_sizes, oname, entry, false);
                    let Some(surface_id) = entry.surface_id() else {
                        continue;
                    };
                    best_area
                        .entry(surface_id)
                        .and_modify(|a| *a = (*a).max(area))
                        .or_insert(area);
                }
            }
        }
        best_area
    }

    pub(crate) fn assign_primary_entries(
        &mut self,
        output_sizes: &HashMap<String, (u64, u64)>,
        best_area: &HashMap<u64, u64>,
    ) {
        for frame in self.frame_set.all_frames_mut() {
            for entry in frame
                .entries
                .iter_mut()
                .filter(|entry| entry.surface.is_some())
            {
                entry.set_primary(false);
            }
        }
        for space in self.floating_spaces.values_mut() {
            for entry in space.surface_entries_mut() {
                entry.set_primary(false);
            }
        }

        let mut assigned: HashSet<u64> = HashSet::new();
        for output in &self.sorted_outputs {
            let oname = output.name();
            if let Some(strip) = self.frame_set.mapped_strip_mut(&oname) {
                let active_idx = strip.active_idx();
                let Some(frame) = strip.frames_mut().get_mut(active_idx) else {
                    continue;
                };
                let fullscreen_entry_id = frame.fullscreen.map(|fullscreen| fullscreen.entry_id);
                for entry in frame
                    .entries
                    .iter_mut()
                    .filter(|entry| entry.surface.is_some())
                {
                    let Some(surface_id) = entry.surface_id() else {
                        continue;
                    };
                    let dominated = best_area.get(&surface_id).copied().unwrap_or(0);
                    let fullscreen = fullscreen_entry_id == Some(entry.id);
                    if Self::layout_entry_area(output_sizes, &oname, entry, fullscreen) == dominated
                        && assigned.insert(surface_id)
                    {
                        entry.set_primary(true);
                    }
                }
            }
            if let Some(space) = self.floating_spaces.get_mut(&oname) {
                for frame in space.frames_mut() {
                    for entry in frame
                        .entries
                        .iter_mut()
                        .filter(|entry| entry.surface.is_some())
                    {
                        let Some(surface_id) = entry.surface_id() else {
                            continue;
                        };
                        let dominated = best_area.get(&surface_id).copied().unwrap_or(0);
                        if Self::layout_entry_area(output_sizes, &oname, entry, false) == dominated
                            && assigned.insert(surface_id)
                        {
                            entry.set_primary(true);
                        }
                    }
                }
            }
        }
    }

    /// Emit FrameShown/FrameHidden when an output's strip-active changes.
    /// Caller snapshots `prev_active_id` before the mutation; the helper
    /// compares against the post-mutation state.  No stored state.
    pub(crate) fn reconcile_active_for_output(
        &mut self,
        output_name: &str,
        prev_active_id: Option<u64>,
    ) {
        let Some(strip) = self.frame_set.mapped_strip(output_name) else {
            return;
        };
        let new_active_id = strip.active_frame().map(|f| f.surface_id);
        if prev_active_id == new_active_id {
            return;
        }
        if let Some(id) = prev_active_id {
            // Skip if the frame was destroyed -- the lisp side already handled it.
            // The overview keeps every frame visible.
            if !self.overview.is_active() && strip.frames().iter().any(|f| f.surface_id == id) {
                self.queue_event(Event::FrameHidden { id });
            }
        }
        if let Some(id) = new_active_id {
            self.queue_event(Event::FrameShown { id });
        }
    }

    /// True when the strip on the output under `pos` is mid-animation.
    pub fn is_strip_animating_at(&self, pos: Point<f64, Logical>) -> bool {
        let Some(output) = self.output_containing(pos) else {
            return false;
        };
        self.frame_set
            .mapped_strip(&output.name())
            .is_some_and(|s| s.animations_ongoing())
    }
}

#[cfg(test)]
mod tests;
