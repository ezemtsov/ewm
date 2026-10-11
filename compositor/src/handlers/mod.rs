pub mod background_effect;
mod compositor;
mod layer_shell;
mod xdg_shell;

use std::collections::HashMap;
use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use std::time::Duration;

use smithay::backend::input::{InputEvent, TabletToolDescriptor};
use smithay::desktop::{PopupKind, PopupManager};
use smithay::input::dnd::{DnDGrab, DndGrabHandler, DndTarget};
use smithay::input::pointer::{CursorImageStatus, PointerHandle};
use smithay::input::tablet::TabletSeatHandler;
use smithay::input::{Seat, SeatHandler, SeatState, dnd};
use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::fractional_scale::FractionalScaleHandler;
use smithay::wayland::idle_inhibit::IdleInhibitHandler;
use smithay::wayland::idle_notify::{IdleNotifierHandler, IdleNotifierState};
use smithay::wayland::input_method::{InputMethodHandler, PopupSurface as IMPopupSurface};
use smithay::wayland::pointer_constraints::{PointerConstraintsHandler, with_pointer_constraint};
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
};
use smithay::wayland::selection::wlr_data_control::{DataControlHandler, DataControlState};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::wayland::xdg_system_bell::XdgSystemBellHandler;
use tracing::{debug, info, trace, warn};

pub(crate) use self::xdg_shell::add_toplevel_pre_commit_hook;
use crate::protocols::foreign_toplevel::{ForeignToplevelHandler, ForeignToplevelManagerState};
use crate::protocols::output_management::{OutputManagementHandler, OutputManagementState};
use crate::protocols::screencopy::{Screencopy, ScreencopyHandler, ScreencopyManagerState};
use crate::protocols::virtual_pointer::{
    VirtualPointerAxisEvent, VirtualPointerButtonEvent, VirtualPointerHandler,
    VirtualPointerInputBackend, VirtualPointerManagerState, VirtualPointerMotionAbsoluteEvent,
    VirtualPointerMotionEvent,
};
use crate::protocols::workspace::{WorkspaceHandler, WorkspaceManagerState};
use crate::{DndIcon, Event, LockRenderState, LockState, OutputConfig, State};

// DMA-BUF
impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.ewm.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        let _ = notifier.successful::<State>();
    }
}

// Seat / input
impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.ewm.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|s| self.ewm.display_handle.get_client(s.id()).ok());
        set_data_device_focus(&self.ewm.display_handle, seat, client.clone());
        set_primary_focus(&self.ewm.display_handle, seat, client);

        // Update text_input focus for input method support
        self.ewm.update_text_input_focus(focused);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.ewm.cursor_manager.set_cursor_image(image);
        self.ewm.queue_redraw_all();
    }
}

impl TabletSeatHandler for State {
    type ToolFocus = WlSurface;

    fn tablet_tool_image(&mut self, _tool: &TabletToolDescriptor, image: CursorImageStatus) {
        self.ewm.cursor_manager.set_cursor_image(image);
        self.ewm.queue_redraw_all();
    }
}

smithay::delegate_dispatch2!(State);

// Data device / selection
impl SelectionHandler for State {
    type SelectionUserData = Arc<[u8]>;

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if ty == SelectionTarget::Clipboard
            && let Some(source) = &source
        {
            let mime_types = source.mime_types();
            if mime_types.iter().any(|m| m.contains("text")) {
                self.read_client_selection_to_emacs();
            }
        }
    }

    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        _mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        let buf = user_data.clone();
        std::thread::spawn(move || {
            use std::io::Write;

            use smithay::reexports::rustix::fs::{OFlags, fcntl_setfl};
            if let Err(err) = fcntl_setfl(&fd, OFlags::empty()) {
                warn!("error clearing flags on selection fd: {err:?}");
            }
            if let Err(err) = std::fs::File::from(fd).write_all(&buf) {
                warn!("error writing selection: {err:?}");
            }
        });
    }
}

impl WaylandDndGrabHandler for State {
    fn dnd_requested<S: dnd::Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: smithay::utils::Serial,
        type_: dnd::GrabType,
    ) {
        match type_ {
            dnd::GrabType::Pointer => {
                let pointer = seat.get_pointer().unwrap();
                let mut start_data = pointer.grab_start_data().unwrap();
                // Seeds the drop location; the click grab's press point is stale.
                start_data.location = pointer.current_location();
                let grab = DnDGrab::new_pointer(&self.ewm.display_handle, start_data, source, seat);
                pointer.set_grab(self, grab, serial, smithay::input::pointer::Focus::Keep);
            }
            dnd::GrabType::Touch => {
                let touch = seat.get_touch().unwrap();
                let start_data = touch.grab_start_data().unwrap();
                let grab = DnDGrab::new_touch(&self.ewm.display_handle, start_data, source, seat);
                touch.set_grab(self, grab, serial);
            }
        }

        // After set_grab: cancelling a superseded DnD grab clears the icon.
        self.ewm.dnd_icon = icon.map(|surface| DndIcon {
            surface,
            offset: Point::from((0, 0)),
        });
        self.ewm.queue_redraw_all();
    }
}

impl DndGrabHandler for State {
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        location: Point<f64, Logical>,
    ) {
        // A drop acts like a click; a torn-off tab's window then opens there.
        self.ewm.pending_drop = Some(location);
        self.ewm.dnd_icon = None;
        self.ewm.queue_redraw_all();
    }

    fn cancelled(&mut self, _seat: Seat<Self>, _location: Point<f64, Logical>) {
        // Forget the hover so focus-follows-mouse re-evaluates on the next motion.
        self.ewm.pointer_entry_id = None;
        self.ewm.dnd_icon = None;
        self.ewm.queue_redraw_all();
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.ewm.data_device_state
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.ewm.primary_selection_state
    }
}

impl DataControlHandler for State {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.ewm.data_control_state
    }
}

// Output
impl smithay::wayland::output::OutputHandler for State {
    fn output_bound(
        &mut self,
        output: Output,
        wl_output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
    ) {
        crate::protocols::workspace::on_output_bound(
            &mut self.ewm.workspace_state,
            &output,
            &wl_output,
        );
        self.ewm
            .foreign_toplevel_state
            .output_bound(&output, &wl_output);
    }
}

// Input Method (the relay or an external input method such as fcitx5)
impl InputMethodHandler for State {
    fn new_popup(&mut self, surface: IMPopupSurface) {
        let popup = PopupKind::InputMethod(surface);
        if let Some(output) = self.ewm.output_for_popup(&popup) {
            let scale = output.current_scale();
            let transform = output.current_transform();
            let wl_surface = popup.wl_surface();
            smithay::wayland::compositor::with_states(wl_surface, |data| {
                crate::utils::send_scale_transform(wl_surface, data, scale, transform);
            });
        }
        self.ewm.unconstrain_popup(&popup);
        if let Err(err) = self.ewm.popups.track_popup(popup) {
            warn!("error tracking input method popup: {err:?}");
        }
    }

    fn dismiss_popup(&mut self, surface: IMPopupSurface) {
        if let Some(parent) = surface.get_parent().map(|parent| parent.surface.clone()) {
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::InputMethod(surface));
        }
    }

    fn popup_repositioned(&mut self, surface: IMPopupSurface) {
        self.ewm.unconstrain_popup(&PopupKind::InputMethod(surface));
    }

    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, smithay::utils::Logical> {
        self.ewm
            .surface_id(parent)
            .and_then(|id| self.ewm.window(id))
            .map(|window| window.geometry())
            .unwrap_or_default()
    }
}

// Virtual Pointer (zwlr-virtual-pointer-v1: lets clients inject pointer motion,
// buttons and axis, routed through the normal input handlers).
impl VirtualPointerHandler for State {
    fn virtual_pointer_manager_state(&mut self) -> &mut VirtualPointerManagerState {
        &mut self.ewm.virtual_pointer_state
    }

    fn on_virtual_pointer_motion(&mut self, event: VirtualPointerMotionEvent) {
        self.process_input_event(InputEvent::<VirtualPointerInputBackend>::PointerMotion { event });
    }

    fn on_virtual_pointer_motion_absolute(&mut self, event: VirtualPointerMotionAbsoluteEvent) {
        self.process_input_event(
            InputEvent::<VirtualPointerInputBackend>::PointerMotionAbsolute { event },
        );
    }

    fn on_virtual_pointer_button(&mut self, event: VirtualPointerButtonEvent) {
        self.process_input_event(InputEvent::<VirtualPointerInputBackend>::PointerButton { event });
    }

    fn on_virtual_pointer_axis(&mut self, event: VirtualPointerAxisEvent) {
        self.process_input_event(InputEvent::<VirtualPointerInputBackend>::PointerAxis { event });
    }
}

pub const XDG_ACTIVATION_TOKEN_TIMEOUT: Duration = Duration::from_secs(10);

struct UrgentOnlyMarker;

// XDG Activation protocol (allows apps to request focus or attention)
impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.ewm.activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        // Tokens without a serial are urgency-only. This mirrors common client
        // behavior for background clients and avoids letting them
        // steal focus through xdg_activation.
        let app_id = data.app_id.as_deref().unwrap_or("unknown");

        let Some((serial, seat)) = data.serial else {
            data.user_data.insert_if_missing(|| UrgentOnlyMarker);
            debug!("xdg_activation: token accepted for {app_id} as urgency-only");
            return true;
        };
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            debug!("xdg_activation: token rejected for {app_id} - invalid seat");
            return false;
        };

        let keyboard = seat.get_keyboard().unwrap();
        let keyboard_valid = keyboard
            .last_enter()
            .map(|last_enter| serial.is_no_older_than(&last_enter))
            .unwrap_or(false);
        let pointer_valid = seat
            .get_pointer()
            .and_then(|pointer| pointer.last_enter())
            .map(|last_enter| serial.is_no_older_than(&last_enter))
            .unwrap_or(false);
        let valid = keyboard_valid || pointer_valid;

        if valid {
            debug!("xdg_activation: token accepted for {app_id}");
        } else {
            debug!(
                "xdg_activation: token rejected for {app_id} - serial not from keyboard or pointer focus"
            );
        }
        valid
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        debug!(
            "xdg_activation: request_activation called for surface {:?}",
            surface.id()
        );

        if token_data.timestamp.elapsed() < XDG_ACTIVATION_TOKEN_TIMEOUT {
            if let Some(id) = self.ewm.surface_id(&surface) {
                // Lisp owns Emacs frame focus via select-frame-set-input-focus;
                // honoring GTK's echo activation here would loop with our own
                // ewm-focus calls.
                if token_data.user_data.get::<UrgentOnlyMarker>().is_some() {
                    if self.ewm.set_surface_urgent(id, true) {
                        info!("xdg_activation: marked surface {} urgent", id);
                    } else {
                        debug!("xdg_activation: urgency ignored for surface {}", id);
                    }
                } else if self.ewm.layout.has_frame(id) {
                    debug!("xdg_activation: ignored for Emacs frame {}", id);
                } else {
                    self.ewm.activate_or_defer_surface(id, "xdg_activation");
                    info!("xdg_activation: granted for surface {}", id);
                }
            } else {
                debug!("xdg_activation: surface is not a toplevel");
            }
        } else {
            debug!(
                "xdg_activation: token expired (age={:?})",
                token_data.timestamp.elapsed()
            );
        }

        // Always remove the token (single-use)
        self.ewm.activation_state.remove_token(&token);
    }
}

impl XdgSystemBellHandler for State {
    fn ring(&mut self, surface: Option<WlSurface>) {
        let id = surface.and_then(|surface| self.ewm.surface_id(&surface));
        // Emacs rang this itself; echoing it back would loop.
        if id.is_some_and(|id| self.ewm.layout.has_frame(id)) {
            return;
        }
        self.ewm.queue_event(Event::Bell { id });
    }
}

// Foreign toplevel management protocol (exposes windows to external tools)
impl ForeignToplevelHandler for State {
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState {
        &mut self.ewm.foreign_toplevel_state
    }

    fn is_read_only_surface(&self, wl_surface: &WlSurface) -> bool {
        self.ewm
            .surface_id(wl_surface)
            .is_some_and(|id| self.ewm.layout.has_frame(id))
    }

    fn activate(&mut self, wl_surface: WlSurface) {
        if let Some(id) = self.ewm.surface_id(&wl_surface) {
            self.ewm.activate_or_defer_surface(id, "foreign_toplevel");
            info!("Foreign toplevel: activated surface {}", id);
        }
    }

    fn close(&mut self, wl_surface: WlSurface) {
        if let Some(id) = self.ewm.surface_id(&wl_surface) {
            let Some(window) = self.ewm.window(id) else {
                return;
            };
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_close();
                info!("Foreign toplevel: sent close request");
            }
        }
    }

    fn set_fullscreen(&mut self, wl_surface: WlSurface, _wl_output: Option<WlOutput>) {
        self.request_fullscreen(&wl_surface, true);
    }

    fn unset_fullscreen(&mut self, wl_surface: WlSurface) {
        self.request_fullscreen(&wl_surface, false);
    }

    fn set_maximized(&mut self, wl_surface: WlSurface) {
        self.ewm.queue_maximize_request_for_surface(&wl_surface);
    }

    fn unset_maximized(&mut self, wl_surface: WlSurface) {
        self.ewm.queue_unmaximize_request_for_surface(&wl_surface);
    }

    fn minimize(&mut self, wl_surface: WlSurface) {
        self.ewm.queue_minimize_request_for_surface(&wl_surface);
    }
}

// Workspace protocol (ext-workspace-v1: Emacs frames as workspaces)
impl WorkspaceHandler for State {
    fn workspace_manager_state(&mut self) -> &mut WorkspaceManagerState {
        &mut self.ewm.workspace_state
    }

    fn activate_workspace(&mut self, frame_surface_id: u64) {
        self.ewm.queue_event(Event::ActivateWorkspace {
            id: frame_surface_id,
        });
    }
}

// Output management protocol (wlr-output-management-unstable-v1)
impl OutputManagementHandler for State {
    fn output_management_state(&mut self) -> &mut OutputManagementState {
        &mut self.ewm.output_management_state
    }

    fn apply_output_config(&mut self, configs: HashMap<String, OutputConfig>) {
        // Merge each config into ewm.output_config and apply.
        // The backend sets output_heads_changed = true; the deferred
        // refresh_output_management() call (before next render) will
        // send protocol updates to clients, after the Dispatch handler
        // has already sent succeeded().
        for (name, config) in configs {
            self.ewm.output_config.insert(name.clone(), config);
            self.apply_output_config_for(&name);
        }
    }
}

// Screencopy protocol
impl ScreencopyHandler for State {
    fn frame(&mut self, manager: &ZwlrScreencopyManagerV1, screencopy: Screencopy) {
        if !self.ewm.output_exists(screencopy.output()) {
            trace!("screencopy output no longer exists");
            return;
        }

        if screencopy.with_damage() {
            // CopyWithDamage: queue for processing during output redraw,
            // where per-queue damage tracking can skip no-change frames.
            if let Some(queue) = self.ewm.screencopy_state.get_queue_mut(manager) {
                queue.push(screencopy);
            }
        } else {
            // Copy: render immediately without waiting for the next redraw cycle.
            let manager = manager.clone();
            let State { backend, ewm } = self;
            backend.with_renderer(|renderer, cursor_texture_cache, event_loop| {
                crate::render::render_screencopy_immediate(
                    ewm,
                    renderer,
                    &manager,
                    screencopy,
                    cursor_texture_cache,
                    event_loop,
                );
            });
        }
    }

    fn screencopy_state(&mut self) -> &mut ScreencopyManagerState {
        &mut self.ewm.screencopy_state
    }
}

// Session Lock protocol (ext-session-lock-v1) for screen locking
impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.ewm.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        // Liveness, not the state, decides: a replacement is the only way out
        // of a lock whose client is gone.
        if self.ewm.lock_client_is_live() {
            info!("Session lock request ignored: a live client holds the lock");
            return;
        }

        info!("Session lock requested");

        // Save current focus to restore after unlock
        if self.ewm.focused_surface_id() != 0 {
            self.ewm.pre_lock_focus = Some(self.ewm.focused_surface_id());
        }

        if self.ewm.output_state.is_empty() {
            // No outputs: lock immediately
            let lock = confirmation.ext_session_lock().clone();
            confirmation.lock();
            self.ewm.lock_state = LockState::Locked(lock);
            info!("Session locked (no outputs)");
        } else {
            // Enter Locking state and queue redraw to show locked frame
            self.ewm.lock_state = LockState::Locking(confirmation);
            // Reset all output lock render states
            for state in self.ewm.output_state.values_mut() {
                state.lock_render_state = LockRenderState::Unlocked;
            }
            self.ewm.queue_redraw_all();
        }

        // Monitors on for the lock screen. After the state is set, so the wake
        // spares the lock client.
        self.ewm.wake_from_idle();
    }

    fn unlock(&mut self) {
        info!("Session unlock requested");
        self.ewm.lock_state = LockState::Unlocked;

        // Clear lock surfaces and reset render states
        for state in self.ewm.output_state.values_mut() {
            state.lock_surface = None;
            state.lock_render_state = LockRenderState::Unlocked;
        }

        // Invalidate tracked focus (was on lock surface) to force sync
        self.ewm.keyboard_focus = None;

        // Restore focus to the surface that was focused before locking
        if let Some(id) = self.ewm.pre_lock_focus.take()
            && self.ewm.surfaces.contains_key(&id)
        {
            info!("Restoring focus to surface {} after unlock", id);
            self.ewm.set_focus(id, false, "unlock");
        }
        self.sync_keyboard_focus();

        self.ewm.queue_redraw_all();

        // Monitors on, so unlocking from a fingerprint reader lights the screen.
        self.ewm.activate_monitors();
        self.ewm.notify_activity();
        self.ewm.reset_idle_timer();

        info!("Session unlocked");
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let Some(output) = self.ewm.output_from_resource(&wl_output) else {
            warn!("Lock surface created for unknown output");
            return;
        };

        info!("New lock surface for output: {}", output.name());

        // Configure lock surface to cover the entire output
        configure_lock_surface(&surface, &output);

        // Store in per-output state
        if let Some(state) = self.ewm.output_state.get_mut(&output) {
            state.lock_surface = Some(surface);
        }

        self.ewm.queue_redraw(&output);
    }
}

// Idle notify protocol (ext-idle-notify-v1)
impl IdleNotifierHandler for State {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.ewm.idle_notifier_state
    }
}

// Idle inhibit protocol (zwp-idle-inhibit-v1)
impl IdleInhibitHandler for State {
    fn inhibit(&mut self, surface: WlSurface) {
        self.ewm.idle_inhibiting_surfaces.insert(surface);
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.ewm.idle_inhibiting_surfaces.remove(&surface);
    }
}

// Gamma control protocol (wlr-gamma-control-unstable-v1)
impl crate::protocols::gamma_control::GammaControlHandler for State {
    fn gamma_control_manager_state(
        &mut self,
    ) -> &mut crate::protocols::gamma_control::GammaControlManagerState {
        &mut self.ewm.gamma_control_state
    }

    fn get_gamma_size(&mut self, output: &Output) -> Option<u32> {
        let drm = self.backend.as_drm_mut()?;
        match drm.get_gamma_size(output) {
            Ok(0) => None,
            Ok(size) => Some(size),
            Err(err) => {
                warn!("error getting gamma size for {}: {err:?}", output.name());
                None
            }
        }
    }

    fn set_gamma(&mut self, output: &Output, ramp: Option<Vec<u16>>) -> Option<()> {
        let drm = self.backend.as_drm_mut()?;
        match drm.set_gamma(output, ramp) {
            Ok(()) => Some(()),
            Err(err) => {
                warn!("error setting gamma for {}: {err:?}", output.name());
                None
            }
        }
    }
}

// Fractional scale protocol (wp-fractional-scale-v1)
// Scale is sent from lifecycle-specific handlers (handle_new_toplevel, handle_layer_surface_commit,
// configure_lock_surface, apply_output_config) that know the correct output.
impl FractionalScaleHandler for State {}

// Pointer constraints protocol (zwp-pointer-constraints-v1)
impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        // Pointer constraints track pointer focus internally, so make sure
        // it's up to date before activating a new one.
        self.refresh_pointer_focus();

        self.maybe_activate_pointer_constraint();
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        // Only apply hint if the constraint is active
        let active =
            with_pointer_constraint(surface, pointer, |c| c.is_some_and(|c| c.is_active()));
        if !active {
            return;
        }

        let Some((ref focus_surface, origin)) = self.ewm.pointer_focus else {
            return;
        };
        if focus_surface != surface {
            return;
        }

        // Clip hint against the union of output geometries.
        let (cx, cy) = self.ewm.pointer_location();
        let target = origin + location;
        let previous = Point::from((cx, cy));
        let clamped = self.ewm.clamp_pointer_to_outputs(target, previous);
        pointer.set_location(clamped);

        // Redraw to update cursor position visually
        self.ewm.queue_redraw_for_pointer();
    }
}

/// Configure a lock surface to cover the full output
pub(crate) fn configure_lock_surface(surface: &LockSurface, output: &Output) {
    use smithay::wayland::compositor::with_states;

    surface.with_pending_state(|states| {
        let size = crate::utils::output_size(output);
        states.size = Some(size.to_i32_round());
    });

    let scale = output.current_scale();
    let transform = output.current_transform();
    let wl_surface = surface.wl_surface();

    with_states(wl_surface, |data| {
        crate::utils::send_scale_transform(wl_surface, data, scale, transform);
    });

    surface.send_configure();
}
