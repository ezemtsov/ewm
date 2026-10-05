use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, find_popup_root_surface,
};
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::xdg_foreign::{XdgForeignHandler, XdgForeignState};
use tracing::warn;

use crate::{Event, State};

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
