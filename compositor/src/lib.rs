//! EWM - Emacs Wayland Manager
//!
//! Wayland compositor core library.
//!
//! # Design Invariants
//!
//! 1. **Focus ownership**: Only one surface has keyboard focus at a time. Focus changes only via
//!    explicit commands from Emacs or validated input events (click-to-focus, XDG activation). The
//!    `focused_surface_id` field is the single source of truth for focus state.
//!
//! 2. **Surface lifecycle**: Surfaces are assigned monotonically increasing IDs starting from 1. ID
//!    0 is reserved for "no surface". IDs are never reused within a session. When a surface is
//!    destroyed, it's removed from all maps and its ID becomes invalid.
//!
//! 3. **Redraw state machine**: Each output has independent redraw state. Two transition paths:
//!    - Real VBlank: `Idle` -> `Queued` -> `WaitingForVBlank` -> `Idle`
//!    - No damage: `Idle` -> `Queued` -> `WaitingForEstimatedVBlank` -> `Idle`
//!
//!    `WaitingForVBlank { redraw_needed }` and `WaitingForEstimatedVBlankAndQueued` handle redraws
//!    requested while waiting. State is owned by `Ewm::output_state`, not the backend.
//!
//! 4. **Emacs ownership**: The compositor runs as a thread within Emacs. Emacs controls window
//!    layout and focus policy. The compositor handles protocol compliance and rendering. This split
//!    means:
//!    - Compositor never initiates focus changes without Emacs consent
//!    - Layout changes come from Emacs via the command queue
//!    - Events flow back to Emacs via the event queue
//!
//! 5. **Thread safety**: Communication between Emacs and compositor uses mutex-protected queues and
//!    atomic flags. The module interface (module.rs) provides the synchronization boundary.

pub mod backend;
pub mod cursor;
#[cfg(feature = "screencast")]
pub mod dbus;
pub mod dbus_screensaver;
pub mod event;
pub mod frame_clock;
pub mod gtk;
pub mod im_relay;
pub mod input;
mod module;
pub mod protocols;
pub mod render;
#[cfg(feature = "screencast")]
pub mod screencasting;
pub mod strip;
pub mod texture;
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
/// Register the server `Display` as a calloop source so inserted clients
/// dispatch on each event-loop tick. Shared by production and test fixtures.
pub fn register_display_source(event_loop: &LoopHandle<State>, display: Display<State>) {
    let source = Generic::new(display, Interest::READ, CalloopMode::Level);
    event_loop
        .insert_source(source, |_, display, state| {
            // SAFETY: the source owns the Display for its lifetime.
            let display = unsafe { display.get_mut() };
            if let Err(e) = display.dispatch_clients(state) {
                tracing::error!("Wayland dispatch error: {e}");
            }
            Ok(PostAction::Continue)
        })
        .expect("failed to insert Display source");
}

pub fn is_laptop_panel(connector_name: &str) -> bool {
    matches!(connector_name.get(..4), Some("eDP-" | "LVDS" | "DSI-"))
}

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use std::time::Duration;

pub use backend::{Backend, DrmBackendState, HeadlessBackend};
pub use event::{Event, OutputInfo, OutputMode};
use serde::{Deserialize, Serialize};
use smithay::backend::renderer::element::solid::SolidColorBuffer;
use smithay::backend::renderer::element::utils::select_dmabuf_feedback;
use smithay::backend::renderer::element::RenderElementStates;
use smithay::desktop::utils::{
    send_dmabuf_feedback_surface_tree, send_frames_surface_tree,
    surface_presentation_feedback_flags_from_states, surface_primary_scanout_output,
    take_presentation_feedback_surface_tree, update_surface_primary_scanout_output,
    OutputPresentationFeedback,
};
use smithay::desktop::{
    find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
    LayerSurface as DesktopLayerSurface, PopupKind, PopupManager, Space, Window, WindowSurfaceType,
};
use smithay::input::dnd::{self, DnDGrab, DndGrabHandler, DndTarget};
use smithay::input::keyboard::xkb::keysyms;
use smithay::input::keyboard::{KeyboardHandle, ModifiersState};
use smithay::input::pointer::{CursorImageStatus, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::Timer;
use smithay::reexports::calloop::{
    Interest, LoopHandle, LoopSignal, Mode as CalloopMode, PostAction, RegistrationToken,
};
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle, Resource};
use smithay::utils::{IsAlive, Logical, Point, Rectangle, Size, Transform, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, is_sync_subsurface, with_surface_tree_downward, CompositorClientState,
    CompositorHandler, CompositorState, SurfaceData, TraversalAction,
};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState};
use smithay::wayland::idle_inhibit::{IdleInhibitHandler, IdleInhibitManagerState};
use smithay::wayland::idle_notify::{IdleNotifierHandler, IdleNotifierState};
use smithay::wayland::input_method::{
    InputMethodHandler, InputMethodManagerState, PopupSurface as IMPopupSurface,
};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::pointer_constraints::{
    with_pointer_constraint, PointerConstraintsHandler, PointerConstraintsState,
};
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::data_device::{
    request_data_device_client_selection, set_data_device_focus, set_data_device_selection,
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{
    set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::selection::wlr_data_control::{DataControlHandler, DataControlState};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};
use smithay::wayland::shell::wlr_layer::{Layer, WlrLayerShellHandler, WlrLayerShellState};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::wayland::xdg_foreign::{XdgForeignHandler, XdgForeignState};
use smithay::{
    delegate_compositor, delegate_data_control, delegate_data_device, delegate_dmabuf,
    delegate_fractional_scale, delegate_idle_inhibit, delegate_idle_notify,
    delegate_input_method_manager, delegate_layer_shell, delegate_output,
    delegate_pointer_constraints, delegate_primary_selection, delegate_relative_pointer,
    delegate_seat, delegate_session_lock, delegate_shm, delegate_text_input_manager,
    delegate_viewporter, delegate_virtual_keyboard_manager, delegate_xdg_activation,
    delegate_xdg_foreign, delegate_xdg_shell,
};
use tracing::{debug, error, info, warn};

use crate::protocols::foreign_toplevel::{
    ForeignToplevelHandler, ForeignToplevelManagerState, WindowInfo,
};
use crate::protocols::output_management::{OutputManagementHandler, OutputManagementState};
use crate::protocols::screencopy::{Screencopy, ScreencopyHandler, ScreencopyManagerState};
use crate::protocols::workspace::{WorkspaceHandler, WorkspaceManagerState};

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
#[derive(Default)]
pub enum LockState {
    /// Session is not locked
    #[default]
    Unlocked,
    /// Lock requested, waiting for all outputs to render locked frame
    Locking(SessionLocker),
    /// Session is fully locked (stores the lock object to detect dead clients)
    Locked(ExtSessionLockV1),
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

/// State for cursor auto-hide on inactivity and hide-on-typing.
///
/// Kept separate from `cursor_image_status` so client cursor requests are
/// preserved across hide/show transitions.
pub struct CursorHideState {
    pub timeout: Option<Duration>,
    pub timer_token: Option<RegistrationToken>,
    pub is_hidden: bool,
    pub last_activity: std::time::Instant,
    /// Hide the cursor immediately on key press; pointer motion restores it.
    pub hide_when_typing: bool,
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
/// get one regardless of the throttling state, as a fallback.
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

/// Cached surface info for change detection
#[derive(Clone, Default, Serialize)]
struct SurfaceInfo {
    app_id: String,
    title: String,
}

/// An entry in a per-output declarative layout.
/// Coordinates are relative to the output's working area (frame-relative).
pub type ViewId = std::num::NonZeroU64;

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct LayoutEntry {
    /// View identity assigned by lisp; one surface in N windows yields N view_ids.
    pub view_id: ViewId,
    /// Which Wayland surface this entry displays (analogous to an Emacs buffer).
    #[serde(rename = "id")]
    pub surface_id: u64,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Largest view per surface, computed by compositor from entry dimensions.
    /// Drives send_configure() size + scale and native-size rendering.
    pub primary: bool,
    /// Fullscreen mode. Surface covers full output, bypassing working area.
    #[serde(default)]
    pub fullscreen: bool,
}

struct LayoutEntryHit<'a> {
    entry: &'a LayoutEntry,
    rect: Rectangle<i32, Logical>,
    output_size: Size<i32, Logical>,
}

type SurfaceHit = (WlSurface, Point<f64, Logical>);

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

/// Pick the keyboard focus: lock surface > layer > toplevel.
pub fn resolve_keyboard_focus_target<S, F>(
    locked: bool,
    lock_surface: Option<S>,
    layer_focus: Option<S>,
    toplevel_focus: F,
) -> Option<S>
where
    F: FnOnce() -> Option<S>,
{
    if locked {
        return lock_surface;
    }
    if layer_focus.is_some() {
        return layer_focus;
    }
    toplevel_focus()
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
    /// modifiers (e.g. Shift+`;` -> `colon`).
    ///
    /// We try `raw_keysym` first for layout-independent matching, then
    /// fall back to `modified_keysym` for shifted punctuation (`:`, `&`,
    /// etc.) where Emacs encodes the character directly rather than
    /// decomposing it into base-key + Shift.
    pub fn matches(&self, raw_keysym: u32, modified_keysym: u32, mods: &ModifiersState) -> bool {
        key_binding_matches(
            self.keysym,
            self.ctrl,
            self.alt,
            self.shift,
            self.logo,
            raw_keysym,
            modified_keysym,
            mods,
        )
    }
}

fn key_binding_matches(
    keysym: u32,
    ctrl: bool,
    alt: bool,
    shift: bool,
    logo: bool,
    raw_keysym: u32,
    modified_keysym: u32,
    mods: &ModifiersState,
) -> bool {
    let raw_match = keysym == raw_keysym
        || ((keysyms::KEY_A..=keysyms::KEY_Z).contains(&raw_keysym)
            && keysym == raw_keysym - keysyms::KEY_A + keysyms::KEY_a);

    if raw_match {
        return ctrl == mods.ctrl
            && alt == mods.alt
            && (shift == mods.shift || (keysyms::KEY_A..=keysyms::KEY_Z).contains(&raw_keysym))
            && logo == mods.logo;
    }

    if raw_keysym != modified_keysym && keysym == modified_keysym {
        return ctrl == mods.ctrl && alt == mods.alt && logo == mods.logo;
    }

    false
}

/// Emulated key: a key combo that is translated to different modifiers
/// when forwarded to non-Emacs surfaces.
///
/// Example: Super+c -> Ctrl+c (universal clipboard copy for Wayland clients).
/// When Emacs has focus, the original key works normally via `ewm-mode-map`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EmulatedKey {
    pub keysym: u32,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(rename = "super", default)]
    pub logo: bool,
    /// Target modifier to substitute (e.g. "ctrl" replaces the source Super)
    pub target_mod: String,
}

impl EmulatedKey {
    /// Check if this emulated key matches the given keysym and modifiers.
    /// Uses the same matching logic as `InterceptedKey::matches`.
    pub fn matches(&self, raw_keysym: u32, modified_keysym: u32, mods: &ModifiersState) -> bool {
        key_binding_matches(
            self.keysym,
            self.ctrl,
            self.alt,
            self.shift,
            self.logo,
            raw_keysym,
            modified_keysym,
            mods,
        )
    }

    /// Build the target ModifiersState: source modifiers with logo replaced by target_mod.
    pub(crate) fn target_modifiers_for(
        current: &ModifiersState,
        target_mod: &str,
    ) -> ModifiersState {
        let mut mods = *current;
        // Remove logo (the source modifier being replaced)
        mods.logo = false;
        // Add target modifier
        match target_mod {
            "ctrl" => mods.ctrl = true,
            "alt" => mods.alt = true,
            "shift" => mods.shift = true,
            "super" => mods.logo = true,
            _ => {}
        }
        mods
    }

    pub fn target_modifiers(&self, current: &ModifiersState) -> ModifiersState {
        Self::target_modifiers_for(current, &self.target_mod)
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
    /// Per-output strip state from Emacs: surfaces nest inside frames inside the strip.
    /// This is the single source of truth for what's visible on each output.
    pub output_strips: HashMap<String, strip::Strip>,
    /// Shared "complete instantly" toggle threaded into every `ViewAnimation`
    /// constructed under this `Ewm`.  Flipped by `ConfigureAnimations`.
    pub animations_clock: strip::AnimationsClock,
    /// Reverse index: surface_id -> set of output names it appears on
    pub surface_outputs: HashMap<u64, HashSet<String>>,
    /// Per-surface open-fade, seeded on the first buffer-bearing commit.
    /// Entries GC'd in `advance_animations` once finished or surface gone.
    pub surface_open_anims: HashMap<u64, strip::ViewAnimation>,
    /// Toplevels that haven't yet committed a buffer. One-shot gate for
    /// seeding `surface_open_anims`: drained on the unmapped->mapped
    /// transition so the fade starts exactly once per surface lifetime.
    pub unmapped_surfaces: HashSet<u64>,

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
    pub focused_frame_id: u64,
    pub focused_view_id: Option<ViewId>,
    pub keyboard_focus: Option<WlSurface>,
    /// Output geometry or working areas changed; shared active_outputs need recomputation.
    pub active_outputs_dirty: bool,
    /// Focus follows mouse
    pub focus_follows_mouse_mode: bool,
    /// Alpha multiplier for inactive layout entries; 1.0 disables.
    pub unfocused_alpha: f32,
    /// UID of the layout entry currently under the pointer (for FFM view detection).
    pub pointer_view_id: Option<ViewId>,

    // Libinput device configuration
    pub input_configs: Vec<input::InputConfigEntry>,

    // PID for matching Emacs-owned wl_surfaces in `new_toplevel` (vs external
    // applications); is-this-an-emacs-frame queries derive from strip membership.
    pub emacs_pid: Option<u32>,

    /// In-flight 3-finger swipe gesture for strip scrolling.
    pub gesture: input::GestureState,

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

    // Virtual keyboard state (zwp-virtual-keyboard-v1 protocol)
    #[allow(dead_code)]
    pub virtual_keyboard_state: VirtualKeyboardManagerState,

    // When true, intercept all keys and send to Emacs for text input
    pub text_input_intercept: bool,
    // Deduplicate IM relay activate/deactivate events
    pub text_input_active: bool,
    // Commits queued during the client's disable->enable gap
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

    pub xdg_foreign_state: XdgForeignState,

    // Foreign toplevel state (exposes windows to external tools)
    pub foreign_toplevel_state: ForeignToplevelManagerState,

    // Workspace state (ext-workspace-v1: exposes Emacs frames to external tools)
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

    // Idle inhibit state (zwp-idle-inhibit-v1 protocol)
    pub idle_inhibit_manager_state: IdleInhibitManagerState,
    /// Surfaces that have requested idle inhibition via the Wayland protocol.
    /// Only visible surfaces actually inhibit idle (checked in refresh_idle_inhibit).
    pub idle_inhibiting_surfaces: std::collections::HashSet<WlSurface>,

    /// Whether the freedesktop ScreenSaver D-Bus interface reports inhibition.
    /// Shared with the D-Bus thread via Arc<AtomicBool>.
    pub is_fdo_idle_inhibited: std::sync::Arc<std::sync::atomic::AtomicBool>,

    /// Whether idle is currently inhibited (cached from last refresh_idle_inhibit).
    pub idle_is_inhibited: bool,

    /// Manually inhibited from Emacs (e.g., `ewm-idle-inhibit-mode').
    pub manual_idle_inhibited: bool,

    // D-Bus ScreenSaver connection (must be kept alive for the interface to work)
    pub dbus_screensaver_conn: Option<zbus::blocking::Connection>,

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

    // Screencasting state: PipeWire connection, active casts,
    // window->output cache.
    #[cfg(feature = "screencast")]
    pub casting: screencasting::Screencasting,

    // Shared output info for D-Bus ScreenCast (thread-safe)
    #[cfg(feature = "screencast")]
    pub dbus_outputs: std::sync::Arc<std::sync::Mutex<Vec<dbus::OutputInfo>>>,

    // D-Bus servers (must be kept alive for interfaces to work)
    #[cfg(feature = "screencast")]
    pub dbus_servers: Option<dbus::DBusServers>,

    // Channel to reply to Introspect GetWindows requests
    #[cfg(feature = "screencast")]
    pub introspect_reply_tx: Option<async_channel::Sender<dbus::CompositorToIntrospect>>,

    // XKB layout state
    pub xkb_layout_names: Vec<String>,
    pub xkb_current_layout: usize,

    /// Keycodes currently being emulated. Their release must also be emulated.
    /// Maps keycode -> target modifier name (e.g. "ctrl").
    pub emulated_keycodes: HashMap<u32, String>,

    // IM relay keepalive. Smithay requires an input_method_v2 instance
    // for text_input_v3. Commits bypass the relay (direct TextInputHandle).
    pub im_relay: Option<im_relay::ImRelay>,

    // Event loop handle (for idle timer registration)
    pub loop_handle: LoopHandle<'static, State>,

    /// Worker-thread sender that routes events back into the main loop.
    pub worker_event_tx: smithay::reexports::calloop::channel::Sender<Event>,

    // xwayland-satellite (on-demand X11 support)
    pub satellite: Option<xwayland::satellite::Satellite>,

    // Native idle timeout state
    pub idle_timeout: IdleTimeoutState,

    // Auto-hide cursor on pointer inactivity
    pub cursor_hide: CursorHideState,

    /// Whether the (idle notifier) activity was notified this event loop iteration.
    ///
    /// Used for limiting the notify to once per iteration, so that it's not spammed with high
    /// resolution mice.
    pub notified_activity_this_iteration: bool,

    // Client cursor image status (Hidden/Surface/Named)
    pub cursor_image_status: CursorImageStatus,

    /// When `Some`, `queue_event` mirrors each event here.
    pub captured_events: Option<Vec<Event>>,
}

#[derive(Debug)]
struct FrameMigration {
    id: u64,
    prev_output: Option<String>,
}

#[derive(Debug)]
struct OutputLayoutSnapshot {
    output_name: String,
    active_frame_id: Option<u64>,
    entry_surface_ids: HashSet<u64>,
}

#[derive(Debug)]
struct ViewLocation {
    output: String,
    frame_idx: usize,
    frame_surface_id: u64,
    surface_id: u64,
}

#[derive(Debug, Clone, Copy)]
struct LayoutEntryLocation<'a> {
    output_name: &'a str,
    frame_idx: usize,
    entry: &'a LayoutEntry,
}

#[derive(Debug, Clone, Copy)]
struct FrameLocation<'a> {
    output_name: &'a str,
    frame_idx: usize,
    strip: &'a strip::Strip,
    frame: &'a strip::Frame,
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

        // Initialize virtual keyboard (zwp-virtual-keyboard-v1)
        let virtual_keyboard_state =
            VirtualKeyboardManagerState::new::<State, _>(&display_handle, |_| true);

        // Initialize layer shell for panels, notifications, etc.
        let layer_shell_state = WlrLayerShellState::new::<State>(&display_handle);

        // Initialize xdg-activation for focus requests
        let activation_state = XdgActivationState::new::<State>(&display_handle);

        let xdg_foreign_state = XdgForeignState::new::<State>(&display_handle);

        // Initialize foreign toplevel management (exposes windows to external tools)
        let foreign_toplevel_state =
            ForeignToplevelManagerState::new::<State, _>(&display_handle, |_| true);

        // Initialize workspace management (ext-workspace-v1: Emacs frames as workspaces)
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

        // Initialize idle inhibit manager (zwp-idle-inhibit-v1)
        let idle_inhibit_manager_state = IdleInhibitManagerState::new::<State>(&display_handle);

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

        let (worker_event_tx, worker_event_rx) =
            smithay::reexports::calloop::channel::channel::<Event>();
        loop_handle_clone
            .insert_source(worker_event_rx, move |event, _, state| match event {
                smithay::reexports::calloop::channel::Event::Msg(msg) => {
                    state.ewm.queue_event(msg);
                }
                smithay::reexports::calloop::channel::Event::Closed => (),
            })
            .expect("failed to register worker event source");

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
            output_strips: HashMap::new(),
            animations_clock: strip::AnimationsClock::default(),
            surface_outputs: HashMap::new(),
            surface_open_anims: HashMap::new(),
            unmapped_surfaces: HashSet::new(),
            output_size: Size::from((0, 0)),
            outputs: Vec::new(),
            output_config: HashMap::new(),
            pointer_focus: None,
            pointer_output: None,
            focused_frame_id: 0,
            focused_view_id: None,
            keyboard_focus: None,
            active_outputs_dirty: true,
            focus_follows_mouse_mode: false,
            unfocused_alpha: 1.0,
            pointer_view_id: None,
            input_configs: Vec::new(),
            emacs_pid: None,
            gesture: input::GestureState::default(),
            pending_screenshot: None,
            output_state: HashMap::new(),
            monitors_active: true,
            pending_early_imports: Vec::new(),
            screencopy_state,
            output_manager_state,
            text_input_state,
            input_method_state,
            virtual_keyboard_state,
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
            xdg_foreign_state,
            foreign_toplevel_state,
            workspace_state,
            output_management_state,
            session_lock_state,
            lock_state: LockState::Unlocked,
            pre_lock_focus: None,
            idle_notifier_state,
            idle_inhibit_manager_state,
            idle_inhibiting_surfaces: std::collections::HashSet::new(),
            is_fdo_idle_inhibited: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            idle_is_inhibited: false,
            manual_idle_inhibited: false,
            dbus_screensaver_conn: None,
            gamma_control_state,
            fractional_scale_state,
            viewporter_state,
            presentation_state,
            relative_pointer_state,
            pointer_constraints_state,
            #[cfg(feature = "screencast")]
            casting: screencasting::Screencasting::new(&loop_handle_clone),
            #[cfg(feature = "screencast")]
            dbus_outputs: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(feature = "screencast")]
            dbus_servers: None,
            #[cfg(feature = "screencast")]
            introspect_reply_tx: None,
            xkb_layout_names: vec!["us".to_string()],
            xkb_current_layout: 0,
            emulated_keycodes: HashMap::new(),
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
            cursor_hide: CursorHideState {
                timeout: None,
                timer_token: None,
                is_hidden: false,
                last_activity: std::time::Instant::now(),
                hide_when_typing: false,
            },
            notified_activity_this_iteration: false,
            cursor_image_status: CursorImageStatus::default_named(),
            captured_events: None,
            worker_event_tx,
        }
    }

    /// Connect the input method relay after socket is ready.
    pub fn connect_im_relay(&mut self, socket_path: &std::path::Path) {
        self.im_relay = Some(im_relay::ImRelay::connect(socket_path));
    }

    /// Remove a surface from all output strips and the reverse index.
    fn remove_surface_from_layouts(&mut self, id: u64) {
        self.surface_outputs.remove(&id);
        for strip in self.output_strips.values_mut() {
            for frame in &mut strip.frames {
                frame.surfaces.retain(|e| e.surface_id != id);
            }
        }
    }

    /// Restore the focus invariants after a layout swap or surface destruction:
    /// drop stale `selected_view_id` / `focused_view_id`, and zero
    /// `focused_frame_id` when its frame no longer exists.
    fn prune_dangling_focus(&mut self) {
        for strip in self.output_strips.values_mut() {
            for frame in &mut strip.frames {
                if let Some(sel) = frame.selected_view_id {
                    if !frame.surfaces.iter().any(|e| e.view_id == sel) {
                        frame.selected_view_id = None;
                    }
                }
            }
        }
        if let Some(v) = self.focused_view_id {
            let in_focused_frame = self
                .frame_location(self.focused_frame_id)
                .map(|location| location.frame)
                .is_some_and(|f| f.surfaces.iter().any(|e| e.view_id == v));
            if !in_focused_frame {
                self.focused_view_id = None;
            }
        }
        if self.focused_frame_id != 0 && self.lookup_frame(self.focused_frame_id).is_none() {
            self.focused_frame_id = 0;
            self.focused_view_id = None;
        }
    }

    /// If no frame is focused but some frame exists, focus an arbitrary one.
    fn focus_any_frame_if_unfocused(&mut self, reason: &'static str) {
        if self.focused_frame_id == 0 {
            if let Some(id) = self
                .output_strips
                .values()
                .flat_map(|s| s.frames.iter())
                .next()
                .map(|f| f.surface_id)
            {
                self.set_focus_frame(id, reason, false);
            }
        }
    }

    /// Drop the Emacs frame `id` from whichever strip holds it.
    fn remove_emacs_frame_from_strip(&mut self, id: u64, active_close: bool) {
        let Some((out, idx)) = self.lookup_frame(id) else {
            return;
        };
        let Some(strip) = self.output_strips.get_mut(&out) else {
            return;
        };
        let prev_active_id = strip.active_frame().map(|f| f.surface_id);
        if active_close {
            strip.remove_active_frame_at(idx);
        } else {
            strip.remove_frame_at(idx);
        }
        self.reconcile_active_for_output(&out, prev_active_id);
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
            let sessions_to_stop: Vec<screencasting::CastSessionId> = self
                .casting
                .casts
                .iter()
                .filter(|cast| cast.target == target)
                .map(|cast| cast.session_id)
                .collect();
            for session_id in sessions_to_stop {
                info!(output = %output_name, %session_id, "stopping cast due to output disconnect");
                self.stop_cast(session_id);
            }
        }

        // Reconcile before `unmap_output` so the wl_output lookup hits.
        let evicted_active_id = if let Some(old_strip) = self.output_strips.remove(&output_name) {
            let was_active = old_strip.active_frame().map(|f| f.surface_id);
            let pre: HashSet<u64> = old_strip.surfaces().map(|e| e.surface_id).collect();
            self.reconcile_output_entries(&output_name, &pre);
            was_active
        } else {
            None
        };
        self.space.unmap_output(output);
        // No reconcile runs for the evicted strip; freeze its active frame.
        if let Some(id) = evicted_active_id {
            self.queue_event(Event::FrameHidden { id });
        }
        self.working_areas.remove(&output_name);

        // Remove from outputs list
        self.outputs.retain(|o| o.name != output_name);

        // Remove from D-Bus outputs
        #[cfg(feature = "screencast")]
        {
            let mut dbus_outputs = self.dbus_outputs.lock().unwrap();
            dbus_outputs.retain(|o| o.name != output_name);
        }

        // Prune dangling focus from the gone strip and hop to a survivor; I5.
        self.prune_dangling_focus();
        self.focus_any_frame_if_unfocused("output_removed");

        self.recalculate_output_size();
        self.send_output_disconnected(&output_name);
        self.output_management_state.output_heads_changed = true;

        info!("Output removed: {}", output_name);
    }

    /// Remove dead windows from the space.
    /// This replaces Space::refresh(). We manage output enter/leave explicitly
    /// rather than relying on automatic spatial overlap detection.
    pub fn cleanup_dead_windows(&mut self) {
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
            let _ = module::take_pending_active_frame_close(id);
            self.remove_surface_from_layouts(id);
            self.remove_emacs_frame_from_strip(id, false);
            self.surface_info.remove(&id);
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

    /// Resolve an embedded view id to its strip location and owning frame.
    fn lookup_view(&self, view_id: ViewId) -> Option<ViewLocation> {
        for (output, strip) in &self.output_strips {
            for (idx, frame) in strip.frames.iter().enumerate() {
                if let Some(entry) = frame.surfaces.iter().find(|e| e.view_id == view_id) {
                    return Some(ViewLocation {
                        output: output.clone(),
                        frame_idx: idx,
                        frame_surface_id: frame.surface_id,
                        surface_id: entry.surface_id,
                    });
                }
            }
        }
        None
    }

    fn frame_location(&self, frame_surface_id: u64) -> Option<FrameLocation<'_>> {
        for (output_name, strip) in &self.output_strips {
            if let Some((frame_idx, frame)) = strip
                .frames
                .iter()
                .enumerate()
                .find(|(_, frame)| frame.surface_id == frame_surface_id)
            {
                return Some(FrameLocation {
                    output_name,
                    frame_idx,
                    strip,
                    frame,
                });
            }
        }
        None
    }

    pub fn is_emacs_frame(&self, id: u64) -> bool {
        self.frame_location(id).is_some()
    }

    pub fn emacs_frame_output(&self, id: u64) -> Option<&str> {
        self.frame_location(id).map(|location| location.output_name)
    }

    pub fn emacs_frames_on<'a>(&'a self, output_name: &'a str) -> impl Iterator<Item = u64> + 'a {
        self.output_strips
            .get(output_name)
            .into_iter()
            .flat_map(|s| s.frames.iter().map(|f| f.surface_id))
            .filter(|&id| id != 0)
    }

    /// Surface id receiving keyboard input.
    pub fn focused_surface_id(&self) -> u64 {
        if let Some(view_id) = self.focused_view_id {
            self.lookup_view(view_id)
                .map(|location| location.surface_id)
                .unwrap_or(self.focused_frame_id)
        } else {
            self.focused_frame_id
        }
    }

    fn set_focus_state(&mut self, frame_id: u64, view_id: Option<ViewId>) {
        self.focused_frame_id = frame_id;
        self.focused_view_id = view_id;
    }

    /// Park focus on an Emacs frame's chrome during an intercepted prefix key;
    /// bypasses `set_focus_frame`'s view derivation.  Restored on the next layout
    /// refresh after `ewm-input--clear-prefix`.
    pub fn enter_prefix_redirect(&mut self, emacs_id: u64) {
        let Some((output, idx)) = self.lookup_frame(emacs_id) else {
            return;
        };
        self.set_focus_state(emacs_id, None);
        self.activate_frame(&output, idx);
    }

    /// Resolve a frame's xdg_toplevel `surface_id` to `(output_name, frame_idx)`.
    pub fn lookup_frame(&self, frame_surface_id: u64) -> Option<(String, usize)> {
        self.frame_location(frame_surface_id)
            .map(|location| (location.output_name.to_string(), location.frame_idx))
    }

    /// Frame surface_id whose `view_id` matches.
    pub fn frame_with_view_id(&self, view_id: ViewId) -> Option<u64> {
        self.output_strips
            .values()
            .flat_map(|s| s.frames.iter())
            .find(|f| f.view_id == view_id)
            .map(|f| f.surface_id)
    }

    /// View id assigned to the frame with this surface id.
    pub fn view_id_of_frame(&self, frame_surface_id: u64) -> Option<ViewId> {
        self.frame_location(frame_surface_id)
            .map(|location| location.frame.view_id)
    }

    /// Focus the view identified by `view_id`.
    pub fn set_focus_view(&mut self, view_id: ViewId, source: &str, notify_emacs: bool) {
        let Some(location) = self.lookup_view(view_id) else {
            return;
        };
        self.set_focus_view_at(view_id, location, source, notify_emacs);
    }

    fn set_focus_view_at(
        &mut self,
        view_id: ViewId,
        location: ViewLocation,
        source: &str,
        notify_emacs: bool,
    ) {
        module::record_focus(location.surface_id, source, None);
        self.set_focus_state(location.frame_surface_id, Some(view_id));
        self.activate_frame(&location.output, location.frame_idx);
        if notify_emacs {
            self.queue_event(Event::Focus {
                view_id,
                x: None,
                y: None,
            });
        }
    }

    /// Focus an Emacs frame, routing keyboard to its selected surface if one exists.
    pub fn set_focus_frame(&mut self, frame_surface_id: u64, source: &str, notify_emacs: bool) {
        let Some(location) = self.frame_location(frame_surface_id) else {
            return;
        };
        let output = location.output_name.to_string();
        let idx = location.frame_idx;
        let frame_view_id = location.frame.view_id;
        let selected_view_id = location.frame.selected_view_id;
        module::record_focus(frame_surface_id, source, None);
        self.set_focus_state(frame_surface_id, selected_view_id);
        self.activate_frame(&output, idx);
        if notify_emacs {
            self.queue_event(Event::Focus {
                view_id: frame_view_id,
                x: None,
                y: None,
            });
        }
    }

    /// Surface id of the Emacs frame whose strip-frame currently hosts
    /// `surface_id`, or `None` if the surface is not under any frame.
    pub fn frame_for_surface(&self, surface_id: u64) -> Option<u64> {
        self.frame_entry_for_surface(surface_id)
            .map(|(frame_id, _)| frame_id)
    }

    fn frame_entry_for_surface(&self, surface_id: u64) -> Option<(u64, ViewId)> {
        self.output_strips
            .values()
            .flat_map(|s| s.frames.iter())
            .find_map(|frame| {
                frame
                    .surfaces
                    .iter()
                    .find(|entry| entry.surface_id == surface_id)
                    .map(|entry| (frame.surface_id, entry.view_id))
            })
    }

    /// `(output_name, frame_idx)` of the strip frame containing `surface_id`,
    /// either as the frame's own xdg_toplevel or in its layout entries.
    pub fn frame_position(&self, surface_id: u64) -> Option<(String, usize)> {
        // Frame surface match is unique.
        if let Some(location) = self.lookup_frame(surface_id) {
            return Some(location);
        }
        // For multi-view surfaces, prefer the focused output's active frame so remote
        // focus requests (xdg_activation, foreign_toplevel, unlock) don't jump outputs.
        if let Some(out) = self.get_focused_output() {
            if let Some(strip) = self.output_strips.get(&out) {
                let idx = strip.active_idx();
                if strip
                    .frames
                    .get(idx)
                    .is_some_and(|f| f.surfaces.iter().any(|e| e.surface_id == surface_id))
                {
                    return Some((out, idx));
                }
            }
        }
        // Fall back to any frame containing the entry.
        for (name, strip) in &self.output_strips {
            for (i, frame) in strip.frames.iter().enumerate() {
                if frame.surfaces.iter().any(|e| e.surface_id == surface_id) {
                    return Some((name.clone(), i));
                }
            }
        }
        None
    }

    /// Surface id of the active Emacs frame on `output_name`.
    pub fn active_frame_surface_id(&self, output_name: &str) -> Option<u64> {
        let strip = self.output_strips.get(output_name)?;
        strip.active_frame().map(|f| f.surface_id)
    }

    fn first_active_frame_surface_id(&self) -> Option<u64> {
        self.output_strips
            .keys()
            .find_map(|name| self.active_frame_surface_id(name))
    }

    /// Mark `new_idx` as active on `output_name`.
    pub fn activate_frame(&mut self, output_name: &str, new_idx: usize) {
        let Some(strip) = self.output_strips.get_mut(output_name) else {
            return;
        };
        let prev_idx = strip.active_idx();
        let prev_active_id = strip.active_frame().map(|f| f.surface_id);
        let frames_len = strip.frames.len();
        let prev_offset = strip.view_offset.current();
        strip.set_active(new_idx);
        let active_changed = strip.active_idx() != prev_idx;
        if module::DEBUG_MODE.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::debug!(
                "activate_frame: {} {} -> {} (frames={}) view_offset {:.1} -> {:.1}",
                output_name,
                prev_idx,
                strip.active_idx(),
                frames_len,
                prev_offset,
                strip.view_offset.current(),
            );
        }

        // Active-idx flip may shift a surface's largest visible entry to/from this
        // output, changing its size and scale.
        if active_changed {
            self.reconcile_active_for_output(output_name, prev_active_id);
            self.refresh_primary_assignments();
            self.queue_redraw_all();
            return;
        }
        let output = self
            .space
            .outputs()
            .find(|o| o.name() == output_name)
            .cloned();
        if let Some(output) = output {
            self.queue_redraw(&output);
        }
    }

    /// Global logical top-left of an Emacs frame's working area,
    /// driven by the strip column (frames aren't placed in `space`).
    pub fn emacs_frame_origin(
        &self,
        surface_id: u64,
    ) -> Option<smithay::utils::Point<i32, smithay::utils::Logical>> {
        let location = self.frame_location(surface_id)?;
        let output = self
            .space
            .outputs()
            .find(|o| o.name() == location.output_name)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);

        Some(
            Self::frame_rect(
                output_geo,
                working_area,
                location.strip,
                location.frame_idx,
                location.frame,
            )
            .loc,
        )
    }

    fn frame_rect(
        output_geo: Rectangle<i32, Logical>,
        working_area: Rectangle<i32, Logical>,
        strip: &strip::Strip,
        frame_idx: usize,
        frame: &strip::Frame,
    ) -> Rectangle<i32, Logical> {
        let frame_dx = strip.screen_x_of_frame(frame_idx) as i32;
        Rectangle::new(
            Point::from((
                output_geo.loc.x + working_area.loc.x + frame_dx,
                output_geo.loc.y + working_area.loc.y,
            )),
            Size::from((frame.width as i32, working_area.size.h)),
        )
    }

    /// Find the focused layout entry for a surface.
    ///
    /// The frame index feeds `Strip::screen_x_of_frame` so callers can apply
    /// the per-frame slide offset when computing global positions.
    fn focused_entry_for_surface(&self, id: u64) -> Option<LayoutEntryLocation<'_>> {
        let focused = self.focused_view_id?;
        self.layout_entry_for_surface(id, |entry| entry.view_id == focused)
    }

    fn layout_entry_for_surface(
        &self,
        id: u64,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryLocation<'_>> {
        for (output_name, strip) in &self.output_strips {
            if let Some((frame_idx, entry)) = strip
                .surfaces_with_frame()
                .find(|(_, entry)| entry.surface_id == id && predicate(entry))
            {
                return Some(LayoutEntryLocation {
                    output_name: output_name.as_str(),
                    frame_idx,
                    entry,
                });
            }
        }
        None
    }

    /// Find the primary output layout entry for a surface.
    /// Used as fallback when no focused entry exists (e.g. popup positioning).
    fn primary_entry_for_surface(&self, id: u64) -> Option<LayoutEntryLocation<'_>> {
        self.layout_entry_for_surface(id, |entry| entry.primary)
    }

    /// Layout entry used for positioning a multi-view surface.
    fn preferred_entry_for_surface(&self, id: u64) -> Option<LayoutEntryLocation<'_>> {
        self.focused_entry_for_surface(id)
            .or_else(|| self.primary_entry_for_surface(id))
    }

    /// Global position of a window: layout entries use the focused-or-primary
    /// entry's screen rect, Emacs frames use `emacs_frame_origin`.
    pub fn window_global_position(
        &self,
        window: &Window,
    ) -> Option<smithay::utils::Point<i32, smithay::utils::Logical>> {
        let id = self.window_ids.get(window).copied()?;
        if !self.surface_outputs.contains_key(&id) {
            return self.emacs_frame_origin(id);
        }
        let location = self.preferred_entry_for_surface(id)?;
        self.entry_origin(location)
    }

    /// On-screen rectangle of one layout view. Fullscreen-primary centers on
    /// the toplevel's geometry.
    fn layout_entry_rect(
        &self,
        output_geo: Rectangle<i32, Logical>,
        working_area: Rectangle<i32, Logical>,
        strip: &strip::Strip,
        frame_idx: usize,
        entry: &LayoutEntry,
    ) -> Rectangle<i32, Logical> {
        if entry.fullscreen {
            if entry.primary {
                let window_size = self
                    .id_windows
                    .get(&entry.surface_id)
                    .map(|w| w.geometry().size)
                    .unwrap_or_default();
                let (ox, oy) = fullscreen_center_offset(window_size, output_geo.size);
                return Rectangle::new(
                    Point::from((output_geo.loc.x + ox, output_geo.loc.y + oy)),
                    window_size,
                );
            }

            return Rectangle::new(
                Point::from((output_geo.loc.x, output_geo.loc.y)),
                Size::from((entry.w as i32, entry.h as i32)),
            );
        }

        let frame_dx = strip.screen_x_of_frame(frame_idx) as i32;
        Rectangle::new(
            Point::from((
                output_geo.loc.x + working_area.loc.x + frame_dx + entry.x,
                output_geo.loc.y + working_area.loc.y + entry.y,
            )),
            Size::from((entry.w as i32, entry.h as i32)),
        )
    }

    fn entry_origin(&self, location: LayoutEntryLocation<'_>) -> Option<Point<i32, Logical>> {
        let output = self
            .space
            .outputs()
            .find(|o| o.name() == location.output_name)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);
        let strip = self.output_strips.get(location.output_name)?;
        Some(
            self.layout_entry_rect(
                output_geo,
                working_area,
                strip,
                location.frame_idx,
                location.entry,
            )
            .loc,
        )
    }

    /// On-screen origins of every layout view of `surface_id`, one per entry.
    pub fn surface_view_origins(&self, surface_id: u64) -> Vec<Point<i32, Logical>> {
        if !self.surface_outputs.contains_key(&surface_id) {
            return self.emacs_frame_origin(surface_id).into_iter().collect();
        }
        let mut origins = Vec::new();
        for (output_name, strip) in &self.output_strips {
            for (frame_idx, entry) in strip.surfaces_with_frame() {
                if entry.surface_id != surface_id {
                    continue;
                }
                let location = LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    entry,
                };
                if let Some(p) = self.entry_origin(location) {
                    origins.push(p);
                }
            }
        }
        origins
    }

    fn rect_contains_f64(rect: Rectangle<i32, Logical>, pos: Point<f64, Logical>) -> bool {
        pos.x >= rect.loc.x as f64
            && pos.y >= rect.loc.y as f64
            && pos.x < (rect.loc.x + rect.size.w) as f64
            && pos.y < (rect.loc.y + rect.size.h) as f64
    }

    /// Find the topmost layout entry containing `pos`, returning the entry
    /// and its global rectangle.
    fn layout_entry_under(
        &self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> Option<LayoutEntryHit<'_>> {
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);
        let strip = self.output_strips.get(&output.name())?;
        let active_idx = strip.active_idx();

        // Fullscreen primary clicks miss in the letterbox; inactive fullscreens render off-screen.
        for (frame_idx, entry) in strip.surfaces_with_frame() {
            let in_active = frame_idx == active_idx;
            if entry.fullscreen && !in_active {
                continue;
            }

            let rect = self.layout_entry_rect(output_geo, working_area, strip, frame_idx, entry);
            if Self::rect_contains_f64(rect, pos) {
                return Some(LayoutEntryHit {
                    entry,
                    rect,
                    output_size: output_geo.size,
                });
            }
        }
        None
    }

    fn pointer_scale_for_entry(
        entry: &LayoutEntry,
        window_size: Size<i32, Logical>,
        output_size: Size<i32, Logical>,
    ) -> f64 {
        if entry.fullscreen {
            if entry.primary {
                return 1.0;
            }

            let uniform = f64::max(
                output_size.w as f64 / window_size.w as f64,
                output_size.h as f64 / window_size.h as f64,
            );
            return 1.0 / uniform;
        }

        if entry.primary || entry.w == 0 || entry.h == 0 {
            return 1.0;
        }

        f64::min(
            window_size.w as f64 / entry.w as f64,
            window_size.h as f64 / entry.h as f64,
        )
    }

    /// Hit-test layout surfaces under a global position; falls back to the
    /// frame's own surface when no entry hits.
    pub fn layout_surface_under(&self, pos: Point<f64, Logical>) -> Option<SurfaceHit> {
        if let Some(surface) = self
            .layout_entry_under(pos)
            .and_then(|hit| self.surface_under_layout_entry(pos, hit))
        {
            return Some(surface);
        }

        self.surface_under_frame(pos)
    }

    fn surface_under_layout_entry(
        &self,
        pos: Point<f64, Logical>,
        hit: LayoutEntryHit<'_>,
    ) -> Option<SurfaceHit> {
        let entry = hit.entry;
        let window = self.id_windows.get(&entry.surface_id)?;
        let window_geo = window.geometry();
        let pointer_scale = Self::pointer_scale_for_entry(entry, window_geo.size, hit.output_size);

        let pos_in_window = Point::from((
            (pos.x - hit.rect.loc.x as f64) * pointer_scale,
            (pos.y - hit.rect.loc.y as f64) * pointer_scale,
        ));
        let window_origin: Point<f64, Logical> =
            Point::from((pos.x - pos_in_window.x, pos.y - pos_in_window.y));

        let (surface, surface_offset) =
            window.surface_under(pos_in_window, WindowSurfaceType::ALL)?;
        let global_loc = Point::from((
            window_origin.x + surface_offset.x as f64,
            window_origin.y + surface_offset.y as f64,
        ));
        Some((surface, global_loc))
    }

    /// Find the frame containing `pos`, returning the frame surface itself.
    fn surface_under_frame(&self, pos: Point<f64, Logical>) -> Option<SurfaceHit> {
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);
        let strip = self.output_strips.get(&output.name())?;

        for (frame_idx, frame) in strip.frames.iter().enumerate() {
            let rect = Self::frame_rect(output_geo, working_area, strip, frame_idx, frame);
            if !Self::rect_contains_f64(rect, pos) {
                continue;
            }
            let Some(window) = self.id_windows.get(&frame.surface_id) else {
                continue;
            };
            let window_origin: Point<f64, Logical> =
                Point::from((rect.loc.x as f64, rect.loc.y as f64));
            let pos_in_window = Point::from((pos.x - window_origin.x, pos.y - window_origin.y));
            if let Some((surface, surface_offset)) =
                window.surface_under(pos_in_window, WindowSurfaceType::ALL)
            {
                let global_loc = Point::from((
                    window_origin.x + surface_offset.x as f64,
                    window_origin.y + surface_offset.y as f64,
                ));
                return Some((surface, global_loc));
            }
        }

        None
    }

    /// Check if a surface is currently displaying as fullscreen on any output.
    pub fn is_surface_fullscreen(&self, id: u64) -> bool {
        self.output_strips
            .values()
            .any(|strip| strip.surfaces().any(|e| e.surface_id == id && e.fullscreen))
    }

    /// True iff the active frame has a fullscreen entry covering the Top layer.
    pub fn render_above_top_layer(&self, output: &Output) -> bool {
        self.output_strips
            .get(&output.name())
            .and_then(|s| s.active_frame())
            .is_some_and(|f| f.surfaces.iter().any(|e| e.fullscreen))
    }

    /// View id of the layout entry under a point.
    pub fn view_id_under(&self, pos: Point<f64, Logical>) -> Option<ViewId> {
        self.layout_entry_under(pos).map(|hit| hit.entry.view_id)
    }

    /// Entry surface ids on `output_name`'s strip; empty if the strip is gone.
    fn strip_entries(&self, output_name: &str) -> HashSet<u64> {
        self.output_strips
            .get(output_name)
            .map(|s| s.surfaces().map(|e| e.surface_id).collect())
            .unwrap_or_default()
    }

    /// Diff `pre` (snapshot before strip mutation) against the current strip;
    /// fire `wl_surface.enter` / `leave` and sync `surface_outputs`.
    fn reconcile_output_entries(&mut self, output_name: &str, pre: &HashSet<u64>) {
        let post = self.strip_entries(output_name);
        let output = self
            .space
            .outputs()
            .find(|o| o.name() == output_name)
            .cloned();
        for &id in pre.difference(&post) {
            if let (Some(out), Some(window)) = (output.as_ref(), self.id_windows.get(&id)) {
                if let Some(surface) = window.wl_surface() {
                    out.leave(&surface);
                }
            }
            if let Some(outputs) = self.surface_outputs.get_mut(&id) {
                outputs.remove(output_name);
                if outputs.is_empty() {
                    self.surface_outputs.remove(&id);
                }
            }
        }
        for &id in post.difference(pre) {
            let Some(window) = self.id_windows.get(&id) else {
                warn!("reconcile_output_entries: surface {} not in id_windows", id);
                continue;
            };
            if let (Some(out), Some(surface)) = (output.as_ref(), window.wl_surface()) {
                out.enter(&surface);
            }
        }
        for &id in &post {
            self.surface_outputs
                .entry(id)
                .or_default()
                .insert(output_name.to_string());
        }
    }

    fn collect_frame_migrations(
        &self,
        output_name: &str,
        frames: &[strip::Frame],
    ) -> Vec<FrameMigration> {
        frames
            .iter()
            .filter_map(|frame| {
                let id = frame.surface_id;
                if id == 0 {
                    return None;
                }
                let prev_output = self.emacs_frame_output(id).map(String::from);
                if prev_output.as_deref() == Some(output_name) {
                    return None;
                }
                Some(FrameMigration { id, prev_output })
            })
            .collect()
    }

    fn output_layout_snapshot(&self, output_name: String) -> OutputLayoutSnapshot {
        let active_frame_id = self
            .output_strips
            .get(&output_name)
            .and_then(|strip| strip.active_frame().map(|frame| frame.surface_id));
        let entry_surface_ids = self.strip_entries(&output_name);
        OutputLayoutSnapshot {
            output_name,
            active_frame_id,
            entry_surface_ids,
        }
    }

    fn output_layout_snapshots(
        &self,
        output_name: &str,
        migrations: &[FrameMigration],
    ) -> Vec<OutputLayoutSnapshot> {
        let mut output_names = vec![output_name.to_string()];
        for migration in migrations {
            if let Some(prev_name) = &migration.prev_output {
                if !output_names.iter().any(|name| name == prev_name) {
                    output_names.push(prev_name.clone());
                }
            }
        }
        output_names
            .into_iter()
            .map(|name| self.output_layout_snapshot(name))
            .collect()
    }

    fn remove_migrated_frames_from_sources(
        &mut self,
        migrations: &[FrameMigration],
        snapshots: &[OutputLayoutSnapshot],
    ) {
        for migration in migrations {
            let Some(prev_name) = migration.prev_output.as_deref() else {
                continue;
            };
            let was_active_in_prev = snapshots.iter().any(|snapshot| {
                snapshot.output_name == prev_name && snapshot.active_frame_id == Some(migration.id)
            });
            if let Some(prev_strip) = self.output_strips.get_mut(prev_name) {
                // `remove_frame_at` adjusts `active_idx` and seeds the
                // slide-in animation on surviving neighbours; plain
                // `retain` would leave `active_idx` stale.
                if let Some(idx) = prev_strip
                    .frames
                    .iter()
                    .position(|frame| frame.surface_id == migration.id)
                {
                    prev_strip.remove_frame_at(idx);
                }
            }
            // Source reconcile skips Hidden for a frame already gone from its
            // strip; freeze it here.
            if was_active_in_prev {
                self.queue_event(Event::FrameHidden { id: migration.id });
            }
        }
    }

    fn reconcile_entry_snapshots(&mut self, snapshots: &[OutputLayoutSnapshot]) {
        for snapshot in snapshots {
            self.reconcile_output_entries(&snapshot.output_name, &snapshot.entry_surface_ids);
        }
    }

    fn send_frame_migration_output_events(
        &self,
        target_output: &Output,
        migrations: &[FrameMigration],
    ) {
        for migration in migrations {
            let Some(window) = self.id_windows.get(&migration.id) else {
                continue;
            };
            let Some(surface) = window.wl_surface() else {
                continue;
            };
            if let Some(prev_name) = migration.prev_output.as_deref() {
                if let Some(prev_output) = self.space.outputs().find(|o| o.name() == prev_name) {
                    prev_output.leave(&surface);
                }
            }
            target_output.enter(&surface);
        }
    }

    fn sync_focus_after_layout_merge(&mut self, output_name: &str) {
        let frame_id = self.focused_frame_id;
        let focused_view_id = self.focused_view_id;
        let mut focus_view_update: Option<Option<ViewId>> = None;

        let Some(strip) = self.output_strips.get_mut(output_name) else {
            return;
        };
        if let Some(new_idx) = strip
            .frames
            .iter()
            .position(|frame| frame.surface_id == frame_id)
        {
            strip.set_active(new_idx);
            let new_view = strip.frames[new_idx].selected_view_id;
            if new_view != focused_view_id {
                focus_view_update = Some(new_view);
            }
        } else if !strip.frames.is_empty() {
            let clamped = strip.active_idx().min(strip.frames.len() - 1);
            strip.set_active(clamped);
        }

        if let Some(new_view) = focus_view_update {
            self.set_focus_state(frame_id, new_view);
            module::record_focus(self.focused_surface_id(), "layout_selected", None);
        }
    }

    /// Apply a declarative per-output layout.
    ///
    /// Replaces the previous layout for the given output. Surfaces removed from
    /// this output get `wl_surface.leave`; new surfaces get `wl_surface.enter`
    /// and scale notification.
    pub fn apply_output_layout(&mut self, output_name: &str, frames: Vec<strip::Frame>) {
        // 1. Find the output
        let output = match self.space.outputs().find(|o| o.name() == output_name) {
            Some(o) => o.clone(),
            None => {
                warn!("apply_output_layout: output '{}' not found", output_name);
                return;
            }
        };

        let migrations = self.collect_frame_migrations(output_name, &frames);
        // Capture affected outputs before the swap; reconciliation runs after
        // the merge against the post-merge strips so kept frames don't leak
        // `surface_outputs`, and active-frame event balance has a stable base.
        let snapshots = self.output_layout_snapshots(output_name, &migrations);
        self.remove_migrated_frames_from_sources(&migrations, &snapshots);

        // Frames omitted by the layout are kept until Wayland destroy arrives.
        // This makes compositor-eager-inserted frames survive layout refreshes
        // that race ahead of Emacs's `ewm--assign-surface-to-frame`.
        let clock = self.animations_clock.clone();
        let strip_entry = self
            .output_strips
            .entry(output_name.to_string())
            .or_insert_with(|| strip::Strip::new(clock));
        strip_entry.merge_layout_frames(frames, module::has_pending_active_frame_close);
        self.sync_focus_after_layout_merge(output_name);

        self.reconcile_entry_snapshots(&snapshots);
        self.send_frame_migration_output_events(&output, &migrations);

        self.refresh_primary_assignments();
        self.update_frames_for_working_area(&output);
        self.prune_dangling_focus();
        // Covers the race where `apply_output_layout` runs before
        // `new_toplevel`'s auto-focus.
        self.focus_any_frame_if_unfocused("auto_focus_on_layout");

        debug!(
            "apply_output_layout: output '{}' with {} surfaces",
            output_name,
            self.strip_entries(output_name).len(),
        );

        for snapshot in snapshots {
            self.reconcile_active_for_output(&snapshot.output_name, snapshot.active_frame_id);
        }
    }

    /// Pick each surface's primary entry (largest active-frame slot) and
    /// configure its toplevel.  Off-screen frames don't bid.
    fn refresh_primary_assignments(&mut self) {
        self.normalize_fullscreen_entries();
        let output_sizes = self.output_sizes_by_name();
        let best_area = self.primary_entry_areas(&output_sizes);
        self.assign_primary_entries(&output_sizes, &best_area);
        self.refresh_mapped_cast_outputs();
        self.configure_primary_entries();
    }

    /// At most one fullscreen entry per frame: clear extras in iteration order.
    fn normalize_fullscreen_entries(&mut self) {
        for strip in self.output_strips.values_mut() {
            for frame in &mut strip.frames {
                for entry in frame.surfaces.iter_mut().filter(|e| e.fullscreen).skip(1) {
                    entry.fullscreen = false;
                }
            }
        }
    }

    fn output_sizes_by_name(&self) -> HashMap<String, (u64, u64)> {
        self.space
            .outputs()
            .map(|o| {
                let size = crate::utils::output_size(o);
                (o.name(), (size.w as u64, size.h as u64))
            })
            .collect()
    }

    /// Fullscreen entries claim the output's full size, not entry.w/h.
    fn layout_entry_area(
        output_sizes: &HashMap<String, (u64, u64)>,
        output_name: &str,
        entry: &LayoutEntry,
    ) -> u64 {
        if entry.fullscreen {
            let (w, h) = output_sizes
                .get(output_name)
                .copied()
                .unwrap_or((entry.w as u64, entry.h as u64));
            w * h
        } else {
            entry.w as u64 * entry.h as u64
        }
    }

    fn primary_entry_areas(&self, output_sizes: &HashMap<String, (u64, u64)>) -> HashMap<u64, u64> {
        let mut best_area: HashMap<u64, u64> = HashMap::new();
        for (oname, strip) in &self.output_strips {
            for entry in strip.active_surfaces() {
                let area = Self::layout_entry_area(output_sizes, oname, entry);
                best_area
                    .entry(entry.surface_id)
                    .and_modify(|a| *a = (*a).max(area))
                    .or_insert(area);
            }
        }
        best_area
    }

    fn assign_primary_entries(
        &mut self,
        output_sizes: &HashMap<String, (u64, u64)>,
        best_area: &HashMap<u64, u64>,
    ) {
        for strip in self.output_strips.values_mut() {
            for entry in strip.surfaces_mut() {
                entry.primary = false;
            }
        }

        let mut assigned: HashSet<u64> = HashSet::new();
        for (oname, strip) in self.output_strips.iter_mut() {
            for entry in strip.active_surfaces_mut() {
                let dominated = best_area.get(&entry.surface_id).copied().unwrap_or(0);
                if Self::layout_entry_area(output_sizes, oname, entry) == dominated
                    && assigned.insert(entry.surface_id)
                {
                    entry.primary = true;
                }
            }
        }
    }

    fn refresh_mapped_cast_outputs(&mut self) {
        #[cfg(feature = "screencast")]
        {
            self.casting.mapped_cast_output.clear();
            for (oname, strip) in &self.output_strips {
                let Some(output) = self.space.outputs().find(|o| &o.name() == oname).cloned()
                else {
                    continue;
                };
                for entry in strip.active_surfaces().filter(|e| e.primary) {
                    self.casting
                        .mapped_cast_output
                        .insert(entry.surface_id, output.clone());
                }
            }
        }
    }

    fn configure_primary_entries(&self) {
        for output in self.space.outputs() {
            let Some(strip) = self.output_strips.get(&output.name()) else {
                continue;
            };
            for entry in strip.active_surfaces().filter(|e| e.primary) {
                self.configure_primary(output, entry);
            }
        }
    }

    fn configure_primary(&self, output: &Output, entry: &LayoutEntry) {
        let Some(window) = self.id_windows.get(&entry.surface_id) else {
            return;
        };
        window.with_surfaces(|s, data| {
            crate::utils::send_scale_transform(
                s,
                data,
                output.current_scale(),
                output.current_transform(),
            );
        });
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let new_size = if entry.fullscreen {
            let s = crate::utils::output_size(output);
            (s.w as i32, s.h as i32).into()
        } else {
            (entry.w as i32, entry.h as i32).into()
        };
        let changed = toplevel.with_pending_state(|state| {
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
            toplevel.send_configure();
        }
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
        } else if let Some(name) = self.emacs_frame_output(id) {
            let name = name.to_string();
            self.space
                .outputs()
                .filter(|o| o.name() == name)
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
    /// This is the single source of truth, updated by `pointer.motion()`,
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
    /// If not, reschedules for the remaining time instead of firing.
    /// Returns `true` if the idle action was actually fired.
    fn on_idle_timer_fired(&mut self) -> bool {
        self.idle_timeout.timer_token = None;

        // Must refresh here: calloop may have been sleeping when D-Bus
        // inhibit arrived, so the per-iteration refresh hasn't run yet.
        self.refresh_idle_inhibit();
        if self.idle_is_inhibited {
            info!("Idle timer fired while inhibited, skipping idle action");
            return false;
        }

        let Some(timeout) = self.idle_timeout.timeout else {
            return false;
        };

        let elapsed = self.idle_timeout.last_activity.elapsed();
        if elapsed < timeout {
            // Activity happened since timer was set; reschedule for remaining time
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
    /// Otherwise just updates the timestamp. The timer callback
    /// will check elapsed time and reschedule if needed.
    pub fn reset_idle_timer(&mut self) {
        if self.idle_timeout.is_idle {
            self.wake_from_idle();
        } else {
            self.idle_timeout.last_activity = std::time::Instant::now();
        }
    }

    pub fn notify_activity(&mut self) {
        if self.notified_activity_this_iteration {
            return;
        }

        tracy_span!("Ewm::notify_activity");

        self.idle_notifier_state.notify_activity(&self.seat);

        self.notified_activity_this_iteration = true;
    }

    pub fn configure_cursor_hide(&mut self, timeout: Option<Duration>, hide_when_typing: bool) {
        if self.cursor_hide.is_hidden {
            self.cursor_hide.is_hidden = false;
            self.queue_redraw_for_pointer();
        }

        self.cancel_cursor_hide_timer();
        self.cursor_hide.timeout = timeout;
        self.cursor_hide.hide_when_typing = hide_when_typing;
        self.cursor_hide.last_activity = std::time::Instant::now();

        if let Some(duration) = timeout {
            self.start_cursor_hide_timer(duration);
        }
        info!(
            "Cursor auto-hide configured: timeout={:?}, hide_when_typing={}",
            timeout, hide_when_typing
        );
    }

    pub fn hide_cursor_for_typing(&mut self) {
        // If the cursor is already invisible, don't redraw on every keystroke.
        if self.cursor_hide.is_hidden {
            return;
        }

        if !self.cursor_hide.hide_when_typing {
            return;
        }

        self.cursor_hide.is_hidden = true;
        self.queue_redraw_for_pointer();
    }

    fn start_cursor_hide_timer(&mut self, duration: Duration) {
        self.cancel_cursor_hide_timer();
        let timer = Timer::from_duration(duration);
        match self.loop_handle.insert_source(timer, |_, _, state| {
            state.ewm.on_cursor_hide_timer_fired();
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        }) {
            Ok(token) => {
                self.cursor_hide.timer_token = Some(token);
            }
            Err(err) => {
                warn!("Failed to insert cursor-hide timer: {:?}", err);
            }
        }
    }

    pub(crate) fn cancel_cursor_hide_timer(&mut self) {
        if let Some(token) = self.cursor_hide.timer_token.take() {
            self.loop_handle.remove(token);
        }
    }

    fn on_cursor_hide_timer_fired(&mut self) {
        self.cursor_hide.timer_token = None;

        let Some(timeout) = self.cursor_hide.timeout else {
            return;
        };

        let elapsed = self.cursor_hide.last_activity.elapsed();
        if elapsed < timeout {
            self.start_cursor_hide_timer(timeout - elapsed);
            return;
        }

        if !self.cursor_hide.is_hidden {
            self.cursor_hide.is_hidden = true;
            self.queue_redraw_for_pointer();
        }
    }

    pub fn reset_cursor_hide_timer(&mut self) {
        if self.cursor_hide.timeout.is_none() && !self.cursor_hide.hide_when_typing {
            return;
        }
        self.cursor_hide.last_activity = std::time::Instant::now();
        if self.cursor_hide.is_hidden {
            self.cursor_hide.is_hidden = false;
            self.queue_redraw_for_pointer();
            if let Some(duration) = self.cursor_hide.timeout {
                self.start_cursor_hide_timer(duration);
            }
        }
    }

    /// Refresh the idle inhibition state.
    ///
    /// Combines three sources of idle inhibition:
    /// 1. **FDO ScreenSaver D-Bus**: Firefox, Chrome, and other apps use
    ///    `org.freedesktop.ScreenSaver.Inhibit()` to prevent screensaver.
    /// 2. **Wayland idle-inhibit protocol**: Video players create `zwp_idle_inhibitor_v1` on their
    ///    surface.
    /// 3. **Fullscreen heuristic**: A fullscreen surface that is currently focused is treated as an
    ///    idle inhibitor (covers video players and games that don't implement the protocol).
    ///
    /// For Wayland protocol inhibitors, only visible surfaces count
    /// (checked via `surface_primary_scanout_output`).  This prevents a
    /// minimized/hidden app from keeping the screen awake.
    ///
    /// The result is propagated to both the ext-idle-notify protocol (for
    /// swayidle) and the native idle timeout (monitor blanking/command).
    pub fn refresh_idle_inhibit(&mut self) {
        use smithay::wayland::compositor::with_states;

        // Clean up dead surfaces
        self.idle_inhibiting_surfaces.retain(|s| s.is_alive());

        let is_inhibited = self.manual_idle_inhibited
            || self
                .is_fdo_idle_inhibited
                .load(std::sync::atomic::Ordering::SeqCst)
            || self.idle_inhibiting_surfaces.iter().any(|surface| {
                with_states(surface, |states| {
                    surface_primary_scanout_output(surface, states).is_some()
                })
            })
            || self.fullscreen_surface_inhibits_idle();

        if is_inhibited != self.idle_is_inhibited {
            info!(
                "Idle inhibition changed: {} -> {}",
                self.idle_is_inhibited, is_inhibited
            );
            self.idle_is_inhibited = is_inhibited;

            // Propagate to ext-idle-notify protocol (for swayidle)
            self.idle_notifier_state.set_is_inhibited(is_inhibited);

            // Propagate to native idle timeout
            if is_inhibited {
                // Cancel the idle timer to prevent monitor blanking
                self.cancel_idle_timer();
                // If already idle, wake up
                if self.idle_timeout.is_idle {
                    self.wake_from_idle();
                }
            } else {
                // Restart the idle timer if configured
                if let Some(duration) = self.idle_timeout.timeout {
                    self.start_idle_timer(duration);
                }
            }
        }
    }

    /// A visible fullscreen surface on any output inhibits idle.
    /// Catches apps that don't implement zwp-idle-inhibit-v1 or D-Bus ScreenSaver.
    fn fullscreen_surface_inhibits_idle(&self) -> bool {
        self.space.outputs().any(|o| self.render_above_top_layer(o))
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

    /// Sweep and clear finished animations for the given output.
    ///
    /// Called before each render so the `unfinished_animations_remain` flag
    /// reflects only live animations.
    fn advance_animations(&mut self, output: &Output) {
        if let Some(strip) = self.output_strips.get_mut(&output.name()) {
            strip.advance();
        }
        let id_windows = &self.id_windows;
        self.surface_open_anims
            .retain(|id, a| !a.is_done() && id_windows.contains_key(id));
    }

    /// Orchestrate a single output redraw.
    ///
    /// Orchestrate a single output redraw:
    /// 1. Get target presentation time from frame clock
    /// 2. Call backend.render() -> RenderResult
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

        // Sweep finished animations so render-path flag reflects live ones.
        self.advance_animations(output);

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
            // Done animations are already cleared by advance_animations() above.
            state.unfinished_animations_remain = false;
        }
        if let Some(strip) = self.output_strips.get(&output.name()) {
            let any_open_fade = strip
                .surfaces()
                .any(|e| self.surface_open_anims.contains_key(&e.surface_id))
                || strip
                    .frames
                    .iter()
                    .any(|f| self.surface_open_anims.contains_key(&f.surface_id));
            if strip.animations_ongoing() || any_open_fade {
                if let Some(state) = self.output_state.get_mut(output) {
                    state.unfinished_animations_remain = true;
                }
            }
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
                    None,
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
                    None,
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
                            None,
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
                        None,
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
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
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
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
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
                            None,
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
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
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
                should_send,
            );
        }

        let layer_map = layer_map_for_output(output);
        for layer in layer_map.layers() {
            layer.send_frame(
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                should_send,
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

    /// Fallback frame callback sender for stuck surfaces.
    ///
    /// Sends frame callbacks to ALL surfaces regardless of output, bypassing
    /// primary scanout output matching. The `FRAME_CALLBACK_THROTTLE` (995ms)
    /// prevents busy-looping since it won't re-send if a callback was already
    /// sent recently through the normal path.
    pub fn send_frame_callbacks_on_fallback_timer(&self) {
        // Off-screen emacs frames mustn't be woken; their pgtk client would
        // redisplay on the emacs main thread for nothing.
        let mut hidden_emacs_ids: HashSet<u64> = HashSet::new();
        for strip in self.output_strips.values() {
            let active = strip.active_frame().map(|f| f.surface_id);
            for f in &strip.frames {
                if Some(f.surface_id) != active {
                    hidden_emacs_ids.insert(f.surface_id);
                }
            }
        }

        // Bogus output: the should_send closure returns None so the output
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

        for (id, window) in self.id_windows.iter() {
            if hidden_emacs_ids.contains(id) {
                continue;
            }
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
                    is_focused: self.focused_surface_id() == id,
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

    /// Output name for a surface, preferring the focused layout entry when a
    /// surface appears on multiple outputs.
    fn surface_output_name(&self, surface_id: u64) -> Option<&str> {
        if self.surface_outputs.contains_key(&surface_id) {
            if let Some(location) = self.focused_entry_for_surface(surface_id) {
                return Some(location.output_name);
            }
            if let Some(name) = self
                .surface_outputs
                .get(&surface_id)
                .and_then(|output_names| output_names.iter().next())
            {
                return Some(name.as_str());
            }
        }

        self.emacs_frame_output(surface_id)
    }

    /// Find the output for a surface (returns Output object)
    fn find_surface_output(&self, surface_id: u64) -> Option<smithay::output::Output> {
        let output_name = self.surface_output_name(surface_id)?;
        self.space
            .outputs()
            .find(|o| o.name() == output_name)
            .cloned()
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
        self.focused_view_id.is_none() && self.is_emacs_frame(self.focused_frame_id)
    }

    /// Focus a surface by id; resolves to either a view or an Emacs frame and
    /// delegates to the canonical mutator (`set_focus_view` / `set_focus_frame`).
    /// Callers with a known kind should call those directly.
    pub fn set_focus(&mut self, id: u64, notify_emacs: bool, source: &str) {
        let (frame_id, view_id) = self.resolve_focus_target(id);
        if let Some(view_id) = view_id {
            self.set_focus_view(view_id, source, notify_emacs);
        } else if frame_id != 0 {
            self.set_focus_frame(frame_id, source, notify_emacs);
        }
    }

    fn resolve_focus_target(&self, id: u64) -> (u64, Option<ViewId>) {
        if self.is_emacs_frame(id) {
            return (id, None);
        }
        if let Some((frame_id, view_id)) = self.frame_entry_for_surface(id) {
            (frame_id, Some(view_id))
        } else {
            (id, None)
        }
    }

    /// Focus the entity under `pos`; returns `(frame_id, view_id)`.
    /// Hit-test `pos` and focus the resulting view or frame.  Callers retain
    /// control over emitting the `Focus` event (for ffm coordinates), so this
    /// passes `notify_emacs=false` to the canonical mutators.
    pub fn focus_under(
        &mut self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
        source: &str,
    ) -> Option<(u64, Option<ViewId>)> {
        let (frame_id, view_id) = if let Some(view_id) = self.view_id_under(pos) {
            let location = self.lookup_view(view_id)?;
            let frame_id = location.frame_surface_id;
            self.set_focus_view_at(view_id, location, source, false);
            (frame_id, Some(view_id))
        } else {
            let (surface, _) = self.surface_under_point(pos)?;
            let sid = self.surface_id(&surface)?;
            let (fid, vid) = self.resolve_focus_target(sid);
            if let Some(view_id) = vid {
                self.set_focus_view(view_id, source, false);
            } else if fid != 0 {
                self.set_focus_frame(fid, source, false);
            } else {
                return None;
            }
            (fid, vid)
        };
        Some((frame_id, view_id))
    }

    pub fn focus_follows_mouse(
        &mut self,
        under: &Option<(
            WlSurface,
            smithay::utils::Point<f64, smithay::utils::Logical>,
        )>,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
        let Some((_, origin)) = under else { return };
        let Some((frame_id, view_id)) = self.focus_under(pos, "pointer_motion") else {
            return;
        };
        if let Some(view_id) = view_id.or_else(|| self.view_id_of_frame(frame_id)) {
            self.queue_event(Event::Focus {
                view_id,
                x: Some(pos.x - origin.x),
                y: Some(pos.y - origin.y),
            });
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
                }
                return;
            }
        }

        // Something else got clicked, clear on-demand layer-shell focus
        if self.layer_shell_on_demand_focus.is_some() {
            self.layer_shell_on_demand_focus = None;
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
            let result = map.layers_on(layer).find_map(|surface| {
                if surface.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
                {
                    Some(surface.wl_surface().clone())
                } else {
                    None
                }
            });
            result
        };

        // Helper: check if on-demand focus is on a layer
        let on_demand_on_layer = |output: &Output, layer: Layer| -> Option<WlSurface> {
            let on_demand = self.layer_shell_on_demand_focus.as_ref()?;
            let map = layer_map_for_output(output);
            let result = map.layers_on(layer).find_map(|surface| {
                if surface == on_demand {
                    Some(surface.wl_surface().clone())
                } else {
                    None
                }
            });
            result
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
            let focused = self.focused_surface_id();
            if focused == 0 || !self.id_windows.contains_key(&focused) {
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

    /// Emit an event to the lisp side.
    pub(crate) fn queue_event(&mut self, event: Event) {
        if let Some(sink) = &mut self.captured_events {
            sink.push(event.clone());
        }
        module::write_event(&event);
    }

    /// Emit FrameShown/FrameHidden when an output's strip-active changes.
    /// Caller snapshots `prev_active_id` before the mutation; the helper
    /// compares against the post-mutation state.  No stored state.
    pub(crate) fn reconcile_active_for_output(
        &mut self,
        output_name: &str,
        prev_active_id: Option<u64>,
    ) {
        let Some(strip) = self.output_strips.get(output_name) else {
            return;
        };
        let new_active_id = strip.active_frame().map(|f| f.surface_id);
        if prev_active_id == new_active_id {
            return;
        }
        if let Some(id) = prev_active_id {
            // Skip if the frame was destroyed -- the lisp side already handled it.
            if strip.frames.iter().any(|f| f.surface_id == id) {
                self.queue_event(Event::FrameHidden { id });
            }
        }
        if let Some(id) = new_active_id {
            self.queue_event(Event::FrameShown { id });
        }
    }

    /// Update text_input focus for input method support.
    ///
    /// Emacs surfaces and None both clear text_input (leave + set_focus(None)),
    /// causing the client to disable. Commits that arrive during the resulting
    /// disable->enable gap are queued in `pending_im_commits` and drained on
    /// the next `ImEvent::Activated`.
    pub fn update_text_input_focus(&self, surface: Option<&WlSurface>, surface_id: Option<u64>) {
        use smithay::wayland::text_input::TextInputSeat;
        let text_input = self.seat.text_input();

        let is_emacs = surface_id.is_some_and(|id| self.is_emacs_frame(id));

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
        self.space
            .output_under(pos)
            .next()
            .or_else(|| self.space.outputs().next())
    }

    /// Find the Output whose geometry strictly contains `pos`.
    ///
    /// Unlike [`Self::output_at`], returns `None` when `pos` lies in dead
    /// space between or outside outputs instead of falling back to an
    /// arbitrary output.
    fn output_containing(
        &self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> Option<&Output> {
        self.space.output_under(pos).next()
    }

    /// True when the strip on the output under `pos` is mid-animation.
    pub fn is_strip_animating_at(
        &self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> bool {
        let Some(output) = self.output_containing(pos) else {
            return false;
        };
        self.output_strips
            .get(&output.name())
            .is_some_and(|s| s.animations_ongoing())
    }

    /// Clamp a pointer target into the union of output geometries.
    ///
    /// If `target` already lies on an output, returns it unchanged.
    /// Otherwise clips the movement against the geometry of the output
    /// containing `previous` (the pointer's prior position), so the
    /// pointer can never wander into dead space between outputs.
    /// Falls back to the centre of the first output if neither point is
    /// on any output.
    pub(crate) fn clamp_pointer_to_outputs(
        &self,
        target: smithay::utils::Point<f64, smithay::utils::Logical>,
        previous: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> smithay::utils::Point<f64, smithay::utils::Logical> {
        if self.output_containing(target).is_some() {
            return target;
        }
        if let Some(output) = self.output_containing(previous) {
            let geom = self.space.output_geometry(output).unwrap();
            let x = target
                .x
                .clamp(geom.loc.x as f64, (geom.loc.x + geom.size.w - 1) as f64);
            let y = target
                .y
                .clamp(geom.loc.y as f64, (geom.loc.y + geom.size.h - 1) as f64);
            return smithay::utils::Point::from((x, y));
        }
        let Some(output) = self.space.outputs().next() else {
            return target;
        };
        let Some(geom) = self.space.output_geometry(output) else {
            return target;
        };
        let cx = geom.loc.x as f64 + geom.size.w as f64 / 2.0;
        let cy = geom.loc.y as f64 + geom.size.h as f64 / 2.0;
        smithay::utils::Point::from((cx, cy))
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
        for layer_surface in map.layers_on(layer).rev() {
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
            for layer_surface in map.layers_on(layer).rev() {
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
    ///   Overlay -> Top -> [popups] -> [layout surfaces] -> [Emacs frames] -> Bottom -> Background
    ///
    /// When a fullscreen surface is active on the output (`render_above_top_layer`):
    ///   Overlay -> [popups] -> [layout surfaces] -> [Emacs frames] -> Top -> Bottom -> Background
    ///
    /// This ensures the input hit-testing order matches the visual render order.
    pub fn surface_under_point(
        &self,
        pos: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> Option<(
        WlSurface,
        smithay::utils::Point<f64, smithay::utils::Logical>,
    )> {
        let output = self.output_at(pos);
        let output_geo = output.and_then(|o| self.space.output_geometry(o));
        let above_top_layer = output.is_some_and(|o| self.render_above_top_layer(o));

        if let (Some(output), Some(geo)) = (output, output_geo) {
            let pos_within_output = pos - geo.loc.to_f64();

            // 1. Overlay layer (always highest, regardless of fullscreen)
            if let Some(result) =
                self.layer_surface_under(output, Layer::Overlay, pos_within_output, geo.loc)
            {
                return Some(result);
            }

            // 2. Top layer, only before windows when not fullscreen
            if !above_top_layer {
                if let Some(result) =
                    self.layer_surface_under(output, Layer::Top, pos_within_output, geo.loc)
                {
                    return Some(result);
                }
            }
        }

        // 3. Layout surfaces (entries + frame's own surface)
        if let Some(result) = self.layout_surface_under(pos) {
            return Some(result);
        }

        // 4. Deferred layers (Top deferred behind fullscreen, then Bottom/Background)
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

    /// Get the output where the focused frame is located
    fn get_focused_output(&self) -> Option<String> {
        if self.focused_frame_id == 0 {
            return None;
        }
        self.emacs_frame_output(self.focused_frame_id)
            .map(str::to_string)
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

    /// Output for surface placement and keyboard nav: focused > cursor > first.
    pub fn active_output(&self) -> Option<String> {
        self.get_focused_output()
            .or_else(|| self.output_under_cursor())
            .or_else(|| self.space.outputs().next().map(|o| o.name()))
    }

    /// Find an output by name in this `Space`.
    pub fn find_output(&self, name: &str) -> Option<Output> {
        self.space.outputs().find(|o| o.name() == name).cloned()
    }

    /// Output adjacent to `current` along one axis. Candidates must overlap
    /// the perpendicular band and the closest center in the requested
    /// direction wins.
    fn output_in_direction(&self, current: &Output, dx: i32, dy: i32) -> Option<Output> {
        let cur = self.space.output_geometry(current)?;
        let horizontal = match (dx.signum(), dy.signum()) {
            (-1 | 1, 0) => true,
            (0, -1 | 1) => false,
            _ => return None,
        };
        let sign = if horizontal { dx.signum() } else { dy.signum() };
        let band = if horizontal {
            Rectangle::new(
                smithay::utils::Point::from((i32::MIN / 2, cur.loc.y)),
                Size::from((i32::MAX, cur.size.h)),
            )
        } else {
            Rectangle::new(
                smithay::utils::Point::from((cur.loc.x, i32::MIN / 2)),
                Size::from((cur.size.w, i32::MAX)),
            )
        };
        let cur_center = if horizontal {
            i64::from(cur.loc.x) + i64::from(cur.size.w) / 2
        } else {
            i64::from(cur.loc.y) + i64::from(cur.size.h) / 2
        };

        self.space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o).map(|g| (o, g)))
            .filter_map(|(output, geo)| {
                if !geo.overlaps(band) {
                    return None;
                }
                let center = if horizontal {
                    i64::from(geo.loc.x) + i64::from(geo.size.w) / 2
                } else {
                    i64::from(geo.loc.y) + i64::from(geo.size.h) / 2
                };
                let distance = i64::from(sign) * (center - cur_center);
                (distance > 0).then_some((output, distance))
            })
            .min_by_key(|(_, distance)| *distance)
            .map(|(o, _)| o.clone())
    }

    /// Focus the active frame on the output spatially adjacent to the
    /// current one.  `dx` is `-1`/`+1` for left/right, `dy` is `-1`/`+1`
    /// for up/down; mixed axes and zero vectors are a no-op.
    pub fn focus_output_direction(&mut self, dx: i32, dy: i32) {
        let cur_name = match self.active_output() {
            Some(n) => n,
            None => return,
        };
        let cur = match self.space.outputs().find(|o| o.name() == cur_name).cloned() {
            Some(o) => o,
            None => return,
        };
        let target = self.output_in_direction(&cur, dx, dy);
        let Some(target) = target else { return };
        let Some(id) = self.active_frame_surface_id(&target.name()) else {
            return;
        };
        self.set_focus_frame(id, "focus_output_direction", true);
    }

    /// Pick the Emacs frame to redirect intercepted keys to.
    pub fn get_emacs_surface_for_focused_output(&self) -> Option<u64> {
        if self.focused_frame_id != 0 && self.is_emacs_frame(self.focused_frame_id) {
            return Some(self.focused_frame_id);
        }
        let output = self.get_focused_output()?;
        self.active_frame_surface_id(&output)
            .or_else(|| self.first_active_frame_surface_id())
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

    /// Immediately sync active_outputs into shared state so that Emacs sees
    /// up-to-date output lists when it handles events queued after this call.
    pub fn flush_active_outputs(&mut self) {
        let mut shared = module::shared_state().lock().unwrap();
        shared.active_outputs = self
            .space
            .outputs()
            .filter_map(|o| {
                let geo = self.space.output_geometry(o)?;
                let wa = self.working_areas.get(&o.name());
                let x = geo.loc.x + wa.map_or(0, |r| r.loc.x);
                let y = geo.loc.y + wa.map_or(0, |r| r.loc.y);
                Some((o.name(), module::ActiveOutput { origin: (x, y) }))
            })
            .collect();
        self.active_outputs_dirty = false;
    }

    /// Send output detected event to Emacs.
    /// Flushes active_outputs to shared state first so Emacs sees the new
    /// output immediately when handling this or subsequent events.
    pub fn send_output_detected(&mut self, output: OutputInfo) {
        self.flush_active_outputs();
        self.queue_event(Event::OutputDetected(output));
    }

    /// Send output disconnected event to Emacs.
    /// Flushes active_outputs to shared state first so Emacs sees the removal
    /// immediately when handling this or subsequent events.
    pub fn send_output_disconnected(&mut self, name: &str) {
        self.flush_active_outputs();
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
                    mode_info.current = mode_info.width == current_mode.size.w
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
            for cast in self.casting.casts.iter_mut() {
                if cast.target == target {
                    if let Err(err) = cast.set_refresh(refresh) {
                        warn!(session_id = %cast.session_id, "set_refresh failed: {err:?}");
                    }
                    if let Err(err) = cast.ensure_size(physical_size) {
                        warn!(session_id = %cast.session_id, "ensure_size failed: {err:?}");
                    }
                }
            }
        }

        // Recalculate total output size, working areas, and queue redraw
        self.recalculate_output_size();
        self.active_outputs_dirty = true;
        self.check_working_area_change(output);
        // Always reposition Emacs frames: the output position may have changed
        // even when the working area (relative to the output) didn't.
        self.update_frames_for_working_area(output);
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
        if let Some(strip) = self.output_strips.get(&output.name()) {
            for entry in strip.surfaces() {
                if let Some(window) = self.id_windows.get(&entry.surface_id) {
                    window.with_surfaces(|surface, data| {
                        crate::utils::send_scale_transform(surface, data, scale, transform);
                    });
                }
            }
        }

        // Emacs frames aren't in `space`; notify them via the strip.
        let output_name = output.name();
        for id in self.emacs_frames_on(&output_name) {
            if let Some(window) = self.id_windows.get(&id) {
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
        let output_name = output.name();

        for id in self.emacs_frames_on(&output_name) {
            let Some(window) = self.id_windows.get(&id) else {
                continue;
            };
            if let Some(toplevel) = window.toplevel() {
                let new_size = Some(working_area.size);
                let changed = toplevel.with_pending_state(|state| {
                    let changed = state.size != new_size;
                    state.size = new_size;
                    changed
                });
                if changed {
                    toplevel.send_configure();
                }
            }
        }

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
            .is_none_or(|prev| *prev != working_area);

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
                for layer_surface in map.layers_on(layer) {
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

    /// Build the JSON object shown by `ewm-show-state`.
    pub fn debug_state(&self) -> serde_json::Value {
        let mut id_window_keys: Vec<u64> = self.id_windows.keys().copied().collect();
        id_window_keys.sort_unstable();
        let emacs_surfaces: HashMap<u64, String> = self
            .output_strips
            .iter()
            .flat_map(|(out, s)| s.frames.iter().map(move |f| (f.surface_id, out.clone())))
            .collect();

        serde_json::json!({
            "surfaces": &self.surface_info,
            "emacs_surfaces": emacs_surfaces,
            "output_strips": &self.output_strips,
            "surface_outputs": &self.surface_outputs,
            "focused_surface_id": self.focused_surface_id(),
            "focused_frame_id": self.focused_frame_id,
            "focused_view_id": self.focused_view_id,
            "id_windows": id_window_keys,
            "outputs": &self.outputs,
            "working_areas": self.get_working_areas_info(),
            "layer_surfaces": self.get_layer_surfaces_info(),
            "pointer_location": self.pointer_location(),
            "intercepted_keys": module::get_intercepted_keys(),
            "emulated_keys": module::get_emulated_keys(),
            "emacs_pid": self.emacs_pid,
            "text_input_intercept": self.text_input_intercept,
            "text_input_active": self.text_input_active,
            "xkb_layouts": &self.xkb_layout_names,
            "xkb_current_layout": self.xkb_current_layout,
            "next_surface_id": self.next_surface_id,
            "redraw_states": self.output_state.iter().map(|(output, state)| {
                serde_json::json!({
                    "output": output.name(),
                    "state": state.redraw_state.to_string(),
                })
            }).collect::<Vec<_>>(),
            "workspace_protocol": self.workspace_state.debug_state(),
            "pending_frame_outputs": module::peek_pending_frame_outputs(),
            "pending_active_frame_closes": module::peek_pending_active_frame_closes(),
            "in_prefix_sequence": module::get_in_prefix_sequence(),
            "debug_mode": module::DEBUG_MODE.load(std::sync::atomic::Ordering::Relaxed),
            "pending_commands": module::peek_commands(),
            "focus_history": module::get_focus_history(),
        })
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
        if let Some((_window, id)) = self.find_window_by_surface(&root) {
            let output_name = self.surface_output_name(id)?;
            return self.space.outputs().find(|o| o.name() == output_name);
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
    /// Derived from the layout entry position, with no dependency on window.geometry(),
    /// so centering offsets and CSD geometry don't affect the result.
    /// Fullscreen: full output. Normal: entry width × output height (horizontally
    /// constrained to the entry, vertically to the full output).
    fn popup_target_rect(&self, id: u64) -> Option<Rectangle<i32, Logical>> {
        let location = self.preferred_entry_for_surface(id)?;
        let entry = location.entry;
        let output = self
            .space
            .outputs()
            .find(|o| o.name() == location.output_name)?;
        let output_geo = self.space.output_geometry(output)?;

        if entry.fullscreen {
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

    /// Handle new toplevel surface from XdgShellHandler.
    pub fn handle_new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_surface_id;
        self.next_surface_id += 1;

        // Check if this surface belongs to the Emacs client
        let is_emacs = self.is_emacs_client(surface.wl_surface());
        if is_emacs {
            info!("Surface {} is an Emacs surface", id);
        }
        if !is_emacs {
            self.unmapped_surfaces.insert(id);
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

        // Extract parent's surface id before consuming `surface` -- used
        // below to place dialogs in the same Emacs frame as their owner.
        let parent_id = surface
            .parent()
            .and_then(|wl| self.find_window_by_surface(&wl))
            .map(|(_, id)| id);

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

        debug!(
            "new_toplevel id={} is_emacs={} pending_output={:?} target_output={:?} pending_queue_after_take={:?}",
            id, is_emacs, frame_output, target_output, module::peek_pending_frame_outputs()
        );

        if is_emacs {
            // Eager-insert into the target strip so `is_emacs_frame(id)`,
            // `set_focus_frame(id)`, etc. work immediately -- before Emacs's
            // first layout-refresh arrives.  Layout-refresh later updates fields
            // (name, width, surfaces, selected_view_id) via MERGE semantics.
            if let Some(output_name) = target_output.as_deref() {
                let clock = self.animations_clock.clone();
                let strip = self
                    .output_strips
                    .entry(output_name.to_string())
                    .or_insert_with(|| strip::Strip::new(clock));
                strip.frames.push(crate::strip::Frame::new(id));
            }
        }

        // Associate window with target output for scale detection.
        // Output association is managed explicitly (not via space.refresh()).
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
            // Direct output enter, bypassing SpaceElement::output_enter which would
            // trigger output_update -> leave for uncommitted surfaces.
            if let Some(surface) = window.wl_surface() {
                output.enter(&surface);
            }
        }

        // Resize Emacs frames to fill their working area
        if let Some(ref output_name) = frame_output {
            if let Some(working_area) = self
                .space
                .outputs()
                .find(|o| o.name() == *output_name)
                .map(|o| self.get_working_area(o))
            {
                if let Some(t) = window.toplevel() {
                    t.with_pending_state(|state| {
                        state.size = Some(working_area.size);
                    });
                    t.send_configure();
                }
            }
        }

        self.surface_info.insert(
            id,
            SurfaceInfo {
                app_id: app.clone(),
                title: String::new(),
            },
        );

        // Auto-focus the first Emacs surface so keyboard input works without
        // a pointer event. Keyboard.enter is replayed on Initialized command;
        // PGTK drops enters sent before its surface's first roundtrip.
        let auto_focus = is_emacs && self.focused_surface_id() == 0;
        if auto_focus {
            self.set_focus_frame(id, "initial_surface", false);
        }

        // Resolve which Emacs frame should host this surface.  Emacs
        // frame xdg_toplevels are themselves frames, so they skip this.
        // For app surfaces, parent-frame wins so dialogs stick with their
        // owner; otherwise land on the active frame of the target output.
        let frame_surface_id = if is_emacs {
            None
        } else {
            parent_id
                .and_then(|pid| self.frame_for_surface(pid))
                .or_else(|| {
                    target_output
                        .as_deref()
                        .and_then(|o| self.active_frame_surface_id(o))
                })
        };

        self.queue_event(Event::New {
            id,
            app: app.clone(),
            output: target_output.clone(),
            frame_surface_id,
            pid: client_pid,
        });
        // Eager-inserted new emacs frames land at the strip's end and aren't
        // the active slot; tell lisp so it can freeze them.  Emitted after
        // Event::New so ewm-surface-id is already bound on the lisp side.
        if is_emacs {
            let is_active = target_output
                .as_deref()
                .and_then(|o| self.active_frame_surface_id(o))
                == Some(id);
            self.queue_event(if is_active {
                Event::FrameShown { id }
            } else {
                Event::FrameHidden { id }
            });
        }
        // Sent after New so the lisp side has assigned ewm-surface-id by the
        // time the Focus event is processed.
        if auto_focus {
            info!("Auto-focusing initial Emacs surface {}", id);
            if let Some(view_id) = self.view_id_of_frame(id) {
                self.queue_event(Event::Focus {
                    view_id,
                    x: None,
                    y: None,
                });
            }
        }
        info!(
            "New toplevel {} ({}) -> {:?}",
            id,
            app,
            target_output.as_deref().unwrap_or("unknown")
        );
    }

    /// Handle toplevel destroyed from XdgShellHandler.
    pub fn handle_toplevel_destroyed(&mut self, surface: ToplevelSurface) -> Option<u64> {
        let id = self
            .id_windows
            .iter()
            .find(|(_, w)| w.toplevel().map(|t| t == &surface).unwrap_or(false))
            .map(|(&id, _)| id)?;
        self.handle_toplevel_destroyed_by_id(id)
    }

    /// Drop a toplevel by surface id; same cleanup as the surface-typed entry
    /// point, but reachable from integration tests without a real toplevel.
    pub fn handle_toplevel_destroyed_by_id(&mut self, id: u64) -> Option<u64> {
        let was_focused = self.focused_frame_id == id;
        let close_was_active = module::take_pending_active_frame_close(id);
        let active_close = was_focused || close_was_active;
        let output = self.find_surface_output(id).map(|o| o.name());

        if let Some(window) = self.id_windows.remove(&id) {
            self.window_ids.remove(&window);
        }
        self.surface_info.remove(&id);
        let was_emacs = self.is_emacs_frame(id);
        self.remove_surface_from_layouts(id);
        self.remove_emacs_frame_from_strip(id, active_close);
        self.prune_dangling_focus();
        self.queue_event(Event::Close { id });
        debug!(
            "Toplevel {} destroyed was_focused={} close_was_active={} was_emacs={} output={:?}",
            id, was_focused, close_was_active, was_emacs, output
        );

        #[cfg(feature = "screencast")]
        self.stop_casts_for_window(id);

        if active_close {
            // Output may be unknown if lisp pruned the frame before destroy fired.
            let refocus = output
                .as_deref()
                .and_then(|out| self.active_frame_surface_id(out))
                .or_else(|| {
                    self.pointer_output
                        .as_ref()
                        .and_then(|o| self.active_frame_surface_id(&o.name()))
                })
                .or_else(|| self.first_active_frame_surface_id());
            debug!("Toplevel {} destroyed: refocus -> {:?}", id, refocus);
            return refocus;
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

        register_display_source(event_loop, display);

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
            if module::DEBUG_MODE.load(std::sync::atomic::Ordering::Relaxed) {
                debug!(
                    "Surface {} info changed: app='{}' title='{}'",
                    id, app_id, title
                );
            }
            self.surface_info.insert(
                id,
                SurfaceInfo {
                    app_id: app_id.clone(),
                    title: title.clone(),
                },
            );
            // Skip Title event for Emacs surfaces; Emacs already knows its own titles
            if !self.is_emacs_frame(id) {
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

            // unmapped->mapped transition: seed the open-fade once. The
            // 16ms delay holds alpha at 0 for one vblank so the empty/white
            // initial buffer some clients ship is rendered invisibly.
            let is_mapped = smithay::backend::renderer::utils::with_renderer_surface_state(
                &root_surface,
                |state| state.buffer().is_some(),
            )
            .unwrap_or(false);
            if is_mapped && self.ewm.unmapped_surfaces.remove(&id) {
                self.ewm.surface_open_anims.insert(
                    id,
                    strip::ViewAnimation::delayed(
                        self.ewm.animations_clock.clone(),
                        std::time::Duration::from_millis(16),
                        0.0,
                        1.0,
                        strip::OPEN_ANIM_DURATION,
                    ),
                );
            }

            self.ewm.queue_redraw_for_surface(id);

            self.ewm.check_surface_info_changes(surface);
            return;
        }

        // 4. Queue redraw for lock surface commits.
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
            use std::io::Write;

            use smithay::reexports::rustix::fs::{fcntl_setfl, OFlags};
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

// Virtual Keyboard (zwp-virtual-keyboard-v1: allows clients like wtype to inject key events)
// Smithay handles key forwarding directly to the focused surface, bypassing the
// compositor's keyboard input pipeline. No VirtualKeyboardHandler impl needed.
delegate_virtual_keyboard_manager!(State);

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
            self.ewm
                .set_focus_frame(refocus_id, "toplevel_destroyed", true);
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
            self.ewm.queue_event(Event::Minimize { id });
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
            self.ewm.queue_event(Event::FullscreenRequest {
                id,
                output: output_name,
            });
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(surface.wl_surface()) {
            self.ewm.queue_event(Event::UnfullscreenRequest { id });
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(surface.wl_surface()) {
            self.ewm.queue_event(Event::MaximizeRequest { id });
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(surface.wl_surface()) {
            self.ewm.queue_event(Event::UnmaximizeRequest { id });
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
                .find(|(w, _)| w.wl_surface().map(|s| *s == surface).unwrap_or(false))
                .map(|(_, id)| id)
            {
                // Lisp owns Emacs frame focus via select-frame-set-input-focus;
                // honoring GTK's echo activation here would loop with our own
                // ewm-focus calls.
                if self.ewm.is_emacs_frame(id) {
                    debug!("xdg_activation: ignored for Emacs frame {}", id);
                } else {
                    self.ewm.set_focus(id, true, "xdg_activation");
                    info!("xdg_activation: granted for surface {}", id);
                }
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

impl XdgForeignHandler for State {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.ewm.xdg_foreign_state
    }
}
delegate_xdg_foreign!(State);

// Foreign toplevel management protocol (exposes windows to external tools)
impl ForeignToplevelHandler for State {
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState {
        &mut self.ewm.foreign_toplevel_state
    }

    fn is_read_only_surface(&self, wl_surface: &WlSurface) -> bool {
        self.ewm
            .find_window_by_surface(wl_surface)
            .is_some_and(|(_, id)| self.ewm.is_emacs_frame(id))
    }

    fn activate(&mut self, wl_surface: WlSurface) {
        if let Some(&id) = self
            .ewm
            .window_ids
            .iter()
            .find(|(w, _)| w.wl_surface().map(|s| *s == wl_surface).unwrap_or(false))
            .map(|(_, id)| id)
        {
            self.ewm.set_focus(id, true, "foreign_toplevel");
            info!("Foreign toplevel: activated surface {}", id);
        }
    }

    fn close(&mut self, wl_surface: WlSurface) {
        if let Some((window, _)) = self
            .ewm
            .window_ids
            .iter()
            .find(|(w, _)| w.wl_surface().map(|s| *s == wl_surface).unwrap_or(false))
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
            self.ewm.queue_event(Event::FullscreenRequest {
                id,
                output: output_name,
            });
        }
    }

    fn unset_fullscreen(&mut self, wl_surface: WlSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(&wl_surface) {
            self.ewm.queue_event(Event::UnfullscreenRequest { id });
        }
    }

    fn set_maximized(&mut self, wl_surface: WlSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(&wl_surface) {
            self.ewm.queue_event(Event::MaximizeRequest { id });
        }
    }

    fn unset_maximized(&mut self, wl_surface: WlSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(&wl_surface) {
            self.ewm.queue_event(Event::UnmaximizeRequest { id });
        }
    }

    fn minimize(&mut self, wl_surface: WlSurface) {
        if let Some((_, id)) = self.ewm.find_window_by_surface(&wl_surface) {
            self.ewm.queue_event(Event::Minimize { id });
        }
    }
}
delegate_foreign_toplevel!(State);

// Workspace protocol (ext-workspace-v1: Emacs frames as workspaces)
impl WorkspaceHandler for State {
    fn workspace_manager_state(&mut self) -> &mut WorkspaceManagerState {
        &mut self.ewm.workspace_state
    }

    fn activate_workspace(&mut self, output: String, frame_index: usize) {
        self.ewm.queue_event(Event::ActivateWorkspace {
            output,
            frame_index,
        });
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
        // send protocol updates to clients, after the Dispatch handler
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
                self.ewm.set_focus(id, false, "unlock");
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

// Idle inhibit protocol (zwp-idle-inhibit-v1)
impl IdleInhibitHandler for State {
    fn inhibit(&mut self, surface: WlSurface) {
        self.ewm.idle_inhibiting_surfaces.insert(surface);
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.ewm.idle_inhibiting_surfaces.remove(&surface);
    }
}
delegate_idle_inhibit!(State);

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
/// Display is owned by the event loop (via Generic source), not by State.
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

    /// Synchronize Wayland keyboard focus with current state.
    ///
    /// Called unconditionally every frame, and before key forwarding in
    /// input handlers. Derives the correct focus from layer shell
    /// interactivity and focused_surface_id. Only calls keyboard.set_focus()
    /// when the resolved focus differs from current.
    pub fn sync_keyboard_focus(&mut self) {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        let locked = self.ewm.is_locked();
        let lock_surface = if locked {
            self.ewm.lock_surface_focus()
        } else {
            None
        };

        if !locked {
            if let Some(surface) = &self.ewm.layer_shell_on_demand_focus {
                let good = surface.alive()
                    && surface.cached_state().keyboard_interactivity
                        == KeyboardInteractivity::OnDemand;
                if !good {
                    self.ewm.layer_shell_on_demand_focus = None;
                }
            }
        }

        let layer_focus = if locked {
            None
        } else {
            self.ewm.resolve_layer_keyboard_focus()
        };
        let ewm = &self.ewm;
        let new_focus = resolve_keyboard_focus_target(locked, lock_surface, layer_focus, || {
            let id = ewm.focused_surface_id();
            ewm.id_windows
                .get(&id)
                .and_then(|w| w.wl_surface())
                .map(|s| s.into_owned())
        });

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
        self.sync_keyboard_focus();

        self.ewm.refresh_idle_inhibit();

        // Refresh workspace protocol state (pull model: diff source of truth vs mirrors).
        crate::protocols::workspace::refresh::<State>(
            &mut self.ewm.workspace_state,
            &self.ewm.output_strips,
            self.ewm.space.outputs(),
        );

        // Process pending early imports
        let pending_imports: Vec<_> = self.ewm.pending_early_imports.drain(..).collect();
        for surface in pending_imports {
            self.backend.early_import(&surface);
        }

        // Render queued outputs, focused first. Between outputs, check for
        // commands that arrived during render. Process them and defer remaining redraws.
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

        self.ewm.notified_activity_this_iteration = false;
    }

    /// Update the shared state snapshot that Emacs reads synchronously.
    /// Called once per tick, after all command processing and state changes.
    fn update_shared_state(&mut self) {
        // Flush active_outputs first (acquires shared_state lock internally).
        if self.ewm.active_outputs_dirty {
            self.ewm.flush_active_outputs();
        }
        let mut shared = module::shared_state().lock().unwrap();
        shared.focused_surface_id = self.ewm.focused_surface_id();
        shared.focused_frame_id = self.ewm.focused_frame_id;
        shared.pointer_location = self.ewm.pointer_location();
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
            ModuleCommand::FocusOutputDirection { dx, dy } => {
                self.ewm.focus_output_direction(dx, dy);
            }
            ModuleCommand::FocusView { view_id } => {
                if self.ewm.lookup_view(view_id).is_some() {
                    self.ewm.set_focus_view(view_id, "lisp_request", false);
                } else if let Some(frame_id) = self.ewm.frame_with_view_id(view_id) {
                    self.ewm.set_focus_frame(frame_id, "lisp_request", false);
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
                    // Client in disable->enable gap; drain on next Activated.
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
                let json =
                    serde_json::to_string_pretty(&self.ewm.debug_state()).unwrap_or_default();
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
            ModuleCommand::OutputLayout { output, frames } => {
                self.ewm.apply_output_layout(&output, frames);
                // Re-evaluate tracked layout entry UID (entries got fresh UIDs)
                let (px, py) = self.ewm.pointer_location();
                self.ewm.pointer_view_id = self.ewm.view_id_under((px, py).into());
                // Re-evaluate pointer focus after layout change
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
                        kb.xkb_layouts.clone().map(|layouts| {
                            (layouts, kb.xkb_variants.clone(), kb.xkb_options.clone())
                        })
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
                if let Some((layouts_str, variants, options)) = kb_xkb {
                    let layout_names: Vec<String> = layouts_str
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if !layout_names.is_empty() {
                        let variants_str = variants.as_deref().unwrap_or("");
                        let xkb_config = smithay::input::keyboard::XkbConfig {
                            layout: &layouts_str,
                            variant: variants_str,
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
                                "Configured XKB layouts: {:?}, variants: {:?}, options: {:?}",
                                layout_names, variants, options
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
            ModuleCommand::ConfigureCursorHide {
                timeout_secs,
                hide_when_typing,
            } => {
                self.ewm
                    .configure_cursor_hide(timeout_secs.map(Duration::from_secs), hide_when_typing);
            }
            ModuleCommand::ConfigureFocusFollowsMouse { state: setting } => {
                self.ewm.focus_follows_mouse_mode = setting;
            }
            ModuleCommand::ConfigureUnfocusedAlpha { alpha } => {
                self.ewm.unfocused_alpha = alpha.clamp(0.0, 1.0);
            }
            ModuleCommand::ConfigureAnimations { enabled } => {
                self.ewm.animations_clock.set_complete_instantly(!enabled);
            }
            ModuleCommand::SetIdleInhibited { inhibited } => {
                self.ewm.manual_idle_inhibited = inhibited;
            }

            ModuleCommand::Initialized => {
                // Force-resend keyboard.enter; PGTK silently dropped the
                // initial one (sent before its first surface roundtrip).
                info!("Client initialized; replaying keyboard focus");
                self.ewm.keyboard_focus = None;
                self.sync_keyboard_focus();
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
            if let Ok(()) = request_data_device_client_selection(&state.ewm.seat, mime, write_fd) {
                let tx = state.ewm.worker_event_tx.clone();
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
                            let _ = tx.send(Event::SelectionChanged { text });
                        }
                    }
                });
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

                    // Drain commits queued during the disable->enable gap.
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

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD_A: &str = "HEAD-A";
    const HEAD_B: &str = "HEAD-B";

    fn nz(raw: u64) -> ViewId {
        std::num::NonZeroU64::new(raw).unwrap()
    }

    fn frame_with_entry(frame_surface_id: u64, view_id: ViewId, surface_id: u64) -> strip::Frame {
        let mut frame = strip::Frame::new(frame_surface_id);
        frame.width = 400.0;
        frame.selected_view_id = Some(view_id);
        frame.surfaces.push(LayoutEntry {
            view_id,
            surface_id,
            x: 0,
            y: 0,
            w: 400,
            h: 400,
            primary: false,
            fullscreen: false,
        });
        frame
    }

    fn frame_with_width(surface_id: u64, width: f64) -> strip::Frame {
        let mut frame = strip::Frame::new(surface_id);
        frame.width = width;
        frame
    }

    fn window_position(fix: &testing::Fixture, surface_id: u64) -> Option<Point<i32, Logical>> {
        let ewm = fix.ewm_ref();
        let window = ewm.id_windows.get(&surface_id)?;
        ewm.window_global_position(window)
    }

    #[test]
    fn surface_output_and_position_prefer_focused_layout_entry() {
        let mut fix = testing::Fixture::new().unwrap();
        let mut client = fix.new_client().unwrap();
        fix.roundtrip(&mut client);
        fix.add_output(HEAD_A, 1600, 900);
        fix.add_output(HEAD_B, 1600, 900);

        let toplevel = fix.open_toplevel(&mut client, "multi-view", Size::from((400, 400)));
        let surface_id = fix.find_surface_id(&toplevel.surface).unwrap();
        let view_a = nz(1001);
        let view_b = nz(1002);
        fix.ewm()
            .apply_output_layout(HEAD_A, vec![frame_with_entry(100, view_a, surface_id)]);
        fix.ewm()
            .apply_output_layout(HEAD_B, vec![frame_with_entry(200, view_b, surface_id)]);

        fix.ewm().set_focus_view(view_b, "test", false);
        assert_eq!(fix.ewm_ref().surface_output_name(surface_id), Some(HEAD_B));
        assert_eq!(
            window_position(&fix, surface_id),
            Some(Point::from((1600, 0)))
        );

        fix.ewm().set_focus_view(view_a, "test", false);
        assert_eq!(fix.ewm_ref().surface_output_name(surface_id), Some(HEAD_A));
        assert_eq!(window_position(&fix, surface_id), Some(Point::from((0, 0))));
    }

    #[test]
    fn emacs_frame_origin_uses_screen_relative_strip_position() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        fix.ewm().apply_output_layout(
            HEAD_A,
            vec![frame_with_width(100, 400.0), frame_with_width(101, 400.0)],
        );
        fix.ewm().animations_clock.set_complete_instantly(true);

        fix.ewm().set_focus_frame(101, "test", false);

        assert_eq!(
            fix.ewm_ref().emacs_frame_origin(101),
            Some(Point::from((0, 0))),
        );
        assert_eq!(
            fix.ewm_ref().emacs_frame_origin(100),
            Some(Point::from((-400, 0))),
        );
    }
}

// Emacs dynamic module initialization
emacs::plugin_is_GPL_compatible! {}

#[emacs::module(name = "ewm-core", defun_prefix = "ewm", mod_in_name = false)]
fn init(_: &emacs::Env) -> emacs::Result<()> {
    Ok(())
}
