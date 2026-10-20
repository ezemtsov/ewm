//! The compositor side of the layout: moves, hotplug and what Emacs is told about them.

use std::collections::{HashMap, HashSet};
use std::mem;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::utils::under_from_surface_tree;
use smithay::desktop::{Window, WindowSurfaceType};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::seat::WaylandFocus;

use super::resize_fade::{Fit, ViewRect, contents_rect};
use super::snapshot::Snapshot;
use super::{FrameLocation, LayoutEntryLocation, Monitor, Moved, floating, strip};
use crate::{
    Event, Ewm, LayoutEntry, LayoutEntryHit, LayoutEntryId, SurfaceHit, fullscreen_center_offset,
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

    /// Hit-test layout toplevel/subsurfaces under a global position. Falls
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
    /// configure its toplevel. Off-screen frames don't bid.
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
    /// Start a strip swipe on the output under the pointer, none while the frames are hidden.
    pub fn strip_gesture_begin(&mut self) -> Option<Output> {
        // A hidden strip would scroll unseen.
        if self.frames_hidden() {
            return None;
        }
        let output = self.output_under_cursor().cloned()?;
        if let Some(strip) = self.layout.strip_mut(&output) {
            strip.gesture_begin();
        }
        Some(output)
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
            .filter(|&id| id != before);
        if let Some(id) = target {
            if self.pointer_is_on_output(output) {
                // The frame settles flush with the working area.
                let output_loc = self
                    .space
                    .output_geometry(output)
                    .map_or(Point::default(), |geo| geo.loc);
                let (px, py) = self.pointer_location();
                let in_content = self
                    .overview_geometry(output)
                    .to_content(Point::from((px, py)) - output_loc.to_f64());
                let pos_in_frame = in_content - self.get_working_area(output).loc.to_f64();
                self.set_focus_frame_from_pointer(id, "gesture", pos_in_frame);
            } else {
                self.set_focus_frame(id, "gesture", true);
            }
        }
    }
}
