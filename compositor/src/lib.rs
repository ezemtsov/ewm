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
mod module;
#[cfg(feature = "screencast")]
pub mod pipewire;
pub mod protocols;
pub mod render;
pub mod tracy;
pub mod utils;
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

use crate::protocols::foreign_toplevel::{
    ForeignToplevelHandler, ForeignToplevelManagerState, WindowInfo,
};
use crate::protocols::screencopy::{Screencopy, ScreencopyHandler, ScreencopyManagerState};
use serde::{Deserialize, Serialize};
use smithay::{
    backend::renderer::element::{solid::SolidColorBuffer, RenderElementStates},
    delegate_compositor, delegate_data_control, delegate_data_device, delegate_dmabuf,
    delegate_fractional_scale, delegate_idle_notify, delegate_input_method_manager,
    delegate_layer_shell, delegate_output,
    delegate_primary_selection, delegate_seat, delegate_session_lock, delegate_shm,
    delegate_text_input_manager, delegate_xdg_activation, delegate_xdg_shell,
    desktop::{
        find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
        utils::{
            send_frames_surface_tree, surface_primary_scanout_output,
            update_surface_primary_scanout_output,
        },
        LayerSurface as DesktopLayerSurface, PopupKind, PopupManager, Space, Window,
        WindowSurfaceType,
    },
    input::{
        keyboard::{xkb::keysyms, KeyboardHandle, ModifiersState},
        pointer::PointerHandle,
        Seat, SeatHandler, SeatState,
    },
    output::Output,
    reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
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
    utils::{IsAlive, Logical, Rectangle, Size, Transform, SERIAL_COUNTER},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            get_parent, is_sync_subsurface, with_surface_tree_downward, CompositorClientState,
            CompositorHandler, CompositorState, SurfaceData, TraversalAction,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState},
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
        input_method::{
            InputMethodHandler, InputMethodManagerState, PopupSurface as IMPopupSurface,
        },
        output::OutputManagerState,
        seat::WaylandFocus,
        selection::{
            data_device::{
                request_data_device_client_selection, set_data_device_focus,
                set_data_device_selection, ClientDndGrabHandler, DataDeviceHandler,
                DataDeviceState, ServerDndGrabHandler,
            },
            primary_selection::{
                set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
            },
            wlr_data_control::{DataControlHandler, DataControlState},
            SelectionHandler, SelectionSource, SelectionTarget,
        },
        session_lock::{LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker},
        shell::wlr_layer::{Layer, WlrLayerShellHandler, WlrLayerShellState},
        shell::xdg::{
            decoration::{XdgDecorationHandler, XdgDecorationState},
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
        socket::ListeningSocketSource,
        text_input::TextInputManagerState,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
    },
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::mem;
use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use std::time::Duration;
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

impl std::fmt::Display for RedrawState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RedrawState::Idle => write!(f, "Idle"),
            RedrawState::Queued => write!(f, "Queued"),
            RedrawState::WaitingForVBlank { redraw_needed } => {
                write!(f, "WaitingForVBlank(redraw={})", redraw_needed)
            }
            RedrawState::WaitingForEstimatedVBlank(_) => write!(f, "WaitingForEstVBlank"),
            RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
                write!(f, "WaitingForEstVBlank+Queued")
            }
        }
    }
}

impl RedrawState {
    /// Transition to request a redraw
    pub fn queue_redraw(self) -> Self {
        match self {
            RedrawState::Idle => RedrawState::Queued,
            RedrawState::WaitingForVBlank { .. } => RedrawState::WaitingForVBlank {
                redraw_needed: true,
            },
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

/// Desired output configuration (from Emacs).
/// Stored per output name; looked up on connect and config changes.
#[derive(Debug, Clone)]
pub struct OutputConfig {
    /// Desired video mode (None = use preferred/auto)
    pub mode: Option<(i32, i32, Option<i32>)>, // (width, height, refresh_mhz)
    /// Desired position (None = auto horizontal layout)
    pub position: Option<(i32, i32)>,
    /// Desired scale (None = 1.0)
    pub scale: Option<f64>,
    /// Desired transform (None = Normal)
    pub transform: Option<Transform>,
    /// Whether output is enabled (default true)
    pub enabled: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            mode: None,
            position: None,
            scale: None,
            transform: None,
            enabled: true,
        }
    }
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
    /// Monotonically increasing sequence number for frame callback throttling.
    /// Incremented each VBlank cycle to prevent sending duplicate frame callbacks
    /// within the same refresh cycle.
    pub frame_callback_sequence: u32,
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
            frame_callback_sequence: 0,
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
            frame_callback_sequence: 0,
        }
    }
}

/// Frame callback throttle duration (matching niri's value).
/// Surfaces that haven't received a frame callback within this duration will
/// get one regardless of the throttling state, as a safety net.
const FRAME_CALLBACK_THROTTLE: Option<Duration> = Some(Duration::from_millis(995));

/// Per-surface state tracking when the last frame callback was sent.
/// Used to prevent sending duplicate frame callbacks within the same VBlank cycle,
/// which would cause clients to re-commit rapidly and overwhelm the display controller.
struct SurfaceFrameThrottlingState {
    /// Output and sequence number at which the frame callback was last sent.
    last_sent_at: RefCell<Option<(Output, u32)>>,
}

impl Default for SurfaceFrameThrottlingState {
    fn default() -> Self {
        Self {
            last_sent_at: RefCell::new(None),
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
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub data_control_state: DataControlState,
    pub seat: Seat<State>,
    /// Cached pointer handle (avoids repeated get_pointer().unwrap() on hot paths)
    pub pointer: PointerHandle<State>,
    /// Cached keyboard handle (avoids repeated get_keyboard().unwrap() on hot paths)
    pub keyboard: KeyboardHandle<State>,

    // Surface tracking
    next_surface_id: u32,
    pub window_ids: HashMap<Window, u32>,
    pub id_windows: HashMap<u32, Window>,
    surface_info: HashMap<u32, SurfaceInfo>,
    pub surface_views: HashMap<u32, Vec<SurfaceView>>,

    // Output
    pub output_size: Size<i32, Logical>,
    pub outputs: Vec<OutputInfo>,
    /// Desired output configuration, keyed by output name.
    /// Looked up when outputs connect; updated by Emacs commands.
    pub output_config: HashMap<String, OutputConfig>,

    // Input
    pub pointer_location: (f64, f64),
    pub focused_surface_id: u32,
    pub keyboard_focus: Option<WlSurface>,
    pub keyboard_focus_dirty: bool,

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
    /// Layer surface with OnDemand keyboard interactivity that was clicked
    pub layer_shell_on_demand_focus: Option<DesktopLayerSurface>,

    // Working area per output (non-exclusive zone from layer-shell surfaces)
    pub working_areas: HashMap<String, Rectangle<i32, smithay::utils::Logical>>,

    // XDG activation state (allows apps to request focus)
    pub activation_state: XdgActivationState,

    // Foreign toplevel state (exposes windows to external tools)
    pub foreign_toplevel_state: ForeignToplevelManagerState,

    // Session lock state (ext-session-lock-v1 protocol)
    pub session_lock_state: SessionLockManagerState,
    pub lock_state: LockState,
    /// Surface ID that was focused before locking (restored on unlock)
    pub pre_lock_focus: Option<u32>,

    // Idle notify state (ext-idle-notify-v1 protocol)
    pub idle_notifier_state: IdleNotifierState<State>,

    // Fractional scale protocol (wp-fractional-scale-v1)
    #[allow(dead_code)]
    pub fractional_scale_state: FractionalScaleManagerState,

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
    pub fn new(display_handle: DisplayHandle, loop_handle: LoopHandle<'static, State>) -> Self {
        let compositor_state = CompositorState::new::<State>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<State>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<State>(&display_handle);
        let shm_state = ShmState::new::<State>(&display_handle, vec![]);
        let dmabuf_state = DmabufState::new();
        let mut seat_state: SeatState<State> = SeatState::new();
        let data_device_state = DataDeviceState::new::<State>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<State>(&display_handle);
        let data_control_state = DataControlState::new::<State, _>(
            &display_handle,
            Some(&primary_selection_state),
            |_| true,
        );
        let mut seat: Seat<State> = seat_state.new_wl_seat(&display_handle, "seat0");
        let keyboard = seat
            .add_keyboard(Default::default(), 200, 25)
            .expect("Failed to add keyboard to seat");
        let pointer = seat.add_pointer();

        // Initialize screencopy state before moving display_handle
        let screencopy_state = ScreencopyManagerState::new::<State, _>(&display_handle, |_| true);

        // Initialize output manager with xdg-output protocol support
        let output_manager_state =
            OutputManagerState::new_with_xdg_output::<State>(&display_handle);

        // Initialize text input for input method support
        let text_input_state = TextInputManagerState::new::<State>(&display_handle);

        // Initialize input method manager (allows Emacs to act as input method)
        let input_method_state =
            InputMethodManagerState::new::<State, _>(&display_handle, |_| true);

        // Initialize layer shell for panels, notifications, etc.
        let layer_shell_state = WlrLayerShellState::new::<State>(&display_handle);

        // Initialize xdg-activation for focus requests
        let activation_state = XdgActivationState::new::<State>(&display_handle);

        // Initialize foreign toplevel management (exposes windows to external tools)
        let foreign_toplevel_state =
            ForeignToplevelManagerState::new::<State, _>(&display_handle, |_| true);

        // Initialize session lock for screen locking (ext-session-lock-v1)
        let session_lock_state =
            SessionLockManagerState::new::<State, _>(&display_handle, |_| true);

        // Initialize idle notifier (ext-idle-notify-v1)
        let idle_notifier_state = IdleNotifierState::new(&display_handle, loop_handle);

        // Initialize fractional scale protocol (wp-fractional-scale-v1)
        let fractional_scale_state = FractionalScaleManagerState::new::<State>(&display_handle);

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
            data_control_state,
            seat,
            pointer,
            keyboard,
            next_surface_id: 1,
            window_ids: HashMap::new(),
            id_windows: HashMap::new(),
            surface_info: HashMap::new(),
            surface_views: HashMap::new(),
            output_size: Size::from((0, 0)),
            outputs: Vec::new(),
            output_config: HashMap::new(),
            pointer_location: (0.0, 0.0),
            focused_surface_id: 0,
            keyboard_focus: None,
            keyboard_focus_dirty: false,
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
            layer_shell_on_demand_focus: None,
            working_areas: HashMap::new(),
            activation_state,
            foreign_toplevel_state,
            session_lock_state,
            lock_state: LockState::Unlocked,
            pre_lock_focus: None,
            idle_notifier_state,
            fractional_scale_state,
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

    /// Update primary scanout output for all surfaces on the given output.
    /// This tracks which output each surface is primarily displayed on,
    /// enabling frame callback throttling to prevent duplicate callbacks.
    pub fn update_primary_scanout_output(
        &self,
        output: &Output,
        render_element_states: &RenderElementStates,
    ) {
        // Update windows
        for window in self.space.elements() {
            window.with_surfaces(|surface, states| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    states,
                    render_element_states,
                    // Windows are shown on one output at a time
                    |_, _, output, _| output,
                );
            });
        }

        // Update layer surfaces
        let layer_map = layer_map_for_output(output);
        for layer in layer_map.layers() {
            layer.with_surfaces(|surface, states| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    states,
                    render_element_states,
                    // Layer surfaces are shown on one output at a time
                    |_, _, output, _| output,
                );
            });
        }
        drop(layer_map);

        // Update lock surfaces
        if let Some(output_state) = self.output_state.get(output) {
            if let Some(ref lock_surface) = output_state.lock_surface {
                with_surface_tree_downward(
                    lock_surface.wl_surface(),
                    (),
                    |_, _, _| TraversalAction::DoChildren(()),
                    |surface, states, _| {
                        update_surface_primary_scanout_output(
                            surface,
                            output,
                            states,
                            render_element_states,
                            |_, _, output, _| output,
                        );
                    },
                    |_, _, _| true,
                );
            }
        }
    }

    /// Send frame callbacks to surfaces on an output with throttling.
    /// Uses primary scanout output tracking to avoid sending callbacks to surfaces
    /// not visible on this output, and frame callback sequence numbers to prevent
    /// duplicate callbacks within the same VBlank cycle.
    pub fn send_frame_callbacks(&self, output: &Output) {
        let sequence = self
            .output_state
            .get(output)
            .map(|s| s.frame_callback_sequence)
            .unwrap_or(0);

        let should_send = |surface: &WlSurface, states: &SurfaceData| {
            // Check if this surface's primary scanout output matches
            let current_primary_output = surface_primary_scanout_output(surface, states);
            if current_primary_output.as_ref() != Some(output) {
                return None;
            }

            // Check throttling: don't send if already sent this cycle
            let frame_throttling_state = states
                .data_map
                .get_or_insert(SurfaceFrameThrottlingState::default);
            let mut last_sent_at = frame_throttling_state.last_sent_at.borrow_mut();

            if let Some((last_output, last_sequence)) = &*last_sent_at {
                if last_output == output && *last_sequence == sequence {
                    return None;
                }
            }

            *last_sent_at = Some((output.clone(), sequence));
            Some(output.clone())
        };

        let frame_callback_time =
            crate::protocols::screencopy::get_monotonic_time();

        for window in self.space.elements() {
            window.send_frame(
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                &should_send,
            );
        }

        let layer_map = layer_map_for_output(output);
        for layer in layer_map.layers() {
            layer.send_frame(
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                &should_send,
            );
        }
        drop(layer_map);

        if let Some(output_state) = self.output_state.get(output) {
            if let Some(ref lock_surface) = output_state.lock_surface {
                send_frames_surface_tree(
                    lock_surface.wl_surface(),
                    output,
                    frame_callback_time,
                    FRAME_CALLBACK_THROTTLE,
                    &should_send,
                );
            }
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

        self.foreign_toplevel_state.refresh::<State>(windows);
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

                if let Ok(iface) = server.interface::<_, dbus::screen_cast::Session>(path.as_str())
                {
                    async_io::block_on(async {
                        let signal_emitter = iface.signal_emitter().clone();
                        iface.get().stop(server.inner(), signal_emitter).await
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

    /// Set focus to a surface and notify Emacs.
    /// Marks keyboard focus dirty for deferred sync.
    pub fn set_focus(&mut self, id: u32) {
        if id != self.focused_surface_id && id != 0 {
            self.focused_surface_id = id;
            self.keyboard_focus_dirty = true;
            crate::module::set_focused_id(id);
            // Notify Emacs about focus change (skip Emacs frames, they handle their own focus)
            if !self.emacs_surfaces.contains(&id) {
                self.queue_event(Event::Focus { id });
            }
        }
    }

    /// Focus a surface, updating internal state only.
    /// Keyboard focus is synced via deferred sync_keyboard_focus().
    pub fn focus_surface(&mut self, id: u32, notify_emacs: bool) {
        self.focus_surface_with_source(id, notify_emacs, "focus_surface", None);
    }

    /// Focus a surface with source tracking for debugging.
    /// Marks keyboard focus dirty for deferred sync via sync_keyboard_focus().
    pub fn focus_surface_with_source(
        &mut self,
        id: u32,
        notify_emacs: bool,
        source: &str,
        context: Option<&str>,
    ) {
        module::record_focus(id, source, context);
        self.focused_surface_id = id;
        self.keyboard_focus_dirty = true;
        crate::module::set_focused_id(id);
        if notify_emacs {
            self.queue_event(Event::Focus { id });
        }
    }

    /// Update on-demand layer shell keyboard focus.
    /// If the surface has OnDemand keyboard interactivity, set it as on-demand focus.
    /// Otherwise, clear on-demand focus. Following niri's focus_layer_surface_if_on_demand.
    pub fn focus_layer_surface_if_on_demand(
        &mut self,
        surface: Option<DesktopLayerSurface>,
    ) {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        if let Some(surface) = surface {
            if surface.cached_state().keyboard_interactivity
                == KeyboardInteractivity::OnDemand
            {
                if self.layer_shell_on_demand_focus.as_ref() != Some(&surface) {
                    self.layer_shell_on_demand_focus = Some(surface);
                    self.keyboard_focus_dirty = true;
                }
                return;
            }
        }

        // Something else got clicked, clear on-demand layer-shell focus
        if self.layer_shell_on_demand_focus.is_some() {
            self.layer_shell_on_demand_focus = None;
            self.keyboard_focus_dirty = true;
        }
    }

    /// Resolve layer shell keyboard focus.
    /// Checks for Exclusive interactivity on Overlay/Top layers first,
    /// then OnDemand focus, following niri's update_keyboard_focus pattern.
    fn resolve_layer_keyboard_focus(&self) -> Option<WlSurface> {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        // Helper: find exclusive focus on a layer
        let excl_on_layer = |output: &Output, layer: Layer| -> Option<WlSurface> {
            let map = layer_map_for_output(output);
            let layers: Vec<_> = map.layers_on(layer).cloned().collect();
            layers.into_iter().find_map(|surface| {
                if surface.cached_state().keyboard_interactivity
                    == KeyboardInteractivity::Exclusive
                {
                    Some(surface.wl_surface().clone())
                } else {
                    None
                }
            })
        };

        // Helper: check if on-demand focus is on a layer
        let on_demand_on_layer = |output: &Output, layer: Layer| -> Option<WlSurface> {
            let on_demand = self.layer_shell_on_demand_focus.as_ref()?;
            let map = layer_map_for_output(output);
            let layers: Vec<_> = map.layers_on(layer).cloned().collect();
            layers.into_iter().find_map(|surface| {
                if &surface == on_demand {
                    Some(surface.wl_surface().clone())
                } else {
                    None
                }
            })
        };

        // Check all outputs (typically just one for EWM)
        for output in self.space.outputs() {
            // Exclusive Overlay takes highest priority
            if let Some(s) = excl_on_layer(output, Layer::Overlay) {
                return Some(s);
            }
            // Exclusive Top
            if let Some(s) = excl_on_layer(output, Layer::Top) {
                return Some(s);
            }
            // OnDemand on any layer
            for layer in [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background] {
                if let Some(s) = on_demand_on_layer(output, layer) {
                    return Some(s);
                }
            }
            // Exclusive Bottom/Background (only when no toplevel has focus)
            if self.focused_surface_id == 0 || self.id_windows.get(&self.focused_surface_id).is_none() {
                if let Some(s) = excl_on_layer(output, Layer::Bottom) {
                    return Some(s);
                }
                if let Some(s) = excl_on_layer(output, Layer::Background) {
                    return Some(s);
                }
            }
        }

        None
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

    /// Find the Output at a global logical position.
    fn output_at(
        &self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> Option<&Output> {
        let point = smithay::utils::Point::from((pos.x as i32, pos.y as i32));
        self.space
            .outputs()
            .find(|o| {
                self.space
                    .output_geometry(o)
                    .map_or(false, |geo| geo.contains(point))
            })
            .or_else(|| self.space.outputs().next())
    }

    /// Check layer surfaces on a specific layer for a surface under the point.
    /// `pos_within_output` is the point relative to the output origin.
    /// Returns the WlSurface and its location in global coordinates.
    fn layer_surface_under(
        &self,
        output: &Output,
        layer: Layer,
        pos_within_output: smithay::utils::Point<f64, smithay::utils::Logical>,
        output_pos: smithay::utils::Point<i32, smithay::utils::Logical>,
    ) -> Option<(
        WlSurface,
        smithay::utils::Point<f64, smithay::utils::Logical>,
    )> {
        let map = layer_map_for_output(output);
        let layers: Vec<_> = map.layers_on(layer).rev().cloned().collect();
        for layer_surface in &layers {
            let geo = match map.layer_geometry(layer_surface) {
                Some(g) => g,
                None => continue,
            };
            let layer_pos = geo.loc.to_f64();
            if let Some((surface, pos_in_layer)) = layer_surface.surface_under(
                pos_within_output - layer_pos,
                WindowSurfaceType::ALL,
            ) {
                let global_pos = (pos_in_layer + geo.loc).to_f64() + output_pos.to_f64();
                return Some((surface, global_pos));
            }
        }
        None
    }

    /// Find the layer surface (desktop type) under a point.
    /// Used for click-to-focus on layer surfaces with OnDemand keyboard interactivity.
    pub fn layer_under_point(
        &self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> Option<DesktopLayerSurface> {
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let pos_within_output = pos - output_geo.loc.to_f64();

        let map = layer_map_for_output(output);
        // Check in render order: Overlay → Top → Bottom → Background
        for layer in [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background] {
            let layers: Vec<_> = map.layers_on(layer).rev().cloned().collect();
            for layer_surface in &layers {
                let geo = match map.layer_geometry(layer_surface) {
                    Some(g) => g,
                    None => continue,
                };
                let layer_pos = geo.loc.to_f64();
                if layer_surface
                    .surface_under(
                        pos_within_output - layer_pos,
                        WindowSurfaceType::ALL,
                    )
                    .is_some()
                {
                    return Some(layer_surface.clone());
                }
            }
        }
        None
    }

    /// Find the surface under a point, checking layers and popups in render order.
    /// Order: Overlay → Top → [window popups] → [toplevels] → Bottom → Background
    /// Returns the surface and its location in global coordinates.
    pub fn surface_under_point(
        &self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> Option<(
        WlSurface,
        smithay::utils::Point<f64, smithay::utils::Logical>,
    )> {
        use smithay::wayland::seat::WaylandFocus;

        let output = self.output_at(pos);
        let output_geo = output.and_then(|o| self.space.output_geometry(o));

        if let (Some(output), Some(geo)) = (output, output_geo) {
            let pos_within_output = pos - geo.loc.to_f64();

            // 1. Overlay layer (highest)
            if let Some(result) =
                self.layer_surface_under(output, Layer::Overlay, pos_within_output, geo.loc)
            {
                return Some(result);
            }

            // 2. Top layer
            if let Some(result) =
                self.layer_surface_under(output, Layer::Top, pos_within_output, geo.loc)
            {
                return Some(result);
            }
        }

        // 3. Window popups
        for window in self.space.elements() {
            if let Some(surface) = window.wl_surface() {
                let window_loc = self.space.element_location(window).unwrap_or_default();
                let window_geo = window.geometry();

                for (popup, popup_offset) in PopupManager::popups_for_surface(&surface) {
                    let popup_loc = (window_loc + window_geo.loc + popup_offset
                        - popup.geometry().loc)
                        .to_f64();
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

        // 4. Toplevels
        if let Some(result) = self
            .space
            .element_under(pos)
            .and_then(|(window, loc)| window.wl_surface().map(|s| (s.into_owned(), loc.to_f64())))
        {
            return Some(result);
        }

        // 5-6. Bottom and Background layers
        if let (Some(output), Some(geo)) = (output, output_geo) {
            let pos_within_output = pos - geo.loc.to_f64();

            if let Some(result) =
                self.layer_surface_under(output, Layer::Bottom, pos_within_output, geo.loc)
            {
                return Some(result);
            }

            if let Some(result) =
                self.layer_surface_under(output, Layer::Background, pos_within_output, geo.loc)
            {
                return Some(result);
            }
        }

        None
    }

    /// Get the output where a surface is located
    fn get_surface_output(&self, surface_id: u32) -> Option<String> {
        let window = self.id_windows.get(&surface_id)?;
        let window_loc = self.space.element_location(window)?;

        // Find which output contains this window's location
        for output in self.space.outputs() {
            if let Some(geo) = self.space.output_geometry(output) {
                if geo.contains(window_loc) {
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
            let window_size = self
                .id_windows
                .get(&window_id)
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
                let window_rect: Rectangle<i32, Logical> =
                    Rectangle::new(loc, Size::from((window_geo.size.w, window_geo.size.h)));

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
            self.space.outputs().fold((0i32, 0i32), |(w, h), output| {
                if let Some(geo) = self.space.output_geometry(output) {
                    (w.max(geo.loc.x + geo.size.w), h.max(geo.loc.y + geo.size.h))
                } else {
                    (w, h)
                }
            });
        self.output_size = Size::from((total_width, total_height));
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

    /// Notify all surfaces on an output about a scale/transform change.
    ///
    /// Iterates windows and layer surfaces on the given output,
    /// sending both integer and fractional scale via `send_scale_transform`.
    /// Called from `apply_output_config` after changing an output's scale or transform.
    pub fn send_scale_transform_to_output_surfaces(&self, output: &Output) {
        let scale = output.current_scale();
        let transform = output.current_transform();

        // Notify windows on this output
        for window in self.space.elements() {
            window.with_surfaces(|surface, data| {
                crate::utils::send_scale_transform(surface, data, scale, transform);
            });
        }

        // Notify layer surfaces on this output
        let layer_map = layer_map_for_output(output);
        for layer in layer_map.layers() {
            layer.with_surfaces(|surface, data| {
                crate::utils::send_scale_transform(surface, data, scale, transform);
            });
        }
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
        // Re-arrange layer map so it picks up any scale/mode/transform change
        layer_map_for_output(output).arrange();
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

            // Update module offset to reflect frame origin (output pos + working area)
            if let Some(output_geo) = self.space.output_geometry(output) {
                crate::module::set_output_offset(
                    &output_name,
                    output_geo.loc.x + working_area.loc.x,
                    output_geo.loc.y + working_area.loc.y,
                );
            }

            // Update Emacs frames to fit new working area
            self.update_frames_for_working_area(output);

            // Notify Emacs
            self.queue_event(Event::WorkingArea {
                output: output_name.clone(),
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

    /// Get info about all mapped layer surfaces for state dump.
    pub fn get_layer_surfaces_info(&self) -> Vec<serde_json::Value> {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        let mut result = Vec::new();
        for output in self.space.outputs() {
            let map = layer_map_for_output(output);
            for layer in [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background] {
                let layers: Vec<_> = map.layers_on(layer).cloned().collect();
                for layer_surface in &layers {
                    let cached = layer_surface.cached_state();
                    let geo = map.layer_geometry(layer_surface);
                    let kb_interactivity = match cached.keyboard_interactivity {
                        KeyboardInteractivity::None => "none",
                        KeyboardInteractivity::Exclusive => "exclusive",
                        KeyboardInteractivity::OnDemand => "on_demand",
                    };
                    let is_on_demand_focused =
                        self.layer_shell_on_demand_focus.as_ref() == Some(layer_surface);
                    result.push(serde_json::json!({
                        "namespace": layer_surface.namespace(),
                        "layer": format!("{:?}", layer),
                        "output": output.name(),
                        "keyboard_interactivity": kb_interactivity,
                        "geometry": geo.map(|g| serde_json::json!({
                            "x": g.loc.x, "y": g.loc.y,
                            "w": g.size.w, "h": g.size.h,
                        })),
                        "on_demand_focused": is_on_demand_focused,
                    }));
                }
            }
        }
        result
    }

    /// Check if there are pending screencopy requests for any output
    pub fn has_pending_screencopies(&self) -> bool {
        // This is a workaround since we can't easily check the internal state
        // without mutable access. We'll always return false here and let
        // the render loop handle it with the mutable state.
        false
    }

    /// Unconstrain a popup's position to keep it within screen bounds
    pub fn unconstrain_popup(&self, popup: &PopupSurface) {
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
        let mut target = Rectangle::from_size(self.output_size);
        target.loc -= window_loc + window_geo.loc;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    /// Handle new toplevel surface from XdgShellHandler
    pub fn handle_new_toplevel(&mut self, surface: ToplevelSurface) {
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
            state.size = Some(self.output_size);
            state.states.set(XdgToplevelState::Maximized);
            state.states.set(XdgToplevelState::Activated);
        });
        surface.send_configure();

        // Send fractional scale and transform to the new surface
        if let Some(output) = self.space.outputs().next() {
            let scale = output.current_scale();
            let transform = output.current_transform();
            smithay::wayland::compositor::with_states(surface.wl_surface(), |data| {
                crate::utils::send_scale_transform(surface.wl_surface(), data, scale, transform);
            });
        }

        let window = Window::new_wayland_window(surface);
        self.window_ids.insert(window.clone(), id);
        self.id_windows.insert(id, window.clone());

        // Determine target output
        let frame_output = module::take_pending_frame_output();
        let target_output = frame_output.clone().or_else(|| self.active_output());

        // Position based on surface type
        let position = if let Some(ref output_name) = frame_output {
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
            (-10000, -10000)
        };
        self.space.map_element(window.clone(), position, false);

        // Resize Emacs frames to fill their working area
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

        // Send event to Emacs
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

    /// Handle toplevel destroyed from XdgShellHandler.
    /// Returns surface ID to refocus, if any.
    pub fn handle_toplevel_destroyed(&mut self, surface: ToplevelSurface) -> Option<u32> {
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel().map(|t| t == &surface).unwrap_or(false))
            .cloned();

        if let Some(window) = window {
            if let Some(id) = self.window_ids.remove(&window) {
                let was_focused = self.focused_surface_id == id;
                let output = self.get_surface_output(id);

                self.id_windows.remove(&id);
                self.surface_info.remove(&id);
                self.surface_views.remove(&id);
                self.emacs_surfaces.remove(&id);
                self.queue_event(Event::Close { id });
                info!("Toplevel {} destroyed", id);

                self.space.unmap_elem(&window);

                // Return refocus target if needed
                if was_focused {
                    return output
                        .as_ref()
                        .and_then(|out| {
                            self.emacs_surfaces
                                .iter()
                                .find(|&&eid| self.get_surface_output(eid).as_ref() == Some(out))
                                .copied()
                        })
                        .or(Some(1));
                }
            } else {
                self.space.unmap_elem(&window);
            }
        }
        None
    }

    pub fn init_wayland_listener(
        display: Display<State>,
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
                if let Err(e) = display.dispatch_clients(state) {
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
            .find(|w| {
                w.wl_surface()
                    .map(|s| s.as_ref() == surface)
                    .unwrap_or(false)
            })
            .cloned();

        if let Some(window) = window {
            if let Some(&id) = self.window_ids.get(&window) {
                // Skip title change events for Emacs surfaces
                if self.emacs_surfaces.contains(&id) {
                    return;
                }

                let (app_id, title) =
                    smithay::wayland::compositor::with_states(surface, |states| {
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
                        self.keyboard_focus_dirty = true;

                        // Auto-focus newly mapped OnDemand surfaces (following niri #641)
                        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;
                        if layer.cached_state().keyboard_interactivity
                            == KeyboardInteractivity::OnDemand
                        {
                            self.layer_shell_on_demand_focus = Some(layer.clone());
                        }
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
impl BufferHandler for State {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

// Compositor protocol
impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.ewm.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("ClientState inserted at connection time")
            .compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // Queue early import for DRM backend (processed in main loop)
        self.ewm.pending_early_imports.push(surface.clone());

        // Handle layer surface commits
        if self.ewm.handle_layer_surface_commit(surface) {
            return;
        }

        // Handle popup commits
        self.ewm.popups.commit(surface);
        if let Some(popup) = self.ewm.popups.find_popup(surface) {
            if let PopupKind::Xdg(ref xdg_popup) = popup {
                if !xdg_popup.is_initial_configure_sent() {
                    xdg_popup
                        .send_configure()
                        .expect("initial configure failed");
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
        let window_and_id = self.ewm.space.elements().find_map(|window| {
            window.wl_surface().and_then(|ws| {
                if *ws == root_surface {
                    self.ewm
                        .window_ids
                        .get(window)
                        .map(|&id| (window.clone(), id))
                } else {
                    None
                }
            })
        });

        if let Some((window, _id)) = window_and_id {
            // Call on_commit only for this specific window
            window.on_commit();

            // Queue redraw for all outputs
            self.ewm.queue_redraw_all();

            // Check for title/app_id changes (only for toplevels)
            self.ewm.check_surface_info_changes(surface);
        }
        // For surfaces without a toplevel (popups, layer surfaces, etc.),
        // the parent's commit or other handlers will manage redraw
    }
}
delegate_compositor!(State);

// Shared memory
impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.ewm.shm_state
    }
}
delegate_shm!(State);

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
delegate_dmabuf!(State);

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
        let surface_id = focused.and_then(|s| self.ewm.surface_id(s));
        self.ewm.update_text_input_focus(focused, surface_id);
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}
delegate_seat!(State);

// Data device / selection
impl SelectionHandler for State {
    type SelectionUserData = Arc<[u8]>;

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if ty == SelectionTarget::Clipboard {
            if let Some(source) = &source {
                let mime_types = source.mime_types();
                if mime_types.iter().any(|m| m.contains("text")) {
                    self.read_client_selection_to_emacs();
                }
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
            use smithay::reexports::rustix::fs::{fcntl_setfl, OFlags};
            use std::io::Write;
            if let Err(err) = fcntl_setfl(&fd, OFlags::empty()) {
                warn!("error clearing flags on selection fd: {err:?}");
            }
            if let Err(err) = std::fs::File::from(fd).write_all(&buf) {
                warn!("error writing selection: {err:?}");
            }
        });
    }
}
impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}
impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.ewm.data_device_state
    }
}
delegate_data_device!(State);

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.ewm.primary_selection_state
    }
}
delegate_primary_selection!(State);

impl DataControlHandler for State {
    fn data_control_state(&self) -> &DataControlState {
        &self.ewm.data_control_state
    }
}
delegate_data_control!(State);

// Output
impl smithay::wayland::output::OutputHandler for State {}
delegate_output!(State);

// Text Input (for input method support)
delegate_text_input_manager!(State);

// Input Method (allows Emacs to act as input method)
impl InputMethodHandler for State {
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
delegate_input_method_manager!(State);

// XDG Shell
impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.ewm.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.ewm.handle_new_toplevel(surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(refocus_id) = self.ewm.handle_toplevel_destroyed(surface) {
            // Refocus to the returned surface (keyboard sync deferred)
            self.ewm.focus_surface(refocus_id, true);
            self.sync_keyboard_focus();
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.ewm.unconstrain_popup(&surface);
        if let Err(err) = self.ewm.popups.track_popup(PopupKind::Xdg(surface)) {
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

        if let Err(err) = self
            .ewm
            .popups
            .grab_popup(root, popup, &self.ewm.seat, serial)
        {
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
        self.ewm.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn popup_destroyed(&mut self, _surface: PopupSurface) {
        // Queue redraw to clear the popup from screen
        self.ewm.queue_redraw_all();
    }
}
delegate_xdg_shell!(State);

// XDG Decoration
impl XdgDecorationHandler for State {
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
smithay::delegate_xdg_decoration!(State);

// Layer Shell
impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.ewm.layer_shell_state
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
            self.ewm.space.outputs().next().cloned()
        };

        let Some(output) = output else {
            warn!("No output for new layer surface, closing");
            surface.send_close();
            return;
        };

        let wl_surface = surface.wl_surface().clone();
        self.ewm.unmapped_layer_surfaces.insert(wl_surface.clone());

        // Send fractional scale and transform for this output
        let scale = output.current_scale();
        let transform = output.current_transform();
        smithay::wayland::compositor::with_states(&wl_surface, |data| {
            crate::utils::send_scale_transform(&wl_surface, data, scale, transform);
        });

        let mut map = layer_map_for_output(&output);
        map.map_layer(&LayerSurface::new(surface, namespace.clone()))
            .unwrap();
        info!(
            "New layer surface: namespace={} on output {}",
            namespace,
            output.name()
        );
    }

    fn layer_destroyed(&mut self, surface: smithay::wayland::shell::wlr_layer::LayerSurface) {
        let wl_surface = surface.wl_surface();
        self.ewm.unmapped_layer_surfaces.remove(wl_surface);

        // Find and unmap the layer surface
        let output = self.ewm.space.outputs().find_map(|o| {
            let map = layer_map_for_output(o);
            let layer = map
                .layers()
                .find(|&layer| layer.layer_surface() == &surface)
                .cloned();
            layer.map(|layer| (o.clone(), layer))
        });

        if let Some((output, layer)) = output {
            // Clear on-demand focus if it was this layer surface
            if self.ewm.layer_shell_on_demand_focus.as_ref() == Some(&layer) {
                self.ewm.layer_shell_on_demand_focus = None;
            }

            let mut map = layer_map_for_output(&output);
            map.unmap_layer(&layer);
            // Re-arrange after unmapping to recalculate exclusive zones
            map.arrange();
            drop(map);

            self.ewm.keyboard_focus_dirty = true;

            // Check for working area expansion (panel removed)
            self.ewm.check_working_area_change(&output);

            self.ewm.queue_redraw(&output);
            info!("Layer surface destroyed");
        }
    }

    fn new_popup(
        &mut self,
        _parent: smithay::wayland::shell::wlr_layer::LayerSurface,
        popup: smithay::wayland::shell::xdg::PopupSurface,
    ) {
        let _ = self.ewm.popups.track_popup(PopupKind::Xdg(popup));
    }
}
delegate_layer_shell!(State);

// XDG Activation protocol (allows apps to request focus)
impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.ewm.activation_state
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
            debug!(
                "xdg_activation: token rejected for {app_id} - serial not from app's focus entry"
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
        use std::time::Duration;
        const TOKEN_TIMEOUT: Duration = Duration::from_secs(10);

        debug!(
            "xdg_activation: request_activation called for surface {:?}",
            surface.id()
        );

        if token_data.timestamp.elapsed() < TOKEN_TIMEOUT {
            // Find the surface ID for this WlSurface
            if let Some(&id) = self
                .ewm
                .window_ids
                .iter()
                .find(|(w, _)| w.wl_surface().map(|s| &*s == &surface).unwrap_or(false))
                .map(|(_, id)| id)
            {
                // Focus the surface and notify Emacs (keyboard sync deferred)
                self.ewm
                    .focus_surface_with_source(id, true, "xdg_activation", None);
                info!("xdg_activation: granted for surface {}", id);
            } else {
                debug!("xdg_activation: surface not found in window_ids");
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
delegate_xdg_activation!(State);

// Foreign toplevel management protocol (exposes windows to external tools)
impl ForeignToplevelHandler for State {
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState {
        &mut self.ewm.foreign_toplevel_state
    }

    fn activate(&mut self, wl_surface: WlSurface) {
        if let Some(&id) = self
            .ewm
            .window_ids
            .iter()
            .find(|(w, _)| w.wl_surface().map(|s| &*s == &wl_surface).unwrap_or(false))
            .map(|(_, id)| id)
        {
            self.ewm
                .focus_surface_with_source(id, true, "foreign_toplevel", None);
            info!("Foreign toplevel: activated surface {}", id);
        }
    }

    fn close(&mut self, wl_surface: WlSurface) {
        if let Some((window, _)) = self
            .ewm
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
delegate_foreign_toplevel!(State);

// Screencopy protocol
impl ScreencopyHandler for State {
    fn frame(&mut self, manager: &ZwlrScreencopyManagerV1, screencopy: Screencopy) {
        // Queue all screencopy requests for processing during render
        // (both with_damage and immediate requests are handled in the render loop)
        if let Some(queue) = self.ewm.screencopy_state.get_queue_mut(manager) {
            queue.push(screencopy);
        }
    }

    fn screencopy_state(&mut self) -> &mut ScreencopyManagerState {
        &mut self.ewm.screencopy_state
    }
}
delegate_screencopy!(State);

// Session Lock protocol (ext-session-lock-v1) for screen locking
impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.ewm.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        // Check for dead locker client holding the lock
        if let LockState::Locked(ref lock) = self.ewm.lock_state {
            if lock.is_alive() {
                info!("Session lock request ignored: already locked with active client");
                return;
            }
            // Previous client died, allow new lock
            info!("Previous lock client dead, allowing new lock");
        } else if !matches!(self.ewm.lock_state, LockState::Unlocked) {
            info!("Session lock request ignored: already locking");
            return;
        }

        info!("Session lock requested");

        // Save current focus to restore after unlock
        if self.ewm.focused_surface_id != 0 {
            self.ewm.pre_lock_focus = Some(self.ewm.focused_surface_id);
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
    }

    fn unlock(&mut self) {
        info!("Session unlock requested");
        self.ewm.lock_state = LockState::Unlocked;

        // Clear lock surfaces and reset render states
        for state in self.ewm.output_state.values_mut() {
            state.lock_surface = None;
            state.lock_render_state = LockRenderState::Unlocked;
        }

        // Restore focus to the surface that was focused before locking
        if let Some(id) = self.ewm.pre_lock_focus.take() {
            if self.ewm.id_windows.contains_key(&id) {
                info!("Restoring focus to surface {} after unlock", id);
                self.ewm
                    .focus_surface_with_source(id, false, "unlock", None);
            }
        }

        self.ewm.queue_redraw_all();
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
        if let Some(state) = self.ewm.output_state.get_mut(&output) {
            state.lock_surface = Some(surface);
        }

        self.ewm.queue_redraw(&output);
    }
}
delegate_session_lock!(State);

// Idle notify protocol (ext-idle-notify-v1)
impl IdleNotifierHandler for State {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.ewm.idle_notifier_state
    }
}
delegate_idle_notify!(State);

// Fractional scale protocol (wp-fractional-scale-v1)
impl FractionalScaleHandler for State {
    fn new_fractional_scale(
        &mut self,
        surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        // Send the current output's fractional scale to the new surface.
        // Find which output the surface's toplevel/layer is on.
        if let Some(output) = self.ewm.space.outputs().next().cloned() {
            let scale = output.current_scale();
            let transform = output.current_transform();
            smithay::wayland::compositor::with_states(&surface, |data| {
                crate::utils::send_scale_transform(&surface, data, scale, transform);
            });
        }
    }
}
delegate_fractional_scale!(State);

/// Configure a lock surface to cover the full output
fn configure_lock_surface(surface: &LockSurface, output: &Output) {
    use smithay::wayland::compositor::with_states;

    surface.with_pending_state(|states| {
        let mode = output.current_mode().unwrap();
        let size = output.current_transform().transform_size(mode.size);
        states.size = Some(Size::from((size.w as u32, size.h as u32)));
    });

    let scale = output.current_scale();
    let transform = output.current_transform();
    let wl_surface = surface.wl_surface();

    with_states(wl_surface, |data| {
        crate::utils::send_scale_transform(wl_surface, data, scale, transform);
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
            && self
                .output_state
                .values()
                .all(|s| s.lock_render_state == LockRenderState::Locked);

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
        let cursor_output = self.output_under_cursor().and_then(|name| {
            self.output_state
                .iter()
                .find(|(o, _)| o.name() == name)
                .map(|(o, _)| o.clone())
        });

        let target_output = cursor_output.or_else(|| self.output_state.keys().next().cloned());

        target_output.and_then(|output| {
            self.output_state
                .get(&output)?
                .lock_surface
                .as_ref()
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

    /// Abort the lock if we failed to render during Locking state.
    /// This prevents the session from being stuck in an unlockable state.
    pub fn abort_lock_on_render_failure(&mut self) {
        if matches!(&self.lock_state, LockState::Locking(_)) {
            warn!("Aborting session lock due to render failure");
            // Reset to unlocked - the SessionLocker will be dropped, signaling failure
            self.lock_state = LockState::Unlocked;
            // Clear any lock surfaces
            for state in self.output_state.values_mut() {
                state.lock_surface = None;
                state.lock_render_state = LockRenderState::Unlocked;
            }
            self.queue_redraw_all();
        }
    }
}

/// Shared state for compositor event loop (passed to all handlers)
///
/// Note: Display is owned by the event loop (via Generic source), not by State.
/// The Backend enum allows using either DRM (production) or Headless (testing) backends.
pub struct State {
    pub backend: backend::Backend,
    pub ewm: Ewm,
}

impl State {
    /// Synchronize Wayland keyboard focus with focused_surface_id.
    ///
    /// This is the primary mechanism for keeping logical focus (focused_surface_id)
    /// in sync with Wayland keyboard focus (keyboard.set_focus). Most focus-changing
    /// code paths just set focused_surface_id + keyboard_focus_dirty=true, and this
    /// function resolves the actual WlSurface and calls keyboard.set_focus().
    ///
    /// Called from: handle_keyboard_event (before filter), after module command
    /// batch, and main loop tick. The intercept_redirect path is the only code
    /// that calls keyboard.set_focus() directly (it must be atomic with key
    /// forwarding).
    pub fn sync_keyboard_focus(&mut self) {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        if !self.ewm.keyboard_focus_dirty {
            return;
        }
        self.ewm.keyboard_focus_dirty = false;

        // Clean up stale on-demand focus
        if let Some(surface) = &self.ewm.layer_shell_on_demand_focus {
            let good = surface.alive()
                && surface.cached_state().keyboard_interactivity
                    == KeyboardInteractivity::OnDemand;
            if !good {
                self.ewm.layer_shell_on_demand_focus = None;
            }
        }

        // Check layer shell surfaces for exclusive/on-demand keyboard focus.
        // Priority: Exclusive on Overlay/Top, then OnDemand, then toplevel.
        let layer_focus = self.ewm.resolve_layer_keyboard_focus();

        let new_focus = if let Some(wl_surface) = layer_focus {
            Some(wl_surface)
        } else {
            // Fall back to toplevel focus
            let target_id = self.ewm.focused_surface_id;
            self.ewm
                .id_windows
                .get(&target_id)
                .and_then(|w| w.wl_surface())
                .map(|s| s.into_owned())
        };

        if self.ewm.keyboard_focus != new_focus {
            self.ewm.keyboard_focus = new_focus.clone();
            let keyboard = self.ewm.keyboard.clone();
            keyboard.set_focus(self, new_focus, SERIAL_COUNTER.next_serial());
        }
    }

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

        // Sync keyboard focus after processing module commands.
        // This catches any focus changes from Emacs commands, xdg_activation, etc.
        self.sync_keyboard_focus();

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
                // Skip if already focused (keyboard sync deferred to after command batch)
                if self.ewm.focused_surface_id != id && self.ewm.id_windows.contains_key(&id) {
                    self.ewm
                        .focus_surface_with_source(id, false, "emacs_command", None);
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
                    self,
                    under,
                    &smithay::input::pointer::MotionEvent {
                        location: (x, y).into(),
                        serial,
                        time: 0,
                    },
                );
                pointer.frame(self);
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
                scale,
                transform,
                enabled,
            } => {
                // Update stored config (merge with existing)
                let config = self.ewm.output_config.entry(name.clone()).or_default();
                if let (Some(w), Some(h)) = (width, height) {
                    config.mode = Some((w, h, refresh));
                }
                if x.is_some() || y.is_some() {
                    let current_pos = self
                        .ewm
                        .space
                        .outputs()
                        .find(|o| o.name() == name)
                        .and_then(|o| self.ewm.space.output_geometry(o))
                        .map(|g| (g.loc.x, g.loc.y))
                        .unwrap_or((0, 0));
                    config.position = Some((
                        x.unwrap_or(current_pos.0),
                        y.unwrap_or(current_pos.1),
                    ));
                }
                if let Some(s) = scale {
                    config.scale = Some(s);
                }
                if let Some(t) = transform {
                    config.transform = Some(backend::int_to_transform(t));
                }
                if let Some(e) = enabled {
                    config.enabled = e;
                }

                // Apply the config
                self.backend.apply_output_config(&mut self.ewm, &name);
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
                if let Err(e) = keyboard.set_xkb_config(self, xkb_config) {
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
                let index = self.ewm.xkb_layout_names.iter().position(|l| l == &layout);
                match index {
                    Some(idx) => {
                        use smithay::input::keyboard::Layout;
                        let keyboard = self.ewm.keyboard.clone();
                        let current_focus = self.ewm.keyboard_focus.clone();
                        keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
                        keyboard.with_xkb_state(self, |mut context| {
                            context.set_layout(Layout(idx as u32));
                        });
                        keyboard.set_focus(self, current_focus, SERIAL_COUNTER.next_serial());
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
                let layer_surfaces_info = self.ewm.get_layer_surfaces_info();
                let state = serde_json::json!({
                    "surfaces": self.ewm.surface_info,
                    "emacs_surfaces": self.ewm.emacs_surfaces,
                    "surface_views": self.ewm.surface_views,
                    "focused_surface_id": self.ewm.focused_surface_id,
                    "id_windows": id_window_keys,
                    "outputs": self.ewm.outputs,
                    "working_areas": self.ewm.get_working_areas_info(),
                    "layer_surfaces": layer_surfaces_info,
                    "pointer_location": self.ewm.pointer_location,
                    "intercepted_keys": module::get_intercepted_keys(),
                    "emacs_pid": self.ewm.emacs_pid,
                    "text_input_intercept": self.ewm.text_input_intercept,
                    "text_input_active": self.ewm.text_input_active,
                    "xkb_layouts": self.ewm.xkb_layout_names,
                    "xkb_current_layout": self.ewm.xkb_current_layout,
                    "next_surface_id": self.ewm.next_surface_id,
                    "redraw_states": self.ewm.output_state.iter().map(|(output, state)| {
                        serde_json::json!({
                            "output": output.name(),
                            "state": state.redraw_state.to_string(),
                        })
                    }).collect::<Vec<_>>(),
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
            ModuleCommand::SetSelection { text } => {
                let data: Arc<[u8]> = Arc::from(text.into_bytes().into_boxed_slice());
                set_data_device_selection(
                    &self.ewm.display_handle,
                    &self.ewm.seat,
                    vec![
                        "text/plain;charset=utf-8".into(),
                        "text/plain".into(),
                        "UTF8_STRING".into(),
                    ],
                    data,
                );
                debug!("Selection set from Emacs");
            }
        }
    }

    /// Read clipboard data from the current client selection and forward to Emacs.
    fn read_client_selection_to_emacs(&mut self) {
        let (read_end, write_end) = std::os::unix::net::UnixStream::pair()
            .expect("UnixStream::pair failed");

        let write_fd: OwnedFd = write_end.into();

        let mime = "text/plain;charset=utf-8".to_string();
        match request_data_device_client_selection(&self.ewm.seat, mime, write_fd) {
            Ok(()) => {
                std::thread::spawn(move || {
                    use std::io::Read;
                    // Set a read timeout so we don't block forever on misbehaving clients
                    let _ = read_end.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut read_end = read_end;
                    let mut buf = Vec::new();
                    if let Err(e) = read_end.read_to_end(&mut buf) {
                        warn!("error reading client selection: {e:?}");
                        return;
                    }
                    if let Ok(text) = String::from_utf8(buf) {
                        if !text.is_empty() {
                            module::push_event(Event::SelectionChanged { text });
                        }
                    }
                });
            }
            Err(_) => {}
        }
    }

    /// Process events from the IM relay and send to Emacs
    pub fn process_im_events(&mut self) {
        // Collect events first to avoid borrow conflict with queue_event
        let events: Vec<_> = self
            .ewm
            .im_relay
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
