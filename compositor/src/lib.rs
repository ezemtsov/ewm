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
//!    `Idle` → `Queued` → `WaitingForVBlank` → `Idle` (real VBlank path)
//!    `Idle` → `Queued` → `WaitingForEstimatedVBlank` → `Idle` (no-damage path)
//!    The estimated-VBlank variants use a timer when no damage occurs.
//!    `WaitingForVBlank { redraw_needed }` and `WaitingForEstimatedVBlankAndQueued`
//!    handle redraws requested while waiting. State is owned by
//!    `Ewm::output_state`, not the backend.
//!
//! 4. **Emacs ownership**: The compositor runs as a thread within Emacs.
//!    Emacs controls window layout and focus policy. The compositor handles
//!    protocol compliance and rendering. This split means:
//!    - Compositor never initiates focus changes without Emacs consent
//!    - Layout changes come from Emacs via the command queue
//!    - Events flow back to Emacs via the event queue
//!
//! 5. **Thread safety**: Communication between Emacs and compositor uses
//!    mutex-protected queues and atomic flags. The module interface (module.rs)
//!    provides the synchronization boundary.

pub mod backend;
pub mod cursor;
#[cfg(feature = "screencast")]
pub mod dbus;
pub mod event;
pub mod frame_clock;
pub mod im_relay;
pub mod input;
mod module;
#[cfg(feature = "screencast")]
pub mod pipewire;
pub mod protocols;
pub mod render;
pub mod tracy;
pub mod utils;
pub mod vblank_throttle;
pub mod xwayland;
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

/// Returns true for embedded laptop panel connectors (eDP, LVDS, DSI).
pub fn is_laptop_panel(connector_name: &str) -> bool {
    matches!(connector_name.get(..4), Some("eDP-" | "LVDS" | "DSI-"))
}

pub use event::{Event, OutputInfo, OutputMode};

pub use backend::{Backend, DrmBackendState, HeadlessBackend};

use crate::protocols::foreign_toplevel::{
    ForeignToplevelHandler, ForeignToplevelManagerState, WindowInfo,
};
use crate::protocols::output_management::{OutputManagementHandler, OutputManagementState};
use crate::protocols::screencopy::{Screencopy, ScreencopyHandler, ScreencopyManagerState};
use crate::protocols::workspace::{WorkspaceHandler, WorkspaceManagerState};
use serde::{Deserialize, Serialize};
use smithay::{
    backend::renderer::element::utils::select_dmabuf_feedback,
    backend::renderer::element::{solid::SolidColorBuffer, RenderElementStates},
    delegate_compositor, delegate_data_control, delegate_data_device, delegate_dmabuf,
    delegate_fractional_scale, delegate_idle_notify, delegate_input_method_manager,
    delegate_layer_shell, delegate_output, delegate_pointer_constraints,
    delegate_primary_selection, delegate_relative_pointer, delegate_seat, delegate_session_lock,
    delegate_shm, delegate_text_input_manager, delegate_viewporter, delegate_xdg_activation,
    delegate_xdg_shell,
    desktop::{
        find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
        utils::{
            send_dmabuf_feedback_surface_tree, send_frames_surface_tree,
            surface_presentation_feedback_flags_from_states, surface_primary_scanout_output,
            take_presentation_feedback_surface_tree, update_surface_primary_scanout_output,
            OutputPresentationFeedback,
        },
        LayerSurface as DesktopLayerSurface, PopupKind, PopupManager, Space, Window,
        WindowSurfaceType,
    },
    input::{
        dnd::{self, DnDGrab, DndGrabHandler, DndTarget},
        keyboard::{xkb::keysyms, KeyboardHandle, ModifiersState},
        pointer::{CursorImageStatus, PointerHandle},
        Seat, SeatHandler, SeatState,
    },
    output::{Output, PhysicalProperties, Subpixel},
    reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
    reexports::{
        calloop::{
            generic::Generic, timer::Timer, Interest, LoopHandle, LoopSignal, Mode as CalloopMode,
            PostAction, RegistrationToken,
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState,
        wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_output::WlOutput, wl_surface::WlSurface},
            Display, DisplayHandle, Resource,
        },
    },
    utils::{IsAlive, Logical, Point, Rectangle, Size, Transform, SERIAL_COUNTER},
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
        pointer_constraints::{
            with_pointer_constraint, PointerConstraintsHandler, PointerConstraintsState,
        },
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        selection::{
            data_device::{
                request_data_device_client_selection, set_data_device_focus,
                set_data_device_selection, DataDeviceHandler, DataDeviceState,
                WaylandDndGrabHandler,
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
        viewporter::ViewporterState,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
    },
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

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

/// Action to perform when the native idle timeout fires.
#[derive(Debug, Clone)]
pub enum IdleAction {
    /// Turn off monitors (deactivate_monitors)
    DeactivateMonitors,
    /// Run a shell command (e.g., screensaver)
    RunCommand(String),
}

/// State for the native idle timeout feature.
///
/// Uses a last-activity timestamp pattern to avoid timer thrashing:
/// input events just update `last_activity` (free), and when the timer
/// fires it checks elapsed time, rescheduling if activity occurred.
pub struct IdleTimeoutState {
    pub timeout: Option<Duration>,
    pub action: IdleAction,
    pub timer_token: Option<RegistrationToken>,
    pub child_process: Option<std::process::Child>,
    pub is_idle: bool,
    pub last_activity: std::time::Instant,
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
    /// Frame clock for accurate VBlank prediction (replaces raw refresh_interval_us)
    pub frame_clock: frame_clock::FrameClock,
    /// Whether unfinished animations remain on this output.
    /// When true, VBlank and estimated VBlank handlers queue another redraw
    /// even if `redraw_needed` is false, keeping animations pumping.
    pub unfinished_animations_remain: bool,
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
    /// Black backdrop for fullscreen surfaces (covers entire output)
    pub fullscreen_backdrop: SolidColorBuffer,
}

impl OutputState {
    /// Create a new OutputState for the given output name and size.
    pub fn new(output_name: &str, refresh_interval: Option<Duration>, size: (i32, i32)) -> Self {
        Self {
            redraw_state: RedrawState::Queued,
            frame_clock: frame_clock::FrameClock::new(refresh_interval),
            unfinished_animations_remain: false,
            vblank_tracker: VBlankFrameTracker::new(output_name),
            lock_surface: None,
            lock_render_state: LockRenderState::Unlocked,
            // Dark gray background for lock screen
            lock_color_buffer: SolidColorBuffer::new(size, [0.1, 0.1, 0.1, 1.0]),
            frame_callback_sequence: 0,
            fullscreen_backdrop: SolidColorBuffer::new(size, [0.0, 0.0, 0.0, 1.0]),
        }
    }

    /// Resize the lock color buffer for this output
    pub fn resize_lock_buffer(&mut self, size: (i32, i32)) {
        self.lock_color_buffer.resize(size);
    }

    /// Resize the fullscreen backdrop buffer for this output
    pub fn resize_fullscreen_backdrop(&mut self, size: (i32, i32)) {
        self.fullscreen_backdrop.resize(size);
    }
}

impl Default for OutputState {
    fn default() -> Self {
        Self {
            redraw_state: RedrawState::Idle,
            frame_clock: frame_clock::FrameClock::new(Some(Duration::from_micros(16_667))),
            unfinished_animations_remain: false,
            vblank_tracker: VBlankFrameTracker::new("default"),
            lock_surface: None,
            lock_render_state: LockRenderState::Unlocked,
            // Default 1920x1080 lock background (will be resized per output)
            lock_color_buffer: SolidColorBuffer::new((1920, 1080), [0.1, 0.1, 0.1, 1.0]),
            frame_callback_sequence: 0,
            fullscreen_backdrop: SolidColorBuffer::new((1920, 1080), [0.0, 0.0, 0.0, 1.0]),
        }
    }
}

/// Frame callback throttle duration.
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

/// Kill combo: Ctrl+Alt+Backspace
/// Returns true if this key event is the kill combo (keysym-based)
pub fn is_kill_combo(keysym: u32, ctrl: bool, alt: bool) -> bool {
    // BackSpace = 0xff08 (standard X11/XKB keysym)
    keysym == 0xff08 && ctrl && alt
}

/// Cached surface info for change detection
#[derive(Clone, Default, Serialize)]
struct SurfaceInfo {
    app_id: String,
    title: String,
}

/// An entry in a per-output declarative layout.
/// Coordinates are relative to the output's working area (frame-relative).
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct LayoutEntry {
    pub id: u64,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Selected/active view — sent by Emacs (selected-window).
    /// Determines output association for focus routing, popups, etc.
    pub focused: bool,
    /// Largest view per surface — computed by compositor from entry dimensions.
    /// Drives send_configure() size + scale and native-size rendering.
    pub primary: bool,
    /// Fullscreen mode — surface covers full output, bypassing working area.
    #[serde(default)]
    pub fullscreen: bool,
}

/// Centering offset for a fullscreen surface within an output.
/// Returns (0, 0) when the surface is at least as large as the output.
pub fn fullscreen_center_offset(
    window_size: Size<i32, Logical>,
    output_size: Size<i32, Logical>,
) -> (i32, i32) {
    (
        if window_size.w < output_size.w {
            (output_size.w - window_size.w) / 2
        } else {
            0
        },
        if window_size.h < output_size.h {
            (output_size.h - window_size.h) / 2
        } else {
            0
        },
    )
}

/// Extract (title, app_id) from a Window via its XDG toplevel state.
/// Used by the Introspect D-Bus interface to list windows for the portal picker.
#[cfg(feature = "screencast")]
pub fn window_title_and_app_id(window: &Window) -> (String, String) {
    use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

    let Some(surface) = window.wl_surface() else {
        return (String::new(), String::new());
    };
    smithay::wayland::compositor::with_states(&surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .map(|d| {
                let data = d.lock().unwrap();
                (
                    data.title.clone().unwrap_or_default(),
                    data.app_id.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_default()
    })
}

/// Unconstrain a popup with 8px padding, falling back to no padding if it
/// doesn't fit. Ported from niri's `unconstrain_with_padding`.
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

    // Padded didn't fit — fall back to the full target.
    positioner.get_unconstrained_geometry(target)
}

/// Intercepted key: resolved keysym + required modifiers.
/// Keysyms are resolved from Emacs key descriptions at registration time
/// using libxkbcommon (via Smithay's re-export).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InterceptedKey {
    pub keysym: u32,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(rename = "super", default)]
    pub logo: bool,
    /// If true, this key is redirected to Emacs even during fullscreen
    #[serde(default)]
    pub allow_fullscreen: bool,
}

impl InterceptedKey {
    /// Check if this key matches the given keysym and modifiers.
    ///
    /// `raw_keysym` is the layout-independent keysym (physical key, no
    /// modifiers).  `modified_keysym` is the keysym after XKB applies
    /// modifiers (e.g. Shift+`;` → `colon`).
    ///
    /// We try `raw_keysym` first for layout-independent matching, then
    /// fall back to `modified_keysym` for shifted punctuation (`:`, `&`,
    /// etc.) where Emacs encodes the character directly rather than
    /// decomposing it into base-key + Shift.
    pub fn matches(&self, raw_keysym: u32, modified_keysym: u32, mods: &ModifiersState) -> bool {
        // Handle case-insensitive letter matching (A-Z vs a-z)
        let raw_match = self.keysym == raw_keysym
            || (raw_keysym >= keysyms::KEY_A
                && raw_keysym <= keysyms::KEY_Z
                && self.keysym == raw_keysym - keysyms::KEY_A + keysyms::KEY_a);

        if raw_match {
            return self.ctrl == mods.ctrl
                && self.alt == mods.alt
                && (self.shift == mods.shift
                    || (raw_keysym >= keysyms::KEY_A && raw_keysym <= keysyms::KEY_Z))
                && self.logo == mods.logo;
        }

        // Fallback: match against the XKB-modified keysym.  This handles
        // shifted punctuation like `:` (Shift+`;`), `&` (Shift+`7`), etc.
        // XKB already consumed Shift to produce the modified keysym, so we
        // skip the shift comparison here.
        if raw_keysym != modified_keysym && self.keysym == modified_keysym {
            return self.ctrl == mods.ctrl && self.alt == mods.alt && self.logo == mods.logo;
        }

        false
    }
}

#[cfg(test)]
mod intercepted_key_tests {
    use super::*;

    fn mods(ctrl: bool, alt: bool, shift: bool, logo: bool) -> ModifiersState {
        ModifiersState {
            ctrl,
            alt,
            shift,
            logo,
            ..Default::default()
        }
    }

    fn key(keysym: u32, ctrl: bool, alt: bool, shift: bool, logo: bool) -> InterceptedKey {
        InterceptedKey {
            keysym,
            ctrl,
            alt,
            shift,
            logo,
            allow_fullscreen: false,
        }
    }

    #[test]
    fn raw_keysym_match() {
        // s-a: Emacs sends keysym=a, logo=true
        let ik = key(keysyms::KEY_a, false, false, false, true);
        // Physical: raw=a, modified=a, mods={logo}
        assert!(ik.matches(
            keysyms::KEY_a,
            keysyms::KEY_a,
            &mods(false, false, false, true)
        ));
    }

    #[test]
    fn case_insensitive_letter() {
        // Emacs sends keysym=a (lowercase), physical key produces A (uppercase, shift held)
        let ik = key(keysyms::KEY_a, false, false, false, true);
        // Shift is ignored for letter keys
        assert!(ik.matches(
            keysyms::KEY_A,
            keysyms::KEY_A,
            &mods(false, false, true, true)
        ));
    }

    #[test]
    fn modifier_mismatch_rejects() {
        // C-x: Emacs sends keysym=x, ctrl=true
        let ik = key(keysyms::KEY_x, true, false, false, false);
        // Physical: x pressed but ctrl NOT held — should not match
        assert!(!ik.matches(
            keysyms::KEY_x,
            keysyms::KEY_x,
            &mods(false, false, false, false)
        ));
    }

    #[test]
    fn wrong_keysym_rejects() {
        // s-a should not match s-b
        let ik = key(keysyms::KEY_a, false, false, false, true);
        assert!(!ik.matches(
            keysyms::KEY_b,
            keysyms::KEY_b,
            &mods(false, false, false, true)
        ));
    }

    #[test]
    fn shifted_punctuation_fallback() {
        // M-: (eval-expression): Emacs sends keysym=colon, alt=true, shift=false
        let ik = key(keysyms::KEY_colon, false, true, false, false);
        // Physical: raw=semicolon, modified=colon (XKB applied Shift), mods={alt, shift}
        assert!(ik.matches(
            keysyms::KEY_semicolon,
            keysyms::KEY_colon,
            &mods(false, true, true, false)
        ));
    }

    #[test]
    fn shifted_punctuation_no_false_positive_on_unshifted() {
        // M-; (comment-dwim): Emacs sends keysym=semicolon, alt=true
        let ik = key(keysyms::KEY_semicolon, false, true, false, false);
        // Physical: raw=semicolon, modified=semicolon (no shift), mods={alt}
        // Should match via raw path
        assert!(ik.matches(
            keysyms::KEY_semicolon,
            keysyms::KEY_semicolon,
            &mods(false, true, false, false)
        ));
        // Physical: raw=semicolon, modified=colon (shift held), mods={alt, shift}
        // Should NOT match — we want semicolon but got shift+semicolon
        assert!(!ik.matches(
            keysyms::KEY_semicolon,
            keysyms::KEY_colon,
            &mods(false, true, true, false)
        ));
    }

    #[test]
    fn shifted_ampersand() {
        // M-& (async-shell-command): Emacs sends keysym=ampersand, alt=true
        let ik = key(keysyms::KEY_ampersand, false, true, false, false);
        // Physical: raw=7, modified=ampersand (Shift+7), mods={alt, shift}
        assert!(ik.matches(
            keysyms::KEY_7,
            keysyms::KEY_ampersand,
            &mods(false, true, true, false)
        ));
    }
}

/// DnD icon surface attached to the pointer during a drag operation.
#[derive(Debug)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
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
    next_surface_id: u64,
    pub window_ids: HashMap<Window, u64>,
    pub id_windows: HashMap<u64, Window>,
    surface_info: HashMap<u64, SurfaceInfo>,
    /// Declared layout per output: output_name → entries
    pub output_layouts: HashMap<String, Vec<LayoutEntry>>,
    /// Per-output workspace (tab) state from Emacs: output_name → tab info
    pub output_workspaces: HashMap<String, Vec<module::TabInfo>>,
    /// Reverse index: surface_id → set of output names it appears on
    pub surface_outputs: HashMap<u64, HashSet<String>>,

    // Output
    pub output_size: Size<i32, Logical>,
    pub outputs: Vec<OutputInfo>,
    /// Desired output configuration, keyed by output name.
    /// Looked up when outputs connect; updated by Emacs commands.
    pub output_config: HashMap<String, OutputConfig>,

    // Input
    /// Surface under the pointer + its global origin, for constraint checks.
    pub pointer_focus: Option<(WlSurface, Point<f64, Logical>)>,
    /// Last output the pointer was on, for per-output cursor redraw.
    pointer_output: Option<Output>,
    pub focused_surface_id: u64,
    pub keyboard_focus: Option<WlSurface>,
    pub keyboard_focus_dirty: bool,
    /// Output geometry or working areas changed; shared active_outputs need recomputation.
    pub active_outputs_dirty: bool,

    // Libinput device configuration
    pub input_configs: Vec<input::InputConfigEntry>,

    // Emacs client tracking - used to identify which surfaces belong to Emacs
    // vs external applications (for key interception)
    pub emacs_pid: Option<u32>,
    pub emacs_surfaces: HashMap<u64, String>,

    // Screenshot request
    pub pending_screenshot: Option<String>,

    // Per-output state (redraw state machine)
    pub output_state: HashMap<Output, OutputState>,

    /// Whether monitors are active (rendering allowed).
    /// Set to false when all screens are off (e.g., lid closed with no external display).
    pub monitors_active: bool,

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
    // Deduplicate IM relay activate/deactivate events
    pub text_input_active: bool,
    // Commits queued during the client's disable→enable gap
    pub pending_im_commits: Vec<String>,

    // Popup manager for XDG popups
    pub popups: PopupManager,

    // DnD icon surface (shown at pointer during drag)
    pub dnd_icon: Option<DndIcon>,

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

    // Workspace state (ext-workspace-v1: exposes Emacs tabs to external tools)
    pub workspace_state: WorkspaceManagerState,

    // Output management state (wlr-output-management-unstable-v1)
    pub output_management_state: OutputManagementState,

    // Session lock state (ext-session-lock-v1 protocol)
    pub session_lock_state: SessionLockManagerState,
    pub lock_state: LockState,
    /// Surface ID that was focused before locking (restored on unlock)
    pub pre_lock_focus: Option<u64>,

    // Idle notify state (ext-idle-notify-v1 protocol)
    pub idle_notifier_state: IdleNotifierState<State>,

    // Gamma control state (wlr-gamma-control-unstable-v1 protocol)
    pub gamma_control_state: crate::protocols::gamma_control::GammaControlManagerState,

    // Fractional scale protocol (wp-fractional-scale-v1)
    #[allow(dead_code)]
    pub fractional_scale_state: FractionalScaleManagerState,

    // Viewporter protocol (wp-viewporter, required for fractional scale clients)
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,

    // Presentation time protocol (wp-presentation-time)
    #[allow(dead_code)]
    pub presentation_state: smithay::wayland::presentation::PresentationState,

    // Relative pointer protocol (zwp-relative-pointer-v1)
    #[allow(dead_code)]
    pub relative_pointer_state: RelativePointerManagerState,

    // Pointer constraints protocol (zwp-pointer-constraints-v1)
    #[allow(dead_code)]
    pub pointer_constraints_state: PointerConstraintsState,

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

    // Channel to reply to Introspect GetWindows requests
    #[cfg(feature = "screencast")]
    pub introspect_reply_tx: Option<async_channel::Sender<dbus::CompositorToIntrospect>>,

    // Cached mapping: window ID → primary output name (for screencast)
    // Rebuilt in apply_output_layout()
    #[cfg(feature = "screencast")]
    pub window_cast_output: HashMap<u64, String>,

    // XKB layout state
    pub xkb_layout_names: Vec<String>,
    pub xkb_current_layout: usize,

    // IM relay keepalive — Smithay requires an input_method_v2 instance
    // for text_input_v3. Commits bypass the relay (direct TextInputHandle).
    pub im_relay: Option<im_relay::ImRelay>,

    // Event loop handle (for idle timer registration)
    pub loop_handle: LoopHandle<'static, State>,

    // xwayland-satellite (on-demand X11 support)
    pub satellite: Option<xwayland::satellite::Satellite>,

    // Native idle timeout state
    pub idle_timeout: IdleTimeoutState,

    // Client cursor image status (Hidden/Surface/Named)
    pub cursor_image_status: CursorImageStatus,
}

impl Ewm {
    pub fn new(
        display_handle: DisplayHandle,
        loop_handle: LoopHandle<'static, State>,
        is_drm: bool,
    ) -> Self {
        let compositor_state = CompositorState::new_v6::<State>(&display_handle);
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities;
        let xdg_shell_state = XdgShellState::new_with_capabilities::<State>(
            &display_handle,
            [
                WmCapabilities::Fullscreen,
                WmCapabilities::Maximize,
                WmCapabilities::Minimize,
            ],
        );
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

        // Initialize workspace management (ext-workspace-v1: Emacs tabs as workspaces)
        let workspace_state = WorkspaceManagerState::new::<State, _>(&display_handle, |_| true);

        // Initialize output management (wlr-output-management-unstable-v1)
        let output_management_state =
            OutputManagementState::new::<State, _>(&display_handle, |_| true);

        // Initialize session lock for screen locking (ext-session-lock-v1)
        let session_lock_state =
            SessionLockManagerState::new::<State, _>(&display_handle, |_| true);

        // Clone loop_handle before IdleNotifierState consumes it
        let loop_handle_clone = loop_handle.clone();

        // Initialize idle notifier (ext-idle-notify-v1)
        let idle_notifier_state = IdleNotifierState::new(&display_handle, loop_handle);

        // Initialize gamma control (wlr-gamma-control-unstable-v1)
        // Only advertise the global on DRM backends where gamma is actually supported
        let gamma_control_state = crate::protocols::gamma_control::GammaControlManagerState::new::<
            State,
            _,
        >(&display_handle, move |_| is_drm);

        // Initialize fractional scale protocol (wp-fractional-scale-v1)
        let fractional_scale_state = FractionalScaleManagerState::new::<State>(&display_handle);

        // Initialize viewporter (wp-viewporter, required by fractional scale clients)
        let viewporter_state = ViewporterState::new::<State>(&display_handle);

        // Initialize presentation time protocol (wp-presentation-time)
        // Clock ID 1 = CLOCK_MONOTONIC
        let presentation_state =
            smithay::wayland::presentation::PresentationState::new::<State>(&display_handle, 1);

        // Initialize relative pointer protocol (zwp-relative-pointer-v1)
        let relative_pointer_state = RelativePointerManagerState::new::<State>(&display_handle);

        // Initialize pointer constraints protocol (zwp-pointer-constraints-v1)
        let pointer_constraints_state = PointerConstraintsState::new::<State>(&display_handle);

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
            output_layouts: HashMap::new(),
            output_workspaces: HashMap::new(),
            surface_outputs: HashMap::new(),
            output_size: Size::from((0, 0)),
            outputs: Vec::new(),
            output_config: HashMap::new(),
            pointer_focus: None,
            pointer_output: None,
            focused_surface_id: 0,
            keyboard_focus: None,
            keyboard_focus_dirty: false,
            active_outputs_dirty: true,
            input_configs: Vec::new(),
            emacs_pid: None,
            emacs_surfaces: HashMap::new(),
            pending_screenshot: None,
            output_state: HashMap::new(),
            monitors_active: true,
            pending_early_imports: Vec::new(),
            screencopy_state,
            output_manager_state,
            text_input_state,
            input_method_state,
            text_input_intercept: false,
            text_input_active: false,
            pending_im_commits: Vec::new(),
            popups: PopupManager::default(),
            dnd_icon: None,
            layer_shell_state,
            unmapped_layer_surfaces: std::collections::HashSet::new(),
            layer_shell_on_demand_focus: None,
            working_areas: HashMap::new(),
            activation_state,
            foreign_toplevel_state,
            workspace_state,
            output_management_state,
            session_lock_state,
            lock_state: LockState::Unlocked,
            pre_lock_focus: None,
            idle_notifier_state,
            gamma_control_state,
            fractional_scale_state,
            viewporter_state,
            presentation_state,
            relative_pointer_state,
            pointer_constraints_state,
            #[cfg(feature = "screencast")]
            pipewire: None,
            #[cfg(feature = "screencast")]
            dbus_outputs: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(feature = "screencast")]
            screen_casts: std::collections::HashMap::new(),
            #[cfg(feature = "screencast")]
            dbus_servers: None,
            #[cfg(feature = "screencast")]
            introspect_reply_tx: None,
            #[cfg(feature = "screencast")]
            window_cast_output: HashMap::new(),
            xkb_layout_names: vec!["us".to_string()],
            xkb_current_layout: 0,
            im_relay: None,
            loop_handle: loop_handle_clone,
            satellite: None,
            idle_timeout: IdleTimeoutState {
                timeout: None,
                action: IdleAction::DeactivateMonitors,
                timer_token: None,
                child_process: None,
                is_idle: false,
                last_activity: std::time::Instant::now(),
            },
            cursor_image_status: CursorImageStatus::default_named(),
        }
    }

    /// Connect the input method relay after socket is ready.
    pub fn connect_im_relay(&mut self, socket_path: &std::path::Path) {
        self.im_relay = Some(im_relay::ImRelay::connect(socket_path));
    }

    /// Remove a surface from all output layouts and the reverse index.
    fn remove_surface_from_layouts(&mut self, id: u64) {
        self.surface_outputs.remove(&id);
        for entries in self.output_layouts.values_mut() {
            entries.retain(|e| e.id != id);
        }
    }

    /// Clean up all Ewm state for a removed output.
    ///
    /// Called by backends after their own teardown (e.g. DRM surface removal).
    /// Handles: output state, lock check, screencasts, space, layouts,
    /// workspaces, working areas, outputs list, D-Bus, output size, IPC.
    pub fn remove_output(&mut self, output: &Output) {
        let output_name = output.name();

        // Cancel pending estimated-VBlank timers
        if let Some(output_state) = self.output_state.remove(output) {
            if let RedrawState::WaitingForEstimatedVBlank(token)
            | RedrawState::WaitingForEstimatedVBlankAndQueued(token) = output_state.redraw_state
            {
                self.loop_handle.remove(token);
            }
        }

        self.check_lock_on_output_removed();

        // Clean up gamma control for this output
        self.gamma_control_state.output_removed(output);

        // Stop screen casts for this output
        #[cfg(feature = "screencast")]
        {
            let target = dbus::CastTarget::Output {
                name: output_name.to_string(),
            };
            let sessions_to_stop: Vec<usize> = self
                .screen_casts
                .iter()
                .filter(|(_, cast)| cast.target == target)
                .map(|(id, _)| *id)
                .collect();
            for session_id in sessions_to_stop {
                info!(output = %output_name, session_id, "stopping cast due to output disconnect");
                self.stop_cast(session_id);
            }
        }

        // Unmap from space
        self.space.unmap_output(output);

        // Clean up layout state
        if let Some(old_entries) = self.output_layouts.remove(&output_name) {
            for entry in &old_entries {
                if let Some(outputs) = self.surface_outputs.get_mut(&entry.id) {
                    outputs.remove(&output_name);
                    if outputs.is_empty() {
                        self.surface_outputs.remove(&entry.id);
                    }
                }
            }
        }
        self.output_workspaces.remove(&output_name);
        self.working_areas.remove(&output_name);

        // Remove from outputs list
        self.outputs.retain(|o| o.name != output_name);

        // Remove from D-Bus outputs
        #[cfg(feature = "screencast")]
        {
            let mut dbus_outputs = self.dbus_outputs.lock().unwrap();
            dbus_outputs.retain(|o| o.name != output_name);
        }

        self.recalculate_output_size();
        self.send_output_disconnected(&output_name);
        self.output_management_state.output_heads_changed = true;

        info!("Output removed: {}", output_name);
    }

    /// Remove dead windows from the space.
    /// This replaces Space::refresh() — we manage output enter/leave explicitly
    /// rather than relying on automatic spatial overlap detection.
    pub fn cleanup_dead_windows(&mut self) {
        // Clean Emacs frames from space
        let dead: Vec<Window> = self
            .space
            .elements()
            .filter(|w| !w.alive())
            .cloned()
            .collect();
        for w in dead {
            self.space.unmap_elem(&w);
        }

        // Clean dead layout surfaces from id_windows
        let dead_ids: Vec<u64> = self
            .id_windows
            .iter()
            .filter(|(_, w)| !w.alive())
            .map(|(&id, _)| id)
            .collect();
        for id in dead_ids {
            if let Some(window) = self.id_windows.remove(&id) {
                self.window_ids.remove(&window);
            }
            self.remove_surface_from_layouts(id);
            self.surface_info.remove(&id);
            self.emacs_surfaces.remove(&id);
            self.queue_event(Event::Close { id });

            // Stop any screen casts targeting this window
            #[cfg(feature = "screencast")]
            self.stop_casts_for_window(id);
        }

        // Reset cursor image if the cursor surface died
        if let CursorImageStatus::Surface(surface) = &self.cursor_image_status {
            if !surface.alive() {
                self.cursor_image_status = CursorImageStatus::default_named();
            }
        }
    }

    /// Build a snapshot of all output states for the output management protocol.
    pub fn build_output_head_states(
        &self,
    ) -> HashMap<String, protocols::output_management::OutputHeadState> {
        use protocols::output_management::{OutputHeadState, OutputModeState};

        let mut states = HashMap::new();
        for info in &self.outputs {
            let output = self.space.outputs().find(|o| o.name() == info.name);
            let geo = output.and_then(|o| self.space.output_geometry(o));

            let enabled = self
                .output_config
                .get(&info.name)
                .map(|c| c.enabled)
                .unwrap_or(true);

            // Find current mode index
            let current_mode = if enabled {
                output.and_then(|o| {
                    let current = o.current_mode()?;
                    info.modes.iter().position(|m| {
                        m.width == current.size.w
                            && m.height == current.size.h
                            && m.refresh == current.refresh
                    })
                })
            } else {
                None
            };

            let head = OutputHeadState {
                name: info.name.clone(),
                make: info.make.clone(),
                model: info.model.clone(),
                serial_number: None,
                physical_size: if info.width_mm > 0 || info.height_mm > 0 {
                    Some((info.width_mm, info.height_mm))
                } else {
                    None
                },
                enabled,
                modes: info
                    .modes
                    .iter()
                    .map(|m| OutputModeState {
                        width: m.width,
                        height: m.height,
                        refresh: m.refresh,
                        preferred: m.preferred,
                    })
                    .collect(),
                current_mode,
                position: geo.map(|g| (g.loc.x, g.loc.y)),
                scale: Some(info.scale),
                transform: Some(backend::int_to_transform(info.transform)),
            };
            states.insert(info.name.clone(), head);
        }
        states
    }

    /// Find a window by its root WlSurface, returning the window and its ID.
    pub fn find_window_by_surface(&self, surface: &WlSurface) -> Option<(Window, u64)> {
        self.id_windows.iter().find_map(|(&id, window)| {
            window.wl_surface().and_then(|ws| {
                if *ws == *surface {
                    Some((window.clone(), id))
                } else {
                    None
                }
            })
        })
    }

    /// Find the focused output layout entry for a surface.
    ///
    /// Scans all output_layouts for a focused entry matching the given surface ID.
    /// Used for output association (focus routing, popups).
    fn focused_output_for_surface(&self, id: u64) -> Option<(&str, &LayoutEntry)> {
        for (output_name, entries) in &self.output_layouts {
            if let Some(entry) = entries.iter().find(|e| e.id == id && e.focused) {
                return Some((output_name.as_str(), entry));
            }
        }
        None
    }

    /// Find the primary output layout entry for a surface.
    ///
    /// Scans all output_layouts for a primary entry matching the given surface ID.
    /// Used as fallback when no focused entry exists (e.g. popup positioning).
    fn primary_output_for_surface(&self, id: u64) -> Option<(&str, &LayoutEntry)> {
        for (output_name, entries) in &self.output_layouts {
            if let Some(entry) = entries.iter().find(|e| e.id == id && e.primary) {
                return Some((output_name.as_str(), entry));
            }
        }
        None
    }

    /// Get the global position of a window.
    ///
    /// For layout surfaces (managed via output_layouts): uses the primary entry's
    /// position relative to the output's working area.
    /// For non-layout surfaces (Emacs frames): uses space.element_location().
    pub fn window_global_position(
        &self,
        window: &Window,
    ) -> Option<smithay::utils::Point<i32, smithay::utils::Logical>> {
        use smithay::utils::Point;

        let id = self.window_ids.get(window).copied()?;

        // Layout surface: use focused entry for output association, fall back to primary
        if self.surface_outputs.contains_key(&id) {
            let (output_name, entry) = self
                .focused_output_for_surface(id)
                .or_else(|| self.primary_output_for_surface(id))?;
            let output = self.space.outputs().find(|o| o.name() == output_name)?;
            let output_geo = self.space.output_geometry(output)?;

            if entry.fullscreen {
                if entry.primary {
                    let (ox, oy) =
                        fullscreen_center_offset(window.geometry().size, output_geo.size);
                    return Some(Point::from((output_geo.loc.x + ox, output_geo.loc.y + oy)));
                } else {
                    // Non-primary fullscreen: stretch from output origin
                    return Some(Point::from((output_geo.loc.x, output_geo.loc.y)));
                }
            }

            let working_area = {
                let map = layer_map_for_output(output);
                map.non_exclusive_zone()
            };
            return Some(Point::from((
                output_geo.loc.x + working_area.loc.x + entry.x,
                output_geo.loc.y + working_area.loc.y + entry.y,
            )));
        }

        // Non-layout surface (Emacs frame): use space position
        self.space.element_location(window)
    }

    /// Find the topmost layout entry containing `pos`, returning the entry
    /// and its global (x, y) origin.
    fn layout_entry_under(
        &self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> Option<(&LayoutEntry, i32, i32)> {
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = {
            let map = layer_map_for_output(output);
            map.non_exclusive_zone()
        };
        let entries = self.output_layouts.get(&output.name())?;

        // Iterate forward (first = topmost, matching render element order)
        for entry in entries.iter() {
            if entry.fullscreen {
                // Fullscreen covers entire output
                return Some((entry, output_geo.loc.x, output_geo.loc.y));
            }

            let entry_x = output_geo.loc.x + working_area.loc.x + entry.x;
            let entry_y = output_geo.loc.y + working_area.loc.y + entry.y;

            if pos.x >= entry_x as f64
                && pos.y >= entry_y as f64
                && pos.x < (entry_x + entry.w as i32) as f64
                && pos.y < (entry_y + entry.h as i32) as f64
            {
                return Some((entry, entry_x, entry_y));
            }
        }
        None
    }

    /// Hit-test layout surfaces under a global position.
    ///
    /// Iterates all entries on the pointer's output and calls surface_under(ALL)
    /// on each window. This catches popups and subsurfaces that extend beyond
    /// their parent entry bounds, without needing a separate popup iteration pass.
    /// Entries are checked in topmost-first order (matching render element order).
    pub fn layout_surface_under(
        &self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> Option<(
        WlSurface,
        smithay::utils::Point<f64, smithay::utils::Logical>,
    )> {
        use smithay::utils::Point;

        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = {
            let map = layer_map_for_output(output);
            map.non_exclusive_zone()
        };
        let entries = self.output_layouts.get(&output.name())?;

        for entry in entries.iter() {
            let window = match self.id_windows.get(&entry.id) {
                Some(w) => w,
                None => continue,
            };
            let window_geo = window.geometry();

            let (entry_x, entry_y) = if entry.fullscreen {
                (output_geo.loc.x, output_geo.loc.y)
            } else {
                (
                    output_geo.loc.x + working_area.loc.x + entry.x,
                    output_geo.loc.y + working_area.loc.y + entry.y,
                )
            };

            let pos_in_entry_x = pos.x - entry_x as f64;
            let pos_in_entry_y = pos.y - entry_y as f64;

            // Scale pointer coords from entry space to window space.
            // Primary/fullscreen-primary use 1:1 mapping. Non-primary entries use
            // uniform fill+crop inverse: min(buf/entry) applied to both axes.
            let (pointer_scale, center_offset_x, center_offset_y) = if entry.fullscreen {
                if entry.primary {
                    let (cx, cy) = fullscreen_center_offset(window_geo.size, output_geo.size);
                    (1.0, cx as f64, cy as f64)
                } else {
                    let uniform = f64::max(
                        output_geo.size.w as f64 / window_geo.size.w as f64,
                        output_geo.size.h as f64 / window_geo.size.h as f64,
                    );
                    (1.0 / uniform, 0.0, 0.0)
                }
            } else if entry.primary {
                (1.0, 0.0, 0.0)
            } else if entry.w > 0 && entry.h > 0 {
                (
                    f64::min(
                        window_geo.size.w as f64 / entry.w as f64,
                        window_geo.size.h as f64 / entry.h as f64,
                    ),
                    0.0,
                    0.0,
                )
            } else {
                (1.0, 0.0, 0.0)
            };

            let pos_in_window = Point::from((
                (pos_in_entry_x - center_offset_x) * pointer_scale,
                (pos_in_entry_y - center_offset_y) * pointer_scale,
            ));

            // Window's global origin: pointer_global - origin = pointer_in_window.
            let window_origin: Point<f64, Logical> =
                Point::from((pos.x - pos_in_window.x, pos.y - pos_in_window.y));

            // surface_under(ALL) traverses the full surface tree: toplevel, popups,
            // and subsurfaces. Popups extending beyond entry bounds are found here.
            if let Some((surface, surface_offset)) =
                window.surface_under(pos_in_window, WindowSurfaceType::ALL)
            {
                let global_loc = Point::from((
                    window_origin.x + surface_offset.x as f64,
                    window_origin.y + surface_offset.y as f64,
                ));
                // Log all hits in fullscreen, and popup/subsurface hits always
                let is_toplevel = window.wl_surface().map_or(false, |ws| *ws == surface);
                if entry.fullscreen || !is_toplevel {
                    info!(
                        "surface_under: {} entry {} (fs={}) \
                         pos={:.0},{:.0} pos_in_window={:.0},{:.0} \
                         surface_offset={},{} global_loc={:.0},{:.0} \
                         geo.loc={},{} geo.size={}x{} center={:.0},{:.0}",
                        if is_toplevel { "toplevel" } else { "popup" },
                        entry.id,
                        entry.fullscreen,
                        pos.x,
                        pos.y,
                        pos_in_window.x,
                        pos_in_window.y,
                        surface_offset.x,
                        surface_offset.y,
                        global_loc.x,
                        global_loc.y,
                        window_geo.loc.x,
                        window_geo.loc.y,
                        window_geo.size.w,
                        window_geo.size.h,
                        center_offset_x,
                        center_offset_y,
                    );
                }
                return Some((surface, global_loc));
            }

            // Fullscreen covers the entire output — no entries below are visible.
            if entry.fullscreen {
                break;
            }
        }

        None
    }

    /// Check if a surface has fullscreen set in any output layout.
    pub fn is_surface_fullscreen(&self, id: u64) -> bool {
        self.output_layouts
            .values()
            .any(|entries| entries.iter().any(|e| e.id == id && e.fullscreen))
    }

    /// Whether layout surfaces render (and receive input) above the Top layer on this output.
    ///
    /// True when a fullscreen surface is active. Used to reorder both the render
    /// element list and the input hit-testing chain so that the fullscreen surface
    /// occludes Top/Bottom/Background layers for input as well as visually.
    pub fn render_above_top_layer(&self, output: &Output) -> bool {
        self.output_layouts
            .get(&output.name())
            .map_or(false, |entries| entries.iter().any(|e| e.fullscreen))
    }

    /// Hit-test layout surfaces under a global position, returning the surface ID.
    ///
    /// Simpler than layout_surface_under — just rectangle containment, no subsurface precision.
    pub fn layout_surface_id_under(
        &self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> Option<u64> {
        self.layout_entry_under(pos).map(|(entry, _, _)| entry.id)
    }

    /// Apply a declarative per-output layout.
    ///
    /// Replaces the previous layout for the given output. Surfaces removed from
    /// this output get `wl_surface.leave`; new surfaces get `wl_surface.enter`
    /// and scale notification.
    pub fn apply_output_layout(&mut self, output_name: &str, entries: Vec<LayoutEntry>) {
        // 1. Find the output
        let output = match self.space.outputs().find(|o| o.name() == output_name) {
            Some(o) => o.clone(),
            None => {
                warn!("apply_output_layout: output '{}' not found", output_name);
                return;
            }
        };

        // 2. Diff old vs new surface IDs
        let old_ids: HashSet<u64> = self
            .output_layouts
            .get(output_name)
            .map(|entries| entries.iter().map(|e| e.id).collect())
            .unwrap_or_default();
        let new_ids: HashSet<u64> = entries.iter().map(|e| e.id).collect();

        // 3. Handle removed surfaces (old - new)
        for &id in old_ids.difference(&new_ids) {
            if let Some(window) = self.id_windows.get(&id) {
                if let Some(surface) = window.wl_surface() {
                    output.leave(&surface);
                }
            }
            // Update reverse index
            if let Some(outputs) = self.surface_outputs.get_mut(&id) {
                outputs.remove(output_name);
                if outputs.is_empty() {
                    self.surface_outputs.remove(&id);
                }
            }
        }

        // 4. Handle newly added surfaces (new - old): enter only.
        // Scale is sent in step 8 for active surfaces — the active output's
        // scale is the canonical one (wp_fractional_scale_v1 is per-surface).
        for &id in new_ids.difference(&old_ids) {
            if let Some(window) = self.id_windows.get(&id) {
                if let Some(surface) = window.wl_surface() {
                    output.enter(&surface);
                }
            } else {
                warn!("apply_output_layout: surface {} not found", id);
            }
        }

        // 5. Update reverse index for all current entries
        for &id in &new_ids {
            self.surface_outputs
                .entry(id)
                .or_default()
                .insert(output_name.to_string());
        }

        // 6. Store the layout
        self.output_layouts.insert(output_name.to_string(), entries);

        // 7. Compute primary flags: largest area per surface across all outputs.
        // Fullscreen entries use the output's logical size (the surface is
        // configured at full output size), not entry.w/h from Emacs.
        {
            // Build output size lookup for fullscreen area computation
            let output_sizes: HashMap<String, (u64, u64)> = self
                .space
                .outputs()
                .map(|o| {
                    let size = crate::utils::output_size(o);
                    (o.name(), (size.w as u64, size.h as u64))
                })
                .collect();

            let entry_area = |entry: &LayoutEntry, oname: &str| -> u64 {
                if entry.fullscreen {
                    let (w, h) = output_sizes
                        .get(oname)
                        .copied()
                        .unwrap_or((entry.w as u64, entry.h as u64));
                    w * h
                } else {
                    entry.w as u64 * entry.h as u64
                }
            };

            let mut best_area: HashMap<u64, u64> = HashMap::new();
            // First pass: find the largest area per surface
            for (oname, entries) in &self.output_layouts {
                for entry in entries {
                    let area = entry_area(entry, oname);
                    best_area
                        .entry(entry.id)
                        .and_modify(|a| *a = (*a).max(area))
                        .or_insert(area);
                }
            }
            // Second pass: mark primary on the matching entry
            let mut assigned: HashSet<u64> = HashSet::new();
            for (oname, entries) in self.output_layouts.iter_mut() {
                for entry in entries.iter_mut() {
                    let dominated = best_area.get(&entry.id).copied().unwrap_or(0);
                    entry.primary =
                        entry_area(entry, oname) == dominated && assigned.insert(entry.id);
                }
            }
        }

        // 7b. Rebuild window-output cache for screencast lookups.
        #[cfg(feature = "screencast")]
        {
            self.window_cast_output.clear();
            for (oname, entries) in &self.output_layouts {
                for entry in entries {
                    if entry.primary {
                        self.window_cast_output
                            .insert(entry.id, oname.clone());
                    }
                }
            }
        }

        // 8. Configure primary surfaces: size + scale from this output.
        // wp_fractional_scale_v1 is per-surface, so the primary output's scale
        // is the canonical one — the client renders at this scale.
        let output_scale = output.current_scale();
        let output_transform = output.current_transform();
        if let Some(entries) = self.output_layouts.get(output_name) {
            for entry in entries {
                if entry.primary {
                    if let Some(window) = self.id_windows.get(&entry.id) {
                        window.with_surfaces(|s, data| {
                            crate::utils::send_scale_transform(
                                s,
                                data,
                                output_scale,
                                output_transform,
                            );
                        });
                        window.toplevel().map(|t| {
                            let changed = t.with_pending_state(|state| {
                                let new_size = if entry.fullscreen {
                                    let s = crate::utils::output_size(&output);
                                    (s.w as i32, s.h as i32).into()
                                } else {
                                    (entry.w as i32, entry.h as i32).into()
                                };
                                let changed = state.size != Some(new_size);
                                state.size = Some(new_size);
                                if entry.fullscreen {
                                    state.states.set(XdgToplevelState::Fullscreen);
                                    state.states.unset(XdgToplevelState::Maximized);
                                } else {
                                    state.states.set(XdgToplevelState::Maximized);
                                    state.states.unset(XdgToplevelState::Fullscreen);
                                }
                                changed
                            });
                            if changed {
                                t.send_configure();
                            }
                        });
                    }
                }
            }
        }

        // 9. Queue redraw for just this output
        self.queue_redraw(&output);

        debug!(
            "apply_output_layout: output '{}' with {} surfaces",
            output_name,
            new_ids.len()
        );
    }

    /// Queue a redraw for all outputs
    pub fn queue_redraw_all(&mut self) {
        for state in self.output_state.values_mut() {
            state.redraw_state = mem::take(&mut state.redraw_state).queue_redraw();
        }
    }

    /// Queue redraws for outputs displaying the given surface.
    ///
    /// Layout-managed surfaces redraw their declared outputs.
    /// Emacs frames redraw their assigned output. Untracked surfaces
    /// fall back to redrawing all outputs.
    pub fn queue_redraw_for_surface(&mut self, id: u64) {
        let outputs: Vec<_> = if let Some(names) = self.surface_outputs.get(&id) {
            self.space
                .outputs()
                .filter(|o| names.contains(&o.name()))
                .cloned()
                .collect()
        } else if let Some(name) = self.emacs_surfaces.get(&id) {
            self.space
                .outputs()
                .filter(|o| o.name() == *name)
                .cloned()
                .collect()
        } else {
            return self.queue_redraw_all();
        };

        for output in outputs {
            self.queue_redraw(&output);
        }
    }

    /// Queue a redraw for a specific output
    pub fn queue_redraw(&mut self, output: &Output) {
        if let Some(state) = self.output_state.get_mut(output) {
            state.redraw_state = mem::take(&mut state.redraw_state).queue_redraw();
        }
    }

    /// Current pointer location, read from Smithay's pointer handle.
    /// This is the single source of truth — updated by `pointer.motion()`,
    /// `pointer.set_location()`, and `cursor_position_hint`.
    pub fn pointer_location(&self) -> (f64, f64) {
        let pos = self.pointer.current_location();
        (pos.x, pos.y)
    }

    /// Queue a redraw for outputs overlapping the cursor.
    /// Redraws the output the pointer is currently on, plus the previous
    /// output if the pointer crossed a boundary.
    pub fn queue_redraw_for_pointer(&mut self) {
        let pos = smithay::utils::Point::from(self.pointer_location());
        let current = self.output_at(pos).cloned();
        let prev = self.pointer_output.take();

        if let Some(ref prev) = prev {
            if current.as_ref() != Some(prev) {
                self.queue_redraw(prev);
            }
        }
        if let Some(ref output) = current {
            self.queue_redraw(output);
        }
        self.pointer_output = current;
    }

    /// Deactivate all monitors (e.g., lid closed with no external display).
    /// Prevents rendering while screens are off.
    pub fn deactivate_monitors(&mut self) {
        if self.monitors_active {
            info!("Monitors deactivated");
            self.monitors_active = false;
        }
    }

    /// Reactivate monitors (e.g., lid opened, or session resume).
    /// Queues redraws for all outputs.
    pub fn activate_monitors(&mut self) {
        if !self.monitors_active {
            info!("Monitors activated");
            self.monitors_active = true;
            self.queue_redraw_all();
        }
    }

    /// Configure the native idle timeout.
    /// Passing `None` for timeout disables it.
    pub fn configure_idle(&mut self, timeout: Option<Duration>, action: IdleAction) {
        // If currently idle, wake without restarting the old timer
        if self.idle_timeout.is_idle {
            self.idle_timeout.is_idle = false;
            self.kill_idle_child();
            self.activate_monitors();
            self.queue_event(event::Event::IdleStateChanged { idle: false });
        }

        self.cancel_idle_timer();
        self.idle_timeout.timeout = timeout;
        self.idle_timeout.action = action;
        self.idle_timeout.last_activity = std::time::Instant::now();

        if let Some(duration) = timeout {
            self.start_idle_timer(duration);
            info!("Idle timeout configured: {:?}", duration);
        } else {
            info!("Idle timeout disabled");
        }
    }

    fn start_idle_timer(&mut self, duration: Duration) {
        self.cancel_idle_timer();
        let timer = Timer::from_duration(duration);
        match self.loop_handle.insert_source(timer, |_, _, state| {
            let went_idle = state.ewm.on_idle_timer_fired();
            if went_idle {
                // Blank the DRM surfaces (DPMS off)
                state.backend.clear_all_surfaces();
            }
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        }) {
            Ok(token) => {
                self.idle_timeout.timer_token = Some(token);
            }
            Err(err) => {
                warn!("Failed to insert idle timer: {:?}", err);
            }
        }
    }

    pub(crate) fn cancel_idle_timer(&mut self) {
        if let Some(token) = self.idle_timeout.timer_token.take() {
            self.loop_handle.remove(token);
        }
    }

    /// Called when the calloop timer fires. Checks whether enough idle time
    /// has actually elapsed (activity may have occurred since the timer was set).
    /// If not, reschedules for the remaining time instead of firing the action.
    /// Returns `true` if the idle action was actually fired.
    fn on_idle_timer_fired(&mut self) -> bool {
        self.idle_timeout.timer_token = None;

        let Some(timeout) = self.idle_timeout.timeout else {
            return false;
        };

        let elapsed = self.idle_timeout.last_activity.elapsed();
        if elapsed < timeout {
            // Activity happened since timer was set — reschedule for remaining time
            let remaining = timeout - elapsed;
            self.start_idle_timer(remaining);
            return false;
        }

        // Actually fire the idle action
        self.idle_timeout.is_idle = true;
        info!("Idle timeout fired");

        match &self.idle_timeout.action {
            IdleAction::DeactivateMonitors => {
                self.deactivate_monitors();
            }
            IdleAction::RunCommand(cmd) => {
                match std::process::Command::new("sh").arg("-c").arg(cmd).spawn() {
                    Ok(child) => {
                        info!("Idle command spawned: {}", cmd);
                        self.idle_timeout.child_process = Some(child);
                    }
                    Err(err) => {
                        warn!("Failed to spawn idle command: {:?}", err);
                    }
                }
            }
        }

        self.queue_event(event::Event::IdleStateChanged { idle: true });
        true
    }

    /// Wake from idle state: kill child process, reactivate monitors, restart timer.
    pub fn wake_from_idle(&mut self) {
        if !self.idle_timeout.is_idle {
            return;
        }
        info!("Waking from idle");
        self.idle_timeout.is_idle = false;

        self.kill_idle_child();
        self.activate_monitors();

        // Restart the timer for the next idle cycle
        self.idle_timeout.last_activity = std::time::Instant::now();
        if let Some(duration) = self.idle_timeout.timeout {
            self.start_idle_timer(duration);
        }

        self.queue_event(event::Event::IdleStateChanged { idle: false });
    }

    /// Record user activity. If idle, wakes immediately.
    /// Otherwise just updates the timestamp — the timer callback
    /// will check elapsed time and reschedule if needed.
    pub fn reset_idle_timer(&mut self) {
        if self.idle_timeout.is_idle {
            self.wake_from_idle();
        } else {
            self.idle_timeout.last_activity = std::time::Instant::now();
        }
    }

    /// Kill idle child process (e.g., on lid close).
    pub fn kill_idle_child(&mut self) {
        if let Some(mut child) = self.idle_timeout.child_process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Process all outputs that have queued redraws.
    ///
    /// This lives on Ewm (not the backend) because
    /// output_state is owned by Ewm. The backend only provides a render() method.
    pub fn redraw_queued_outputs(&mut self, backend: &mut backend::Backend) {
        tracy_span!("redraw_queued_outputs");

        if !self.monitors_active {
            return;
        }

        // Use while-let with find() so outputs queued during
        // rendering (e.g., by VBlank handlers) are picked up in the same pass.
        while let Some(output) = self
            .output_state
            .iter()
            .find(|(_, state)| {
                matches!(
                    state.redraw_state,
                    RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
                )
            })
            .map(|(output, _)| output.clone())
        {
            self.redraw(backend, &output);
        }
    }

    /// Return the next output needing a redraw, preferring `prefer` if queued.
    fn next_queued_redraw(&self, prefer: Option<&str>) -> Option<Output> {
        if !self.monitors_active {
            return None;
        }
        let needs_redraw = |s: &OutputState| {
            matches!(
                s.redraw_state,
                RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
            )
        };
        if let Some(name) = prefer {
            let found = self
                .output_state
                .iter()
                .find(|(o, s)| o.name() == name && needs_redraw(s));
            if let Some((output, _)) = found {
                return Some(output.clone());
            }
        }
        self.output_state
            .iter()
            .find(|(_, s)| needs_redraw(s))
            .map(|(o, _)| o.clone())
    }

    /// Orchestrate a single output redraw.
    ///
    /// Orchestrate a single output redraw:
    /// 1. Get target presentation time from frame clock
    /// 2. Call backend.render() → RenderResult
    /// 3. Handle state transitions based on result
    /// 4. Update lock render state
    /// 5. Send frame callbacks
    /// 6. Process screencopy/screencast via backend
    fn redraw(&mut self, backend: &mut backend::Backend, output: &Output) {
        tracy_span!("ewm_redraw");

        // Verify our invariant and get target presentation time
        let target_presentation_time = {
            let Some(state) = self.output_state.get(output) else {
                return;
            };
            debug_assert!(matches!(
                state.redraw_state,
                RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
            ));
            state.frame_clock.next_presentation_time()
        };

        // Refresh protocol state before rendering
        self.refresh_foreign_toplevel();
        self.refresh_output_management();

        // Render via backend
        let res = backend.render(self, output, target_presentation_time);

        // Handle state transitions based on render result
        let is_locked = self.is_locked();
        if let Some(state) = self.output_state.get_mut(output) {
            if res == backend::RenderResult::Skipped {
                // Preserve estimated vblank timer if one exists, otherwise go Idle.
                // Submitted and NoDamage state transitions are owned by each backend's render().
                state.redraw_state = if let RedrawState::WaitingForEstimatedVBlank(token)
                | RedrawState::WaitingForEstimatedVBlankAndQueued(token) =
                    state.redraw_state
                {
                    RedrawState::WaitingForEstimatedVBlank(token)
                } else {
                    RedrawState::Idle
                };
            }

            // Update lock render state on every successful render.
            // Setting both Locked and Unlocked prevents stale state if
            // the session transitions between locked and unlocked.
            if res != backend::RenderResult::Skipped {
                state.lock_render_state = if is_locked {
                    LockRenderState::Locked
                } else {
                    LockRenderState::Unlocked
                };
            }

            // Compute whether animations need another frame on this output.
            // When true, VBlank handlers queue another redraw to keep animations pumping.
            // TODO: check resize/crossfade animations, animated cursor, layer surface animations
            state.unfinished_animations_remain = false;
        }

        // Check lock confirmation requirements
        if res != backend::RenderResult::Skipped {
            self.check_lock_complete();
        } else {
            self.abort_lock_on_render_failure();
        }

        // Send frame callbacks
        self.send_frame_callbacks(output);

        // Process screencopy and screencast via backend
        if res != backend::RenderResult::Skipped {
            backend.post_render(self, output);
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
        // Update windows (all windows, including layout surfaces not in Space)
        for window in self.id_windows.values() {
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

        // Update DnD icon
        if let Some(icon) = self.dnd_icon.as_ref() {
            with_surface_tree_downward(
                &icon.surface,
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

    /// Send DMA-BUF feedback to clients, telling them which formats/modifiers
    /// the compositor can scanout directly vs. which require GPU composition.
    pub fn send_dmabuf_feedbacks(
        &self,
        output: &Output,
        feedback: &backend::drm::SurfaceDmabufFeedback,
        render_element_states: &RenderElementStates,
    ) {
        for window in self.id_windows.values() {
            window.send_dmabuf_feedback(
                output,
                |_, _| Some(output.clone()),
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }

        let layer_map = layer_map_for_output(output);
        for layer in layer_map.layers() {
            layer.send_dmabuf_feedback(
                output,
                |_, _| Some(output.clone()),
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }
        drop(layer_map);

        if let Some(output_state) = self.output_state.get(output) {
            if let Some(ref lock_surface) = output_state.lock_surface {
                send_dmabuf_feedback_surface_tree(
                    lock_surface.wl_surface(),
                    output,
                    |_, _| Some(output.clone()),
                    |surface, _| {
                        select_dmabuf_feedback(
                            surface,
                            render_element_states,
                            &feedback.render,
                            &feedback.scanout,
                        )
                    },
                );
            }
        }

        // DnD icon
        if let Some(icon) = self.dnd_icon.as_ref() {
            send_dmabuf_feedback_surface_tree(
                &icon.surface,
                output,
                surface_primary_scanout_output,
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }
    }

    /// Collect presentation feedback callbacks from all surfaces on this output.
    ///
    /// Drains pending `wp_presentation_feedback` callbacks from each surface's
    /// cached state, filtering to surfaces whose primary scanout matches this output.
    /// The collected feedback is passed through `queue_frame()` and delivered in the
    /// VBlank handler via `feedback.presented()`.
    pub fn take_presentation_feedbacks(
        &self,
        output: &Output,
        render_element_states: &RenderElementStates,
    ) -> OutputPresentationFeedback {
        let mut feedback = OutputPresentationFeedback::new(output);

        // Collect from windows
        for window in self.id_windows.values() {
            window.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(surface, render_element_states)
                },
            );
        }

        // Collect from layer surfaces
        let layer_map = layer_map_for_output(output);
        for layer in layer_map.layers() {
            layer.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(surface, render_element_states)
                },
            );
        }
        drop(layer_map);

        // Collect from lock surface
        if let Some(output_state) = self.output_state.get(output) {
            if let Some(ref lock_surface) = output_state.lock_surface {
                take_presentation_feedback_surface_tree(
                    lock_surface.wl_surface(),
                    &mut feedback,
                    surface_primary_scanout_output,
                    |surface, _| {
                        surface_presentation_feedback_flags_from_states(
                            surface,
                            render_element_states,
                        )
                    },
                );
            }
        }

        // Collect from DnD icon
        if let Some(icon) = self.dnd_icon.as_ref() {
            take_presentation_feedback_surface_tree(
                &icon.surface,
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(surface, render_element_states)
                },
            );
        }

        feedback
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

        let frame_callback_time = crate::protocols::screencopy::get_monotonic_time();

        for window in self.id_windows.values() {
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

        // DnD icon
        if let Some(icon) = self.dnd_icon.as_ref() {
            send_frames_surface_tree(
                &icon.surface,
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                &should_send,
            );
        }
    }

    /// Fallback frame callback sender — safety net for stuck surfaces.
    ///
    /// Sends frame callbacks to ALL surfaces regardless of output, bypassing
    /// primary scanout output matching. The `FRAME_CALLBACK_THROTTLE` (995ms)
    /// prevents busy-looping since it won't re-send if a callback was already
    /// sent recently through the normal path.
    pub fn send_frame_callbacks_on_fallback_timer(&self) {
        // Bogus output — the should_send closure returns None so the output
        // is never used for matching, but send_frame requires a reference.
        let output = Output::new(
            String::new(),
            PhysicalProperties {
                size: Size::from((0, 0)),
                subpixel: Subpixel::Unknown,
                make: String::new(),
                model: String::new(),
                serial_number: String::new(),
            },
        );

        let frame_callback_time = crate::protocols::screencopy::get_monotonic_time();

        for window in self.id_windows.values() {
            window.send_frame(
                &output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                |_, _| None,
            );
        }

        for (out, state) in self.output_state.iter() {
            let layer_map = layer_map_for_output(out);
            for layer in layer_map.layers() {
                layer.send_frame(out, frame_callback_time, FRAME_CALLBACK_THROTTLE, |_, _| {
                    None
                });
            }

            if let Some(ref lock_surface) = state.lock_surface {
                send_frames_surface_tree(
                    lock_surface.wl_surface(),
                    out,
                    frame_callback_time,
                    FRAME_CALLBACK_THROTTLE,
                    |_, _| None,
                );
            }
        }

        // DnD icon (not tied to a specific output)
        if let Some(icon) = self.dnd_icon.as_ref() {
            let output = self.output_state.keys().next().unwrap_or(&output);
            send_frames_surface_tree(
                &icon.surface,
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                |_, _| None,
            );
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

        let windows: Vec<WindowInfo> = self
            .id_windows
            .iter()
            .filter_map(|(&id, window)| {
                let surface = window.wl_surface()?.into_owned();
                let info = self.surface_info.get(&id)?;
                let output = self.find_surface_output(id);
                let is_fullscreen = self.is_surface_fullscreen(id);
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
                    is_fullscreen,
                })
            })
            .collect();

        self.foreign_toplevel_state.refresh::<State>(windows);
    }

    /// Refresh output management protocol if output heads changed.
    /// Called from redraw() before rendering, so updates are deferred
    /// until after any in-flight Dispatch handlers have finished.
    pub fn refresh_output_management(&mut self) {
        if !self.output_management_state.output_heads_changed {
            return;
        }
        self.output_management_state.output_heads_changed = false;
        let new_state = self.build_output_head_states();
        self.output_management_state.notify_changes(new_state);
    }

    /// Find the output for a surface (returns Output object)
    fn find_surface_output(&self, surface_id: u64) -> Option<smithay::output::Output> {
        // Layout surfaces: prefer focused output, then any
        if self.surface_outputs.contains_key(&surface_id) {
            if let Some((name, _)) = self.focused_output_for_surface(surface_id) {
                return self.space.outputs().find(|o| o.name() == name).cloned();
            }
            if let Some(output_names) = self.surface_outputs.get(&surface_id) {
                if let Some(name) = output_names.iter().next() {
                    return self.space.outputs().find(|o| o.name() == *name).cloned();
                }
            }
        }
        // Non-layout surfaces (Emacs frames): use space geometry
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

    /// Find which output a window is on (by compositor window ID).
    /// Returns the output name, or None if the window is not found.
    /// Uses the cached `window_cast_output` map, populated in `apply_output_layout()`.
    #[cfg(feature = "screencast")]
    pub fn window_output_name(&self, window_id: u64) -> Option<String> {
        self.window_cast_output.get(&window_id).cloned()
    }

    /// Stop all screen casts targeting a specific window.
    #[cfg(feature = "screencast")]
    pub fn stop_casts_for_window(&mut self, window_id: u64) {
        let target = dbus::CastTarget::Window {
            id: window_id,
        };
        let sessions_to_stop: Vec<usize> = self
            .screen_casts
            .iter()
            .filter(|(_, cast)| cast.target == target)
            .map(|(session_id, _)| *session_id)
            .collect();
        for session_id in sessions_to_stop {
            tracing::info!(
                session_id,
                window_id,
                "stopping window cast (window closed)"
            );
            self.stop_cast(session_id);
        }
    }

    /// Render a window into a screencast buffer.
    ///
    /// Returns true if a frame was rendered. The `cast` is passed separately
    /// because `screen_casts` is detached from `self` during the render loop.
    #[cfg(feature = "screencast")]
    pub fn render_window_for_screen_cast(
        &self,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        cast: &mut pipewire::stream::Cast,
        window_id: u64,
        output: &Output,
        cursor_buffer: &cursor::CursorBuffer,
        output_scale: smithay::utils::Scale<f64>,
        target_frame_time: std::time::Duration,
    ) -> bool {
        use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
        use smithay::backend::renderer::element::Kind;
        use smithay::wayland::seat::WaylandFocus;

        let Some(window) = self.id_windows.get(&window_id) else {
            return false;
        };

        // Use bbox_with_popups for accurate size including popups,
        // scaled to physical pixels via the output's fractional scale.
        let bbox = window.bbox_with_popups().to_physical_precise_up(output_scale);
        let window_size = bbox.size;

        let refresh = output
            .current_mode()
            .map(|m| (m.refresh / 1000) as u32)
            .unwrap_or(60);

        cast.ensure_size(window_size, refresh);

        if cast.is_resize_pending() {
            return false;
        }

        if cast.check_time_and_schedule(output, target_frame_time) {
            return false;
        }

        // bbox.loc is the offset of the bounding box from the window origin;
        // negate it so popups extending beyond the window are visible.
        let render_offset = smithay::utils::Point::from((-bbox.loc.x, -bbox.loc.y));

        let window_elements: Vec<render::EwmRenderElement> =
            if let Some(surface) = window.wl_surface() {
                let elems: Vec<_> = render_elements_from_surface_tree(
                    renderer,
                    &surface,
                    render_offset,
                    output_scale,
                    1.0,
                    Kind::Unspecified,
                );
                elems
                    .into_iter()
                    .map(render::EwmRenderElement::Surface)
                    .collect()
            } else {
                Vec::new()
            };

        // Render cursor into window cast if cursor_mode != 0
        let mut cursor_elements: Vec<render::EwmRenderElement> = Vec::new();
        let mut cursor_location =
            smithay::utils::Point::<i32, smithay::utils::Physical>::from((0, 0));

        if cast.cursor_mode != 0 {
            let (px, py) = self.pointer_location();
            let output_geo = self.space.output_geometry(output);

            if let Some(output_geo) = output_geo {
                if let Some(entries) = self.output_layouts.get(&output.name()) {
                    let working_area = {
                        let map = smithay::desktop::layer_map_for_output(output);
                        map.non_exclusive_zone()
                    };
                    for entry in entries {
                        if entry.id != window_id {
                            continue;
                        }
                        let entry_x = output_geo.loc.x + working_area.loc.x + entry.x;
                        let entry_y = output_geo.loc.y + working_area.loc.y + entry.y;

                        if px >= entry_x as f64
                            && py >= entry_y as f64
                            && px < (entry_x + entry.w as i32) as f64
                            && py < (entry_y + entry.h as i32) as f64
                        {
                            // Cursor position relative to the screencast buffer
                            let bbox_logical = bbox.loc.to_f64().to_logical(output_scale);
                            let cursor_x =
                                px - entry_x as f64 - bbox_logical.x - cursor::CURSOR_HOTSPOT.0 as f64;
                            let cursor_y =
                                py - entry_y as f64 - bbox_logical.y - cursor::CURSOR_HOTSPOT.1 as f64;
                            let cursor_pos: smithay::utils::Point<i32, smithay::utils::Physical> =
                                smithay::utils::Point::from((cursor_x, cursor_y))
                                    .to_physical_precise_round(output_scale);

                            // cursor_location for SPA_META_Cursor (without hotspot offset)
                            let loc_x = px - entry_x as f64 - bbox_logical.x;
                            let loc_y = py - entry_y as f64 - bbox_logical.y;
                            cursor_location = smithay::utils::Point::from((loc_x, loc_y))
                                .to_physical_precise_round(output_scale);

                            if let Ok(cursor_element) =
                                cursor_buffer.render_element(renderer, cursor_pos)
                            {
                                cursor_elements
                                    .push(render::EwmRenderElement::Cursor(cursor_element));
                            }
                        }
                        break;
                    }
                }
            }
        }

        if cast.dequeue_buffer_and_render(
            renderer,
            &window_elements,
            &cursor_elements,
            cursor_location,
            window_size,
            output_scale,
        ) {
            cast.last_frame_time = target_frame_time;
            true
        } else {
            false
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
        self.emacs_surfaces.contains_key(&self.focused_surface_id)
    }

    /// Set focus to a surface and notify Emacs.
    /// Marks keyboard focus dirty for deferred sync.
    /// Focus a surface with source tracking for debugging.
    /// Marks keyboard focus dirty for deferred sync via sync_keyboard_focus().
    pub fn set_focus(&mut self, id: u64, notify_emacs: bool, source: &str, context: Option<&str>) {
        module::record_focus(id, source, context);
        self.focused_surface_id = id;
        self.keyboard_focus_dirty = true;
        if notify_emacs {
            self.queue_event(Event::Focus { id });
        }
    }

    /// Update on-demand layer shell keyboard focus.
    /// If the surface has OnDemand keyboard interactivity, set it as on-demand focus.
    /// Otherwise, clear on-demand focus.
    pub fn focus_layer_surface_if_on_demand(&mut self, surface: Option<DesktopLayerSurface>) {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        if let Some(surface) = surface {
            if surface.cached_state().keyboard_interactivity == KeyboardInteractivity::OnDemand {
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
    /// then OnDemand focus.
    fn resolve_layer_keyboard_focus(&self) -> Option<WlSurface> {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        // Helper: find exclusive focus on a layer
        let excl_on_layer = |output: &Output, layer: Layer| -> Option<WlSurface> {
            let map = layer_map_for_output(output);
            let layers: Vec<_> = map.layers_on(layer).cloned().collect();
            layers.into_iter().find_map(|surface| {
                if surface.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
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
            if self.focused_surface_id == 0
                || self.id_windows.get(&self.focused_surface_id).is_none()
            {
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
    ///
    /// Emacs surfaces and None both clear text_input (leave + set_focus(None)),
    /// causing the client to disable. Commits that arrive during the resulting
    /// disable→enable gap are queued in `pending_im_commits` and drained on
    /// the next `ImEvent::Activated`.
    pub fn update_text_input_focus(&self, surface: Option<&WlSurface>, surface_id: Option<u64>) {
        use smithay::wayland::text_input::TextInputSeat;
        let text_input = self.seat.text_input();

        let is_emacs = surface_id.map_or(false, |id| self.emacs_surfaces.contains_key(&id));

        if is_emacs || surface.is_none() {
            text_input.leave();
            text_input.set_focus(None);
        } else if let Some(s) = surface {
            text_input.set_focus(Some(s.clone()));
            text_input.enter();
        }
    }

    /// Get surface ID from a WlSurface
    pub fn surface_id(&self, surface: &WlSurface) -> Option<u64> {
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
            if let Some((surface, pos_in_layer)) =
                layer_surface.surface_under(pos_within_output - layer_pos, WindowSurfaceType::ALL)
            {
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
        let above_top_layer = self.render_above_top_layer(output);

        let map = layer_map_for_output(output);
        // When fullscreen, only Overlay receives input above the fullscreen surface.
        // Top/Bottom/Background are visually behind it and must not intercept clicks.
        let layers: &[Layer] = if above_top_layer {
            &[Layer::Overlay]
        } else {
            &[Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background]
        };
        for &layer in layers {
            let surfaces: Vec<_> = map.layers_on(layer).rev().cloned().collect();
            for layer_surface in &surfaces {
                let geo = match map.layer_geometry(layer_surface) {
                    Some(g) => g,
                    None => continue,
                };
                let layer_pos = geo.loc.to_f64();
                if layer_surface
                    .surface_under(pos_within_output - layer_pos, WindowSurfaceType::ALL)
                    .is_some()
                {
                    return Some(layer_surface.clone());
                }
            }
        }
        None
    }

    /// Find the surface under a point, checking layers and popups in render order.
    ///
    /// When no fullscreen surface is active:
    ///   Overlay → Top → [popups] → [layout surfaces] → [Emacs frames] → Bottom → Background
    ///
    /// When a fullscreen surface is active on the output (`render_above_top_layer`):
    ///   Overlay → [popups] → [layout surfaces] → [Emacs frames] → Top → Bottom → Background
    ///
    /// This ensures the input hit-testing order matches the visual render order.
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
        let above_top_layer = output.map_or(false, |o| self.render_above_top_layer(o));

        if let (Some(output), Some(geo)) = (output, output_geo) {
            let pos_within_output = pos - geo.loc.to_f64();

            // 1. Overlay layer (always highest, regardless of fullscreen)
            if let Some(result) =
                self.layer_surface_under(output, Layer::Overlay, pos_within_output, geo.loc)
            {
                return Some(result);
            }

            // 2. Top layer — only before windows when not fullscreen
            if !above_top_layer {
                if let Some(result) =
                    self.layer_surface_under(output, Layer::Top, pos_within_output, geo.loc)
                {
                    return Some(result);
                }
            }
        }

        // 3. Layout surfaces (output_layouts-managed, includes popup/subsurface hit-testing)
        if let Some(result) = self.layout_surface_under(pos) {
            return Some(result);
        }

        // 4. Emacs frames (still in Space)
        if let Some(result) = self
            .space
            .element_under(pos)
            .and_then(|(window, loc)| window.wl_surface().map(|s| (s.into_owned(), loc.to_f64())))
        {
            return Some(result);
        }

        // 5. Deferred layers (Top deferred behind fullscreen, then Bottom/Background)
        if let (Some(output), Some(geo)) = (output, output_geo) {
            let pos_within_output = pos - geo.loc.to_f64();

            if above_top_layer {
                if let Some(result) =
                    self.layer_surface_under(output, Layer::Top, pos_within_output, geo.loc)
                {
                    return Some(result);
                }
            }

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

    /// Get the output where the focused surface is located
    fn get_focused_output(&self) -> Option<String> {
        if self.focused_surface_id == 0 {
            return None;
        }
        self.find_surface_output(self.focused_surface_id)
            .map(|o| o.name())
    }

    /// Get the output under the cursor position
    fn output_under_cursor(&self) -> Option<String> {
        use smithay::utils::Point;
        let (px, py) = self.pointer_location();
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

    /// Find the Emacs surface on the same output as the focused surface
    pub fn get_emacs_surface_for_focused_output(&self) -> Option<u64> {
        let focused_output = self.get_focused_output()?;

        // Find an Emacs surface assigned to the focused output
        self.emacs_surfaces
            .iter()
            .find(|(_, out)| *out == &focused_output)
            .map(|(&id, _)| id)
            .or_else(|| self.emacs_surfaces.keys().next().copied())
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
        self.active_outputs_dirty = true;
        self.queue_event(Event::OutputDetected(output));
    }

    /// Send output disconnected event to Emacs
    pub fn send_output_disconnected(&mut self, name: &str) {
        self.active_outputs_dirty = true;
        self.queue_event(Event::OutputDisconnected {
            name: name.to_string(),
        });
    }

    /// Register a newly connected output.
    ///
    /// Called by backends after hardware setup (DRM surface, virtual output) and
    /// after the output is mapped in the space. Handles all backend-agnostic
    /// bookkeeping: OutputInfo registration, D-Bus output, size recalculation,
    /// IPC event to Emacs, initial working area, and output management state.
    pub fn add_output(&mut self, output: &Output, info: OutputInfo) {
        let output_name = info.name.clone();

        // Register D-Bus output for screen casting
        #[cfg(feature = "screencast")]
        {
            let current_mode = output.current_mode().unwrap_or(smithay::output::Mode {
                size: (0, 0).into(),
                refresh: 0,
            });
            let mut dbus_outputs = self.dbus_outputs.lock().unwrap();
            dbus_outputs.push(crate::dbus::OutputInfo {
                name: output_name.clone(),
                x: info.x,
                y: info.y,
                width: current_mode.size.w,
                height: current_mode.size.h,
                refresh: (current_mode.refresh / 1000) as u32,
            });
            info!(
                "Added D-Bus output: {} at ({}, {}) (total: {})",
                output_name,
                info.x,
                info.y,
                dbus_outputs.len()
            );
        }

        self.outputs.push(info.clone());

        self.recalculate_output_size();

        self.send_output_detected(info);

        // Send initial working area (full output initially, before any panels)
        let working_area = self.get_working_area(output);
        self.working_areas.insert(output_name.clone(), working_area);
        self.queue_event(Event::WorkingArea {
            output: output_name,
            x: working_area.loc.x,
            y: working_area.loc.y,
            width: working_area.size.w,
            height: working_area.size.h,
        });

        self.output_management_state.output_heads_changed = true;
    }

    /// Handle output configuration changes (mode, scale, transform, position).
    ///
    /// Called by backends after applying hardware state changes. Handles all
    /// backend-agnostic bookkeeping: OutputInfo update, D-Bus update, scale
    /// notification, lock buffer resize, screencast size, recalculation,
    /// working areas, redraw, IPC event to Emacs, and output management state.
    pub fn output_config_changed(&mut self, output: &Output) {
        let output_name = output.name();
        let current_mode = output.current_mode().unwrap_or(smithay::output::Mode {
            size: (0, 0).into(),
            refresh: 0,
        });
        let current_scale = output.current_scale().fractional_scale();
        let current_transform = output.current_transform();
        let current_geo = self.space.output_geometry(output).unwrap_or_default();

        // Update OutputInfo in-place
        for out_info in &mut self.outputs {
            if out_info.name == output_name {
                out_info.scale = current_scale;
                out_info.transform = backend::transform_to_int(current_transform);
                out_info.x = current_geo.loc.x;
                out_info.y = current_geo.loc.y;
                for mode_info in &mut out_info.modes {
                    mode_info.preferred = mode_info.width == current_mode.size.w
                        && mode_info.height == current_mode.size.h
                        && mode_info.refresh == current_mode.refresh;
                }
            }
        }

        // Update D-Bus outputs
        #[cfg(feature = "screencast")]
        {
            let mut dbus_outputs = self.dbus_outputs.lock().unwrap();
            for dbus_out in dbus_outputs.iter_mut() {
                if dbus_out.name == output_name {
                    dbus_out.width = current_mode.size.w;
                    dbus_out.height = current_mode.size.h;
                    dbus_out.refresh = (current_mode.refresh / 1000) as u32;
                    dbus_out.x = current_geo.loc.x;
                    dbus_out.y = current_geo.loc.y;
                }
            }
        }

        // Notify existing surfaces of scale/transform change
        self.send_scale_transform_to_output_surfaces(output);

        // Resize lock buffer and reconfigure lock surface for new output size
        let output_size = crate::utils::output_size(output);
        let is_locked = self.is_locked();
        if let Some(state) = self.output_state.get_mut(output) {
            state.resize_lock_buffer((output_size.w as i32, output_size.h as i32));
            state.resize_fullscreen_backdrop((output_size.w as i32, output_size.h as i32));
            if is_locked {
                if let Some(lock_surface) = &state.lock_surface {
                    configure_lock_surface(lock_surface, output);
                }
            }
        }

        // Notify output screen casts of size change
        #[cfg(feature = "screencast")]
        {
            let physical_size = Size::from((current_mode.size.w, current_mode.size.h));
            let refresh = (current_mode.refresh / 1000) as u32;
            let target = dbus::CastTarget::Output {
                name: output_name.to_string(),
            };
            for cast in self.screen_casts.values_mut() {
                if cast.target == target {
                    cast.ensure_size(physical_size, refresh);
                    cast.set_refresh(refresh);
                }
            }
        }

        // Recalculate total output size, working areas, and queue redraw
        self.recalculate_output_size();
        self.active_outputs_dirty = true;
        self.check_working_area_change(output);
        self.queue_redraw_all();

        // Notify Emacs of the applied config
        self.queue_event(Event::OutputConfigChanged {
            name: output_name.to_string(),
            width: current_mode.size.w,
            height: current_mode.size.h,
            refresh: current_mode.refresh,
            x: current_geo.loc.x,
            y: current_geo.loc.y,
            scale: current_scale,
            transform: backend::transform_to_int(current_transform),
        });

        self.output_management_state.output_heads_changed = true;
    }

    /// Notify all surfaces on an output about a scale/transform change.
    ///
    /// Iterates windows and layer surfaces on the given output,
    /// sending both integer and fractional scale via `send_scale_transform`.
    /// Called from `apply_output_config` after changing an output's scale or transform.
    pub fn send_scale_transform_to_output_surfaces(&self, output: &Output) {
        let scale = output.current_scale();
        let transform = output.current_transform();

        // Notify declared surfaces on this output
        if let Some(entries) = self.output_layouts.get(&output.name()) {
            for entry in entries {
                if let Some(window) = self.id_windows.get(&entry.id) {
                    window.with_surfaces(|surface, data| {
                        crate::utils::send_scale_transform(surface, data, scale, transform);
                    });
                }
            }
        }

        // Notify undeclared windows (Emacs frames) that intersect this output
        for window in self.space.elements() {
            let window_id = self.window_ids.get(window).copied().unwrap_or(0);
            if self.surface_outputs.contains_key(&window_id) {
                continue; // managed by output_layouts, already handled above
            }
            let loc = self.space.element_location(window).unwrap_or_default();
            let output_geo = self.space.output_geometry(output).unwrap_or_default();
            if output_geo.contains(loc) {
                window.with_surfaces(|surface, data| {
                    crate::utils::send_scale_transform(surface, data, scale, transform);
                });
            }
        }

        // Notify layer surfaces on this output
        let layer_map = layer_map_for_output(output);
        for layer in layer_map.layers() {
            layer.with_surfaces(|surface, data| {
                crate::utils::send_scale_transform(surface, data, scale, transform);
            });
        }

        // Notify DnD icon
        if let Some(icon) = self.dnd_icon.as_ref() {
            smithay::wayland::compositor::with_states(&icon.surface, |data| {
                crate::utils::send_scale_transform(&icon.surface, data, scale, transform);
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

        // Find Emacs frame surfaces assigned to this output and update their position/size
        for (&id, window) in &self.id_windows {
            // Only update Emacs surfaces assigned to this output
            if self.emacs_surfaces.get(&id).map(|s| s.as_str()) != Some(&*output.name()) {
                continue;
            }

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
            self.active_outputs_dirty = true;

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

    /// Find the output for a popup's root surface (window or layer surface).
    pub fn output_for_popup(&self, popup: &PopupKind) -> Option<&Output> {
        let root = find_popup_root_surface(popup).ok()?;

        // Check windows (layout surfaces + Emacs frames)
        if let Some((window, id)) = self.find_window_by_surface(&root) {
            // Layout surface: prefer focused output
            if let Some((name, _)) = self.focused_output_for_surface(id) {
                return self.space.outputs().find(|o| o.name() == name);
            }
            // Fallback: any output from surface_outputs
            if let Some(output_names) = self.surface_outputs.get(&id) {
                if let Some(name) = output_names.iter().next() {
                    return self.space.outputs().find(|o| o.name() == *name);
                }
            }
            // Emacs frame: use space geometry
            let window_loc = self.space.element_location(&window).unwrap_or_default();
            return self.space.outputs().find(|o| {
                self.space
                    .output_geometry(o)
                    .map(|geo| geo.contains(window_loc))
                    .unwrap_or(false)
            });
        }

        // Check layer surfaces
        self.space.outputs().find(|o| {
            layer_map_for_output(o)
                .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                .is_some()
        })
    }

    /// Compute the available rectangle for a popup in geometry-local coordinates.
    ///
    /// Derived from the layout entry position — no dependency on window.geometry(),
    /// so centering offsets and CSD geometry don't affect the result.
    /// Fullscreen: full output. Normal: entry width × output height (horizontally
    /// constrained to the entry, vertically to the full output).
    fn popup_target_rect(&self, id: u64) -> Option<Rectangle<i32, Logical>> {
        let (output_name, entry) = self
            .focused_output_for_surface(id)
            .or_else(|| self.primary_output_for_surface(id))?;
        let output = self.space.outputs().find(|o| o.name() == output_name)?;
        let output_geo = self.space.output_geometry(output)?;

        if entry.fullscreen {
            return Some(Rectangle::from_size(output_geo.size));
        }

        let working_area = {
            let map = layer_map_for_output(output);
            map.non_exclusive_zone()
        };

        // Horizontal: constrain to entry width (popup stays within the window).
        // Vertical: full working area height relative to the entry's Y position.
        // Origin is in window-geometry-local coords: x=0 (window's left edge),
        // y=-entry.y (working area's top edge relative to window).
        Some(Rectangle::new(
            smithay::utils::Point::from((0, -(entry.y))),
            Size::from((entry.w as i32, working_area.size.h)),
        ))
    }

    /// Unconstrain a popup's position to keep it within screen bounds.
    ///
    /// For window popups, computes the target from the layout entry position
    /// (not window.geometry()), matching niri's popup_target_rect pattern.
    /// For layer-shell popups, uses the output bounds adjusted for the layer
    /// surface position and non-exclusive zone.
    pub fn unconstrain_popup(&self, popup: &PopupSurface) {
        let popup_kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&popup_kind) else {
            return;
        };

        // Window popup path
        if let Some((_window, id)) = self.find_window_by_surface(&root) {
            let raw_target = self
                .popup_target_rect(id)
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
        if let Some((output, layer_geo)) = self.space.outputs().find_map(|o| {
            let map = layer_map_for_output(o);
            let ls = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)?;
            let geo = map.layer_geometry(ls)?;
            Some((o.clone(), geo))
        }) {
            let output_geo = self
                .space
                .output_geometry(&output)
                .unwrap_or_else(|| Rectangle::from_size(self.output_size));

            let mut target = Rectangle::from_size(output_geo.size);
            target.loc -= layer_geo.loc;
            target.loc -= get_popup_toplevel_coords(&popup_kind);

            popup.with_pending_state(|state| {
                state.geometry = state.positioner.get_unconstrained_geometry(target);
            });
        }
    }

    /// Handle new toplevel surface from XdgShellHandler
    pub fn handle_new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_surface_id;
        self.next_surface_id += 1;

        // Check if this surface belongs to the Emacs client
        let is_emacs = self.is_emacs_client(surface.wl_surface());
        if is_emacs {
            info!("Surface {} is an Emacs surface", id);
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

        // Extract client PID (before surface is moved)
        let client_pid = self
            .display_handle
            .get_client(surface.wl_surface().id())
            .ok()
            .and_then(|c| c.get_credentials(&self.display_handle).ok())
            .map(|creds| creds.pid)
            .unwrap_or(0);

        surface.with_pending_state(|state| {
            state.size = Some(self.output_size);
            state.states.set(XdgToplevelState::Maximized);
            state.states.set(XdgToplevelState::Activated);
        });
        surface.send_configure();

        let window = Window::new_wayland_window(surface);
        self.window_ids.insert(window.clone(), id);
        self.id_windows.insert(id, window.clone());

        // Determine target output
        let frame_output = module::take_pending_frame_output();
        let target_output = frame_output.clone().or_else(|| self.active_output());

        if is_emacs {
            self.emacs_surfaces
                .insert(id, target_output.clone().unwrap_or_default());
        }

        // Associate window with target output for scale detection.
        // Output association is managed explicitly — not via space.refresh().
        // We send scale info and enter the output before the client's first commit
        // so it knows the correct scale to render at.
        let scale_output = target_output
            .as_ref()
            .and_then(|name| self.space.outputs().find(|o| o.name() == *name))
            .or_else(|| self.space.outputs().next())
            .cloned();
        if let Some(output) = scale_output {
            let scale = output.current_scale();
            let transform = output.current_transform();
            window.with_surfaces(|surface, data| {
                crate::utils::send_scale_transform(surface, data, scale, transform);
            });
            // Direct output enter — bypasses SpaceElement::output_enter which would
            // trigger output_update → leave for uncommitted surfaces.
            if let Some(surface) = window.wl_surface() {
                output.enter(&surface);
            }
        }

        // Only map Emacs frames into the space — layout surfaces are
        // positioned exclusively via output_layouts.
        if let Some(ref output_name) = frame_output {
            let position = self
                .space
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
                .unwrap_or((-10000, -10000));
            self.space.map_element(window.clone(), position, false);
        }

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

        self.surface_info.insert(
            id,
            SurfaceInfo {
                app_id: app.clone(),
                title: String::new(),
            },
        );

        // Send event to Emacs
        self.queue_event(Event::New {
            id,
            app: app.clone(),
            output: target_output.clone(),
            pid: client_pid,
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
    pub fn handle_toplevel_destroyed(&mut self, surface: ToplevelSurface) -> Option<u64> {
        let window = self
            .id_windows
            .iter()
            .find(|(_, w)| w.toplevel().map(|t| t == &surface).unwrap_or(false))
            .map(|(&id, w)| (id, w.clone()));

        if let Some((id, window)) = window {
            let was_focused = self.focused_surface_id == id;
            let output = self.find_surface_output(id).map(|o| o.name());

            self.window_ids.remove(&window);
            self.id_windows.remove(&id);
            self.surface_info.remove(&id);
            self.remove_surface_from_layouts(id);
            self.emacs_surfaces.remove(&id);
            self.queue_event(Event::Close { id });
            info!("Toplevel {} destroyed", id);

            // Stop any screen casts targeting this window
            #[cfg(feature = "screencast")]
            self.stop_casts_for_window(id);

            // Unmap from space (no-op if not in space)
            self.space.unmap_elem(&window);

            // Return refocus target if needed
            if was_focused {
                return output
                    .as_ref()
                    .and_then(|out| {
                        self.emacs_surfaces
                            .iter()
                            .find(|(_, o)| o.as_str() == out)
                            .map(|(&eid, _)| eid)
                    })
                    .or(Some(1));
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
        let Some((_window, id)) = self.find_window_by_surface(surface) else {
            return;
        };

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
            // Skip Title event for Emacs surfaces — Emacs already knows its own titles
            if !self.emacs_surfaces.contains_key(&id) {
                self.queue_event(Event::Title {
                    id,
                    app: app_id,
                    title,
                });
            }
        }
    }

    /// Handle commit for layer surfaces. Returns true if this was a layer surface.
    /// Handle layer shell surface commit.
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

                        // Auto-focus newly mapped OnDemand surfaces
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

    fn new_subsurface(&mut self, surface: &WlSurface, parent: &WlSurface) {
        crate::utils::propagate_preferred_scale(surface, parent);
    }

    fn commit(&mut self, surface: &WlSurface) {
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // Queue early import for DRM backend (processed in main loop)
        self.ewm.pending_early_imports.push(surface.clone());

        // DnD icon surface: update offset from buffer delta and redraw.
        if let Some(icon) = &self.ewm.dnd_icon {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if icon.surface == root {
                if surface == &icon.surface {
                    let dnd_icon = self.ewm.dnd_icon.as_mut().unwrap();
                    smithay::wayland::compositor::with_states(&dnd_icon.surface, |states| {
                        let delta = states
                            .cached_state
                            .get::<smithay::wayland::compositor::SurfaceAttributes>()
                            .current()
                            .buffer_delta
                            .take()
                            .unwrap_or_default();
                        dnd_icon.offset += delta;
                    });
                }
                self.ewm.queue_redraw_all();
                return;
            }
        }

        // Surface type dispatch order:
        // 1. Layer surfaces  2. Popups  3. Windows  4. Lock surfaces
        // When adding new surface types, add a branch here.

        // 1. Handle layer surface commits
        if self.ewm.handle_layer_surface_commit(surface) {
            return;
        }

        // 2. Handle popup commits
        self.ewm.popups.commit(surface);
        if let Some(popup) = self.ewm.popups.find_popup(surface) {
            if let PopupKind::Xdg(ref xdg_popup) = popup {
                if !xdg_popup.is_initial_configure_sent() {
                    if let Some(output) = self.ewm.output_for_popup(&popup).cloned() {
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

        // 3. Find the window that owns this root surface
        let window_and_id = self.ewm.find_window_by_surface(&root_surface);

        if let Some((window, id)) = window_and_id {
            window.on_commit();

            self.ewm.queue_redraw_for_surface(id);

            self.ewm.check_surface_info_changes(surface);
            return;
        }

        // 4. Queue redraw for lock surface commits (not in space.elements())
        if self.ewm.is_locked() {
            let output = self
                .ewm
                .output_state
                .iter()
                .find(|(_, state)| {
                    state
                        .lock_surface
                        .as_ref()
                        .is_some_and(|ls| ls.wl_surface() == &root_surface)
                })
                .map(|(o, _)| o.clone());
            if let Some(output) = output {
                self.ewm.queue_redraw(&output);
            }
        }
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

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.ewm.cursor_image_status = image;
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
impl WaylandDndGrabHandler for State {
    fn dnd_requested<S: dnd::Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: smithay::utils::Serial,
        type_: dnd::GrabType,
    ) {
        self.ewm.dnd_icon = icon.map(|surface| DndIcon {
            surface,
            offset: Point::from((0, 0)),
        });

        match type_ {
            dnd::GrabType::Pointer => {
                let pointer = seat.get_pointer().unwrap();
                let start_data = pointer.grab_start_data().unwrap();
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

        self.ewm.queue_redraw_all();
    }
}
impl DndGrabHandler for State {
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.ewm.dnd_icon = None;
        self.ewm.queue_redraw_all();
    }
}
impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.ewm.data_device_state
    }
}
delegate_data_device!(State);

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.ewm.primary_selection_state
    }
}
delegate_primary_selection!(State);

impl DataControlHandler for State {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.ewm.data_control_state
    }
}
delegate_data_control!(State);

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
    }
}
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
            self.ewm
                .set_focus(refocus_id, true, "toplevel_destroyed", None);
            self.sync_keyboard_focus();
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.ewm.unconstrain_popup(&surface);
        if let Err(err) = self.ewm.popups.track_popup(PopupKind::Xdg(surface)) {
            warn!("error tracking popup: {err:?}");
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(surface.wl_surface()) {
            module::push_event(Event::Minimize { id });
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

    fn fullscreen_request(&mut self, surface: ToplevelSurface, output: Option<WlOutput>) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(surface.wl_surface()) {
            let output_name = output
                .and_then(|o| Output::from_resource(&o))
                .map(|o| o.name())
                .or_else(|| self.ewm.find_surface_output(id).map(|o| o.name()));
            module::push_event(Event::FullscreenRequest {
                id,
                output: output_name,
            });
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(surface.wl_surface()) {
            module::push_event(Event::UnfullscreenRequest { id });
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(surface.wl_surface()) {
            module::push_event(Event::MaximizeRequest { id });
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(surface.wl_surface()) {
            module::push_event(Event::UnmaximizeRequest { id });
        }
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
            state.decoration_mode = Some(Mode::ClientSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(
        &mut self,
        toplevel: ToplevelSurface,
        mode: smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    ) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
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
            let name = self.ewm.active_output();
            name.and_then(|n| self.ewm.space.outputs().find(|o| o.name() == n).cloned())
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
        // Handle XDG activation token
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
                self.ewm.set_focus(id, true, "xdg_activation", None);
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

    fn is_read_only_surface(&self, wl_surface: &WlSurface) -> bool {
        self.ewm
            .find_window_by_surface(wl_surface)
            .is_some_and(|(_, id)| self.ewm.emacs_surfaces.contains_key(&id))
    }

    fn activate(&mut self, wl_surface: WlSurface) {
        if let Some(&id) = self
            .ewm
            .window_ids
            .iter()
            .find(|(w, _)| w.wl_surface().map(|s| &*s == &wl_surface).unwrap_or(false))
            .map(|(_, id)| id)
        {
            self.ewm.set_focus(id, true, "foreign_toplevel", None);
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

    fn set_fullscreen(&mut self, wl_surface: WlSurface, wl_output: Option<WlOutput>) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(&wl_surface) {
            let output_name = wl_output
                .and_then(|o| Output::from_resource(&o))
                .map(|o| o.name())
                .or_else(|| self.ewm.find_surface_output(id).map(|o| o.name()));
            module::push_event(Event::FullscreenRequest {
                id,
                output: output_name,
            });
        }
    }

    fn unset_fullscreen(&mut self, wl_surface: WlSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(&wl_surface) {
            module::push_event(Event::UnfullscreenRequest { id });
        }
    }

    fn set_maximized(&mut self, wl_surface: WlSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(&wl_surface) {
            module::push_event(Event::MaximizeRequest { id });
        }
    }

    fn unset_maximized(&mut self, wl_surface: WlSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(&wl_surface) {
            module::push_event(Event::UnmaximizeRequest { id });
        }
    }

    fn minimize(&mut self, wl_surface: WlSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(&wl_surface) {
            module::push_event(Event::Minimize { id });
        }
    }
}
delegate_foreign_toplevel!(State);

// Workspace protocol (ext-workspace-v1: Emacs tabs as workspaces)
impl WorkspaceHandler for State {
    fn workspace_manager_state(&mut self) -> &mut WorkspaceManagerState {
        &mut self.ewm.workspace_state
    }

    fn activate_workspace(&mut self, output: String, tab_index: usize) {
        module::push_event(Event::ActivateWorkspace { output, tab_index });
    }
}
delegate_workspace!(State);

// Output management protocol (wlr-output-management-unstable-v1)
impl OutputManagementHandler for State {
    fn output_management_state(&mut self) -> &mut OutputManagementState {
        &mut self.ewm.output_management_state
    }

    fn apply_output_config(&mut self, configs: HashMap<String, OutputConfig>) {
        // Merge each config into ewm.output_config and apply.
        // The backend sets output_heads_changed = true; the deferred
        // refresh_output_management() call (before next render) will
        // send protocol updates to clients — after the Dispatch handler
        // has already sent succeeded().
        for (name, config) in configs {
            self.ewm.output_config.insert(name.clone(), config);
            self.apply_output_config_for(&name);
        }
    }
}
delegate_output_management!(State);

// Screencopy protocol
impl ScreencopyHandler for State {
    fn frame(&mut self, manager: &ZwlrScreencopyManagerV1, screencopy: Screencopy) {
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
            backend.with_renderer(|renderer, cursor_buffer, event_loop| {
                crate::render::render_screencopy_immediate(
                    ewm,
                    renderer,
                    &manager,
                    screencopy,
                    cursor_buffer,
                    event_loop,
                );
            });
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

        // Wake from idle if locked while idle (need monitors on for lock screen)
        self.ewm.wake_from_idle();

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

        // Invalidate tracked focus (was on lock surface) to force sync
        self.ewm.keyboard_focus = None;

        // Restore focus to the surface that was focused before locking
        if let Some(id) = self.ewm.pre_lock_focus.take() {
            if self.ewm.id_windows.contains_key(&id) {
                info!("Restoring focus to surface {} after unlock", id);
                self.ewm.set_focus(id, false, "unlock", None);
            }
        }
        self.sync_keyboard_focus();

        self.ewm.queue_redraw_all();

        // Restart idle timer after unlock
        self.ewm.reset_idle_timer();

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

        // Trigger keyboard focus sync to the lock surface
        self.ewm.keyboard_focus_dirty = true;

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
delegate_gamma_control!(State);

// Fractional scale protocol (wp-fractional-scale-v1)
// Scale is sent from lifecycle-specific handlers (handle_new_toplevel, new_layer_surface,
// configure_lock_surface, apply_output_config) that know the correct output.
impl FractionalScaleHandler for State {}
delegate_fractional_scale!(State);

// Presentation time protocol (wp-presentation-time)
smithay::delegate_presentation!(State);

// Viewporter protocol (wp-viewporter, required by fractional scale clients)
delegate_viewporter!(State);

// Relative pointer protocol (zwp-relative-pointer-v1)
delegate_relative_pointer!(State);

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

        // Clamp hint to output bounds
        let target = origin + location;
        let clamped_x = target.x.clamp(0.0, self.ewm.output_size.w as f64);
        let clamped_y = target.y.clamp(0.0, self.ewm.output_size.h as f64);
        pointer.set_location((clamped_x, clamped_y).into());

        // Redraw to update cursor position visually
        self.ewm.queue_redraw_for_pointer();
    }
}
delegate_pointer_constraints!(State);

/// Configure a lock surface to cover the full output
fn configure_lock_surface(surface: &LockSurface, output: &Output) {
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
    /// Warp the pointer to an absolute position, notifying clients.
    /// Does NOT activate pointer constraints (programmatic warps from Emacs
    /// during layout changes should not trap the pointer).
    pub fn warp_pointer(&mut self, x: f64, y: f64) {
        let pointer = self.ewm.pointer.clone();
        let under = self.ewm.surface_under_point(Point::from((x, y)));
        self.ewm.pointer_focus = under.clone();
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location: (x, y).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time: 0,
            },
        );
        pointer.frame(self);
        self.ewm.queue_redraw_for_pointer();
    }

    /// Re-compute `pointer_focus` from the current pointer location and send
    /// a motion event so Smithay's internal pointer focus is up to date.
    /// Called before constraint activation to ensure focus is synced.
    fn refresh_pointer_focus(&mut self) {
        let pointer = self.ewm.pointer.clone();
        let (x, y) = self.ewm.pointer_location();
        let pos: Point<f64, Logical> = (x, y).into();

        let under = if self.ewm.is_locked() {
            self.ewm
                .lock_surface_focus()
                .map(|s| (s, (0.0, 0.0).into()))
        } else {
            self.ewm.surface_under_point(pos)
        };

        if self.ewm.pointer_focus == under {
            return;
        }

        self.ewm.pointer_focus = under.clone();

        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time: 0,
            },
        );
        pointer.frame(self);
    }

    /// Activate a pointer constraint if the pointer is over a surface that
    /// has a pending constraint and the pointer is within the region.
    fn maybe_activate_pointer_constraint(&self) {
        let Some((ref surface, surface_loc)) = self.ewm.pointer_focus else {
            return;
        };
        let pointer = &self.ewm.pointer;
        if !pointer.current_focus().is_some_and(|s| &s == surface) {
            return;
        }

        with_pointer_constraint(surface, pointer, |constraint| {
            let Some(constraint) = constraint else { return };
            if constraint.is_active() {
                return;
            }
            // Constraint does not apply if not within region.
            if let Some(region) = constraint.region() {
                let pos = Point::from(self.ewm.pointer_location()) - surface_loc;
                if !region.contains(pos.to_i32_round()) {
                    return;
                }
            }
            constraint.activate();
        });
    }

    /// Apply output config and adjust the pointer if the output geometry changed.
    /// Preserves the pointer's relative position within the output (e.g. center
    /// stays center after a scale change).
    fn apply_output_config_for(&mut self, output_name: &str) {
        let pos: Point<f64, Logical> = self.ewm.pointer_location().into();
        let output = self
            .ewm
            .space
            .outputs()
            .find(|o| o.name() == output_name)
            .cloned();
        let old_geo = output
            .as_ref()
            .and_then(|o| self.ewm.space.output_geometry(o));

        self.backend.apply_output_config(&mut self.ewm, output_name);

        if let (Some(ref output), Some(old_geo)) = (&output, old_geo) {
            let point = Point::from((pos.x as i32, pos.y as i32));
            if old_geo.contains(point) {
                if let Some(new_geo) = self.ewm.space.output_geometry(output) {
                    if old_geo != new_geo {
                        let rel_x = (pos.x - old_geo.loc.x as f64) / old_geo.size.w as f64;
                        let rel_y = (pos.y - old_geo.loc.y as f64) / old_geo.size.h as f64;
                        let new_x = new_geo.loc.x as f64 + rel_x * new_geo.size.w as f64;
                        let new_y = new_geo.loc.y as f64 + rel_y * new_geo.size.h as f64;
                        self.warp_pointer(new_x, new_y);
                    }
                }
            }
        }
    }

    /// Center the pointer on the first output. Called once during startup so the
    /// cursor doesn't begin at (0, 0).
    pub fn center_pointer_on_first_output(&mut self) {
        let Some(output) = self.ewm.space.outputs().next().cloned() else {
            return;
        };
        let Some(geo) = self.ewm.space.output_geometry(&output) else {
            return;
        };
        let x = geo.loc.x as f64 + geo.size.w as f64 / 2.0;
        let y = geo.loc.y as f64 + geo.size.h as f64 / 2.0;
        self.warp_pointer(x, y);
    }

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

        // When locked, focus the lock surface
        if self.ewm.is_locked() {
            let new_focus = self.ewm.lock_surface_focus();
            if self.ewm.keyboard_focus != new_focus {
                self.ewm.keyboard_focus = new_focus.clone();
                let keyboard = self.ewm.keyboard.clone();
                keyboard.set_focus(self, new_focus, SERIAL_COUNTER.next_serial());
            }
            return;
        }

        // Clean up stale on-demand focus
        if let Some(surface) = &self.ewm.layer_shell_on_demand_focus {
            let good = surface.alive()
                && surface.cached_state().keyboard_interactivity == KeyboardInteractivity::OnDemand;
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

    /// Drain pending module commands, dispatch them, and sync keyboard focus.
    /// Returns true if any commands were processed.
    fn process_pending_commands(&mut self) -> bool {
        let commands = crate::module::drain_commands();
        if commands.is_empty() {
            return false;
        }
        for cmd in commands {
            self.handle_module_command(cmd);
        }
        self.sync_keyboard_focus();
        true
    }

    /// Per-frame processing callback for the event loop.
    /// Called after each dispatch to handle redraws, events, and client flushing.
    pub fn refresh_and_flush_clients(&mut self) {
        // Check if stop was requested from module (ewm-stop)
        if crate::module::STOP_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            info!("Stop requested from Emacs, shutting down");
            self.ewm.stop();
        }

        self.process_pending_commands();

        // Refresh workspace protocol state (pull model: diff source of truth vs mirrors).
        crate::protocols::workspace::refresh::<State>(
            &mut self.ewm.workspace_state,
            &self.ewm.output_workspaces,
            self.ewm.space.outputs(),
        );

        // Process pending early imports
        let pending_imports: Vec<_> = self.ewm.pending_early_imports.drain(..).collect();
        for surface in pending_imports {
            self.backend.early_import(&surface);
        }

        // Render queued outputs, focused first. Between outputs, check for
        // commands that arrived during render — process them and defer remaining redraws.
        let focused = self.ewm.get_focused_output();
        let mut rendered_focused = false;

        while let Some(output) = self.ewm.next_queued_redraw(focused.as_deref()) {
            let is_focused = focused.as_deref() == Some(output.name().as_str());

            if rendered_focused && !is_focused && self.process_pending_commands() {
                break;
            }

            self.ewm.redraw(&mut self.backend, &output);
            if is_focused {
                rendered_focused = true;
            }
        }

        // Process IM relay events and send to Emacs
        self.process_im_events();

        // Clean up dead elements from space.
        // Output enter/leave is managed explicitly in handle_new_toplevel and
        // the Layout command, not via automatic spatial overlap detection.
        self.ewm.cleanup_dead_windows();

        // Update shared state snapshot for Emacs to read synchronously
        self.update_shared_state();

        // Flush Wayland clients
        if let Err(e) = self.ewm.display_handle.flush_clients() {
            tracing::warn!("Failed to flush Wayland clients: {e}");
        }
    }

    /// Update the shared state snapshot that Emacs reads synchronously.
    /// Called once per tick, after all command processing and state changes.
    fn update_shared_state(&mut self) {
        let mut shared = module::shared_state().lock().unwrap();
        shared.focused_surface_id = self.ewm.focused_surface_id;
        shared.pointer_location = self.ewm.pointer_location();
        if self.ewm.active_outputs_dirty {
            self.ewm.active_outputs_dirty = false;
            // Derive active_outputs from output geometry + working area.
            // Falls back to raw output origin when working_areas not yet populated
            // (before first layer-shell commit).
            shared.active_outputs = self
                .ewm
                .space
                .outputs()
                .filter_map(|o| {
                    let geo = self.ewm.space.output_geometry(o)?;
                    let wa = self.ewm.working_areas.get(&o.name());
                    let x = geo.loc.x + wa.map_or(0, |r| r.loc.x);
                    let y = geo.loc.y + wa.map_or(0, |r| r.loc.y);
                    Some((o.name(), module::ActiveOutput { origin: (x, y) }))
                })
                .collect();
        }
    }

    /// Handle a module command (from Emacs via dynamic module).
    fn handle_module_command(&mut self, cmd: module::ModuleCommand) {
        tracy_span!("handle_module_command");

        use module::ModuleCommand;
        match cmd {
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
                    self.ewm.set_focus(id, false, "emacs_command", None);
                }
            }
            ModuleCommand::WarpPointer { x, y } => {
                self.warp_pointer(x, y);
            }
            ModuleCommand::Screenshot { path } => {
                let target = path.unwrap_or_else(|| "/tmp/ewm-screenshot.png".to_string());
                info!("Screenshot requested: {}", target);
                self.ewm.pending_screenshot = Some(target);
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
                if width.is_some() || height.is_some() || refresh.is_some() {
                    // Fall back to current mode for unspecified parameters
                    let current = self
                        .ewm
                        .space
                        .outputs()
                        .find(|o| o.name() == name)
                        .and_then(|o| o.current_mode());
                    let w = width.unwrap_or_else(|| current.map(|m| m.size.w).unwrap_or(1920));
                    let h = height.unwrap_or_else(|| current.map(|m| m.size.h).unwrap_or(1080));
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
                    config.position =
                        Some((x.unwrap_or(current_pos.0), y.unwrap_or(current_pos.1)));
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

                // Apply the config (adjusts pointer if geometry changed)
                self.apply_output_config_for(&name);
            }
            ModuleCommand::ImCommit { text, surface_id } => {
                use smithay::wayland::text_input::TextInputSeat;
                let text_input = self.ewm.seat.text_input();
                let mut delivered = false;
                text_input.with_active_text_input(|ti, _surface| {
                    ti.commit_string(Some(text.clone()));
                    delivered = true;
                });
                if delivered {
                    text_input.done(false);
                } else {
                    // Client in disable→enable gap — drain on next Activated.
                    debug!("ImCommit: queued (surface_id={surface_id})");
                    self.ewm.pending_im_commits.push(text);
                }
            }
            ModuleCommand::TextInputIntercept { enabled } => {
                if self.ewm.text_input_intercept != enabled {
                    info!("Text input intercept: {}", enabled);
                    self.ewm.text_input_intercept = enabled;
                }
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
            ModuleCommand::GetDebugState => {
                let id_window_keys: Vec<u64> = self.ewm.id_windows.keys().copied().collect();
                let layer_surfaces_info = self.ewm.get_layer_surfaces_info();
                let state = serde_json::json!({
                    "surfaces": self.ewm.surface_info,
                    "emacs_surfaces": self.ewm.emacs_surfaces,
                    "output_layouts": self.ewm.output_layouts,
                    "output_workspaces": self.ewm.output_workspaces,
                    "surface_outputs": self.ewm.surface_outputs,
                    "focused_surface_id": self.ewm.focused_surface_id,
                    "id_windows": id_window_keys,
                    "outputs": self.ewm.outputs,
                    "working_areas": self.ewm.get_working_areas_info(),
                    "layer_surfaces": layer_surfaces_info,
                    "pointer_location": self.ewm.pointer_location(),
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
                    "debug_mode": module::DEBUG_MODE.load(std::sync::atomic::Ordering::Relaxed),
                    "pending_commands": module::peek_commands(),
                    "focus_history": module::get_focus_history(),
                });
                let json = serde_json::to_string_pretty(&state).unwrap_or_default();
                self.ewm.queue_event(Event::DebugState { json });
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
            ModuleCommand::OutputLayout {
                output,
                surfaces,
                tabs,
            } => {
                self.ewm.apply_output_layout(&output, surfaces);
                // Re-evaluate pointer focus after layout change
                let (px, py) = self.ewm.pointer_location();
                let under = self
                    .ewm
                    .surface_under_point(smithay::utils::Point::from((px, py)));
                let pointer = self.ewm.pointer.clone();
                let serial = SERIAL_COUNTER.next_serial();
                pointer.motion(
                    self,
                    under,
                    &smithay::input::pointer::MotionEvent {
                        location: (px, py).into(),
                        serial,
                        time: 0,
                    },
                );
                pointer.frame(self);

                // Store tab state (source of truth for workspace::refresh)
                self.ewm.output_workspaces.insert(output, tabs);
            }
            ModuleCommand::ConfigureInput { configs } => {
                info!("Input config updated: {} entries", configs.len());

                // Extract keyboard settings before storing configs (avoids borrow conflict)
                let kb_repeat = configs
                    .iter()
                    .find(|c| c.device_type == Some(crate::input::DeviceType::Keyboard))
                    .and_then(|kb| {
                        if kb.repeat_rate.is_some() || kb.repeat_delay.is_some() {
                            Some((kb.repeat_rate.unwrap_or(25), kb.repeat_delay.unwrap_or(200)))
                        } else {
                            None
                        }
                    });
                let kb_xkb = configs
                    .iter()
                    .find(|c| c.device_type == Some(crate::input::DeviceType::Keyboard))
                    .and_then(|kb| {
                        kb.xkb_layouts
                            .clone()
                            .map(|layouts| (layouts, kb.xkb_options.clone()))
                    });

                self.ewm.input_configs = configs;
                self.backend
                    .reapply_libinput_config(&self.ewm.input_configs);

                // Apply keyboard repeat settings
                if let Some((rate, delay)) = kb_repeat {
                    let keyboard = self.ewm.keyboard.clone();
                    keyboard.change_repeat_info(rate, delay);
                    info!("Keyboard repeat: rate={}, delay={}", rate, delay);
                }

                // Apply XKB layout settings
                if let Some((layouts_str, options)) = kb_xkb {
                    let layout_names: Vec<String> = layouts_str
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if !layout_names.is_empty() {
                        let xkb_config = smithay::input::keyboard::XkbConfig {
                            layout: &layouts_str,
                            options: options.clone(),
                            ..Default::default()
                        };
                        let keyboard = self.ewm.keyboard.clone();
                        if let Err(e) = keyboard.set_xkb_config(self, xkb_config) {
                            error!("Failed to configure XKB: {:?}", e);
                        } else {
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
                    }
                }
            }
            ModuleCommand::ConfigureIdle {
                timeout_secs,
                action,
            } => {
                let idle_action = if action == "blank" {
                    IdleAction::DeactivateMonitors
                } else {
                    IdleAction::RunCommand(action)
                };
                self.ewm
                    .configure_idle(timeout_secs.map(Duration::from_secs), idle_action);
            }
        }
    }

    /// Read clipboard data from the current client selection and forward to Emacs.
    ///
    /// Deferred to an idle callback because `new_selection()` is called before
    /// Smithay updates `SeatData`, so `request_data_device_client_selection()`
    /// would read the old selection if called synchronously.
    fn read_client_selection_to_emacs(&mut self) {
        self.ewm.loop_handle.insert_idle(|state| {
            let (read_end, write_end) =
                std::os::unix::net::UnixStream::pair().expect("UnixStream::pair failed");

            let write_fd: OwnedFd = write_end.into();
            let mime = "text/plain;charset=utf-8".to_string();
            match request_data_device_client_selection(&state.ewm.seat, mime, write_fd) {
                Ok(()) => {
                    std::thread::spawn(move || {
                        use std::io::Read;
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
        });
    }

    /// Handle lid open/close from libinput switch events.
    ///
    /// Delegates to DrmBackendState::on_lid_state_changed() which disconnects
    /// the laptop panel when closed (if external monitor exists) or re-scans
    /// connectors when opened.
    pub fn handle_lid_state(&mut self, is_closed: bool) {
        if let Some(drm) = self.backend.as_drm_mut() {
            if drm.lid_closed == is_closed {
                return;
            }
            drm.lid_closed = is_closed;
            drm.on_lid_state_changed(&mut self.ewm);
        }
    }

    /// Poll IM relay for activate/deactivate events.
    pub fn process_im_events(&mut self) {
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
                        self.ewm.queue_event(Event::TextInputActivated);
                    }

                    // Drain commits queued during the disable→enable gap.
                    if !self.ewm.pending_im_commits.is_empty() {
                        use smithay::wayland::text_input::TextInputSeat;
                        let text_input = self.ewm.seat.text_input();
                        let text = self.ewm.pending_im_commits.join("");
                        let count = self.ewm.pending_im_commits.len();
                        self.ewm.pending_im_commits.clear();
                        debug!("ImCommit: draining {count} queued commit(s)");
                        text_input.with_active_text_input(|ti, _surface| {
                            ti.commit_string(Some(text.clone()));
                        });
                        text_input.done(false);
                    }
                }
                im_relay::ImEvent::Deactivated => {
                    if self.ewm.text_input_active {
                        self.ewm.text_input_active = false;
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
