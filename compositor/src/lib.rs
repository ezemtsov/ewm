//! EWM - Emacs Wayland Manager
//!
//! Wayland compositor core library.
pub mod backend;
pub mod cursor;
#[cfg(feature = "screencast")]
pub mod dbus;
pub mod event;
pub mod im_relay;
pub mod input;
pub mod ipc;
#[cfg(feature = "screencast")]
pub mod pipewire;
pub mod protocols;
pub mod render;
mod module;

pub use event::{Event, OutputInfo, OutputMode};

pub use backend::DrmBackendState;

use smithay::{
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_input_method_manager,
    delegate_output, delegate_primary_selection, delegate_seat, delegate_shm,
    delegate_text_input_manager, delegate_xdg_shell,
    desktop::{
        find_popup_root_surface, get_popup_toplevel_coords, PopupKind, PopupManager, Space, Window,
    },
    output::Output,
    input::{
        keyboard::xkb::keysyms,
        keyboard::{Layout, ModifiersState},
        Seat, SeatHandler, SeatState,
    },
    reexports::{
        calloop::{
            generic::Generic, Interest, LoopHandle, LoopSignal, Mode as CalloopMode, PostAction,
            RegistrationToken,
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState,
        wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle, Resource,
        },
    },
    utils::{Rectangle, Size, SERIAL_COUNTER},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            get_parent, is_sync_subsurface, CompositorClientState, CompositorHandler,
            CompositorState,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        seat::WaylandFocus,
        selection::{
            data_device::{
                set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
                ServerDndGrabHandler,
            },
            primary_selection::{
                set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
            },
            SelectionHandler,
        },
        shell::xdg::{
            decoration::{XdgDecorationHandler, XdgDecorationState},
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
        output::OutputManagerState,
        socket::ListeningSocketSource,
        text_input::TextInputManagerState,
        input_method::{InputMethodHandler, InputMethodManagerState, PopupSurface as IMPopupSurface},
    },
};
use crate::protocols::screencopy::{Screencopy, ScreencopyHandler, ScreencopyManagerState};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::Arc;
use std::mem;
use tracing::{error, info, warn};

/// Redraw state machine for proper VBlank synchronization.
///
/// Redraw state is owned by the compositor, not the backend.
/// This allows any code with access to Ewm to queue redraws.
#[derive(Debug, Default)]
pub enum RedrawState {
    /// No redraw pending, output is idle
    #[default]
    Idle,
    /// A redraw has been requested but not yet started
    Queued,
    /// Frame has been queued to DRM, waiting for VBlank
    /// redraw_needed tracks if another redraw was requested while waiting
    WaitingForVBlank { redraw_needed: bool },
    /// No damage, using estimated VBlank timer instead of real one
    WaitingForEstimatedVBlank(RegistrationToken),
    /// Estimated VBlank timer active AND a new redraw was queued
    WaitingForEstimatedVBlankAndQueued(RegistrationToken),
}

impl RedrawState {
    /// Transition to request a redraw
    pub fn queue_redraw(self) -> Self {
        match self {
            RedrawState::Idle => RedrawState::Queued,
            RedrawState::WaitingForVBlank { .. } => RedrawState::WaitingForVBlank { redraw_needed: true },
            RedrawState::WaitingForEstimatedVBlank(token) => {
                RedrawState::WaitingForEstimatedVBlankAndQueued(token)
            }
            other => other, // Already queued, no-op
        }
    }
}

/// Per-output state for redraw synchronization
pub struct OutputState {
    pub redraw_state: RedrawState,
    /// Refresh interval in microseconds (for estimated VBlank timer)
    pub refresh_interval_us: u64,
}

impl Default for OutputState {
    fn default() -> Self {
        Self {
            redraw_state: RedrawState::Idle,
            refresh_interval_us: 16_667, // ~60Hz default
        }
    }
}

/// Kill combo: Super+Ctrl+Backspace
/// Returns true if this key event is the kill combo
/// Note: keycode 22 (X11/xkb) or 14 (evdev) for Backspace
pub fn is_kill_combo(keycode: u32, ctrl: bool, logo: bool) -> bool {
    (keycode == 14 || keycode == 22) && ctrl && logo
}

/// Cached surface info for change detection
#[derive(Clone, Default)]
struct SurfaceInfo {
    app_id: String,
    title: String,
}

/// A single view of a surface (position in an Emacs window)
#[derive(Deserialize, Clone, Debug)]
pub struct SurfaceView {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub active: bool, // True for the view in the selected Emacs window
}

/// Commands received from Emacs
#[derive(Deserialize)]
#[serde(tag = "cmd")]
enum Command {
    #[serde(rename = "layout")]
    Layout { id: u32, x: i32, y: i32, w: u32, h: u32 },
    #[serde(rename = "views")]
    Views { id: u32, views: Vec<SurfaceView> },
    #[serde(rename = "hide")]
    Hide { id: u32 },
    #[serde(rename = "close")]
    Close { id: u32 },
    #[serde(rename = "focus")]
    Focus { id: u32 },
    #[serde(rename = "warp-pointer")]
    WarpPointer { x: f64, y: f64 },
    #[serde(rename = "screenshot")]
    Screenshot { path: Option<String> },
    #[serde(rename = "intercept-keys")]
    InterceptKeys { keys: Vec<InterceptedKey> },
    #[serde(rename = "configure-output")]
    ConfigureOutput {
        name: String,
        x: Option<i32>,
        y: Option<i32>,
        #[allow(dead_code)]
        width: Option<i32>,
        #[allow(dead_code)]
        height: Option<i32>,
        #[allow(dead_code)]
        refresh: Option<i32>,
        #[allow(dead_code)]
        scale: Option<f64>,
        enabled: Option<bool>,
    },
    #[serde(rename = "assign-output")]
    AssignOutput { id: u32, output: String },
    #[serde(rename = "prepare-frame")]
    PrepareFrame { output: String },
    #[serde(rename = "configure-xkb")]
    ConfigureXkb { layouts: String, options: Option<String> },
    #[serde(rename = "switch-layout")]
    SwitchLayout { layout: String },
    #[serde(rename = "get-layouts")]
    GetLayouts,
    #[serde(rename = "im-commit")]
    ImCommit { text: String },
    #[serde(rename = "text-input-intercept")]
    TextInputIntercept { enabled: bool },
}

/// Key identifier: either a keysym integer or a named key string
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum KeyId {
    Keysym(u32),
    Named(String),
}

impl KeyId {
    /// Convert to keysym, mapping named keys to XKB keysyms
    fn to_keysym(&self) -> Option<u32> {
        match self {
            KeyId::Keysym(k) => Some(*k),
            KeyId::Named(name) => match name.as_str() {
                // Arrow keys
                "left" => Some(keysyms::KEY_Left),
                "right" => Some(keysyms::KEY_Right),
                "up" => Some(keysyms::KEY_Up),
                "down" => Some(keysyms::KEY_Down),
                // Navigation
                "home" => Some(keysyms::KEY_Home),
                "end" => Some(keysyms::KEY_End),
                "prior" => Some(keysyms::KEY_Prior),
                "next" => Some(keysyms::KEY_Next),
                "insert" => Some(keysyms::KEY_Insert),
                "delete" => Some(keysyms::KEY_Delete),
                // Function keys
                "f1" => Some(keysyms::KEY_F1),
                "f2" => Some(keysyms::KEY_F2),
                "f3" => Some(keysyms::KEY_F3),
                "f4" => Some(keysyms::KEY_F4),
                "f5" => Some(keysyms::KEY_F5),
                "f6" => Some(keysyms::KEY_F6),
                "f7" => Some(keysyms::KEY_F7),
                "f8" => Some(keysyms::KEY_F8),
                "f9" => Some(keysyms::KEY_F9),
                "f10" => Some(keysyms::KEY_F10),
                "f11" => Some(keysyms::KEY_F11),
                "f12" => Some(keysyms::KEY_F12),
                // Special keys
                "return" => Some(keysyms::KEY_Return),
                "tab" => Some(keysyms::KEY_Tab),
                "escape" => Some(keysyms::KEY_Escape),
                "backspace" => Some(keysyms::KEY_BackSpace),
                _ => {
                    warn!("Unknown key name: {}", name);
                    None
                }
            },
        }
    }
}

/// Intercepted key: key + required modifiers (sent pre-parsed from Emacs)
#[derive(Debug, Clone, Deserialize)]
pub struct InterceptedKey {
    key: KeyId,
    #[serde(default)]
    ctrl: bool,
    #[serde(default)]
    alt: bool,
    #[serde(default)]
    shift: bool,
    #[serde(rename = "super", default)]
    logo: bool,
}

impl InterceptedKey {
    /// Check if this key matches the given keysym and modifiers
    pub fn matches(&self, keysym: u32, mods: &ModifiersState) -> bool {
        let target_keysym = match self.key.to_keysym() {
            Some(k) => k,
            None => return false,
        };

        // Handle case-insensitive letter matching (A-Z vs a-z)
        let keysym_match = target_keysym == keysym
            || (keysym >= keysyms::KEY_A
                && keysym <= keysyms::KEY_Z
                && target_keysym == keysym - keysyms::KEY_A + keysyms::KEY_a);

        keysym_match
            && self.ctrl == mods.ctrl
            && self.alt == mods.alt
            && (self.shift == mods.shift || (keysym >= keysyms::KEY_A && keysym <= keysyms::KEY_Z))
            && self.logo == mods.logo
    }
}

pub struct Ewm {
    pub stop_signal: Option<LoopSignal>,
    pub space: Space<Window>,
    pub display_handle: DisplayHandle,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub seat: Seat<Self>,

    // IPC
    next_surface_id: u32,
    pub window_ids: HashMap<Window, u32>,
    pub id_windows: HashMap<u32, Window>,
    surface_info: HashMap<u32, SurfaceInfo>,
    pub surface_views: HashMap<u32, Vec<SurfaceView>>,
    pending_events: Vec<Event>,

    // Output
    pub output_size: (i32, i32),
    pub outputs: Vec<OutputInfo>,

    // Input
    pub pointer_location: (f64, f64),
    pub focused_surface_id: u32,
    pub keyboard_focus: Option<WlSurface>,
    pub intercepted_keys: Vec<InterceptedKey>,

    // Emacs client tracking - used to identify which surfaces belong to Emacs
    // vs external applications (for key interception)
    pub emacs_pid: Option<u32>,
    pub emacs_surfaces: std::collections::HashSet<u32>,

    // Screenshot request
    pub pending_screenshot: Option<String>,

    // Per-output state (redraw state machine)
    pub output_state: HashMap<Output, OutputState>,

    // Pending early imports (surfaces that need dmabuf import before rendering)
    pub pending_early_imports: Vec<WlSurface>,

    // Pending frame-to-output assignments (from prepare-frame command)
    pending_frame_outputs: Vec<String>,

    // Screencopy protocol state
    pub screencopy_state: ScreencopyManagerState,

    // Output manager state (provides xdg-output protocol)
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,

    // Text input state (provides zwp_text_input_v3 protocol)
    #[allow(dead_code)]
    pub text_input_state: TextInputManagerState,

    // Input method state (provides zwp_input_method_v2 protocol)
    #[allow(dead_code)]
    pub input_method_state: InputMethodManagerState,

    // When true, intercept all keys and send to Emacs for text input
    pub text_input_intercept: bool,

    // Popup manager for XDG popups
    pub popups: PopupManager,

    // PipeWire for screen sharing (initialized lazily)
    #[cfg(feature = "screencast")]
    pub pipewire: Option<pipewire::PipeWire>,

    // Shared output info for D-Bus ScreenCast (thread-safe)
    #[cfg(feature = "screencast")]
    pub dbus_outputs: std::sync::Arc<std::sync::Mutex<Vec<dbus::OutputInfo>>>,

    // Active screen cast sessions (keyed by session_id)
    #[cfg(feature = "screencast")]
    pub screen_casts: std::collections::HashMap<usize, pipewire::stream::Cast>,

    // D-Bus servers (must be kept alive for interfaces to work)
    #[cfg(feature = "screencast")]
    pub dbus_servers: Option<dbus::DBusServers>,

    // XKB layout state
    pub xkb_layout_names: Vec<String>,
    pub xkb_current_layout: usize,

    // Input method relay (self-connection to activate input_method protocol)
    #[allow(dead_code)]
    pub im_relay: Option<im_relay::ImRelay>,
}

impl Ewm {
    pub fn new(display_handle: DisplayHandle) -> Self {
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let dmabuf_state = DmabufState::new();
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&display_handle);

        let mut seat = seat_state.new_wl_seat(&display_handle, "seat0");
        seat.add_keyboard(Default::default(), 200, 25).unwrap();
        seat.add_pointer();

        // Initialize screencopy state before moving display_handle
        let screencopy_state = ScreencopyManagerState::new::<Self, _>(&display_handle, |_| true);

        // Initialize output manager with xdg-output protocol support
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);

        // Initialize text input for input method support
        let text_input_state = TextInputManagerState::new::<Self>(&display_handle);

        // Initialize input method manager (allows Emacs to act as input method)
        let input_method_state = InputMethodManagerState::new::<Self, _>(&display_handle, |_| true);

        Self {
            stop_signal: None,
            space: Space::default(),
            display_handle,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            dmabuf_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            seat,
            next_surface_id: 1,
            window_ids: HashMap::new(),
            id_windows: HashMap::new(),
            surface_info: HashMap::new(),
            surface_views: HashMap::new(),
            pending_events: Vec::new(),
            output_size: (800, 600),
            outputs: Vec::new(),
            pointer_location: (0.0, 0.0),
            focused_surface_id: 1,
            keyboard_focus: None,
            intercepted_keys: Vec::new(),
            emacs_pid: None,
            emacs_surfaces: std::collections::HashSet::new(),
            pending_screenshot: None,
            output_state: HashMap::new(),
            pending_early_imports: Vec::new(),
            pending_frame_outputs: Vec::new(),
            screencopy_state,
            output_manager_state,
            text_input_state,
            input_method_state,
            text_input_intercept: false,
            popups: PopupManager::default(),
            #[cfg(feature = "screencast")]
            pipewire: None,
            #[cfg(feature = "screencast")]
            dbus_outputs: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(feature = "screencast")]
            screen_casts: std::collections::HashMap::new(),
            #[cfg(feature = "screencast")]
            dbus_servers: None,
            xkb_layout_names: vec!["us".to_string()],
            xkb_current_layout: 0,
            im_relay: None,
        }
    }

    /// Connect the input method relay after socket is ready
    pub fn connect_im_relay(&mut self, socket_path: &std::path::Path) {
        self.im_relay = im_relay::ImRelay::connect(socket_path);
        if self.im_relay.is_some() {
            info!("Input method relay connected successfully");
        } else {
            warn!("Failed to connect input method relay");
        }
    }

    /// Queue a redraw for all outputs
    pub fn queue_redraw_all(&mut self) {
        for state in self.output_state.values_mut() {
            state.redraw_state = mem::take(&mut state.redraw_state).queue_redraw();
        }
    }

    /// Queue a redraw for a specific output
    pub fn queue_redraw(&mut self, output: &Output) {
        if let Some(state) = self.output_state.get_mut(output) {
            state.redraw_state = mem::take(&mut state.redraw_state).queue_redraw();
        }
    }

    /// Set the loop signal for graceful shutdown
    pub fn set_stop_signal(&mut self, signal: LoopSignal) {
        self.stop_signal = Some(signal);
    }

    /// Request event loop to stop
    pub fn stop(&self) {
        if let Some(signal) = &self.stop_signal {
            info!("Stopping event loop");
            signal.stop();
        }
    }

    /// Stop a screen cast session properly (PipeWire + D-Bus cleanup)
    #[cfg(feature = "screencast")]
    pub fn stop_cast(&mut self, session_id: usize) {
        use tracing::debug;

        debug!(session_id, "stop_cast");

        // Remove cast from our map (Drop impl disconnects PipeWire stream)
        if self.screen_casts.remove(&session_id).is_none() {
            return; // Cast not found
        }

        // Call Session::stop() on D-Bus to emit Closed signal
        if let Some(ref dbus) = self.dbus_servers {
            if let Some(ref conn) = dbus.conn_screen_cast {
                let server = conn.object_server();
                let path = format!("/org/gnome/Mutter/ScreenCast/Session/u{}", session_id);

                if let Ok(iface) = server.interface::<_, dbus::screen_cast::Session>(path.as_str()) {
                    async_io::block_on(async {
                        let signal_emitter = iface.signal_emitter().clone();
                        iface
                            .get()
                            .stop(server.inner(), signal_emitter)
                            .await
                    });
                }
            }
        }
    }

    /// Set the Emacs process PID for client identification
    pub fn set_emacs_pid(&mut self, pid: u32) {
        info!("Tracking Emacs PID: {}", pid);
        self.emacs_pid = Some(pid);
    }

    /// Check if a surface belongs to the Emacs client
    fn is_emacs_client(&self, surface: &WlSurface) -> bool {
        if let Some(emacs_pid) = self.emacs_pid {
            if let Ok(client) = self.display_handle.get_client(surface.id()) {
                if let Ok(creds) = client.get_credentials(&self.display_handle) {
                    return creds.pid == emacs_pid as i32;
                }
            }
        }
        false
    }

    /// Check if focus is on an Emacs surface (for key interception decisions)
    pub fn is_focus_on_emacs(&self) -> bool {
        self.emacs_surfaces.contains(&self.focused_surface_id)
    }

    /// Set focus to a surface and notify Emacs
    pub fn set_focus(&mut self, id: u32) {
        if id != self.focused_surface_id && id != 0 {
            self.focused_surface_id = id;
            // Notify Emacs about focus change (skip Emacs frames, they handle their own focus)
            if !self.emacs_surfaces.contains(&id) {
                self.queue_event(Event::Focus { id });
            }
        }
    }

    /// Focus a surface, updating internal state, keyboard focus, and text input.
    /// If `notify_emacs` is true, sends Event::Focus to Emacs.
    pub fn focus_surface(&mut self, id: u32, notify_emacs: bool) {
        self.focused_surface_id = id;
        if notify_emacs {
            self.queue_event(Event::Focus { id });
        }

        if let Some(window) = self.id_windows.get(&id) {
            if let Some(surface) = window.wl_surface() {
                let keyboard = self.seat.get_keyboard().unwrap();
                let focus_surface = surface.into_owned();
                self.keyboard_focus = Some(focus_surface.clone());
                keyboard.set_focus(self, Some(focus_surface.clone()), SERIAL_COUNTER.next_serial());
                self.update_text_input_focus(Some(&focus_surface), Some(id));
            }
        }
    }

    /// Queue an event to be sent to Emacs (via both IPC and module queue)
    pub(crate) fn queue_event(&mut self, event: Event) {
        // Push to module queue (for embedded mode)
        module::push_event(event.clone());

        // Push to IPC queue (for socket mode)
        self.pending_events.push(event);
    }

    /// Update text_input focus for input method support.
    /// Skips Emacs surfaces since Emacs handles its own input methods.
    pub fn update_text_input_focus(&self, surface: Option<&WlSurface>, surface_id: Option<u32>) {
        use smithay::wayland::text_input::TextInputSeat;
        let text_input = self.seat.text_input();

        // Skip if this is an Emacs surface
        let is_emacs = surface_id.map_or(false, |id| self.emacs_surfaces.contains(&id));

        if is_emacs || surface.is_none() {
            text_input.leave();
            text_input.set_focus(None);
        } else if let Some(s) = surface {
            text_input.set_focus(Some(s.clone()));
            text_input.enter();
        }
    }

    /// Get surface ID from a WlSurface
    pub fn surface_id(&self, surface: &WlSurface) -> Option<u32> {
        self.window_ids
            .iter()
            .find(|(w, _)| w.wl_surface().map(|s| &*s == surface).unwrap_or(false))
            .map(|(_, &id)| id)
    }

    /// Find the surface under a point, checking popups first (they're on top)
    /// Returns the surface and its location in global coordinates
    pub fn surface_under_point(
        &self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> Option<(WlSurface, smithay::utils::Point<f64, smithay::utils::Logical>)> {
        use smithay::wayland::seat::WaylandFocus;

        // Check popups first (they're on top)
        for window in self.space.elements() {
            if let Some(surface) = window.wl_surface() {
                let window_loc = self.space.element_location(window).unwrap_or_default();
                let window_geo = window.geometry();

                for (popup, popup_offset) in PopupManager::popups_for_surface(&surface) {
                    let popup_loc =
                        (window_loc + window_geo.loc + popup_offset - popup.geometry().loc).to_f64();
                    let pos_in_popup = pos - popup_loc;
                    let popup_geo = popup.geometry();

                    if pos_in_popup.x >= 0.0
                        && pos_in_popup.y >= 0.0
                        && pos_in_popup.x < popup_geo.size.w as f64
                        && pos_in_popup.y < popup_geo.size.h as f64
                    {
                        return Some((popup.wl_surface().clone(), popup_loc));
                    }
                }
            }
        }

        // Fall back to toplevels
        self.space.element_under(pos).and_then(|(window, loc)| {
            window
                .wl_surface()
                .map(|s| (s.into_owned(), loc.to_f64()))
        })
    }

    /// Get the output where a surface is located
    fn get_surface_output(&self, surface_id: u32) -> Option<String> {
        let window = self.id_windows.get(&surface_id)?;
        let window_loc = self.space.element_location(window)?;

        // Find which output contains this window's location
        for output in self.space.outputs() {
            if let Some(geo) = self.space.output_geometry(output) {
                if window_loc.x >= geo.loc.x
                    && window_loc.x < geo.loc.x + geo.size.w
                    && window_loc.y >= geo.loc.y
                    && window_loc.y < geo.loc.y + geo.size.h
                {
                    return Some(output.name());
                }
            }
        }
        // Fallback to first output
        self.space.outputs().next().map(|o| o.name())
    }

    /// Get the output where the focused surface is located
    fn get_focused_output(&self) -> Option<String> {
        self.get_surface_output(self.focused_surface_id)
    }

    /// Get all outputs that a window intersects with (considering views)
    #[allow(dead_code)]
    fn outputs_for_window(&self, window_id: u32) -> Vec<Output> {
        use smithay::utils::{Logical, Point, Rectangle, Size};

        let mut outputs = Vec::new();

        // Check if window has views
        if let Some(views) = self.surface_views.get(&window_id) {
            // Get window geometry for view size
            let window_size = self.id_windows.get(&window_id)
                .map(|w| w.geometry().size)
                .unwrap_or_else(|| Size::from((100, 100)));

            for view in views {
                let view_rect: Rectangle<i32, Logical> = Rectangle::new(
                    Point::from((view.x, view.y)),
                    Size::from((window_size.w, window_size.h)),
                );

                for output in self.space.outputs() {
                    if let Some(output_geo) = self.space.output_geometry(output) {
                        if output_geo.overlaps(view_rect) && !outputs.contains(output) {
                            outputs.push(output.clone());
                        }
                    }
                }
            }
        } else if let Some(window) = self.id_windows.get(&window_id) {
            // No views - use window's position in space
            if let Some(loc) = self.space.element_location(window) {
                let window_geo = window.geometry();
                let window_rect: Rectangle<i32, Logical> = Rectangle::new(
                    loc,
                    Size::from((window_geo.size.w, window_geo.size.h)),
                );

                for output in self.space.outputs() {
                    if let Some(output_geo) = self.space.output_geometry(output) {
                        if output_geo.overlaps(window_rect) {
                            outputs.push(output.clone());
                        }
                    }
                }
            }
        }

        outputs
    }

    /// Find the Emacs surface on the same output as the focused surface
    pub fn get_emacs_surface_for_focused_output(&self) -> u32 {
        let focused_output = self.get_focused_output();

        // Find an Emacs surface on the same output
        for &emacs_id in &self.emacs_surfaces {
            if self.get_surface_output(emacs_id) == focused_output {
                return emacs_id;
            }
        }

        // Fallback to surface 1 (primary Emacs)
        1
    }

    /// Recalculate total output size from current space geometry
    pub fn recalculate_output_size(&mut self) {
        let (total_width, total_height) =
            self.space
                .outputs()
                .fold((0i32, 0i32), |(w, h), output| {
                    if let Some(geo) = self.space.output_geometry(output) {
                        (w.max(geo.loc.x + geo.size.w), h.max(geo.loc.y + geo.size.h))
                    } else {
                        (w, h)
                    }
                });
        self.output_size = (total_width, total_height);
    }

    /// Send output detected event to Emacs
    pub fn send_output_detected(&mut self, output: OutputInfo) {
        self.queue_event(Event::OutputDetected(output));
    }

    /// Send output disconnected event to Emacs
    pub fn send_output_disconnected(&mut self, name: &str) {
        self.queue_event(Event::OutputDisconnected {
            name: name.to_string(),
        });
    }

    /// Check if there are pending screencopy requests for any output
    pub fn has_pending_screencopies(&self) -> bool {
        // This is a workaround since we can't easily check the internal state
        // without mutable access. We'll always return false here and let
        // the render loop handle it with the mutable state.
        false
    }

    /// Unconstrain a popup's position to keep it within screen bounds
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };

        // Find the window that owns this popup
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.wl_surface().map(|s| *s == root).unwrap_or(false))
        else {
            return;
        };

        // Get window location in global coordinates
        let window_loc = self.space.element_location(window).unwrap_or_default();
        let window_geo = window.geometry();

        // Target rectangle is the full output, adjusted for popup's position in window
        let mut target = Rectangle::from_size(Size::from(self.output_size));
        target.loc -= window_loc + window_geo.loc;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    pub fn init_wayland_listener(
        display: Display<Ewm>,
        event_loop: &LoopHandle<State>,
    ) -> Result<std::ffi::OsString, Box<dyn std::error::Error>> {
        let socket_name_env =
            std::env::var("EWM_SOCKET").unwrap_or_else(|_| "wayland-ewm".to_string());
        info!("Creating Wayland socket with name: {}", socket_name_env);
        let socket = ListeningSocketSource::with_name(&socket_name_env)?;
        let socket_name = socket.socket_name().to_os_string();

        // Socket listener - uses display_handle from state
        event_loop
            .insert_source(socket, |client, _, state| {
                if let Err(e) = state
                    .ewm
                    .display_handle
                    .insert_client(client, Arc::new(ClientState::default()))
                {
                    warn!("Failed to insert client: {}", e);
                }
            })
            .expect("Failed to init wayland socket source");

        // Display source - owns the Display for dispatch_clients
        // Display lifetime is tied to event loop, not State
        let display_source = Generic::new(display, Interest::READ, CalloopMode::Level);
        event_loop
            .insert_source(display_source, |_, display, state| {
                // SAFETY: we don't drop the display while the event loop is running
                let display = unsafe { display.get_mut() };
                display.dispatch_clients(&mut state.ewm).unwrap();
                Ok(PostAction::Continue)
            })
            .expect("Failed to init wayland source");

        Ok(socket_name)
    }

    fn get_client_process_name(&self, surface: &WlSurface) -> Option<String> {
        let client = self.display_handle.get_client(surface.id()).ok()?;
        let creds = client.get_credentials(&self.display_handle).ok()?;
        let comm_path = format!("/proc/{}/comm", creds.pid);
        std::fs::read_to_string(&comm_path)
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn check_surface_info_changes(&mut self, surface: &WlSurface) {
        let window = self
            .space
            .elements()
            .find(|w| w.wl_surface().map(|s| s.as_ref() == surface).unwrap_or(false))
            .cloned();

        if let Some(window) = window {
            if let Some(&id) = self.window_ids.get(&window) {
                // Skip title change events for Emacs surfaces
                if self.emacs_surfaces.contains(&id) {
                    return;
                }

                let (app_id, title) = smithay::wayland::compositor::with_states(surface, |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .map(|d| {
                            let data = d.lock().unwrap();
                            (
                                data.app_id.clone().unwrap_or_default(),
                                data.title.clone().unwrap_or_default(),
                            )
                        })
                        .unwrap_or_default()
                });

                let cached = self.surface_info.get(&id);
                let changed = match cached {
                    Some(info) => info.app_id != app_id || info.title != title,
                    None => true,
                };

                if changed && (!app_id.is_empty() || !title.is_empty()) {
                    info!(
                        "Surface {} info changed: app='{}' title='{}'",
                        id, app_id, title
                    );
                    self.surface_info.insert(
                        id,
                        SurfaceInfo {
                            app_id: app_id.clone(),
                            title: title.clone(),
                        },
                    );
                    self.queue_event(Event::Title {
                        id,
                        app: app_id,
                        title,
                    });
                }
            }
        }
    }
}

// Client tracking
#[derive(Default)]
pub struct ClientState {
    pub compositor: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        info!("Client connected: {:?}", client_id);
    }
    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        info!("Client disconnected: {:?}, reason: {:?}", client_id, reason);
    }
}

// Buffer handling
impl BufferHandler for Ewm {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

// Compositor protocol
impl CompositorHandler for Ewm {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // Queue early import for DRM backend (processed in main loop)
        self.pending_early_imports.push(surface.clone());

        // Handle popup commits
        self.popups.commit(surface);
        if let Some(popup) = self.popups.find_popup(surface) {
            if let PopupKind::Xdg(ref xdg_popup) = popup {
                if !xdg_popup.is_initial_configure_sent() {
                    xdg_popup.send_configure().expect("initial configure failed");
                }
            }
        }

        // Early return for sync subsurfaces - parent commit will handle them
        if is_sync_subsurface(surface) {
            return;
        }

        // Find the root surface (toplevel) for this surface
        let mut root_surface = surface.clone();
        while let Some(parent) = get_parent(&root_surface) {
            root_surface = parent;
        }

        // Find the window that owns this root surface
        let window_and_id = self.space.elements().find_map(|window| {
            window.wl_surface().and_then(|ws| {
                if *ws == root_surface {
                    self.window_ids.get(window).map(|&id| (window.clone(), id))
                } else {
                    None
                }
            })
        });

        if let Some((window, _id)) = window_and_id {
            // Call on_commit only for this specific window
            window.on_commit();

            // Queue redraw for all outputs
            self.queue_redraw_all();

            // Check for title/app_id changes (only for toplevels)
            self.check_surface_info_changes(surface);
        }
        // For surfaces without a toplevel (popups, layer surfaces, etc.),
        // the parent's commit or other handlers will manage redraw
    }
}
delegate_compositor!(Ewm);

// Shared memory
impl ShmHandler for Ewm {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
delegate_shm!(Ewm);

// DMA-BUF
impl DmabufHandler for Ewm {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        let _ = notifier.successful::<Ewm>();
    }
}
delegate_dmabuf!(Ewm);

// Seat / input
impl SeatHandler for Ewm {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        tracing::info!("focus_changed: {:?}", focused.map(|s| s.id()));

        let client = focused.and_then(|s| self.display_handle.get_client(s.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);

        // Update text_input focus for input method support
        let surface_id = focused.and_then(|s| self.surface_id(s));
        self.update_text_input_focus(focused, surface_id);
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}
delegate_seat!(Ewm);

// Data device / selection
impl SelectionHandler for Ewm {
    type SelectionUserData = ();
}
impl ClientDndGrabHandler for Ewm {}
impl ServerDndGrabHandler for Ewm {}
impl DataDeviceHandler for Ewm {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
delegate_data_device!(Ewm);

impl PrimarySelectionHandler for Ewm {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}
delegate_primary_selection!(Ewm);

// Output
impl smithay::wayland::output::OutputHandler for Ewm {}
delegate_output!(Ewm);

// Text Input (for input method support)
delegate_text_input_manager!(Ewm);

// Input Method (allows Emacs to act as input method)
impl InputMethodHandler for Ewm {
    fn new_popup(&mut self, _surface: IMPopupSurface) {
        // Input method popups not supported yet
    }

    fn dismiss_popup(&mut self, _surface: IMPopupSurface) {
        // Input method popups not supported yet
    }

    fn popup_repositioned(&mut self, _surface: IMPopupSurface) {
        // Input method popups not supported yet
    }

    fn parent_geometry(&self, _parent: &WlSurface) -> Rectangle<i32, smithay::utils::Logical> {
        Rectangle::default()
    }
}
delegate_input_method_manager!(Ewm);

// XDG Shell
impl XdgShellHandler for Ewm {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_surface_id;
        self.next_surface_id += 1;

        // Check if this surface belongs to the Emacs client
        let is_emacs = self.is_emacs_client(surface.wl_surface());
        if is_emacs {
            info!("Surface {} is an Emacs surface", id);
            self.emacs_surfaces.insert(id);
        }

        let app = smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().unwrap().app_id.clone())
        })
        .unwrap_or_else(|| {
            self.get_client_process_name(surface.wl_surface())
                .unwrap_or_else(|| "unknown".to_string())
        });

        surface.with_pending_state(|state| {
            state.size = Some(self.output_size.into());
            state.states.set(XdgToplevelState::Maximized);
            state.states.set(XdgToplevelState::Activated);
        });
        surface.send_configure();

        let window = Window::new_wayland_window(surface);
        self.window_ids.insert(window.clone(), id);
        self.id_windows.insert(id, window.clone());

        // Determine target output:
        // 1. Emacs frames: from prepare-frame command (position at output, size to fill)
        // 2. Other surfaces: from currently focused surface's output (offscreen, layout handles)
        let frame_output = if !self.pending_frame_outputs.is_empty() {
            Some(self.pending_frame_outputs.remove(0))
        } else {
            None
        };
        let target_output = frame_output.clone().or_else(|| self.get_focused_output());

        // Position based on surface type
        let position = if id == 1 {
            (0, 0)
        } else if let Some(ref output_name) = frame_output {
            // Emacs frame: position at output origin
            self.space
                .outputs()
                .find(|o| o.name() == *output_name)
                .and_then(|o| self.space.output_geometry(o))
                .map(|geo| (geo.loc.x, geo.loc.y))
                .unwrap_or((-10000, -10000))
        } else {
            // External app: offscreen, Emacs layout will position
            (-10000, -10000)
        };
        self.space.map_element(window.clone(), position, false);

        // Resize Emacs frames to fill their output
        if let Some(ref output_name) = frame_output {
            if let Some(geo) = self
                .space
                .outputs()
                .find(|o| o.name() == *output_name)
                .and_then(|o| self.space.output_geometry(o))
            {
                window.toplevel().map(|t| {
                    t.with_pending_state(|state| {
                        state.size = Some((geo.size.w, geo.size.h).into());
                    });
                    t.send_configure();
                });
            }
        }

        // Send event to Emacs with target output
        if id != 1 {
            self.queue_event(Event::New {
                id,
                app: app.clone(),
                output: target_output.clone(),
            });
        }
        info!(
            "New toplevel {} ({}) -> {:?}",
            id,
            app,
            target_output.as_deref().unwrap_or("unknown")
        );
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel().map(|t| t == &surface).unwrap_or(false))
            .cloned();

        if let Some(window) = window {
            if let Some(id) = self.window_ids.remove(&window) {
                // Capture output before removing, in case this was the focused surface
                let was_focused = self.focused_surface_id == id;
                let output = self.get_surface_output(id);

                self.id_windows.remove(&id);
                self.surface_info.remove(&id);
                self.surface_views.remove(&id);
                self.emacs_surfaces.remove(&id);
                self.queue_event(Event::Close { id });
                info!("Toplevel {} destroyed", id);

                // If destroyed surface had focus, refocus Emacs on same output
                if was_focused {
                    let emacs_id = output
                        .as_ref()
                        .and_then(|out| {
                            self.emacs_surfaces
                                .iter()
                                .find(|&&eid| self.get_surface_output(eid).as_ref() == Some(out))
                                .copied()
                        })
                        .unwrap_or(1);
                    self.focus_surface(emacs_id, true);
                }
            }
            self.space.unmap_elem(&window);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            warn!("error tracking popup: {err:?}");
        }
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

        if let Err(err) = self.popups.grab_popup(root, popup, &self.seat, serial) {
            warn!("error grabbing popup: {err:?}");
        }
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
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn popup_destroyed(&mut self, _surface: PopupSurface) {
        // Queue redraw to clear the popup from screen
        self.queue_redraw_all();
    }
}
delegate_xdg_shell!(Ewm);

// XDG Decoration
impl XdgDecorationHandler for Ewm {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(
        &mut self,
        toplevel: ToplevelSurface,
        _mode: smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    ) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, _toplevel: ToplevelSurface) {}
}
smithay::delegate_xdg_decoration!(Ewm);

// Screencopy protocol
impl ScreencopyHandler for Ewm {
    fn frame(&mut self, manager: &ZwlrScreencopyManagerV1, screencopy: Screencopy) {
        // Queue all screencopy requests for processing during render
        // (both with_damage and immediate requests are handled in the render loop)
        if let Some(queue) = self.screencopy_state.get_queue_mut(manager) {
            queue.push(screencopy);
        }
    }

    fn screencopy_state(&mut self) -> &mut ScreencopyManagerState {
        &mut self.screencopy_state
    }
}
delegate_screencopy!(Ewm);

/// Shared state for compositor event loop (passed to all handlers)
///
/// Note: Display is owned by the event loop (via Generic source), not by State.
pub struct State {
    pub backend: DrmBackendState,
    pub ewm: Ewm,
    pub emacs: Option<UnixStream>,
    /// IPC stream registration token (for cleanup on reconnect)
    pub ipc_stream_token: Option<smithay::reexports::calloop::RegistrationToken>,
    /// Spawned client process (for standalone mode)
    pub client_process: Option<std::process::Child>,
}

impl State {
    /// Per-frame processing callback for the event loop.
    /// Called after each dispatch to handle redraws, events, and client flushing.
    pub fn refresh_and_flush_clients(&mut self) {
        // Check if stop was requested from module (ewm-stop)
        if crate::module::STOP_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            info!("Stop requested from Emacs, shutting down");
            self.ewm.stop();
        }

        // Check if client exited (standalone mode)
        if let Some(ref mut child) = self.client_process {
            if let Ok(Some(status)) = child.try_wait() {
                info!("Client exited with status: {}", status);
                self.ewm.stop();
            }
        }

        // Process pending early imports
        let pending_imports: Vec<_> = self.ewm.pending_early_imports.drain(..).collect();
        for surface in pending_imports {
            self.backend.early_import(&surface);
        }

        // Process any queued redraws
        self.backend.redraw_queued_outputs(&mut self.ewm);

        // Process IM relay events and send to Emacs
        self.process_im_events();

        // Flush pending events to Emacs
        self.flush_events();

        // Flush Wayland clients
        self.ewm.display_handle.flush_clients().unwrap();
    }

    /// Send an IPC event to Emacs
    fn send_event(&mut self, event: &Event) {
        if let Some(ref mut stream) = self.emacs {
            if let Ok(json) = serde_json::to_string(event) {
                if writeln!(stream, "{}", json).is_err() {
                    warn!("Failed to send event to Emacs, disconnecting");
                    self.emacs = None;
                } else {
                    let _ = stream.flush();
                }
            }
        }
    }

    /// Flush all pending events to Emacs
    pub fn flush_events(&mut self) {
        let events: Vec<_> = self.ewm.pending_events.drain(..).collect();
        for event in events {
            self.send_event(&event);
        }
    }

    /// Send output detected events for all known outputs
    pub fn send_output_events(&mut self) {
        let outputs: Vec<_> = self.ewm.outputs.clone();
        for output in outputs {
            self.send_event(&Event::OutputDetected(output));
        }
        // Signal that all outputs have been sent
        self.send_event(&Event::OutputsComplete);
    }

    /// Process IPC commands from a stream
    pub fn process_commands_from_stream(&mut self, stream: &mut UnixStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if let Ok(cmd) = serde_json::from_str::<Command>(&line) {
                self.handle_command(cmd);
            }
            line.clear();
        }
    }

    /// Handle a single IPC command
    fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::Layout { id, x, y, w, h } => {
                if let Some(window) = self.ewm.id_windows.get(&id) {
                    self.ewm.space.map_element(window.clone(), (x, y), true);
                    self.ewm.space.raise_element(window, true);
                    window.toplevel().map(|t| {
                        t.with_pending_state(|state| {
                            state.size = Some((w as i32, h as i32).into());
                        });
                        t.send_configure();
                    });
                    self.ewm.queue_redraw_all();
                    info!("Layout surface {} at ({}, {}) {}x{}", id, x, y, w, h);
                }
            }
            Command::Views { id, views } => {
                if let Some(window) = self.ewm.id_windows.get(&id) {
                    let primary_view = views.iter().find(|v| v.active).or_else(|| views.first());

                    if let Some(view) = primary_view {
                        self.ewm
                            .space
                            .map_element(window.clone(), (view.x, view.y), true);
                        self.ewm.space.raise_element(window, true);
                        window.toplevel().map(|t| {
                            t.with_pending_state(|state| {
                                state.size = Some((view.w as i32, view.h as i32).into());
                            });
                            t.send_configure();
                        });
                    }
                    self.ewm.surface_views.insert(id, views.clone());
                    self.ewm.queue_redraw_all();
                }
            }
            Command::Hide { id } => {
                if let Some(window) = self.ewm.id_windows.get(&id) {
                    self.ewm
                        .space
                        .map_element(window.clone(), (-10000, -10000), false);
                    self.ewm.surface_views.remove(&id);
                    self.ewm.queue_redraw_all();
                    info!("Hide surface {}", id);
                }
            }
            Command::Close { id } => {
                if let Some(window) = self.ewm.id_windows.get(&id) {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.send_close();
                        info!("Close surface {} (sent close request)", id);
                    }
                }
            }
            Command::Focus { id } => {
                if self.ewm.id_windows.contains_key(&id) {
                    self.ewm.focus_surface(id, false);
                    info!("Focus surface {}", id);
                }
            }
            Command::WarpPointer { x, y } => {
                self.ewm.pointer_location = (x, y);
                let pointer = self.ewm.seat.get_pointer().unwrap();
                let serial = SERIAL_COUNTER.next_serial();

                // Find surface under new pointer location
                let under = self
                    .ewm
                    .space
                    .element_under((x, y))
                    .and_then(|(window, loc)| {
                        window.wl_surface().map(|s| {
                            (
                                s.into_owned(),
                                smithay::utils::Point::from((loc.x as f64, loc.y as f64)),
                            )
                        })
                    });

                pointer.motion(
                    &mut self.ewm,
                    under,
                    &smithay::input::pointer::MotionEvent {
                        location: (x, y).into(),
                        serial,
                        time: 0,
                    },
                );
                pointer.frame(&mut self.ewm);

                self.ewm.queue_redraw_all();
            }
            Command::Screenshot { path } => {
                let target = path.unwrap_or_else(|| "/tmp/ewm-screenshot.png".to_string());
                info!("Screenshot requested: {}", target);
                self.ewm.pending_screenshot = Some(target);
            }
            Command::InterceptKeys { keys } => {
                self.ewm.intercepted_keys = keys;
                info!("Intercepted keys set: {:?}", self.ewm.intercepted_keys);
            }
            Command::ConfigureOutput {
                name,
                x,
                y,
                width,
                height,
                refresh,
                scale: _,
                enabled,
            } => {
                // Find output by name
                let output = self
                    .ewm
                    .space
                    .outputs()
                    .find(|o| o.name() == name)
                    .cloned();

                if let Some(output) = output {
                    // Handle enable/disable
                    if let Some(false) = enabled {
                        // Disable: unmap from space
                        self.ewm.space.unmap_output(&output);
                        info!("Disabled output {}", name);
                    } else {
                        // Handle mode change if width/height specified
                        if let (Some(w), Some(h)) = (width, height) {
                            self.backend.set_mode(&mut self.ewm, &name, w, h, refresh);
                        }

                        // Reposition if coordinates provided
                        let new_x = x.unwrap_or(0);
                        let new_y = y.unwrap_or(0);
                        let new_pos = (new_x, new_y);

                        self.ewm.space.map_output(&output, new_pos);

                        // Update output's internal position state
                        output.change_current_state(
                            None,
                            None,
                            None,
                            Some(new_pos.into()),
                        );

                        // Update our cached output info
                        for out_info in &mut self.ewm.outputs {
                            if out_info.name == name {
                                out_info.x = new_x;
                                out_info.y = new_y;
                            }
                        }

                        // Recalculate total output size
                        self.ewm.recalculate_output_size();

                        info!("Configured output {} at ({}, {})", name, new_x, new_y);
                    }

                    // Queue redraw
                    self.ewm.queue_redraw_all();
                } else {
                    warn!("Output not found: {}", name);
                }
            }
            Command::AssignOutput { id, output } => {
                // Find the output by name and get its geometry
                let output_geo = self
                    .ewm
                    .space
                    .outputs()
                    .find(|o| o.name() == output)
                    .and_then(|o| self.ewm.space.output_geometry(o));

                if let Some(geo) = output_geo {
                    if let Some(window) = self.ewm.id_windows.get(&id) {
                        // Position surface to fill the output
                        self.ewm
                            .space
                            .map_element(window.clone(), (geo.loc.x, geo.loc.y), true);
                        self.ewm.space.raise_element(window, true);

                        // Resize to fill output
                        window.toplevel().map(|t| {
                            t.with_pending_state(|state| {
                                state.size = Some((geo.size.w, geo.size.h).into());
                            });
                            t.send_configure();
                        });

                        self.ewm.queue_redraw_all();
                        info!(
                            "Assigned surface {} to output {} at ({}, {}) {}x{}",
                            id, output, geo.loc.x, geo.loc.y, geo.size.w, geo.size.h
                        );
                    } else {
                        warn!("Surface not found: {}", id);
                    }
                } else {
                    warn!("Output not found: {}", output);
                }
            }
            Command::PrepareFrame { output } => {
                // Queue output for next frame creation
                self.ewm.pending_frame_outputs.push(output.clone());
                info!("Prepared frame for output {}", output);
            }
            Command::ConfigureXkb { layouts, options } => {
                // Parse layout names from comma-separated string
                let layout_names: Vec<String> = layouts
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                if layout_names.is_empty() {
                    warn!("No valid layouts in configure-xkb");
                    return;
                }

                // Build XKB configuration
                let xkb_config = smithay::input::keyboard::XkbConfig {
                    layout: &layouts,
                    options: options.clone(),
                    ..Default::default()
                };

                // Reconfigure keyboard with new XKB settings
                let keyboard = self.ewm.seat.get_keyboard().unwrap();
                if let Err(e) = keyboard.set_xkb_config(&mut self.ewm, xkb_config) {
                    error!("Failed to configure XKB: {:?}", e);
                    return;
                }

                // Store layout names for name→index lookup
                self.ewm.xkb_layout_names = layout_names.clone();
                self.ewm.xkb_current_layout = 0;

                info!(
                    "Configured XKB layouts: {:?}, options: {:?}",
                    layout_names,
                    options
                );

                // Send layouts event back to Emacs
                self.ewm.queue_event(Event::Layouts {
                    layouts: layout_names,
                    current: 0,
                });
            }
            Command::SwitchLayout { layout } => {
                // Find index of layout name
                let index = self
                    .ewm
                    .xkb_layout_names
                    .iter()
                    .position(|l| l == &layout);

                match index {
                    Some(idx) => {
                        // Switch to the layout using Smithay's keyboard API
                        let keyboard = self.ewm.seat.get_keyboard().unwrap();

                        // Clear focus, switch layout, restore focus
                        let current_focus = self.ewm.keyboard_focus.clone();
                        keyboard.set_focus(&mut self.ewm, None, SERIAL_COUNTER.next_serial());
                        keyboard.with_xkb_state(&mut self.ewm, |mut context| {
                            context.set_layout(Layout(idx as u32));
                        });
                        keyboard.set_focus(&mut self.ewm, current_focus, SERIAL_COUNTER.next_serial());

                        // Update internal state
                        self.ewm.xkb_current_layout = idx;

                        info!("Switched to layout: {} (index {})", layout, idx);

                        // Emit layout-switched event
                        self.ewm.queue_event(Event::LayoutSwitched {
                            layout: layout.clone(),
                            index: idx,
                        });
                    }
                    None => {
                        warn!(
                            "Layout '{}' not found. Available: {:?}",
                            layout, self.ewm.xkb_layout_names
                        );
                    }
                }
            }
            Command::GetLayouts => {
                // Query current layouts and active index
                self.ewm.queue_event(Event::Layouts {
                    layouts: self.ewm.xkb_layout_names.clone(),
                    current: self.ewm.xkb_current_layout,
                });
            }
            Command::ImCommit { text } => {
                // Forward text to input method relay for commit
                if let Some(ref relay) = self.ewm.im_relay {
                    relay.commit_string(text);
                } else {
                    warn!("im-commit received but no IM relay connected");
                }
            }
            Command::TextInputIntercept { enabled } => {
                info!("Text input intercept: {}", enabled);
                self.ewm.text_input_intercept = enabled;
            }
        }
    }

    /// Process events from the IM relay and send to Emacs
    pub fn process_im_events(&mut self) {
        // Collect events first to avoid borrow conflict with queue_event
        let events: Vec<_> = self.ewm.im_relay
            .as_ref()
            .map(|relay| relay.event_rx.try_iter().collect())
            .unwrap_or_default();

        for event in events {
            match event {
                im_relay::ImEvent::Activated => {
                    info!("Text input activated, notifying Emacs");
                    self.ewm.queue_event(Event::TextInputActivated);
                }
                im_relay::ImEvent::Deactivated => {
                    info!("Text input deactivated, notifying Emacs");
                    self.ewm.queue_event(Event::TextInputDeactivated);
                }
            }
        }
    }
}

pub fn spawn_client(program: &str, args: &[String], wayland_display: &str) -> std::io::Result<Child> {
    let mut final_args = args.to_vec();

    // If spawning Emacs and EWM_INIT is set, inject -l <path> -f ewm-connect before user args
    if program == "emacs" || program.ends_with("/emacs") {
        if let Ok(ewm_init) = std::env::var("EWM_INIT") {
            info!("Auto-loading EWM from: {}", ewm_init);
            let mut emacs_args = vec![
                "-l".to_string(),
                ewm_init,
                "-f".to_string(),
                "ewm-connect".to_string(),
            ];
            emacs_args.extend(final_args);
            final_args = emacs_args;
        }
    }

    info!("Spawning: {} {:?}", program, final_args);
    info!("  WAYLAND_DISPLAY={}", wayland_display);

    std::process::Command::new(program)
        .args(&final_args)
        .env("WAYLAND_DISPLAY", wayland_display)
        .spawn()
}

// Emacs dynamic module initialization
emacs::plugin_is_GPL_compatible! {}

#[emacs::module(name = "ewm-core", defun_prefix = "ewm", mod_in_name = false)]
fn init(_: &emacs::Env) -> emacs::Result<()> {
    Ok(())
}
