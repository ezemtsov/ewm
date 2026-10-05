use smithay::desktop::{
    LayerSurface as DesktopLayerSurface, PopupKeyboardGrab, PopupKind, PopupPointerGrab,
    PopupUngrabStrategy, WindowSurfaceType, find_popup_root_surface, get_popup_toplevel_coords,
    layer_map_for_output,
};
use smithay::output::Output;
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle, Size};
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::wayland::shell::wlr_layer::Layer;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::xdg_foreign::{XdgForeignHandler, XdgForeignState};
use tracing::{info, warn};

use crate::{Event, Ewm, State};

// XDG Shell
impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.ewm.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.ewm.handle_new_toplevel(surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Defer: event sends from resource destructors abort on a dying client.
        self.ewm.loop_handle.insert_idle(move |state| {
            let ewm = &mut state.ewm;
            if let Some(refocus_id) = ewm.handle_toplevel_destroyed(surface) {
                ewm.set_focus_frame(refocus_id, "toplevel_destroyed", true);
            }
        });
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.ewm.unconstrain_popup(&surface);
        if let Err(err) = self.ewm.popups.track_popup(PopupKind::Xdg(surface)) {
            warn!("error tracking popup: {err:?}");
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        self.ewm
            .queue_minimize_request_for_surface(surface.wl_surface());
    }

    fn grab(
        &mut self,
        surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        serial: smithay::utils::Serial,
    ) {
        let popup = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&popup) else {
            return;
        };

        let seat = self.ewm.seat.clone();
        let mut grab = match self.ewm.popups.grab_popup(root, popup, &seat, serial) {
            Ok(grab) => grab,
            Err(err) => {
                warn!("error grabbing popup: {err:?}");
                return;
            }
        };

        // Install the grab on the seat so a click outside the popup dismisses it.
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(
                self,
                PopupPointerGrab::new(&grab),
                serial,
                smithay::input::pointer::Focus::Keep,
            );
        }
        self.ewm.popup_grab = Some(grab);
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.ewm.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        self.ewm
            .queue_surface_request(surface.wl_surface(), |id| Event::FullscreenRequest { id });
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.ewm
            .queue_surface_request(surface.wl_surface(), |id| Event::UnfullscreenRequest { id });
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.ewm
            .queue_maximize_request_for_surface(surface.wl_surface());
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.ewm
            .queue_unmaximize_request_for_surface(surface.wl_surface());
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        if let Some(output) = self.ewm.output_for_popup(&PopupKind::Xdg(surface)) {
            self.ewm.queue_redraw(&output);
        }
    }
}

// XDG Decoration
/// Emacs draws the frames, so a client that asks is told not to decorate.
fn set_server_side_decoration(toplevel: &ToplevelSurface) {
    use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(Mode::ServerSide);
    });
    toplevel.send_configure();
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        set_server_side_decoration(&toplevel);
    }

    fn request_mode(
        &mut self,
        toplevel: ToplevelSurface,
        _mode: smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    ) {
        set_server_side_decoration(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        set_server_side_decoration(&toplevel);
    }
}

// KDE Decoration, the only kind GTK understands. GTK re-requests its mode until it is echoed.
impl KdeDecorationHandler for State {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.ewm.kde_decoration_state
    }

    fn new_decoration(
        &mut self,
        _surface: &WlSurface,
        decoration: &org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration,
    ) {
        decoration.mode(org_kde_kwin_server_decoration::Mode::Server);
    }
}

impl XdgForeignHandler for State {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.ewm.xdg_foreign_state
    }
}

/// Unconstrain a popup with 8px padding, falling back to no padding if it
/// doesn't fit.
fn unconstrain_with_padding(
    positioner: PositionerState,
    target: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_positioner::ConstraintAdjustment;

    const PADDING: i32 = 8;

    let mut padded = target;
    if PADDING * 2 < padded.size.w {
        padded.loc.x += PADDING;
        padded.size.w -= PADDING * 2;
    }
    if PADDING * 2 < padded.size.h {
        padded.loc.y += PADDING;
        padded.size.h -= PADDING * 2;
    }

    if padded == target {
        return positioner.get_unconstrained_geometry(target);
    }

    // Try padded without resize adjustments first.
    let mut no_resize = positioner;
    no_resize
        .constraint_adjustment
        .remove(ConstraintAdjustment::ResizeX);
    no_resize
        .constraint_adjustment
        .remove(ConstraintAdjustment::ResizeY);

    let geo = no_resize.get_unconstrained_geometry(padded);
    if padded.contains_rect(geo) {
        return geo;
    }

    // Padded didn't fit; fall back to the full target.
    positioner.get_unconstrained_geometry(target)
}

impl Ewm {
    pub(crate) fn layer_for_root(&self, root: &WlSurface) -> Option<(Output, DesktopLayerSurface)> {
        self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            let layer = map.layer_for_surface(root, WindowSurfaceType::TOPLEVEL)?;
            Some((output.clone(), layer.clone()))
        })
    }

    pub(crate) fn output_for_root(&self, root: &WlSurface) -> Option<Output> {
        if let Some(id) = self.surface_id(root) {
            let output_name = self.surface_output_name(id)?;
            return self.find_mapped_output(output_name);
        }

        self.layer_for_root(root).map(|(output, _)| output)
    }

    /// Find the output for a popup's root surface (window or layer surface).
    pub fn output_for_popup(&self, popup: &PopupKind) -> Option<Output> {
        let root = find_popup_root_surface(popup).ok()?;
        self.output_for_root(&root)
    }

    /// Compute the available rectangle for a popup in geometry-local coordinates.
    ///
    /// Derived from the layout entry position, with no dependency on window.geometry(),
    /// so centering offsets and CSD geometry don't affect the result.
    /// Fullscreen: full output. Normal: entry width × output height (horizontally
    /// constrained to the entry, vertically to the full output).
    fn popup_target_rect(&self, id: u64) -> Option<Rectangle<i32, Logical>> {
        let location = self.preferred_entry_for_surface(id)?;
        let entry = location.entry;
        let output = self.mapped_output_by_name(location.output_name)?;
        let output_geo = self.space.output_geometry(output)?;

        if self.layout_entry_location_fullscreen(location) {
            return Some(Rectangle::from_size(output_geo.size));
        }

        let working_area = self.get_working_area(output);

        // Horizontal: constrain to entry width (popup stays within the window).
        // Vertical: full working area height relative to the entry's Y position.
        // Origin is in window-geometry-local coords: x=0 (window's left edge),
        // y=-entry.y (working area's top edge relative to window).
        Some(Rectangle::new(
            smithay::utils::Point::from((0, -(entry.y))),
            Size::from((entry.w as i32, working_area.size.h)),
        ))
    }

    /// Working area in an Emacs frame's own coordinates.
    fn frame_popup_target_rect(&self, id: u64) -> Option<Rectangle<i32, Logical>> {
        let output = self.mapped_output_by_name(self.frame_location(id)?.output_name)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);
        let origin = self.emacs_frame_origin(id)?;
        Some(Rectangle::new(
            output_geo.loc + working_area.loc - origin,
            working_area.size,
        ))
    }

    /// Unconstrain a popup's position to keep it within screen bounds.
    ///
    /// For window popups, computes the target from the layout entry position
    /// (not window.geometry()) so centering offsets and CSD geometry don't
    /// affect placement.  For layer-shell popups, uses the output bounds
    /// adjusted for the layer surface position and non-exclusive zone.
    pub fn unconstrain_popup(&self, popup: &PopupSurface) {
        let popup_kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&popup_kind) else {
            return;
        };

        // Window popup path
        if let Some(id) = self.surface_id(&root) {
            let raw_target = if self.is_emacs_frame(id) {
                self.frame_popup_target_rect(id)
            } else {
                self.popup_target_rect(id)
            }
            .unwrap_or_else(|| Rectangle::from_size(self.output_size));
            let toplevel_coords = get_popup_toplevel_coords(&popup_kind);
            let mut target = raw_target;
            target.loc -= toplevel_coords;

            popup.with_pending_state(|state| {
                let result = unconstrain_with_padding(state.positioner, target);
                info!(
                    "unconstrain_popup: id={} raw_target={:?} \
                     toplevel_coords={:?} target={:?} \
                     positioner.rect={:?} positioner.size={:?} \
                     result={:?}",
                    id,
                    raw_target,
                    toplevel_coords,
                    target,
                    state.positioner.anchor_rect,
                    state.positioner.rect_size,
                    result,
                );
                state.geometry = result;
            });
            return;
        }

        // Layer-shell popup path
        if let Some((output, layer)) = self.layer_for_root(&root) {
            let map = layer_map_for_output(&output);
            let Some(layer_geo) = map.layer_geometry(&layer) else {
                return;
            };
            let output_geo = self
                .space
                .output_geometry(&output)
                .unwrap_or_else(|| Rectangle::from_size(self.output_size));

            let mut target = Rectangle::from_size(output_geo.size);
            if matches!(layer.layer(), Layer::Background | Layer::Bottom) {
                target = map.non_exclusive_zone();
            }
            target.loc -= layer_geo.loc;
            target.loc -= get_popup_toplevel_coords(&popup_kind);

            popup.with_pending_state(|state| {
                state.geometry = state.positioner.get_unconstrained_geometry(target);
            });
        }
    }

    /// Handle commit for tracked popups. Returns true if this was a popup.
    pub fn handle_popup_commit(&mut self, surface: &WlSurface) -> bool {
        self.popups.commit(surface);

        let Some(popup) = self.popups.find_popup(surface) else {
            return false;
        };

        let output = self.output_for_popup(&popup);
        if let PopupKind::Xdg(ref xdg_popup) = popup
            && !xdg_popup.is_initial_configure_sent()
        {
            if let Some(output) = output.as_ref() {
                let scale = output.current_scale();
                let transform = output.current_transform();
                smithay::wayland::compositor::with_states(surface, |data| {
                    crate::utils::send_scale_transform(surface, data, scale, transform);
                });
            }
            xdg_popup
                .send_configure()
                .expect("initial configure failed");
        }

        if let Some(output) = output {
            self.queue_redraw(&output);
        }

        true
    }
}
