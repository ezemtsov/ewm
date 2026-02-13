//! EWM - Emacs Wayland Manager
//!
//! Wayland compositor core library.
//!
//! # Design Invariants
//!
//! 1. **Focus ownership**: Only one surface has keyboard focus at a time.
//!    Focus changes only via explicit commands from Emacs or validated input
//!    events (click-to-focus, XDG activation). The `focused_surface_id` field
//!    is the single source of truth for focus state.
//!
//! 2. **Surface lifecycle**: Surfaces are assigned monotonically increasing IDs
//!    starting from 1. ID 0 is reserved for "no surface". IDs are never reused
//!    within a session. When a surface is destroyed, it's removed from all maps
//!    and its ID becomes invalid.
//!
//! 3. **Redraw state machine**: Each output has independent redraw state:
//!    `Idle` → `Queued` → `WaitingForVBlank` → `Idle`
//!    This prevents double-buffering issues and busy-waiting. The state is owned
//!    by `Ewm::output_state`, not the backend.
//!
//! 4. **Emacs ownership**: The compositor runs as a thread within Emacs.
//!    Emacs controls window layout and focus policy. The compositor handles
//!    protocol compliance and rendering. This split means:
//!    - Compositor never initiates focus changes without Emacs consent
//!    - Layout changes come from Emacs via the command queue
//!    - Events flow back to Emacs via the event queue
//!
//! 5. **Thread safety**: Communication between Emacs and compositor uses
//!    lock-free queues and atomic flags. The module interface (module.rs)
//!    provides the synchronization boundary.

pub mod backend;
pub mod cursor;
#[cfg(feature = "screencast")]
pub mod dbus;
pub mod event;
pub mod im_relay;
pub mod input;
#[cfg(feature = "screencast")]
pub mod pipewire;
pub mod protocols;
pub mod render;
mod module;
pub mod tracy;
pub use tracy::VBlankFrameTracker;

// Testing module is always compiled but only used by tests
#[doc(hidden)]
pub mod testing;

/// Get the current VT (virtual terminal) number.
/// Returns None if not running on a VT or detection fails.
pub fn current_vt() -> Option<u32> {
    std::fs::read_to_string("/sys/class/tty/tty0/active")
        .ok()
        .and_then(|s| s.trim().strip_prefix("tty")?.parse().ok())
}

/// Get a VT-specific suffix for socket names.
/// Returns "-vt{N}" if on a VT, empty string otherwise.
pub fn vt_suffix() -> String {
    current_vt()
        .map(|vt| format!("-vt{}", vt))
        .unwrap_or_default()
}

pub use event::{Event, OutputInfo, OutputMode};

pub use backend::{Backend, DrmBackendState, HeadlessBackend};

use smithay::{
    backend::renderer::element::solid::SolidColorBuffer,
    delegate_compositor, delegate_data_device, delegate_dmabuf,
    delegate_input_method_manager, delegate_layer_shell, delegate_output, delegate_primary_selection,
    delegate_seat, delegate_session_lock, delegate_shm, delegate_text_input_manager,
    delegate_xdg_activation, delegate_xdg_shell,
    reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
    desktop::{
        find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output, PopupKind,
        PopupManager, Space, Window,
    },
    output::Output,
    input::{
        keyboard::{xkb::keysyms, KeyboardHandle, ModifiersState},
        pointer::PointerHandle,
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
            protocol::{wl_output::WlOutput, wl_surface::WlSurface},
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
        shell::wlr_layer::{
            Layer, WlrLayerShellHandler, WlrLayerShellState,
        },
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
        session_lock::{
            LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
        },
    },
};
use crate::protocols::foreign_toplevel::{
    ForeignToplevelHandler, ForeignToplevelManagerState, WindowInfo,
};
use crate::protocols::screencopy::{Screencopy, ScreencopyHandler, ScreencopyManagerState};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::mem;
use tracing::{debug, error, info, trace, warn};

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

/// Session lock state machine for secure screen locking.
///
/// Follows the ext-session-lock-v1 protocol requirements:
/// - Lock is confirmed only after all outputs render a locked frame
/// - Input is blocked during locking/locked states
pub enum LockState {
    /// Session is not locked
    Unlocked,
    /// Lock requested, waiting for all outputs to render locked frame
    Locking(SessionLocker),
    /// Session is fully locked (stores the lock object to detect dead clients)
    Locked(ExtSessionLockV1),
}

impl Default for LockState {
    fn default() -> Self {
        LockState::Unlocked
    }
}

/// Per-output lock render state for tracking lock confirmation.
#[derive(Default, PartialEq, Eq, Clone, Copy, Debug)]
pub enum LockRenderState {
    /// Output is showing normal content (or not yet rendered locked)
    #[default]
    Unlocked,
    /// Output has rendered a locked frame
    Locked,
}

/// Per-output state for redraw synchronization
pub struct OutputState {
    pub redraw_state: RedrawState,
    /// Refresh interval in microseconds (for estimated VBlank timer)
    pub refresh_interval_us: u64,
    /// Tracy frame tracker for VBlank profiling (no-op when feature disabled)
    pub vblank_tracker: VBlankFrameTracker,
    /// Lock surface for this output (when session is locked)
    pub lock_surface: Option<LockSurface>,
    /// Render state for session lock (tracks whether locked frame was rendered)
    pub lock_render_state: LockRenderState,
    /// Solid color background for lock screen (shown before lock surface renders)
    pub lock_color_buffer: SolidColorBuffer,
}

impl OutputState {
    /// Create a new OutputState for the given output name and size.
    pub fn new(output_name: &str, refresh_interval_us: u64, size: (i32, i32)) -> Self {
        Self {
            redraw_state: RedrawState::Queued,
            refresh_interval_us,
            vblank_tracker: VBlankFrameTracker::new(output_name),
            lock_surface: None,
            lock_render_state: LockRenderState::Unlocked,
            // Dark gray background for lock screen
            lock_color_buffer: SolidColorBuffer::new(size, [0.1, 0.1, 0.1, 1.0]),
        }
    }

    /// Resize the lock color buffer for this output
    pub fn resize_lock_buffer(&mut self, size: (i32, i32)) {
        self.lock_color_buffer.resize(size);
    }
}

impl Default for OutputState {
    fn default() -> Self {
        Self {
            redraw_state: RedrawState::Idle,
            refresh_interval_us: 16_667, // ~60Hz default
            vblank_tracker: VBlankFrameTracker::new("default"),
            lock_surface: None,
            lock_render_state: LockRenderState::Unlocked,
            // Default 1920x1080 lock background (will be resized per output)
            lock_color_buffer: SolidColorBuffer::new((1920, 1080), [0.1, 0.1, 0.1, 1.0]),
        }
    }
}

/// Kill combo: Super+Shift+E
/// Returns true if this key event is the kill combo (keysym-based)
pub fn is_kill_combo(keysym: u32, shift: bool, logo: bool) -> bool {
    // 'e' = 0x65, 'E' = 0x45 (standard X11/XKB keysyms)
    (keysym == 0x65 || keysym == 0x45) && shift && logo
}

/// Cached surface info for change detection
#[derive(Clone, Default, Serialize)]
struct SurfaceInfo {
    app_id: String,
    title: String,
}

/// A single view of a surface (position in an Emacs window)
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct SurfaceView {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub active: bool, // True for the view in the selected Emacs window
}

/// Key identifier: either a keysym integer or a named key string
#[derive(Debug, Clone, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InterceptedKey {
    pub key: KeyId,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(rename = "super", default)]
    pub logo: bool,
    /// True if this key is bound to a keymap (prefix key)
    #[serde(default)]
    pub is_prefix: bool,
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
    /// Cached pointer handle (avoids repeated get_pointer().unwrap() on hot paths)
    pub pointer: PointerHandle<Self>,
    /// Cached keyboard handle (avoids repeated get_keyboard().unwrap() on hot paths)
    pub keyboard: KeyboardHandle<Self>,

    // Surface tracking
    next_surface_id: u32,
    pub window_ids: HashMap<Window, u32>,
    pub id_windows: HashMap<u32, Window>,
    surface_info: HashMap<u32, SurfaceInfo>,
    pub surface_views: HashMap<u32, Vec<SurfaceView>>,

    // Output
    pub output_size: (i32, i32),
    pub outputs: Vec<OutputInfo>,

    // Input
    pub pointer_location: (f64, f64),
    pub focused_surface_id: u32,
    pub keyboard_focus: Option<WlSurface>,

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
    // Track text input active state to deduplicate IM relay events
    pub text_input_active: bool,

    // Popup manager for XDG popups
    pub popups: PopupManager,

    // Layer shell state
    pub layer_shell_state: WlrLayerShellState,
    pub unmapped_layer_surfaces: std::collections::HashSet<WlSurface>,

    // Working area per output (non-exclusive zone from layer-shell surfaces)
    pub working_areas: HashMap<String, Rectangle<i32, smithay::utils::Logical>>,

    // XDG activation state (allows apps to request focus)
    pub activation_state: XdgActivationState,

    // Foreign toplevel state (exposes windows to external tools)
    pub foreign_toplevel_state: ForeignToplevelManagerState,

    // Session lock state (ext-session-lock-v1 protocol)
    pub session_lock_state: SessionLockManagerState,
    pub lock_state: LockState,

    // TODO: ext-idle-notify-v1 protocol support
    // IdleNotifierState<D> requires LoopHandle<D> and Seat<D> to match,
    // but our event loop uses State while handlers use Ewm.
    // This needs architectural changes to support properly.

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
        let keyboard = seat.add_keyboard(Default::default(), 200, 25)
            .expect("Failed to add keyboard to seat");
        let pointer = seat.add_pointer();

        // Initialize screencopy state before moving display_handle
        let screencopy_state = ScreencopyManagerState::new::<Self, _>(&display_handle, |_| true);

        // Initialize output manager with xdg-output protocol support
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);

        // Initialize text input for input method support
        let text_input_state = TextInputManagerState::new::<Self>(&display_handle);

        // Initialize input method manager (allows Emacs to act as input method)
        let input_method_state = InputMethodManagerState::new::<Self, _>(&display_handle, |_| true);

        // Initialize layer shell for panels, notifications, etc.
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);

        // Initialize xdg-activation for focus requests
        let activation_state = XdgActivationState::new::<Self>(&display_handle);

        // Initialize foreign toplevel management (exposes windows to external tools)
        let foreign_toplevel_state =
            ForeignToplevelManagerState::new::<Self, _>(&display_handle, |_| true);

        // Initialize session lock for screen locking (ext-session-lock-v1)
        let session_lock_state = SessionLockManagerState::new::<Self, _>(&display_handle, |_| true);

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
            pointer,
            keyboard,
            next_surface_id: 1,
            window_ids: HashMap::new(),
            id_windows: HashMap::new(),
            surface_info: HashMap::new(),
            surface_views: HashMap::new(),
            output_size: (800, 600),
            outputs: Vec::new(),
            pointer_location: (0.0, 0.0),
            focused_surface_id: 0,
            keyboard_focus: None,
            emacs_pid: None,
            emacs_surfaces: std::collections::HashSet::new(),
            pending_screenshot: None,
            output_state: HashMap::new(),
            pending_early_imports: Vec::new(),
            screencopy_state,
            output_manager_state,
            text_input_state,
            input_method_state,
            text_input_intercept: false,
            text_input_active: false,
            popups: PopupManager::default(),
            layer_shell_state,
            unmapped_layer_surfaces: std::collections::HashSet::new(),
            working_areas: HashMap::new(),
            activation_state,
            foreign_toplevel_state,
            session_lock_state,
            lock_state: LockState::Unlocked,
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

    /// Refresh foreign toplevel state (notify external tools of window changes)
    pub fn refresh_foreign_toplevel(&mut self) {
        use smithay::wayland::seat::WaylandFocus;

        // Collect window info for all non-Emacs surfaces
        let windows: Vec<WindowInfo> = self
            .id_windows
            .iter()
            .filter(|(id, _)| !self.emacs_surfaces.contains(id))
            .filter_map(|(&id, window)| {
                let surface = window.wl_surface()?.into_owned();
                let info = self.surface_info.get(&id)?;
                let output = self.find_surface_output(id);
                Some(WindowInfo {
                    surface,
                    title: if info.title.is_empty() {
                        None
                    } else {
                        Some(info.title.clone())
                    },
                    app_id: Some(info.app_id.clone()),
                    output,
                    is_focused: self.focused_surface_id == id,
                })
            })
            .collect();

        self.foreign_toplevel_state.refresh::<Self>(windows);
    }

    /// Find the output for a surface (returns Output object)
    fn find_surface_output(&self, surface_id: u32) -> Option<smithay::output::Output> {
        let window = self.id_windows.get(&surface_id)?;
        let window_loc = self.space.element_location(window)?;
        self.space
            .outputs()
            .find(|o| {
                self.space
                    .output_geometry(o)
                    .map(|geo| geo.contains(window_loc))
                    .unwrap_or(false)
            })
            .cloned()
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
            crate::module::set_focused_id(id);
            // Notify Emacs about focus change (skip Emacs frames, they handle their own focus)
            if !self.emacs_surfaces.contains(&id) {
                self.queue_event(Event::Focus { id });
            }
        }
    }

    /// Focus a surface, updating internal state, keyboard focus, and text input.
    /// If `notify_emacs` is true, sends Event::Focus to Emacs.
    pub fn focus_surface(&mut self, id: u32, notify_emacs: bool) {
        self.focus_surface_with_source(id, notify_emacs, "focus_surface", None);
    }

    /// Focus a surface with source tracking for debugging.
    pub fn focus_surface_with_source(&mut self, id: u32, notify_emacs: bool, source: &str, context: Option<&str>) {
        module::record_focus(id, source, context);
        self.focused_surface_id = id;
        crate::module::set_focused_id(id);
        if notify_emacs {
            self.queue_event(Event::Focus { id });
        }

        if let Some(window) = self.id_windows.get(&id) {
            if let Some(surface) = window.wl_surface() {
                let keyboard = self.keyboard.clone();
                let focus_surface = surface.into_owned();
                self.keyboard_focus = Some(focus_surface.clone());
                // focus_changed handles text_input focus
                keyboard.set_focus(self, Some(focus_surface.clone()), SERIAL_COUNTER.next_serial());
            }
        }
    }

    /// Queue an event to be sent to Emacs via the module queue
    pub(crate) fn queue_event(&mut self, event: Event) {
        module::push_event(event);
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
        if self.focused_surface_id == 0 {
            return None;
        }
        self.get_surface_output(self.focused_surface_id)
    }

    /// Get the output under the cursor position
    fn output_under_cursor(&self) -> Option<String> {
        use smithay::utils::Point;
        let (px, py) = self.pointer_location;
        let cursor_point = Point::from((px as i32, py as i32));

        for output in self.space.outputs() {
            if let Some(geo) = self.space.output_geometry(output) {
                if geo.contains(cursor_point) {
                    return Some(output.name());
                }
            }
        }
        None
    }

    /// Get active output for placing new non-Emacs surfaces.
    /// Priority: cursor position > focused output > first output
    fn active_output(&self) -> Option<String> {
        self.output_under_cursor()
            .or_else(|| self.get_focused_output())
            .or_else(|| self.space.outputs().next().map(|o| o.name()))
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
    pub fn get_emacs_surface_for_focused_output(&self) -> Option<u32> {
        let focused_output = self.get_focused_output()?;

        // Find an Emacs surface on the same output
        for &emacs_id in &self.emacs_surfaces {
            if self.get_surface_output(emacs_id) == Some(focused_output.clone()) {
                return Some(emacs_id);
            }
        }

        // Fallback to first Emacs surface
        self.emacs_surfaces.iter().next().copied()
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
        crate::module::set_output_offset(&output.name, output.x, output.y);
        self.queue_event(Event::OutputDetected(output));
    }

    /// Send output disconnected event to Emacs
    pub fn send_output_disconnected(&mut self, name: &str) {
        crate::module::remove_output(name);
        self.queue_event(Event::OutputDisconnected {
            name: name.to_string(),
        });
    }

    /// Get the working area for an output (non-exclusive zone from layer surfaces).
    /// This is the area available for Emacs frames after panels reserve their space.
    pub fn get_working_area(&self, output: &Output) -> Rectangle<i32, smithay::utils::Logical> {
        let map = layer_map_for_output(output);
        map.non_exclusive_zone()
    }

    /// Update Emacs frames to fit within the working area of an output.
    /// Called when layer surface exclusive zones change.
    pub fn update_frames_for_working_area(&mut self, output: &Output) {
        let working_area = self.get_working_area(output);
        let output_geo = match self.space.output_geometry(output) {
            Some(geo) => geo,
            None => return,
        };

        // Find Emacs frame surfaces on this output and update their position/size
        for (&id, window) in &self.id_windows {
            // Only update Emacs surfaces
            if !self.emacs_surfaces.contains(&id) {
                continue;
            }

            // Check if this window is on the target output by comparing position
            if let Some(window_loc) = self.space.element_location(window) {
                if output_geo.contains(window_loc) {
                    // Reposition to working area origin (relative to output)
                    let new_pos = (
                        output_geo.loc.x + working_area.loc.x,
                        output_geo.loc.y + working_area.loc.y,
                    );

                    debug!(
                        "Updating Emacs frame {} position to ({}, {}) size {}x{}",
                        id, new_pos.0, new_pos.1, working_area.size.w, working_area.size.h
                    );

                    self.space.map_element(window.clone(), new_pos, false);

                    // Resize to working area
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.with_pending_state(|state| {
                            state.size = Some(working_area.size);
                        });
                        toplevel.send_configure();
                    }
                }
            }
        }

        // Queue redraw
        self.queue_redraw(output);
    }

    /// Check and update working area for an output, sending event if changed.
    pub fn check_working_area_change(&mut self, output: &Output) {
        let working_area = self.get_working_area(output);
        let output_name = output.name();

        // Check if changed
        let changed = self
            .working_areas
            .get(&output_name)
            .map_or(true, |prev| *prev != working_area);

        if changed {
            info!(
                "Working area for {} changed: {}x{}+{}+{}",
                output_name,
                working_area.size.w,
                working_area.size.h,
                working_area.loc.x,
                working_area.loc.y
            );

            self.working_areas.insert(output_name.clone(), working_area);

            // Update Emacs frames to fit new working area
            self.update_frames_for_working_area(output);

            // Notify Emacs
            self.queue_event(Event::WorkingArea {
                output: output_name,
                x: working_area.loc.x,
                y: working_area.loc.y,
                width: working_area.size.w,
                height: working_area.size.h,
            });
        }
    }

    /// Get working areas as serializable structs for state dump.
    pub fn get_working_areas_info(&self) -> Vec<crate::event::WorkingAreaInfo> {
        self.working_areas
            .iter()
            .map(|(name, rect)| crate::event::WorkingAreaInfo {
                output: name.clone(),
                x: rect.loc.x,
                y: rect.loc.y,
                width: rect.size.w,
                height: rect.size.h,
            })
            .collect()
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
        // Automatically derive socket name from current VT for multi-instance support
        let socket_name = format!("wayland-ewm{}", crate::vt_suffix());
        info!("Creating Wayland socket with name: {}", socket_name);
        let socket = ListeningSocketSource::with_name(&socket_name)?;
        let socket_name = socket.socket_name().to_os_string();

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
                if let Err(e) = display.dispatch_clients(&mut state.ewm) {
                    tracing::error!("Wayland dispatch error: {e}");
                }
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

    /// Handle commit for layer surfaces. Returns true if this was a layer surface.
    /// Following niri's layer_shell_handle_commit pattern.
    pub fn handle_layer_surface_commit(&mut self, surface: &WlSurface) -> bool {
        use smithay::backend::renderer::utils::with_renderer_surface_state;
        use smithay::desktop::WindowSurfaceType;
        use smithay::wayland::compositor::get_parent;
        use smithay::wayland::shell::wlr_layer::LayerSurfaceData;

        // Find root surface
        let mut root_surface = surface.clone();
        while let Some(parent) = get_parent(&root_surface) {
            root_surface = parent;
        }

        // Find which output has this layer surface
        let output = self
            .space
            .outputs()
            .find(|o| {
                let map = layer_map_for_output(o);
                map.layer_for_surface(&root_surface, WindowSurfaceType::TOPLEVEL)
                    .is_some()
            })
            .cloned();

        let Some(output) = output else {
            return false;
        };

        if surface == &root_surface {
            let initial_configure_sent =
                smithay::wayland::compositor::with_states(surface, |states| {
                    states
                        .data_map
                        .get::<LayerSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .initial_configure_sent
                });

            let mut map = layer_map_for_output(&output);

            // Arrange the layers before sending the initial configure
            map.arrange();

            let layer = map
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .unwrap();

            if initial_configure_sent {
                let is_mapped =
                    with_renderer_surface_state(surface, |state| state.buffer().is_some())
                        .unwrap_or(false);

                if is_mapped {
                    let was_unmapped = self.unmapped_layer_surfaces.remove(surface);
                    if was_unmapped {
                        debug!("Layer surface mapped");
                    }
                } else {
                    self.unmapped_layer_surfaces.insert(surface.clone());
                }
            } else {
                layer.layer_surface().send_configure();
            }
            drop(map);

            // Check for working area changes (exclusive zones from panels)
            self.check_working_area_change(&output);

            self.queue_redraw(&output);
        } else {
            // This is a layer-shell subsurface
            self.queue_redraw(&output);
        }

        true
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
        &client.get_data::<ClientState>()
            .expect("ClientState inserted at connection time")
            .compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // Queue early import for DRM backend (processed in main loop)
        self.pending_early_imports.push(surface.clone());

        // Handle layer surface commits
        if self.handle_layer_surface_commit(surface) {
            return;
        }

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
        // 1. Emacs frames: from prepare-frame (synchronous mutex, not command queue)
        // 2. Other surfaces: from active_output (cursor > focused > first)
        let frame_output = module::take_pending_frame_output();
        let target_output = frame_output.clone().or_else(|| self.active_output());

        // Position based on surface type
        let position = if let Some(ref output_name) = frame_output {
            // Emacs frame: position at working area origin (respects panel exclusive zones)
            self.space
                .outputs()
                .find(|o| o.name() == *output_name)
                .map(|o| {
                    let output_geo = self.space.output_geometry(o).unwrap_or_default();
                    let working_area = self.get_working_area(o);
                    (
                        output_geo.loc.x + working_area.loc.x,
                        output_geo.loc.y + working_area.loc.y,
                    )
                })
                .unwrap_or((-10000, -10000))
        } else {
            // External app: offscreen, Emacs layout will position
            (-10000, -10000)
        };
        self.space.map_element(window.clone(), position, false);

        // Resize Emacs frames to fill their working area (not full output)
        if let Some(ref output_name) = frame_output {
            if let Some(working_area) = self
                .space
                .outputs()
                .find(|o| o.name() == *output_name)
                .map(|o| self.get_working_area(o))
            {
                window.toplevel().map(|t| {
                    t.with_pending_state(|state| {
                        state.size = Some(working_area.size);
                    });
                    t.send_configure();
                });
            }
        }

        // Initialize surface_info for non-Emacs surfaces
        if !is_emacs {
            self.surface_info.insert(
                id,
                SurfaceInfo {
                    app_id: app.clone(),
                    title: String::new(),
                },
            );
        }

        // Send event to Emacs with target output (for all surfaces)
        self.queue_event(Event::New {
            id,
            app: app.clone(),
            output: target_output.clone(),
        });
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

// Layer Shell
impl WlrLayerShellHandler for Ewm {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: smithay::wayland::shell::wlr_layer::LayerSurface,
        wl_output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        use smithay::desktop::LayerSurface;

        // Get the output for this layer surface
        let output = if let Some(wl_output) = &wl_output {
            Output::from_resource(wl_output)
        } else {
            self.space.outputs().next().cloned()
        };

        let Some(output) = output else {
            warn!("No output for new layer surface, closing");
            surface.send_close();
            return;
        };

        let wl_surface = surface.wl_surface().clone();
        self.unmapped_layer_surfaces.insert(wl_surface);

        let mut map = layer_map_for_output(&output);
        map.map_layer(&LayerSurface::new(surface, namespace.clone()))
            .unwrap();
        info!("New layer surface: namespace={} on output {}", namespace, output.name());
    }

    fn layer_destroyed(&mut self, surface: smithay::wayland::shell::wlr_layer::LayerSurface) {
        let wl_surface = surface.wl_surface();
        self.unmapped_layer_surfaces.remove(wl_surface);

        // Find and unmap the layer surface
        let output = self.space.outputs().find_map(|o| {
            let map = layer_map_for_output(o);
            let layer = map
                .layers()
                .find(|&layer| layer.layer_surface() == &surface)
                .cloned();
            layer.map(|layer| (o.clone(), layer))
        });

        if let Some((output, layer)) = output {
            let mut map = layer_map_for_output(&output);
            map.unmap_layer(&layer);
            // Re-arrange after unmapping to recalculate exclusive zones
            map.arrange();
            drop(map);

            // Check for working area expansion (panel removed)
            self.check_working_area_change(&output);

            self.queue_redraw(&output);
            info!("Layer surface destroyed");
        }
    }

    fn new_popup(
        &mut self,
        _parent: smithay::wayland::shell::wlr_layer::LayerSurface,
        popup: smithay::wayland::shell::xdg::PopupSurface,
    ) {
        let _ = self.popups.track_popup(PopupKind::Xdg(popup));
    }
}
delegate_layer_shell!(Ewm);

// XDG Activation protocol (allows apps to request focus)
impl XdgActivationHandler for Ewm {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        // Only accept tokens created while the requesting app had keyboard focus.
        // This prevents apps from stealing focus via xdg_activation.
        // Reference: niri's implementation in src/handlers/mod.rs
        let app_id = data.app_id.as_deref().unwrap_or("unknown");

        let Some((serial, seat)) = data.serial else {
            debug!("xdg_activation: token rejected for {app_id} - no serial provided");
            return false;
        };
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            debug!("xdg_activation: token rejected for {app_id} - invalid seat");
            return false;
        };

        let keyboard = seat.get_keyboard().unwrap();
        let valid = keyboard
            .last_enter()
            .map(|last_enter| serial.is_no_older_than(&last_enter))
            .unwrap_or(false);

        if valid {
            debug!("xdg_activation: token accepted for {app_id}");
        } else {
            debug!("xdg_activation: token rejected for {app_id} - serial not from app's focus entry");
        }
        valid
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        use std::time::Duration;
        const TOKEN_TIMEOUT: Duration = Duration::from_secs(10);

        debug!("xdg_activation: request_activation called for surface {:?}", surface.id());

        if token_data.timestamp.elapsed() < TOKEN_TIMEOUT {
            // Find the surface ID for this WlSurface
            if let Some(&id) = self.window_ids.iter()
                .find(|(w, _)| w.wl_surface().map(|s| &*s == &surface).unwrap_or(false))
                .map(|(_, id)| id)
            {
                // Focus the surface and notify Emacs
                self.focus_surface_with_source(id, true, "xdg_activation", None);
                info!("xdg_activation: granted for surface {}", id);
            } else {
                debug!("xdg_activation: surface not found in window_ids");
            }
        } else {
            debug!("xdg_activation: token expired (age={:?})", token_data.timestamp.elapsed());
        }

        // Always remove the token (single-use)
        self.activation_state.remove_token(&token);
    }
}
delegate_xdg_activation!(Ewm);

// Foreign toplevel management protocol (exposes windows to external tools)
impl ForeignToplevelHandler for Ewm {
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState {
        &mut self.foreign_toplevel_state
    }

    fn activate(&mut self, wl_surface: WlSurface) {
        if let Some(&id) = self
            .window_ids
            .iter()
            .find(|(w, _)| w.wl_surface().map(|s| &*s == &wl_surface).unwrap_or(false))
            .map(|(_, id)| id)
        {
            self.focus_surface_with_source(id, true, "foreign_toplevel", None);
            info!("Foreign toplevel: activated surface {}", id);
        }
    }

    fn close(&mut self, wl_surface: WlSurface) {
        if let Some((window, _)) = self
            .window_ids
            .iter()
            .find(|(w, _)| w.wl_surface().map(|s| &*s == &wl_surface).unwrap_or(false))
        {
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_close();
                info!("Foreign toplevel: sent close request");
            }
        }
    }

    fn set_fullscreen(&mut self, _wl_surface: WlSurface, _wl_output: Option<WlOutput>) {
        // EWM doesn't have a fullscreen concept - windows fill Emacs windows
    }

    fn unset_fullscreen(&mut self, _wl_surface: WlSurface) {
        // No-op for EWM
    }
}
delegate_foreign_toplevel!(Ewm);

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

// Session Lock protocol (ext-session-lock-v1) for screen locking
impl SessionLockHandler for Ewm {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        // Check for dead locker client holding the lock
        if let LockState::Locked(ref lock) = self.lock_state {
            if lock.is_alive() {
                info!("Session lock request ignored: already locked with active client");
                return;
            }
            // Previous client died, allow new lock
            info!("Previous lock client dead, allowing new lock");
        } else if !matches!(self.lock_state, LockState::Unlocked) {
            info!("Session lock request ignored: already locking");
            return;
        }

        info!("Session lock requested");

        if self.output_state.is_empty() {
            // No outputs: lock immediately
            let lock = confirmation.ext_session_lock().clone();
            confirmation.lock();
            self.lock_state = LockState::Locked(lock);
            info!("Session locked (no outputs)");
        } else {
            // Enter Locking state and queue redraw to show locked frame
            self.lock_state = LockState::Locking(confirmation);
            // Reset all output lock render states
            for state in self.output_state.values_mut() {
                state.lock_render_state = LockRenderState::Unlocked;
            }
            self.queue_redraw_all();
        }
    }

    fn unlock(&mut self) {
        info!("Session unlock requested");
        self.lock_state = LockState::Unlocked;

        // Clear lock surfaces and reset render states
        for state in self.output_state.values_mut() {
            state.lock_surface = None;
            state.lock_render_state = LockRenderState::Unlocked;
        }

        self.queue_redraw_all();
        info!("Session unlocked");
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let Some(output) = Output::from_resource(&wl_output) else {
            warn!("Lock surface created for unknown output");
            return;
        };

        info!("New lock surface for output: {}", output.name());

        // Configure lock surface to cover the entire output
        configure_lock_surface(&surface, &output);

        // Store in per-output state
        if let Some(state) = self.output_state.get_mut(&output) {
            state.lock_surface = Some(surface);
        }

        self.queue_redraw(&output);
    }
}
delegate_session_lock!(Ewm);

/// Configure a lock surface to cover the full output
fn configure_lock_surface(surface: &LockSurface, output: &Output) {
    use smithay::wayland::compositor::with_states;
    use smithay::wayland::fractional_scale::with_fractional_scale;

    surface.with_pending_state(|states| {
        let mode = output.current_mode().unwrap();
        let size = output.current_transform().transform_size(mode.size);
        states.size = Some(Size::from((size.w as u32, size.h as u32)));
    });

    let scale = output.current_scale();
    let transform = output.current_transform();
    let wl_surface = surface.wl_surface();

    with_states(wl_surface, |data| {
        // Send preferred scale
        if let Some(fractional_scale) = with_fractional_scale(data, |fractional| {
            fractional.preferred_scale()
        }) {
            // Already has fractional scale configured
            let _ = fractional_scale;
        }

        // Send preferred buffer scale and transform
        smithay::wayland::compositor::send_surface_state(
            wl_surface,
            data,
            scale.integer_scale(),
            transform,
        );
    });

    surface.send_configure();
}


impl Ewm {
    /// Check if the session is locked (locking or fully locked)
    pub fn is_locked(&self) -> bool {
        !matches!(self.lock_state, LockState::Unlocked)
    }

    /// Check if all outputs have rendered locked frames and confirm lock if so
    pub fn check_lock_complete(&mut self) {
        // Check if we're in Locking state and all outputs have rendered
        let should_confirm = matches!(&self.lock_state, LockState::Locking(_))
            && self.output_state.values().all(|s| s.lock_render_state == LockRenderState::Locked);

        if should_confirm {
            // Take ownership of the SessionLocker to call lock()
            // Use a temporary Unlocked state (will be replaced immediately)
            let old_state = mem::replace(&mut self.lock_state, LockState::Unlocked);
            if let LockState::Locking(confirmation) = old_state {
                info!("All outputs rendered locked frame, confirming lock");
                let lock = confirmation.ext_session_lock().clone();
                confirmation.lock();
                self.lock_state = LockState::Locked(lock);
            }
        }
    }

    /// Get the lock surface for keyboard focus when locked
    pub fn lock_surface_focus(&self) -> Option<WlSurface> {
        // Prefer lock surface on output under cursor, then any output
        let cursor_output = self.output_under_cursor()
            .and_then(|name| {
                self.output_state.iter()
                    .find(|(o, _)| o.name() == name)
                    .map(|(o, _)| o.clone())
            });

        let target_output = cursor_output
            .or_else(|| self.output_state.keys().next().cloned());

        target_output.and_then(|output| {
            self.output_state.get(&output)?
                .lock_surface.as_ref()
                .map(|s| s.wl_surface().clone())
        })
    }

    /// Check lock state after output removal.
    /// If in Locking state, the removed output no longer needs to render a locked frame.
    pub fn check_lock_on_output_removed(&mut self) {
        if matches!(&self.lock_state, LockState::Locking(_)) {
            // Re-check if all remaining outputs are locked
            self.check_lock_complete();
        }
    }
}

/// Shared state for compositor event loop (passed to all handlers)
///
/// Note: Display is owned by the event loop (via Generic source), not by State.
pub struct State {
    pub backend: DrmBackendState,
    pub ewm: Ewm,
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

        // Process module commands (from Emacs via dynamic module)
        let commands = crate::module::drain_commands();
        if !commands.is_empty() {
            let _start = std::time::Instant::now();
            let _count = commands.len();
            for cmd in commands {
                self.handle_module_command(cmd);
            }
            let _elapsed_us = _start.elapsed().as_micros() as f64;
            crate::tracy_plot!("emacs_cmd_count", _count as f64);
            crate::tracy_plot!("emacs_cmd_time_us", _elapsed_us);
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

        // Flush Wayland clients
        if let Err(e) = self.ewm.display_handle.flush_clients() {
            tracing::warn!("Failed to flush Wayland clients: {e}");
        }
    }

    /// Handle a module command (from Emacs via dynamic module).
    fn handle_module_command(&mut self, cmd: module::ModuleCommand) {
        tracy_span!("handle_module_command");

        use module::ModuleCommand;
        match cmd {
            ModuleCommand::Layout { id, x, y, w, h } => {
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
                    debug!("Layout surface {} at ({}, {}) {}x{}", id, x, y, w, h);
                }
            }
            ModuleCommand::Views { id, views } => {
                // Skip if views unchanged
                if self.ewm.surface_views.get(&id) == Some(&views) {
                    trace!("Views surface {} unchanged, skipping", id);
                    return;
                }
                trace!("Views surface {} changed: {:?}", id, views);
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
                    debug!("Views surface {} ({} views)", id, views.len());
                    self.ewm.surface_views.insert(id, views);
                    self.ewm.queue_redraw_all();
                }
            }
            ModuleCommand::Hide { id } => {
                // Only hide if surface has views (skip if already hidden)
                if self.ewm.surface_views.contains_key(&id) {
                    if let Some(window) = self.ewm.id_windows.get(&id) {
                        self.ewm
                            .space
                            .map_element(window.clone(), (-10000, -10000), false);
                        self.ewm.surface_views.remove(&id);
                        self.ewm.queue_redraw_all();
                        debug!("Hide surface {}", id);
                    }
                }
            }
            ModuleCommand::Close { id } => {
                if let Some(window) = self.ewm.id_windows.get(&id) {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.send_close();
                        info!("Close surface {} (sent close request)", id);
                    }
                }
            }
            ModuleCommand::Focus { id } => {
                // Skip if already focused
                if self.ewm.focused_surface_id != id && self.ewm.id_windows.contains_key(&id) {
                    self.ewm.focus_surface_with_source(id, false, "emacs_command", None);
                }
            }
            ModuleCommand::WarpPointer { x, y } => {
                self.ewm.pointer_location = (x, y);
                module::set_pointer_location(x, y);
                let pointer = self.ewm.pointer.clone();
                let serial = SERIAL_COUNTER.next_serial();
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
            ModuleCommand::Screenshot { path } => {
                let target = path.unwrap_or_else(|| "/tmp/ewm-screenshot.png".to_string());
                info!("Screenshot requested: {}", target);
                self.ewm.pending_screenshot = Some(target);
            }
            ModuleCommand::AssignOutput { id, output } => {
                let output_geo = self
                    .ewm
                    .space
                    .outputs()
                    .find(|o| o.name() == output)
                    .and_then(|o| self.ewm.space.output_geometry(o));
                if let Some(geo) = output_geo {
                    if let Some(window) = self.ewm.id_windows.get(&id) {
                        self.ewm
                            .space
                            .map_element(window.clone(), (geo.loc.x, geo.loc.y), true);
                        self.ewm.space.raise_element(window, true);
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
            ModuleCommand::ConfigureOutput {
                name,
                x,
                y,
                width,
                height,
                refresh,
                enabled,
            } => {
                let output = self
                    .ewm
                    .space
                    .outputs()
                    .find(|o| o.name() == name)
                    .cloned();
                if let Some(output) = output {
                    if let Some(false) = enabled {
                        self.ewm.space.unmap_output(&output);
                        info!("Disabled output {}", name);
                    } else {
                        if let (Some(w), Some(h)) = (width, height) {
                            self.backend.set_mode(&mut self.ewm, &name, w, h, refresh);
                        }
                        // Only update position if x or y is explicitly specified
                        if x.is_some() || y.is_some() {
                            // Get current position as default
                            let current_pos = self.ewm.space.output_geometry(&output)
                                .map(|g| (g.loc.x, g.loc.y))
                                .unwrap_or((0, 0));
                            let new_x = x.unwrap_or(current_pos.0);
                            let new_y = y.unwrap_or(current_pos.1);
                            let new_pos = (new_x, new_y);
                            self.ewm.space.map_output(&output, new_pos);
                            output.change_current_state(None, None, None, Some(new_pos.into()));
                            for out_info in &mut self.ewm.outputs {
                                if out_info.name == name {
                                    out_info.x = new_x;
                                    out_info.y = new_y;
                                }
                            }
                            crate::module::set_output_offset(&name, new_x, new_y);
                            self.ewm.recalculate_output_size();
                            info!("Configured output {} at ({}, {})", name, new_x, new_y);
                        } else {
                            info!("Configured output {} (mode only)", name);
                        }
                    }
                    self.ewm.queue_redraw_all();
                } else {
                    warn!("Output not found: {}", name);
                }
            }
            ModuleCommand::ImCommit { text } => {
                if let Some(ref relay) = self.ewm.im_relay {
                    relay.commit_string(text);
                } else {
                    warn!("im-commit received but no IM relay connected");
                }
            }
            ModuleCommand::TextInputIntercept { enabled } => {
                if self.ewm.text_input_intercept != enabled {
                    info!("Text input intercept: {}", enabled);
                    self.ewm.text_input_intercept = enabled;
                }
            }
            ModuleCommand::ConfigureXkb { layouts, options } => {
                let layout_names: Vec<String> = layouts
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if layout_names.is_empty() {
                    warn!("No valid layouts in configure-xkb");
                    return;
                }
                let xkb_config = smithay::input::keyboard::XkbConfig {
                    layout: &layouts,
                    options: options.clone(),
                    ..Default::default()
                };
                let keyboard = self.ewm.keyboard.clone();
                if let Err(e) = keyboard.set_xkb_config(&mut self.ewm, xkb_config) {
                    error!("Failed to configure XKB: {:?}", e);
                    return;
                }
                self.ewm.xkb_layout_names = layout_names.clone();
                self.ewm.xkb_current_layout = 0;
                info!(
                    "Configured XKB layouts: {:?}, options: {:?}",
                    layout_names, options
                );
                self.ewm.queue_event(Event::Layouts {
                    layouts: layout_names,
                    current: 0,
                });
            }
            ModuleCommand::SwitchLayout { layout } => {
                let index = self
                    .ewm
                    .xkb_layout_names
                    .iter()
                    .position(|l| l == &layout);
                match index {
                    Some(idx) => {
                        use smithay::input::keyboard::Layout;
                        let keyboard = self.ewm.keyboard.clone();
                        let current_focus = self.ewm.keyboard_focus.clone();
                        keyboard.set_focus(&mut self.ewm, None, SERIAL_COUNTER.next_serial());
                        keyboard.with_xkb_state(&mut self.ewm, |mut context| {
                            context.set_layout(Layout(idx as u32));
                        });
                        keyboard.set_focus(&mut self.ewm, current_focus, SERIAL_COUNTER.next_serial());
                        self.ewm.xkb_current_layout = idx;
                        info!("Switched to layout: {} (index {})", layout, idx);
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
            ModuleCommand::GetLayouts => {
                self.ewm.queue_event(Event::Layouts {
                    layouts: self.ewm.xkb_layout_names.clone(),
                    current: self.ewm.xkb_current_layout,
                });
            }
            ModuleCommand::GetState => {
                let id_window_keys: Vec<u32> = self.ewm.id_windows.keys().copied().collect();
                let state = serde_json::json!({
                    "surfaces": self.ewm.surface_info,
                    "emacs_surfaces": self.ewm.emacs_surfaces,
                    "surface_views": self.ewm.surface_views,
                    "focused_surface_id": self.ewm.focused_surface_id,
                    "id_windows": id_window_keys,
                    "outputs": self.ewm.outputs,
                    "working_areas": self.ewm.get_working_areas_info(),
                    "pointer_location": self.ewm.pointer_location,
                    "intercepted_keys": module::get_intercepted_keys(),
                    "emacs_pid": self.ewm.emacs_pid,
                    "text_input_intercept": self.ewm.text_input_intercept,
                    "text_input_active": self.ewm.text_input_active,
                    "xkb_layouts": self.ewm.xkb_layout_names,
                    "xkb_current_layout": self.ewm.xkb_current_layout,
                    "next_surface_id": self.ewm.next_surface_id,
                    "pending_frame_outputs": module::peek_pending_frame_outputs(),
                    "in_prefix_sequence": module::get_in_prefix_sequence(),
                    // Debug info
                    "debug_mode": module::DEBUG_MODE.load(std::sync::atomic::Ordering::Relaxed),
                    "pending_commands": module::peek_commands(),
                    "focus_history": module::get_focus_history(),
                });
                let json = serde_json::to_string_pretty(&state).unwrap_or_default();
                self.ewm.queue_event(Event::State { json });
            }
            ModuleCommand::CreateActivationToken => {
                // Create an activation token for Emacs to pass to spawned processes
                let (token, _) = self.ewm.activation_state.create_external_token(None);
                let token_str = token.as_str().to_string();
                debug!("Created activation token for Emacs: {}", token_str);
                module::push_activation_token(token_str);
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
                    if !self.ewm.text_input_active {
                        self.ewm.text_input_active = true;
                        info!("Text input activated, notifying Emacs");
                        self.ewm.queue_event(Event::TextInputActivated);
                    }
                }
                im_relay::ImEvent::Deactivated => {
                    if self.ewm.text_input_active {
                        self.ewm.text_input_active = false;
                        info!("Text input deactivated, notifying Emacs");
                        self.ewm.queue_event(Event::TextInputDeactivated);
                    }
                }
            }
        }
    }
}

// Emacs dynamic module initialization
emacs::plugin_is_GPL_compatible! {}

#[emacs::module(name = "ewm-core", defun_prefix = "ewm", mod_in_name = false)]
fn init(_: &emacs::Env) -> emacs::Result<()> {
    Ok(())
}
