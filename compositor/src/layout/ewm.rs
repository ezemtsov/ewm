//! The compositor side of the layout: moves, hotplug, overview, fades and what Emacs is told.

use std::collections::{HashMap, HashSet};
use std::mem;
use std::time::Duration;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::utils::under_from_surface_tree;
use smithay::desktop::{Window, WindowSurfaceType, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::seat::WaylandFocus;

use super::overview::{Geometry, Overview};
use super::resize_fade::{ResizeFade, ViewRect, fade_animation, resize_fades};
use super::snapshot::Snapshot;
use super::transition::Transition;
use super::{
    ActiveChange, FrameLocation, LayoutEntryLocation, Monitor, Moved, entry_size, floating, strip,
};
use crate::input::{Direction, End, Gesture};
use crate::render::render_view_snapshot;
use crate::surface::view_box;
use crate::{
    Event, Ewm, FocusSource, LayoutEntry, LayoutEntryHit, LayoutEntryId, Notify, State, SurfaceHit,
    module,
};

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
        if let Some(change) = &removed {
            self.reconcile_output_entries(&change.output);
            self.reconcile_active_for_output(&change.output, change.prev_active);
            self.refresh_primary_assignments();
        }
        removed.map(|change| change.output)
    }

    /// Move `output`'s frames to the next output, else park them, and withdraw it from clients.
    pub(crate) fn detach_output(&mut self, output: &Output) {
        let fallback = self.output_next_of(output).filter(|next| next != output);
        let moved = self.layout.detach_output(output, fallback.as_ref());
        self.settle_moved_frames(&moved);
        for id in moved.retired {
            self.queue_event(Event::Close { id });
        }
        self.space.unmap_output(output);
        let mut layers = layer_map_for_output(output);
        for layer in layers.layers().cloned().collect::<Vec<_>>() {
            layer.layer_surface().send_close();
            layers.unmap_layer(&layer);
        }
        drop(layers);
        self.gamma_control_state.output_removed(output);
        self.screencopy_state.remove_output(output);
        #[cfg(feature = "screencast")]
        self.stop_output_casts(output);
        self.output_state.get_mut(output).unwrap().lock_surface = None;
        self.maybe_continue_to_locking();
        self.retire_output_global(output);
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
            .frame_location(self.focused_frame_id()?)?
            .entries()
            .find(|location| location.entry.surface_id() == Some(id))
    }

    /// Follow a layout change of some output's active frame, `shown` its output and old active.
    pub(crate) fn settle_shown(&mut self, shown: Option<ActiveChange>) {
        let Some(ActiveChange {
            output,
            prev_active,
        }) = shown
        else {
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
        let output_geo = self.layout.output_rect(output)?;
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
        let output_geo = location.monitor.rect()?;
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
        let surface = entry.surface_id().and_then(|id| self.surfaces.get(&id));
        let slot = Self::fullscreen_slot(location, output_geo).to_f64();
        view_box(surface, entry, origin.to_f64(), slot).to_i32_round()
    }

    /// The output-sized slot of `location`'s frame.
    fn fullscreen_slot(
        location: LayoutEntryLocation<'_>,
        output_geo: Rectangle<i32, Logical>,
    ) -> Rectangle<i32, Logical> {
        let dx = location.frame_location().slot_x().round() as i32;
        Rectangle::new(output_geo.loc + Point::from((dx, 0)), output_geo.size)
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
        let output_geo = self.layout.output_rect(output)?;
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
            for location in location.entries() {
                let entry = location.entry;
                if !self.view_visible(location) || !predicate(entry) {
                    continue;
                }
                let rect = self.entry_rect_at(location, output_geo, slot.loc);
                if Self::rect_contains_f64(rect, pos) {
                    return Some((entry, rect));
                }
            }
            if Self::rect_contains_f64(slot, pos) {
                return None;
            }
        }
        self.tiled_entry_under_matching(monitor, pos, predicate)
    }

    /// Hit-test `pos` against a monitor's tiled entries only (no floating overlay).
    fn tiled_entry_under_matching<'a>(
        &self,
        monitor: &'a Monitor,
        pos: Point<f64, Logical>,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryHit<'a>> {
        monitor
            .entry_locations_in(false)
            .filter(|location| predicate(location.entry))
            // Primary clicks miss in the letterbox.
            .filter(|location| self.view_visible(*location))
            .find_map(|location| {
                let rect = self.drawn_rect(location)?;
                Self::rect_contains_f64(rect, pos).then_some((location.entry, rect))
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
        let output_geo = self.layout.output_rect(monitor.output())?;
        let center = output_geo.loc.to_f64() + center;
        self.tiled_entry_under_matching(monitor, center, |_| true)
            .map(|(entry, _)| entry.id)
    }

    /// Hit-test layout toplevel/subsurfaces under a global position. Falls
    /// back to the frame's own surface when no entry hits. Popups are handled
    /// separately by `popup_surface_under`, so popup placement and z-order stay
    /// identical to rendering.
    pub fn layout_surface_under(&self, pos: Point<f64, Logical>) -> Option<SurfaceHit> {
        let (monitor, output_geo) = self.monitor_at(pos)?;
        let working_area = self.get_working_area(monitor.output());
        self.layout_entry_in(monitor, output_geo, pos, |entry| {
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
        let (entry, rect) = hit;
        let surface = self.surfaces.get(&entry.surface_id()?)?;
        let (_, scale) = surface.contents_rect(entry, rect.size, rect.loc.to_f64())?;
        let pos_in_window = (pos - rect.loc.to_f64()).downscale(scale);
        Self::surface_under_non_popup_window(&surface.window, pos_in_window, pos - pos_in_window)
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
        let output_geo = self.layout.output_rect(output)?;
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

    /// The views of surface `id` as drawn.
    pub fn views_of(&self, id: u64) -> Vec<ViewRect> {
        self.view_rects()
            .into_iter()
            .filter(|view| view.surface == id)
            .collect()
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
                let mut window = self.drawn_rect(location)?;
                window.loc -= location.monitor.rect()?.loc;
                let (rect, scale) = state.contents_rect(entry, window.size, window.loc.to_f64())?;
                Some(ViewRect {
                    surface,
                    entry: entry.id,
                    output: location.monitor.output().clone(),
                    rect: rect.to_i32_round(),
                    scale,
                    native: state.presents(entry),
                    window,
                    settles: location.monitor.settles_at_once(location.frame.surface_id),
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
        if let Some((_, entry_id)) = self
            .focused_frame_id()
            .and_then(|id| self.layout.fullscreen_float_over(id))
            && self.focused_entry_id() != Some(entry_id)
        {
            self.set_focus_entry(entry_id, FocusSource::FullscreenOver, Notify::Emacs);
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
            .is_none_or(|id| {
                id == frame.surface_id || Some(frame.surface_id) == self.focused_frame_id()
            })
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
        self.layout_entry_in(monitor, output_geo, pos, |_| true)
            .map(|(entry, _)| entry.id)
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
                moved.ids.to_vec()
            } else {
                Vec::new()
            };
            for id in hidden {
                self.queue_event(Event::FrameHidden { id });
            }
            self.reconcile_active_for_output(src, moved.src_prev_active);
        }
        if let Some(dst) = &moved.dst {
            // Frames back from parking under the overview show with the rest of their strip.
            if moved.src.is_none() && self.overview.is_active() {
                let active = self.layout.active_frame_id(dst);
                for id in moved.ids.iter().copied().filter(|id| Some(*id) != active) {
                    self.queue_event(Event::FrameShown { id });
                }
            }
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
    fn sync_focus_after_merge(&mut self, before: (Option<u64>, Option<LayoutEntryId>)) {
        let after = (self.focused_surface_id(), self.focused_entry_id());
        if after != before {
            self.clear_focused_urgency();
            if let Some(id) = after.0 {
                self.flush_pending_title_event(id);
                module::record_focus(id, FocusSource::LayoutSelected);
            }
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

    /// Run `f` on the floating space owning `id`, redrawing on change. False if none.
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
        let geo = self.layout.output_rect(output)?;
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

    /// Pick each surface's primary entry (largest active-frame slot) and configure its toplevel.
    /// Off-screen frames don't bid, nor does fullscreen the surface no longer asks for.
    pub(crate) fn refresh_primary_assignments(&mut self) {
        let surfaces = &self.surfaces;
        let requested = |id| {
            surfaces
                .get(&id)
                .is_some_and(|surface| surface.pending_fullscreen)
        };
        self.layout
            .assign_primaries(&self.sorted_outputs, requested);
        self.refresh_mapped_cast_outputs();
        self.configure_primary_entries();
    }

    /// The size and fullscreen state surface `id`, primary at `location`, is configured for.
    fn primary_configure(
        &self,
        id: u64,
        location: LayoutEntryLocation<'_>,
    ) -> (Size<i32, Logical>, bool) {
        let fullscreen = self.pending_fullscreen(id);
        (
            entry_size(location.monitor.output(), location.entry, fullscreen),
            fullscreen,
        )
    }

    /// Configure every shown primary entry's surface for its slot, fading the answer to a resize.
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
            let (size, fullscreen) = self.primary_configure(id, location);
            let fades = !location.monitor.settles_at_once(location.frame.surface_id);
            if let Some(surface) = self.surfaces.get_mut(&id) {
                surface.configure(output, size, fullscreen, fades);
                surface.present(location.entry.id);
            }
        }
    }

    /// Present the primary of surface `id` once its client has committed the last configure.
    pub(crate) fn sync_presented(&mut self, id: u64) {
        let primary = self
            .layout_entry_for_surface(id, LayoutEntry::primary)
            .map(|location| location.entry.id);
        if let (Some(primary), Some(surface)) = (primary, self.surfaces.get_mut(&id)) {
            surface.present(primary);
        }
    }

    /// Panic unless the layout holds and the surfaces follow it.
    pub fn verify_invariants(&self) {
        self.layout.verify_invariants();
        let mut primaries: HashMap<u64, usize> = HashMap::new();
        for location in self.layout.entry_locations() {
            let entry = location.entry;
            let Some(id) = entry.surface_id() else {
                continue;
            };
            if location.shown() {
                *primaries.entry(id).or_default() += usize::from(entry.primary());
            } else {
                assert!(
                    !entry.primary(),
                    "view {} of surface {id} is primary off screen",
                    entry.id
                );
            }
            let Some(surface) = self.surfaces.get(&id) else {
                continue;
            };
            assert!(
                !entry.fullscreen() || surface.fullscreen,
                "view {} shows surface {id} fullscreen before it committed",
                entry.id
            );
            assert!(
                location.frame.fullscreen_entry().is_some() || !surface.fullscreen,
                "frame {} marks no view of its committed fullscreen surface {id}",
                location.frame.surface_id
            );
            if entry.primary() {
                assert_eq!(
                    surface.configured(),
                    Some(self.primary_configure(id, location)),
                    "surface {id} is not configured for its primary view {}",
                    entry.id
                );
                assert!(
                    !surface.synced() || surface.presents(entry),
                    "surface {id} caught up yet presents another view than its primary {}",
                    entry.id
                );
            }
        }
        for (id, count) in primaries {
            assert_eq!(count, 1, "primary views of surface {id} on screen");
        }
    }

    /// Tell Emacs when `output`'s active strip frame differs from `prev_active_id`.
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

impl Ewm {
    /// The strip under the pointer and the working width a swipe is measured against.
    fn swiped_strip(&self) -> Option<(Output, f64)> {
        let output = self.output_under_cursor().cloned()?;
        let working_w = self.get_working_area(&output).size.w as f64;
        Some((output, working_w))
    }

    /// Start `gesture`, returning the direction whose travel advances it, none with nothing to drive.
    pub fn gesture_begin(&mut self, gesture: Gesture) -> Option<Direction> {
        match gesture {
            Gesture::Strip { forward } => {
                // A hidden strip would scroll unseen.
                if self.frames_hidden() {
                    return None;
                }
                let (output, _) = self.swiped_strip()?;
                self.layout.strip_mut(&output)?.gesture_begin();
                Some(forward)
            }
            Gesture::Overview { direction, end } => {
                Some(self.overview_gesture_begin(direction, end))
            }
            Gesture::Hide { direction, end } => Some(self.hide_gesture_begin(direction, end)),
        }
    }

    /// Push `travel` along the direction `gesture_begin` returned.
    pub fn gesture_update(&mut self, gesture: Gesture, travel: f64, timestamp: Duration) {
        let changed = match gesture {
            Gesture::Strip { .. } => {
                let Some((output, working_w)) = self.swiped_strip() else {
                    return;
                };
                // Zoomed-out content still follows the fingers on screen.
                let travel = travel / self.overview.zoom();
                if let Some(strip) = self.layout.strip_mut(&output) {
                    strip.gesture_update(travel, timestamp, working_w);
                }
                self.queue_redraw(&output);
                return;
            }
            Gesture::Overview { .. } => self.overview.gesture_update(travel, timestamp),
            Gesture::Hide { .. } => self.hide.gesture_update(travel, timestamp),
        };
        if changed == Some(true) {
            self.queue_redraw_all();
        }
    }

    /// Settle `gesture` where the fingers left it, true when the scene under the pointer changed.
    pub fn gesture_end(&mut self, gesture: Gesture) -> bool {
        let changed = match gesture {
            Gesture::Strip { .. } => {
                if let Some((output, working_w)) = self.swiped_strip() {
                    self.strip_gesture_end(&output, working_w);
                }
                return false;
            }
            Gesture::Overview { .. } => self.overview_gesture_end(),
            Gesture::Hide { .. } => self.hide_gesture_end(),
        };
        if changed {
            self.queue_redraw_all();
        }
        changed
    }

    /// End a strip swipe on `output`: snap to the nearest frame, the keyboard following unless a
    /// float keeps it on screen.
    pub fn strip_gesture_end(&mut self, output: &Output, working_w: f64) {
        let before = self.focused_frame_id();
        let shown = self.layout.strip_gesture_end(output, working_w);
        self.settle_shown(shown);
        // The swiped output takes the keyboard on the frame holding it there, unless it has it.
        let target = self
            .layout
            .focused_frame_on(output)
            .filter(|&id| Some(id) != before);
        if let Some(id) = target {
            if self.pointer_is_on_output(output) {
                // The frame settles flush with the working area.
                let output_loc = self
                    .layout
                    .output_rect(output)
                    .map_or(Point::default(), |geo| geo.loc);
                let (px, py) = self.pointer_location();
                let in_content = self
                    .overview_geometry(output)
                    .to_content(Point::from((px, py)) - output_loc.to_f64());
                let pos_in_frame = in_content - self.get_working_area(output).loc.to_f64();
                self.set_focus_frame(id, FocusSource::Gesture, Notify::Pointer(pos_in_frame));
            } else {
                self.set_focus_frame(id, FocusSource::Gesture, Notify::Emacs);
            }
        }
    }
}

impl Ewm {
    /// Where `output`'s content lands on screen under the overview zoom.
    pub fn overview_geometry(&self, output: &Output) -> Geometry {
        let Some(progress) = self.overview.progress() else {
            return Geometry::default();
        };
        let size = self
            .layout
            .output_rect(output)
            .map(|geo| geo.size.to_f64())
            .unwrap_or_default();
        let scale = output.current_scale().fractional_scale();
        let shift = self
            .layout
            .strip(output)
            .map_or(0.0, strip::Strip::overview_shift)
            * progress;
        Geometry::new(size, scale, self.overview.zoom(), shift)
    }

    /// Strip frame under an output-local content point, with the point's
    /// frame-relative position.
    fn overview_frame_under(
        &self,
        output: &Output,
        geometry: &Geometry,
        in_content: Point<f64, Logical>,
    ) -> Option<(usize, Point<f64, Logical>)> {
        let working_area = self.get_working_area(output);
        let in_strip = in_content - working_area.loc.to_f64();
        let strip = self.layout.strip(output)?;
        strip
            .frames_with_render_geo(
                working_area.size.to_f64(),
                geometry.strip_viewport(working_area),
            )
            .find(|(_, _, rect, _)| rect.contains(in_strip))
            .map(|(idx, _, rect, _)| (idx, in_strip - rect.loc))
    }

    /// Click in the open overview: a frame under `pos` becomes active, and a
    /// click anywhere on the zoomed output closes the overview.
    pub fn overview_click(&mut self, pos: Point<f64, Logical>) {
        let Some(output) = self.output_at(pos).cloned() else {
            return;
        };
        let Some(output_geo) = self.layout.output_rect(&output) else {
            return;
        };
        let pos_within_output = pos - output_geo.loc.to_f64();
        let geometry = self.overview_geometry(&output);
        let in_output = geometry.to_content(pos_within_output);
        // Hidden frames have slid up, so the click maps onto where they were.
        let slide = self.hide.value() * f64::from(output_geo.size.h);
        let in_content = in_output + Point::from((0., slide));
        let floating_frame = self.layout.monitor_of(&output).and_then(|monitor| {
            monitor
                .floats_with_render_geo()
                .find(|(_, rect)| rect.contains(in_content))
                .map(|(frame, rect)| (frame.surface_id, rect))
        });
        if let Some((frame_id, rect)) = floating_frame {
            self.set_focus_frame(
                frame_id,
                FocusSource::Overview,
                Notify::Pointer(in_content - rect.loc),
            );
            self.overview_close();
        } else if let Some((idx, pos_in_frame)) =
            self.overview_frame_under(&output, &geometry, in_content)
        {
            self.overview_activate_frame(&output, idx, pos_in_frame);
        } else if Rectangle::from_size(output_geo.size.to_f64()).contains(in_output) {
            self.overview_close();
        }
    }

    /// Activate frame `idx` and close. The strip snaps onto it and the zoom carries the slide.
    fn overview_activate_frame(
        &mut self,
        output: &Output,
        idx: usize,
        pos_in_frame: Point<f64, Logical>,
    ) {
        self.bake_overview_shifts();
        let Some(strip) = self.layout.strip(output) else {
            return;
        };
        let Some(target) = strip.frames().get(idx).map(|frame| frame.surface_id) else {
            return;
        };
        let prev_view_pos = strip.view_pos();
        let zoom = self.overview.zoom();
        let progress = self.overview.progress().unwrap_or(0.0);
        self.set_focus_frame(target, FocusSource::Overview, Notify::Pointer(pos_in_frame));
        if progress > 0.01
            && let Some(strip) = self.layout.strip_mut(output)
        {
            strip.carry_slide_as_overview_shift(prev_view_pos, zoom, progress);
        }
        self.with_overview(|overview| overview.close());
        self.queue_redraw_all();
    }

    /// Change the overview and tell Emacs when it opens or closes.
    fn with_overview<R>(&mut self, change: impl FnOnce(&mut Overview) -> R) -> R {
        let was_open = self.overview.is_open();
        let result = change(&mut self.overview);
        let open = self.overview.is_open();
        if open != was_open {
            self.queue_event(Event::Overview { open });
        }
        result
    }

    pub fn overview_gesture_end(&mut self) -> bool {
        self.with_overview(|overview| overview.gesture_end())
    }

    /// Turn leftover overview shifts back into strip slides before the zoom changes course.
    fn bake_overview_shifts(&mut self) {
        let progress = self.overview.progress();
        let zoom = self.overview.zoom();
        for strip in self.layout.strips_mut() {
            strip.bake_overview_shift(progress, zoom);
        }
    }

    /// Run overview `action`.
    pub(crate) fn overview(&mut self, action: module::OverviewAction) {
        match action {
            module::OverviewAction::Toggle => self.overview_toggle(),
            module::OverviewAction::Open => {
                self.overview_open();
            }
            module::OverviewAction::Close => {
                self.overview_close();
            }
        }
    }

    pub fn overview_toggle(&mut self) {
        self.bake_overview_shifts();
        let was_active = self.overview.is_active();
        self.with_overview(|overview| overview.toggle());
        if !was_active {
            self.set_inactive_frames_shown(true);
        }
        self.queue_redraw_all();
    }

    /// Thaw every frame while the overview shows them all, or refreeze the inactive ones.
    pub(crate) fn set_inactive_frames_shown(&mut self, shown: bool) {
        let ids: Vec<u64> = self.layout.inactive_frame_ids().collect();
        for id in ids {
            self.queue_event(if shown {
                Event::FrameShown { id }
            } else {
                Event::FrameHidden { id }
            });
        }
    }

    pub fn overview_open(&mut self) -> bool {
        if self.overview.is_open() {
            return false;
        }
        self.overview_toggle();
        true
    }

    pub fn overview_close(&mut self) -> bool {
        if !self.overview.is_open() {
            return false;
        }
        self.overview_toggle();
        true
    }

    /// Start an overview swipe toward `end`, the fingers moving `direction` heading there, and
    /// return the direction whose travel opens.
    pub fn overview_gesture_begin(&mut self, direction: Direction, end: End) -> Direction {
        let forward = end.forward(self.overview.is_open(), direction);
        self.bake_overview_shifts();
        let was_active = self.overview.is_active();
        self.with_overview(|overview| overview.gesture_begin());
        if !was_active {
            self.set_inactive_frames_shown(true);
        }
        self.queue_redraw_all();
        forward
    }

    pub fn frames_hidden(&self) -> bool {
        self.hide.is_on()
    }

    /// Neither the overview nor the hide has moved the frames.
    pub fn frames_in_place(&self) -> bool {
        !self.overview.is_active() && !self.hide.is_active()
    }

    /// Run `change` on the hide, telling Emacs when the frames flip between hidden and shown.
    fn with_hide<R>(&mut self, change: impl FnOnce(&mut Transition) -> R) -> R {
        let was_hidden = self.hide.is_on();
        let result = change(&mut self.hide);
        let hidden = self.hide.is_on();
        if hidden != was_hidden {
            self.queue_event(Event::Hide { hidden });
        }
        result
    }

    /// Slide every frame off its output or bring them back, and tell Emacs.
    pub fn hide_toggle(&mut self) {
        self.with_hide(|hide| hide.toggle());
        self.queue_redraw_all();
    }

    /// Start a hide swipe toward `end`, the fingers moving `direction` heading there, and return
    /// the direction whose travel hides.
    pub fn hide_gesture_begin(&mut self, direction: Direction, end: End) -> Direction {
        let forward = end.forward(self.hide.is_on(), direction);
        self.with_hide(|hide| hide.gesture_begin());
        self.queue_redraw_all();
        forward
    }

    /// Settle the frames hidden or shown, whichever the projected swipe is closer to.
    pub fn hide_gesture_end(&mut self) -> bool {
        self.with_hide(|hide| hide.gesture_end())
    }
}

impl State {
    /// Fade the contents of surface `id` as drawn in `before` into what it draws next, the
    /// backdrop along if it `flips`.
    pub(crate) fn start_resize_fade(&mut self, id: u64, before: Vec<ViewRect>, flips: bool) {
        let ewm = &mut self.ewm;
        let Some(surface) = ewm.surfaces.get_mut(&id) else {
            return;
        };
        // An open fade would hide it, a held one keeps the picture from before the first layout.
        if before.is_empty()
            || surface.open.as_ref().is_some_and(|open| !open.is_done())
            || surface.resize.as_ref().is_some_and(ResizeFade::held)
            || ewm.animations_clock.should_complete_instantly()
        {
            return;
        }
        // The highest resolution any view draws the old contents at, capped at their own.
        let scale = before
            .iter()
            .map(|view| view.scale.min(1.) * view.output.current_scale().fractional_scale())
            .fold(0., f64::max);
        let clock = (!surface.fade_pending()).then(|| ewm.animations_clock.clone());
        self.backend.with_renderer(|renderer, _, _| {
            if let Some(snapshot) = render_view_snapshot(&surface.window, renderer, scale.into()) {
                surface.resize = Some(ResizeFade {
                    snapshot,
                    anim: clock.map(fade_animation),
                    views: before,
                    flips,
                });
            }
        });
    }

    /// Apply Emacs's layout, fading every surface it refits a view of or configures with a fade.
    pub(crate) fn apply_layout(&mut self, frames: Vec<strip::Frame>) {
        let mut before = self.ewm.view_rects();
        self.ewm.apply_layout(frames);
        let now = self.ewm.view_rects();
        let fading: HashSet<u64> = before
            .iter()
            .filter(|was| {
                self.ewm.surfaces[&was.surface].fade_pending()
                    || now.iter().any(|view| {
                        view.entry == was.entry
                            && !view.settles
                            && resize_fades(was.rect.size, view.rect.size)
                    })
            })
            .map(|was| was.surface)
            .collect();
        // The old contents of every view, the ones the layout dropped too.
        for id in fading {
            let views = before.extract_if(.., |was| was.surface == id).collect();
            self.start_resize_fade(id, views, false);
        }
        self.ewm.layout.settle_resized();
    }
}

impl State {
    /// Bake close snapshots for the frames among `ids` lacking one, and for the other ids' views.
    pub(crate) fn store_close_snapshots(&mut self, ids: &[u64]) {
        let ewm = &mut self.ewm;
        if ewm.animations_clock.should_complete_instantly() {
            return;
        }
        let ids: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|&id| match ewm.layout.frame_location(id) {
                Some(location) => location.frame.close_snapshot.is_none(),
                None => ewm.surface_entry_locations(id).next().is_some(),
            })
            .collect();
        if ids.is_empty() {
            return;
        }
        self.backend.with_renderer(|renderer, _, _| {
            for id in ids {
                ewm.store_close_snapshot(renderer, id);
            }
        });
    }
}
