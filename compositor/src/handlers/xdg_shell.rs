use smithay::desktop::{
    LayerSurface as DesktopLayerSurface, PopupKeyboardGrab, PopupKind, PopupPointerGrab,
    PopupUngrabStrategy, WindowSurfaceType, find_popup_root_surface, get_popup_toplevel_coords,
    layer_map_for_output,
};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState;
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle, Size};
use smithay::wayland::compositor::{
    BufferAssignment, HookId, SurfaceAttributes, add_pre_commit_hook, with_states,
};
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::wayland::shell::wlr_layer::Layer;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::xdg_foreign::{XdgForeignHandler, XdgForeignState};
use tracing::{debug, info, warn};

use crate::layout::enter_output_for_window;
use crate::surface::SurfaceState;
use crate::{Event, Ewm, FocusSource, Notify, Placement, State, client_identity, im, module};

/// Snapshot an unmapping frame or view, fade a resize answer, note where views sat before they
/// move.
pub(crate) fn add_toplevel_pre_commit_hook(surface: &WlSurface, id: u64) -> HookId {
    add_pre_commit_hook::<State, _>(surface, move |state, _dh, surface| {
        let (removed, attached, acked) = with_states(surface, |states| {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let buffer = &attributes.pending().buffer;
            let removed = matches!(buffer, Some(BufferAssignment::Removed));
            let attached = matches!(buffer, Some(BufferAssignment::NewBuffer(_)));
            let acked = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|role| {
                    let role = role.lock().unwrap();
                    let configure = role.last_acked.as_ref()?;
                    let fullscreen = configure
                        .state
                        .states
                        .contains(XdgToplevelState::Fullscreen);
                    Some((configure.serial, fullscreen))
                });
            (removed, attached, acked)
        });
        if removed {
            state.store_close_snapshots(&[id]);
            return;
        }
        let Some((serial, fullscreen)) = acked else {
            return;
        };
        let Some(surface) = state.ewm.surfaces.get_mut(&id) else {
            return;
        };
        // An ack without a buffer leaves the resize pending for the commit that brings one.
        let mut answered = attached
            .then(|| surface.take_answered_resize(serial))
            .flatten();
        let flips = surface.fullscreen != fullscreen;
        if answered.is_some() || flips {
            let rects: Vec<_> = state
                .ewm
                .view_rects()
                .into_iter()
                .filter(|view| view.surface == id)
                .collect();
            if let Some(pending) = &mut answered {
                pending.place(&rects);
            }
            if let Some(surface) = state.ewm.surfaces.get_mut(&id) {
                surface.rects_before = rects;
            }
        }
        if let Some(pending) = answered {
            state.start_resize_fade(id, pending, flips);
        }
    })
}

// XDG Shell
impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.ewm.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.ewm.handle_new_toplevel(surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Every toplevel gets an id in handle_new_toplevel.
        let Some(id) = self.ewm.surface_id(surface.wl_surface()) else {
            warn!("destroyed toplevel has no record");
            return;
        };
        // Toolkits that destroy without unmapping still hold the buffer here.
        self.store_close_snapshots(&[id]);

        // Defer: event sends from resource destructors abort on a dying client. The id is
        // taken now, since the client may back a new toplevel with this surface before then.
        self.ewm.loop_handle.insert_idle(move |state| {
            let ewm = &mut state.ewm;
            if let Some(refocus_id) = ewm.handle_toplevel_destroyed_by_id(id) {
                ewm.set_focus_frame(refocus_id, FocusSource::ToplevelDestroyed, Notify::Emacs);
            }
        });
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let popup = PopupKind::Xdg(surface);
        self.ewm.unconstrain_popup(&popup);
        if let Err(err) = self.ewm.popups.track_popup(popup) {
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
            self.flush_deferred_keys(grab.current_grab().as_ref());
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
        self.ewm.unconstrain_popup(&PopupKind::Xdg(surface.clone()));
        surface.send_repositioned(token);
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        self.request_fullscreen(surface.wl_surface(), true);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.request_fullscreen(surface.wl_surface(), false);
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

    // Padded didn't fit. Fall back to the full target.
    positioner.get_unconstrained_geometry(target)
}

impl Ewm {
    /// Register a toplevel, configure it and place an Emacs frame right away.
    fn handle_new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_surface_id;
        self.next_surface_id += 1;

        let identity = client_identity(surface.wl_surface());
        let client_pid = identity.as_ref().map_or(0, |i| i.credentials.pid);
        // Only Emacs connects to the listening socket from this process.
        let is_emacs = self.emacs_pid.is_some_and(|pid| client_pid == pid as i32);
        if is_emacs {
            info!("Surface {} is an Emacs surface", id);
        }

        let app = with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().unwrap().app_id.clone())
        })
        .or_else(|| identity.and_then(|i| i.comm))
        .unwrap_or_else(|| "unknown".to_string());

        // Determine target output before the initial configure so floating
        // Emacs frames don't get the normal maximized/working-area configure.
        let pending_frame = is_emacs.then(module::take_pending_frame).flatten();
        let target_output = pending_frame
            .as_ref()
            .and_then(|pending| self.layout.output(&pending.output))
            .or_else(|| self.active_output())
            .cloned();
        let frame_seq = pending_frame.as_ref().map(|pending| pending.seq);
        let placement = pending_frame.map_or(Placement::Tiled { after: None }, |pending| {
            pending.placement
        });
        let frame_is_floating = matches!(placement, Placement::Floating { .. });

        surface.with_pending_state(|state| {
            if is_emacs && frame_is_floating {
                // Let Emacs drive the size. The frame fits itself to its content.
                state.size = None;
            } else {
                state.size = Some(self.output_size);
                state.states.set(XdgToplevelState::Maximized);
            }
            state.states.set(XdgToplevelState::Activated);
        });
        surface.send_configure();

        let record = SurfaceState::new(id, surface, is_emacs, app.clone());
        let window = record.window.clone();
        self.surfaces.insert(id, record);

        debug!(
            "new_toplevel id={} is_emacs={} seq={:?} floating={} target_output={:?} pending_queue_after_take={:?}",
            id,
            is_emacs,
            frame_seq,
            frame_is_floating,
            target_output,
            module::peek_pending_frame_outputs()
        );

        // Place the frame now so `layout.has_frame(id)`, `set_focus_frame(id)`,
        // etc. work before Emacs's first layout arrives. That layout only
        // fills in contents (name, width, entries, selected_entry_id).
        if is_emacs && let Some(output) = &target_output {
            self.layout.add_frame(id, output, placement);
            self.reconcile_output_entries(output);
        } else if let Some(output) = target_output.as_ref().or_else(|| self.first_output()) {
            // Scale and enter before the first commit, bypassing SpaceElement::output_enter
            // which would leave uncommitted surfaces.
            enter_output_for_window(&window, output);
        }

        // Resize Emacs frames to fill their working area
        if is_emacs
            && !frame_is_floating
            && let Some(output) = &target_output
            && let Some(t) = window.toplevel()
        {
            let working_area = self.get_working_area(output);
            t.with_pending_state(|state| {
                state.size = Some(working_area.size);
            });
            t.send_configure();
        }

        // App surfaces resolve their host frame at map. `New` only creates the buffer.
        self.queue_event(Event::New {
            id,
            app: app.clone(),
            seq: frame_seq,
            pid: client_pid,
        });
        // A new emacs frame joins its strip inactive. Tell lisp so it can
        // freeze it, after Event::New so ewm-surface-id is already bound on
        // the lisp side. The overview shows every frame, a new one too.
        if is_emacs {
            let is_active = target_output
                .as_ref()
                .and_then(|output| self.layout.active_frame_id(output))
                == Some(id);
            self.queue_event(
                if frame_is_floating || is_active || self.overview.is_active() {
                    Event::FrameShown { id }
                } else {
                    Event::FrameHidden { id }
                },
            );
        }
        // The first Emacs frame took the keyboard at placement. Sent after New so the lisp side
        // has assigned ewm-surface-id by the time the Focus event is processed.
        if is_emacs && self.focused_frame_id() == id {
            info!("Initial Emacs frame {} holds the keyboard", id);
            if let Some(focus_id) = self.layout.focus_id_of_frame(id) {
                self.queue_event(Event::Focus {
                    focus_id,
                    x: None,
                    y: None,
                    pointer: false,
                });
            }
        }
        info!(
            "New toplevel {} ({}) -> {:?}",
            id,
            app,
            target_output.as_ref().map(Output::name)
        );
    }

    /// Drop toplevel `id`, returning the frame to refocus.
    pub fn handle_toplevel_destroyed_by_id(&mut self, id: u64) -> Option<u64> {
        let was_focused = self.focused_frame_id() == id;
        let close_was_active = module::take_pending_active_frame_close(id);
        let active_close = was_focused || close_was_active;

        self.flush_pending_title_event(id);
        self.surfaces.remove(&id);
        let was_emacs = self.layout.has_frame(id);
        self.layout.detach_surface(id);
        self.reconcile_fullscreen();
        let output = self.remove_emacs_frame(id, active_close);
        self.queue_event(Event::Close { id });
        debug!(
            "Toplevel {} destroyed was_focused={} close_was_active={} was_emacs={} output={:?}",
            id,
            was_focused,
            close_was_active,
            was_emacs,
            output.as_ref().map(Output::name)
        );

        if was_emacs {
            self.refresh_mapped_cast_outputs();
        }

        if active_close {
            // Output may be unknown if lisp pruned the frame before destroy fired.
            let refocus = [output.as_ref(), self.pointer_output.as_ref()]
                .into_iter()
                .flatten()
                .find_map(|output| self.layout.focused_frame_on(output))
                .or_else(|| Some(self.focused_frame_id()).filter(|&id| id != 0));
            debug!("Toplevel {} destroyed: refocus -> {:?}", id, refocus);
            return refocus;
        }
        None
    }

    pub(crate) fn layer_for_root(&self, root: &WlSurface) -> Option<(Output, DesktopLayerSurface)> {
        self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            let layer = map.layer_for_surface(root, WindowSurfaceType::TOPLEVEL)?;
            Some((output.clone(), layer.clone()))
        })
    }

    pub(crate) fn output_for_root(&self, root: &WlSurface) -> Option<Output> {
        if let Some(id) = self.surface_id(root) {
            return self.find_surface_output(id);
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
        let output = location.monitor.output();
        let output_geo = self.layout.output_rect(output)?;

        if location.entry.fullscreen() {
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
        let output = self.layout.frame_location(id)?.monitor.output();
        let output_geo = self.layout.output_rect(output)?;
        let working_area = self.get_working_area(output);
        let origin = self.emacs_frame_origin(id)?;
        Some(Rectangle::new(
            output_geo.loc + working_area.loc - origin,
            working_area.size,
        ))
    }

    /// Unconstrain a popup's position to keep it within screen bounds.
    ///
    /// Dispatches to the window or layer-shell path by the popup's root surface.
    pub fn unconstrain_popup(&self, popup: &PopupKind) {
        let Ok(root) = find_popup_root_surface(popup) else {
            return;
        };

        if let Some(id) = self.surface_id(&root) {
            self.unconstrain_window_popup(popup, id);
        } else if let Some((output, layer)) = self.layer_for_root(&root) {
            self.unconstrain_layer_shell_popup(popup, &layer, &output);
        }
    }

    /// Place a window (or Emacs frame) popup within its working area.
    ///
    /// The target comes from the layout entry position (not window.geometry())
    /// so centering offsets and CSD geometry don't affect placement.
    fn unconstrain_window_popup(&self, popup: &PopupKind, id: u64) {
        let mut target = if self.layout.has_frame(id) {
            self.frame_popup_target_rect(id)
        } else {
            self.popup_target_rect(id)
        }
        .unwrap_or_else(|| Rectangle::from_size(self.output_size));
        target.loc -= get_popup_toplevel_coords(popup);

        self.position_popup_within_rect(popup, target, true);
    }

    /// Place a layer-shell popup within the output, within the non-exclusive
    /// zone for background and bottom layers.
    fn unconstrain_layer_shell_popup(
        &self,
        popup: &PopupKind,
        layer: &DesktopLayerSurface,
        output: &Output,
    ) {
        let map = layer_map_for_output(output);
        let Some(layer_geo) = map.layer_geometry(layer) else {
            return;
        };
        let output_geo = self
            .layout
            .output_rect(output)
            .unwrap_or_else(|| Rectangle::from_size(self.output_size));

        let mut target = Rectangle::from_size(output_geo.size);
        if matches!(layer.layer(), Layer::Background | Layer::Bottom) {
            target = map.non_exclusive_zone();
        }
        target.loc -= layer_geo.loc;
        target.loc -= get_popup_toplevel_coords(popup);

        // No padding for layer-shell popups: it's not needed and unexpected.
        self.position_popup_within_rect(popup, target, false);
    }

    /// Position a popup within `target` (geometry-local): an XDG popup through
    /// its positioner, an input method popup below its text cursor.
    fn position_popup_within_rect(
        &self,
        popup: &PopupKind,
        target: Rectangle<i32, Logical>,
        padding: bool,
    ) {
        match popup {
            PopupKind::Xdg(popup) => popup.with_pending_state(|state| {
                state.geometry = if padding {
                    unconstrain_with_padding(state.positioner, target)
                } else {
                    state.positioner.get_unconstrained_geometry(target)
                };
            }),
            PopupKind::InputMethod(popup) => im::popup::place(popup, target),
        }
    }

    /// Handle commit for tracked popups. Returns true if this was a popup.
    pub fn handle_popup_commit(&mut self, surface: &WlSurface) -> bool {
        self.popups.commit(surface);

        let Some(popup) = self.popups.find_popup(surface) else {
            return false;
        };

        let output = self.output_for_popup(&popup);
        match &popup {
            PopupKind::Xdg(xdg_popup) => {
                if !xdg_popup.is_initial_configure_sent() {
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
            }
            // Its size may change on any commit.
            PopupKind::InputMethod(_) => self.unconstrain_popup(&popup),
        }

        if let Some(output) = output {
            self.queue_redraw(&output);
        }

        true
    }
}
