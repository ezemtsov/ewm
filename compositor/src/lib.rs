//! EWM - Emacs Wayland Manager
//!
//! Wayland compositor core library.
//!
//! # Design Invariants
//!
//! 1. **Surfaces and views are distinct**: A Wayland surface is a client-owned object; a view is
//!    one compositor-owned placement of that surface. The same surface may appear as multiple
//!    views.
//!
//! 2. **Focus follows the frame/view model**: Logical focus is on an Emacs frame, optionally
//!    narrowed to one view inside that frame. The keyboard surface is derived from that logical
//!    focus.
//!
//! 3. **Strips define the visible world**: Per-output strips are the compositor's model of which
//!    frames and views exist on each output. Rendering, hit-testing, output membership, and
//!    protocol mirrors derive from strips.
//!
//! 4. **Wayland destruction is authoritative**: Layout can move, add, or omit views and frames, but
//!    surface destruction is the final signal that removes compositor state tied to that surface.
//!
//! 5. **Emacs owns policy; Rust preserves invariants**: Emacs decides layout and focus intent. Rust
//!    applies those decisions, handles Wayland protocol state, and repairs local state when async
//!    ordering would otherwise break invariants.
//!
//! 6. **Rendering state is per-output**: Each output advances independently through its redraw
//!    state machine.

pub mod backend;
pub mod cursor;
#[cfg(feature = "screencast")]
pub mod dbus;
pub mod dbus_screensaver;
pub mod event;
pub mod floating;
mod floating_move_grab;
mod floating_resize_grab;
mod floating_shadow;
mod frame_click_grab;
pub mod frame_clock;
pub mod gtk;
pub mod handlers;
pub mod im;
pub mod input;
mod module;
pub mod output_mode;
pub mod overview;
pub mod protocols;
pub mod render;
mod render_helpers;
#[cfg(feature = "screencast")]
pub mod screencasting;
mod shadow_style;
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
use std::os::unix::net::UnixStream;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub use backend::{Backend, DrmBackendState, HeadlessBackend};
pub use event::{Event, OutputInfo, OutputMode};
use im::repeat::{HeldKeyOwner, InterceptRepeat, InterceptRepeatState, TextInputKey};
use serde::{Deserialize, Serialize};
use smithay::backend::input::{InputTime, KeyState, TabletToolDescriptor};
use smithay::backend::renderer::element::solid::SolidColorBuffer;
use smithay::backend::renderer::element::utils::select_dmabuf_feedback;
use smithay::backend::renderer::element::{
    default_primary_scanout_output_compare, RenderElementStates,
};
use smithay::desktop::utils::{
    bbox_from_surface_tree, output_update, send_dmabuf_feedback_surface_tree,
    send_frames_surface_tree, surface_presentation_feedback_flags_from_states,
    surface_primary_scanout_output, take_presentation_feedback_surface_tree,
    under_from_surface_tree, update_surface_primary_scanout_output, OutputPresentationFeedback,
};
use smithay::desktop::{
    find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
    LayerSurface as DesktopLayerSurface, PopupGrab, PopupKeyboardGrab, PopupKind, PopupManager,
    PopupPointerGrab, PopupUngrabStrategy, Space, Window, WindowSurfaceType,
};
use smithay::input::dnd::{self, DnDGrab, DndGrabHandler, DndTarget};
use smithay::input::keyboard::xkb::keysyms;
use smithay::input::keyboard::{KeyboardHandle, ModifiersState};
use smithay::input::pointer::{
    ClickGrab, CursorImageStatus, CursorImageSurfaceData, PointerHandle,
};
use smithay::input::tablet::TabletSeatHandler;
use smithay::input::touch::TouchHandle;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{self, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::Timer;
use smithay::reexports::calloop::{
    Interest, LoopHandle, LoopSignal, Mode as CalloopMode, PostAction, RegistrationToken,
};
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState;
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::{
    Mode as KdeDecorationMode, OrgKdeKwinServerDecoration,
};
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeDefaultMode;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use smithay::reexports::wayland_server::backend::{
    ClientData, ClientId, Credentials, DisconnectReason,
};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource, WEnum};
use smithay::utils::{IsAlive, Logical, Point, Rectangle, Size, Transform, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::background_effect::BackgroundEffectState;
use smithay::wayland::compositor::{
    get_parent, is_sync_subsurface, with_surface_tree_downward, CompositorClientState,
    CompositorHandler, CompositorState, SurfaceData, TraversalAction,
};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
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
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::wayland::shell::wlr_layer::{Layer, WlrLayerShellHandler, WlrLayerShellState};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::tablet_manager::TabletManagerState;
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::wayland::xdg_foreign::{XdgForeignHandler, XdgForeignState};
use smithay::wayland::xdg_system_bell::{XdgSystemBellHandler, XdgSystemBellState};
use tracing::{debug, error, info, trace, warn};

use crate::protocols::foreign_toplevel::{
    ForeignToplevelHandler, ForeignToplevelManagerState, WindowInfo,
};
use crate::protocols::output_management::{OutputManagementHandler, OutputManagementState};
use crate::protocols::screencopy::{Screencopy, ScreencopyHandler, ScreencopyManagerState};
use crate::protocols::virtual_pointer::{
    VirtualPointerAxisEvent, VirtualPointerButtonEvent, VirtualPointerHandler,
    VirtualPointerInputBackend, VirtualPointerManagerState, VirtualPointerMotionAbsoluteEvent,
    VirtualPointerMotionEvent,
};
use crate::protocols::workspace::{WorkspaceHandler, WorkspaceManagerState};

fn surface_root(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    root
}

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
    /// Whether this output should be rendered by the immediate redraw loop.
    fn is_render_queued(&self) -> bool {
        matches!(
            self,
            RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
        )
    }

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
/// Kept separate from `cursor_manager` so client cursor requests are
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
    pub mode: Option<output_mode::Mode>,
    /// Explicit DRM modeline; takes precedence over `mode` when set
    pub modeline: Option<output_mode::Modeline>,
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
            modeline: None,
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
    /// Fill of the output behind its frames.
    pub background: SolidColorBuffer,
    /// Backdrop behind the zoomed-out output in the overview.
    pub overview_backdrop: SolidColorBuffer,
    /// Shadow under the zoomed-out output in the overview.
    pub overview_shadow: floating_shadow::Shadow,
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
            background: SolidColorBuffer::new(size, overview::DEFAULT_BACKGROUND_COLOR),
            overview_backdrop: SolidColorBuffer::new(size, overview::DEFAULT_BACKDROP_COLOR),
            overview_shadow: floating_shadow::Shadow::new(overview::shadow_config(f64::from(
                size.1,
            ))),
        }
    }

    /// Resize the lock color buffer for this output
    pub fn resize_lock_buffer(&mut self, size: (i32, i32)) {
        self.lock_color_buffer.resize(size);
    }

    /// Resize the fullscreen and overview backdrops for this output.
    pub fn resize_backdrops(&mut self, size: (i32, i32)) {
        self.fullscreen_backdrop.resize(size);
        self.background.resize(size);
        self.overview_backdrop.resize(size);
        self.overview_shadow
            .update_config(overview::shadow_config(f64::from(size.1)));
    }
}

impl Default for OutputState {
    fn default() -> Self {
        let mut state = Self::new("default", Some(Duration::from_micros(16_667)), (1920, 1080));
        state.redraw_state = RedrawState::Idle;
        state
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
pub type LayoutEntryId = std::num::NonZeroU64;
pub type FocusId = std::num::NonZeroU64;

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct LayoutEntry {
    /// Entry identity assigned by lisp; names one Emacs window in a frame.
    pub id: LayoutEntryId,
    /// Buffer name shown in this Emacs window.
    #[serde(default)]
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Surface-specific attachment when this Emacs window displays a Wayland surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<LayoutEntrySurface>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct LayoutEntrySurface {
    /// Which Wayland surface this entry displays (analogous to an Emacs buffer).
    pub surface_id: u64,
    /// Largest view per surface, computed by compositor from entry dimensions.
    /// Drives send_configure() size + scale and native-size rendering.
    pub primary: bool,
}

impl LayoutEntry {
    pub fn emacs_window(id: LayoutEntryId, name: String, x: i32, y: i32, w: u32, h: u32) -> Self {
        Self {
            id,
            name,
            x,
            y,
            w,
            h,
            surface: None,
        }
    }

    pub fn surface(
        id: LayoutEntryId,
        name: String,
        surface_id: u64,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Self {
        Self {
            id,
            name,
            x,
            y,
            w,
            h,
            surface: Some(LayoutEntrySurface {
                surface_id,
                primary: false,
            }),
        }
    }

    pub fn surface_id(&self) -> Option<u64> {
        self.surface.as_ref().map(|surface| surface.surface_id)
    }

    pub fn primary(&self) -> bool {
        self.surface.as_ref().is_some_and(|surface| surface.primary)
    }

    pub fn set_primary(&mut self, primary: bool) {
        if let Some(surface) = &mut self.surface {
            surface.primary = primary;
        }
    }
}

struct LayoutEntryHit<'a> {
    entry: &'a LayoutEntry,
    fullscreen: bool,
    rect: Rectangle<i32, Logical>,
    output_size: Size<i32, Logical>,
}

pub(crate) type SurfaceHit = (WlSurface, Point<f64, Logical>);

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

#[cfg(feature = "screencast")]
fn window_properties(info: &SurfaceInfo) -> dbus::WindowProperties {
    dbus::WindowProperties {
        title: info.title.clone(),
        app_id: info.app_id.clone(),
    }
}

#[cfg(feature = "screencast")]
fn layout_entry_window_title(
    output_name: &str,
    workspace_name: &str,
    entry: &LayoutEntry,
    fallback_title: &str,
) -> String {
    let location_name = match (output_name.is_empty(), workspace_name.is_empty()) {
        (true, true) => String::new(),
        (true, false) => workspace_name.to_string(),
        (false, true) => output_name.to_string(),
        (false, false) => format!("{output_name} | {workspace_name}"),
    };
    let entry_name = if entry.name.is_empty() {
        fallback_title
    } else {
        entry.name.as_str()
    };
    match (location_name.is_empty(), entry_name.is_empty()) {
        (true, true) => String::new(),
        (true, false) => entry_name.to_string(),
        (false, true) => location_name,
        (false, false) => format!("{location_name} | {entry_name}"),
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
    pub key: String,
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
    #[serde(default)]
    pub dispatch: InterceptDispatch,
    #[serde(default)]
    pub repeat: bool,
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
        let raw_match = self.keysym == raw_keysym
            || ((keysyms::KEY_A..=keysyms::KEY_Z).contains(&raw_keysym)
                && self.keysym == raw_keysym - keysyms::KEY_A + keysyms::KEY_a);

        if raw_match {
            return self.ctrl == mods.ctrl
                && self.alt == mods.alt
                && (self.shift == mods.shift
                    || (keysyms::KEY_A..=keysyms::KEY_Z).contains(&raw_keysym))
                && self.logo == mods.logo;
        }

        if raw_keysym != modified_keysym && self.keysym == modified_keysym {
            return self.ctrl == mods.ctrl && self.alt == mods.alt && self.logo == mods.logo;
        }

        false
    }
}

/// How an intercepted key is delivered to Emacs.
///
/// Keyboard delivery temporarily gives Emacs Wayland keyboard focus and lets
/// Emacs's command loop read the following key sequence. Command delivery
/// executes the resolved Emacs binding through the event pipe, so Wayland
/// keyboard focus stays on the client surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum InterceptDispatch {
    #[default]
    Keyboard,
    Command,
    /// Translate to a different key delivered to a non-Emacs surface.
    Translate(TranslateTarget),
}

/// Target key delivered to a non-Emacs surface in place of a matched key:
/// a keysym plus the modifiers the surface should see.
///
/// Example: Super+c is matched and Ctrl+c (clipboard copy) is delivered; or
/// Ctrl+j delivers Down. When Emacs has focus the matched key works normally
/// via `ewm-mode-map`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct TranslateTarget {
    pub keysym: u32,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(rename = "super", default)]
    pub logo: bool,
}

/// DnD icon surface attached to the pointer during a drag operation.
#[derive(Debug)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

const TITLE_EVENT_IDLE_FLUSH_DELAY: Duration = Duration::from_millis(500);
const TITLE_EVENT_MAX_STALE: Duration = Duration::from_millis(1000);

// Coalesces compositor -> Lisp title notifications only. `surface_info`
// remains exact and is updated before anything reaches this queue.
struct PendingTitleEvent {
    app: String,
    title: String,
    queued_at: Instant,
}

pub struct Ewm {
    pub stop_signal: Option<LoopSignal>,
    pub space: Space<Window>,
    pub display_handle: DisplayHandle,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    pub kde_decoration_state: KdeDecorationState,
    pub background_effect_state: BackgroundEffectState,
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
    /// Cached touch handle, present only while a touch-capable device exists.
    pub touch: Option<TouchHandle<State>>,
    pub tablet_cursor_location: Option<Point<f64, Logical>>,

    // Surface tracking
    next_surface_id: u64,
    pub window_ids: HashMap<Window, u64>,
    pub id_windows: HashMap<u64, Window>,
    surface_info: HashMap<u64, SurfaceInfo>,
    pending_title_events: HashMap<u64, PendingTitleEvent>,
    title_flush_timer_token: Option<RegistrationToken>,
    last_emacs_keyboard_activity: Instant,
    /// Compositor-owned Emacs frame set. Mapped strips are currently visible;
    /// homeless frames wait for any output when none is mapped.
    pub frame_set: FrameSet,
    /// Per-output floating Emacs frame spaces, attached to the output's strip/workspace.
    pub floating_spaces: HashMap<String, floating::FloatingSpace>,
    /// Shared "complete instantly" toggle threaded into every `ViewAnimation`
    /// constructed under this `Ewm`.  Flipped by `ConfigureAnimations`.
    pub animations_clock: strip::AnimationsClock,
    /// Zoomed-out strip view shared by all outputs.
    pub overview: overview::Overview,
    /// Frames' slide off the top of every output: 0 in place, 1 fully off.
    pub hide: strip::ViewOffset,
    /// Reverse index: surface_id -> set of output names it appears on
    pub surface_outputs: HashMap<u64, HashSet<String>>,
    /// Per-surface open-fade, seeded on the first buffer-bearing commit.
    /// Entries GC'd in `advance_animations` once finished or surface gone.
    pub surface_open_anims: HashMap<u64, strip::ViewAnimation>,
    /// Toplevels that haven't yet committed a buffer. One-shot gate for
    /// seeding `surface_open_anims`: drained on the unmapped->mapped
    /// transition so the fade starts exactly once per surface lifetime.
    pub unmapped_surfaces: HashSet<u64>,
    /// App surfaces that requested attention.
    ///
    /// ext-workspace urgency is derived from this set and the frame layout.
    /// Focus clears the bit, matching the per-window urgency model.
    pub urgent_surfaces: HashSet<u64>,
    /// Emacs frame workspaces explicitly marked as requiring attention.
    ///
    /// This covers terminal/editor activity inside an Emacs frame, where no
    /// child Wayland surface exists for `urgent_surfaces` to track.
    pub urgent_workspaces: HashSet<u64>,
    /// Valid activation requests that arrived before the surface was attached
    /// to an Emacs frame.  Drained after layout refresh makes them focusable.
    pub pending_activation_surfaces: HashSet<u64>,

    // Output
    pub output_size: Size<i32, Logical>,
    /// Mapped compositor outputs in deterministic spatial order.
    pub sorted_outputs: Vec<Output>,
    /// Desired output configuration, keyed by output name.
    /// Looked up when outputs connect; updated by Emacs commands.
    pub output_config: HashMap<String, OutputConfig>,

    // Input
    /// Surface under the pointer + its global origin, for constraint checks.
    pub pointer_focus: Option<SurfaceHit>,
    /// Last output the cursor was drawn on, for per-output cursor redraw.
    pointer_output: Option<Output>,
    pub focused_frame_id: u64,
    pub focused_entry_id: Option<LayoutEntryId>,
    pub keyboard_focus: Option<WlSurface>,
    pub(crate) im_repeat: InterceptRepeatState,
    /// Output geometry or working areas changed; shared active_outputs need recomputation.
    pub active_outputs_dirty: bool,
    /// Introspected window list changed; announce it next tick.
    pub introspect_windows_dirty: bool,
    /// Focus follows mouse
    pub focus_follows_mouse_mode: bool,
    /// Emacs is tracking a drag source (`track-mouse' is `drag-source').
    pub emacs_drag_source: bool,
    /// Alpha multiplier for inactive layout entries; 1.0 disables.
    pub unfocused_alpha: f32,
    /// Blur settings for surfaces that request background blur.
    pub blur_config: shadow_style::Blur,
    /// UID of the layout entry currently under the pointer (for FFM view detection).
    pub pointer_entry_id: Option<LayoutEntryId>,

    // Libinput device configuration
    pub input_configs: Vec<input::InputConfigEntry>,

    // PID for matching Emacs-owned wl_surfaces in `new_toplevel` (vs external
    // applications); is-this-an-emacs-frame queries derive from strip membership.
    pub emacs_pid: Option<u32>,

    /// In-flight 3-finger swipe gesture for strip scrolling.
    pub gesture: input::GestureState,

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

    // Virtual pointer state (zwlr-virtual-pointer-v1 protocol)
    pub virtual_pointer_state: VirtualPointerManagerState,

    // When true, intercept all keys and send to Emacs for text input
    pub text_input_intercept: bool,
    // Deduplicates relay activate/deactivate events and holds commits queued
    // during the client's disable->enable gap.
    pub im_text_input: im::text_input::TextInputCommitState,

    // Popup manager for XDG popups
    pub popups: PopupManager,

    // Active popup (menu) grab; suppresses EWM's own focus routing while live.
    pub popup_grab: Option<PopupGrab<State>>,

    // DnD icon surface (shown at pointer during drag)
    pub dnd_icon: Option<DndIcon>,
    // Drop location from `dropped`, focused by the input handler outside the pointer lock.
    pub pending_drop: Option<Point<f64, Logical>>,

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

    pub xdg_system_bell_state: XdgSystemBellState,

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

    // Cursor shape protocol (wp-cursor-shape-v1)
    #[allow(dead_code)]
    pub cursor_shape_manager_state: CursorShapeManagerState,

    // Tablet protocol (zwp-tablet-v2)
    #[allow(dead_code)]
    pub tablet_manager_state: TabletManagerState,

    // Screencasting state: PipeWire connection, active casts,
    // window->output cache.
    #[cfg(feature = "screencast")]
    pub casting: screencasting::Screencasting,

    // D-Bus servers (must be kept alive for interfaces to work)
    #[cfg(feature = "screencast")]
    pub dbus_servers: Option<dbus::DBusServers>,

    // Channel to reply to Introspect GetWindows requests
    #[cfg(feature = "screencast")]
    pub introspect_reply_tx: Option<async_channel::Sender<dbus::CompositorToIntrospect>>,

    // XKB layout state
    pub xkb_layout_names: Vec<String>,
    pub xkb_current_layout: usize,

    // Event loop handle. Resource destructors must not send events (aborts
    // on a dying client); defer event-sending cleanup via insert_idle.
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

    pub cursor_manager: cursor::CursorManager,

    /// When `Some`, `queue_event` mirrors each event here.
    pub captured_events: Option<Vec<Event>>,
}

#[derive(Debug)]
struct FrameMigration {
    id: u64,
    prev_output: Option<String>,
    previous_frame: Option<strip::Frame>,
}

#[derive(Debug)]
struct OutputLayoutSnapshot {
    output_name: String,
    active_frame_id: Option<u64>,
    entry_surface_ids: HashSet<u64>,
}

#[derive(Debug)]
struct EntryLocation {
    output: String,
    frame_idx: usize,
    floating: bool,
    frame_surface_id: u64,
    surface_id: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LayoutEntryLocation<'a> {
    pub(crate) output_name: &'a str,
    pub(crate) frame_idx: usize,
    pub(crate) floating: bool,
    pub(crate) entry: &'a LayoutEntry,
}

#[derive(Debug, Clone, Copy)]
struct FrameLocation<'a> {
    output_name: &'a str,
    frame_idx: usize,
    strip: Option<&'a strip::Strip>,
    frame: &'a strip::Frame,
}

#[derive(Debug, Default)]
pub struct FrameSet {
    mapped: HashMap<String, strip::Strip>,
    /// Frames with no mapped output; the next output to map adopts them all.
    homeless: Vec<strip::Frame>,
    /// Frame surface id -> home output identity (`output_identity`, not the
    /// connector name), so a frame returns to its origin even when the display
    /// re-enumerates on a different connector after a replug.
    home_outputs: HashMap<u64, String>,
    /// Last active frame per output identity (`output_identity`, matching
    /// `home_outputs`), restored when the display maps again -- across a
    /// connector rename, not just the same connector.
    last_active: HashMap<String, u64>,
}

impl FrameSet {
    pub fn mapped_strips(&self) -> &HashMap<String, strip::Strip> {
        &self.mapped
    }

    pub fn homeless_frames(&self) -> &[strip::Frame] {
        &self.homeless
    }

    pub fn mapped_strip(&self, output_name: &str) -> Option<&strip::Strip> {
        self.mapped.get(output_name)
    }

    pub fn mapped_strip_mut(&mut self, output_name: &str) -> Option<&mut strip::Strip> {
        self.mapped.get_mut(output_name)
    }

    pub fn ensure_mapped_strip(
        &mut self,
        output_name: &str,
        clock: strip::AnimationsClock,
    ) -> &mut strip::Strip {
        self.mapped
            .entry(output_name.to_string())
            .or_insert_with(|| strip::Strip::new(clock))
    }

    pub fn insert_mapped_strip(
        &mut self,
        output_name: String,
        strip: strip::Strip,
    ) -> Option<strip::Strip> {
        self.mapped.insert(output_name, strip)
    }

    pub fn mapped_strips_mut(&mut self) -> impl Iterator<Item = &mut strip::Strip> {
        self.mapped.values_mut()
    }

    pub fn all_frames(&self) -> impl Iterator<Item = &strip::Frame> {
        self.mapped
            .values()
            .flat_map(|strip| strip.frames.iter())
            .chain(self.homeless.iter())
    }

    fn all_frames_mut(&mut self) -> impl Iterator<Item = &mut strip::Frame> {
        self.mapped
            .values_mut()
            .flat_map(|strip| strip.frames.iter_mut())
            .chain(self.homeless.iter_mut())
    }

    fn frame_mut(&mut self, frame_surface_id: u64) -> Option<&mut strip::Frame> {
        self.all_frames_mut()
            .find(|frame| frame.surface_id == frame_surface_id)
    }

    fn is_homeless(&self, frame_surface_id: u64) -> bool {
        self.homeless
            .iter()
            .any(|frame| frame.surface_id == frame_surface_id)
    }

    /// Take every homeless frame for adoption by a newly mapped output.
    fn take_homeless_frames(&mut self) -> Vec<strip::Frame> {
        mem::take(&mut self.homeless)
    }

    fn remove_homeless_frame(&mut self, frame_surface_id: u64) -> bool {
        let len = self.homeless.len();
        self.homeless
            .retain(|frame| frame.surface_id != frame_surface_id);
        self.homeless.len() != len
    }

    /// Park a mapped strip's frames in the homeless pool.
    fn pool_mapped_strip(&mut self, output_name: &str) {
        let Some(mut strip) = self.mapped.remove(output_name) else {
            return;
        };
        self.homeless.append(&mut strip.frames);
    }

    fn remember_last_active(&mut self, identity: &str, frame_id: Option<u64>) {
        match frame_id {
            Some(id) => {
                self.last_active.insert(identity.to_string(), id);
            }
            None => {
                self.last_active.remove(identity);
            }
        }
    }

    fn take_last_active(&mut self, identity: &str) -> Option<u64> {
        self.last_active.remove(identity)
    }

    fn remember_frame_homes(&mut self, home: &str, frame_ids: impl IntoIterator<Item = u64>) {
        for id in frame_ids {
            if id != 0 {
                self.remember_home_if_missing(id, home);
            }
        }
    }

    fn remember_home_if_missing(&mut self, frame_id: u64, home: &str) {
        self.home_outputs
            .entry(frame_id)
            .or_insert_with(|| home.to_string());
    }

    fn forget_frame_home(&mut self, frame_id: u64) {
        self.home_outputs.remove(&frame_id);
    }

    fn home_output(&self, frame_id: u64) -> Option<&str> {
        self.home_outputs.get(&frame_id).map(String::as_str)
    }
}

/// Whether a toplevel opens floating: parented (dialog), portal or fixed-height.
fn compute_open_floating(toplevel: &ToplevelSurface) -> bool {
    use smithay::wayland::compositor::with_states;
    use smithay::wayland::shell::xdg::SurfaceCachedState;

    // Windows with a parent (usually dialogs) open as floating by default.
    if toplevel.parent().is_some() {
        return true;
    }

    // Portal dialogs float even when the requester passed no parent_window.
    if is_portal_surface(toplevel.wl_surface()) {
        return true;
    }

    let (min_size, max_size) = with_states(toplevel.wl_surface(), |state| {
        let mut guard = state.cached_state.get::<SurfaceCachedState>();
        let current = guard.current();
        (current.min_size, current.max_size)
    });

    // We open fixed-height windows as floating.
    min_size.h > 0 && min_size.h == max_size.h
}

fn ensure_min_max_size(x: i32, min: i32, max: i32) -> i32 {
    let x = if max > 0 { x.min(max) } else { x };
    if min > 0 { x.max(min) } else { x }
}

/// The window's geometry clamped to its min/max size hints.
fn natural_open_size(window: &Window) -> Option<Size<i32, Logical>> {
    use smithay::wayland::compositor::with_states;
    use smithay::wayland::shell::xdg::SurfaceCachedState;

    let toplevel = window.toplevel()?;
    let geo = window.geometry().size;
    let (min, max) = with_states(toplevel.wl_surface(), |state| {
        let mut guard = state.cached_state.get::<SurfaceCachedState>();
        let current = guard.current();
        (current.min_size, current.max_size)
    });
    let w = ensure_min_max_size(geo.w, min.w, max.w);
    let h = ensure_min_max_size(geo.h, min.h, max.h);
    (w > 0 && h > 0).then(|| Size::from((w, h)))
}

impl Ewm {
    pub fn new(
        display_handle: DisplayHandle,
        loop_handle: LoopHandle<'static, State>,
        is_drm: bool,
        cursor_config: cursor::CursorConfig,
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
        let kde_decoration_state =
            KdeDecorationState::new::<State>(&display_handle, KdeDefaultMode::Server);
        let background_effect_state = BackgroundEffectState::new::<State>(&display_handle);
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
            .add_keyboard(
                Default::default(),
                input::DEFAULT_KEYBOARD_REPEAT_DELAY,
                input::DEFAULT_KEYBOARD_REPEAT_RATE,
            )
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

        // Initialize virtual pointer (zwlr-virtual-pointer-v1)
        let virtual_pointer_state =
            VirtualPointerManagerState::new::<State, _>(&display_handle, |_| true);

        // Initialize layer shell for panels, notifications, etc.
        let layer_shell_state = WlrLayerShellState::new::<State>(&display_handle);

        // Initialize xdg-activation for focus requests
        let activation_state = XdgActivationState::new::<State>(&display_handle);

        let xdg_foreign_state = XdgForeignState::new::<State>(&display_handle);

        let xdg_system_bell_state = XdgSystemBellState::new::<State>(&display_handle);

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

        // Initialize cursor shape protocol (wp-cursor-shape-v1)
        let cursor_shape_manager_state = CursorShapeManagerState::new::<State>(&display_handle);

        // Initialize tablet protocol (zwp-tablet-v2)
        let tablet_manager_state = TabletManagerState::new::<State>(&display_handle);

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

        let animations_clock = strip::AnimationsClock::default();

        Self {
            stop_signal: None,
            space: Space::default(),
            display_handle,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            kde_decoration_state,
            background_effect_state,
            shm_state,
            dmabuf_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            data_control_state,
            seat,
            pointer,
            keyboard,
            touch: None,
            tablet_cursor_location: None,
            next_surface_id: 1,
            window_ids: HashMap::new(),
            id_windows: HashMap::new(),
            surface_info: HashMap::new(),
            pending_title_events: HashMap::new(),
            title_flush_timer_token: None,
            last_emacs_keyboard_activity: Instant::now(),
            frame_set: FrameSet::default(),
            floating_spaces: HashMap::new(),
            overview: overview::Overview::new(animations_clock.clone()),
            hide: strip::ViewOffset::default(),
            animations_clock,
            surface_outputs: HashMap::new(),
            surface_open_anims: HashMap::new(),
            unmapped_surfaces: HashSet::new(),
            urgent_surfaces: HashSet::new(),
            urgent_workspaces: HashSet::new(),
            pending_activation_surfaces: HashSet::new(),
            output_size: Size::from((0, 0)),
            sorted_outputs: Vec::new(),
            output_config: HashMap::new(),
            pointer_focus: None,
            pointer_output: None,
            focused_frame_id: 0,
            focused_entry_id: None,
            keyboard_focus: None,
            im_repeat: InterceptRepeatState::new(
                input::DEFAULT_KEYBOARD_REPEAT_RATE,
                input::DEFAULT_KEYBOARD_REPEAT_DELAY,
            ),
            active_outputs_dirty: true,
            introspect_windows_dirty: false,
            focus_follows_mouse_mode: false,
            emacs_drag_source: false,
            unfocused_alpha: 1.0,
            blur_config: shadow_style::Blur::default(),
            pointer_entry_id: None,
            input_configs: Vec::new(),
            emacs_pid: None,
            gesture: input::GestureState::default(),
            output_state: HashMap::new(),
            monitors_active: true,
            pending_early_imports: Vec::new(),
            screencopy_state,
            output_manager_state,
            text_input_state,
            input_method_state,
            virtual_keyboard_state,
            virtual_pointer_state,
            text_input_intercept: false,
            im_text_input: im::text_input::TextInputCommitState::default(),
            popups: PopupManager::default(),
            popup_grab: None,
            dnd_icon: None,
            pending_drop: None,
            layer_shell_state,
            unmapped_layer_surfaces: std::collections::HashSet::new(),
            layer_shell_on_demand_focus: None,
            working_areas: HashMap::new(),
            activation_state,
            xdg_foreign_state,
            xdg_system_bell_state,
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
            cursor_shape_manager_state,
            tablet_manager_state,
            #[cfg(feature = "screencast")]
            casting: screencasting::Screencasting::new(&loop_handle_clone),
            #[cfg(feature = "screencast")]
            dbus_servers: None,
            #[cfg(feature = "screencast")]
            introspect_reply_tx: None,
            xkb_layout_names: vec!["us".to_string()],
            xkb_current_layout: 0,
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
            cursor_manager: cursor::CursorManager::new(&cursor_config.theme, cursor_config.size),
            captured_events: None,
            worker_event_tx,
        }
    }

    /// Connect the input method relay over a socketpair of our own.
    pub fn connect_im_relay(&mut self) {
        let (server_end, client_end) =
            UnixStream::pair().expect("failed to create IM relay socketpair");
        // Socketpair peer credentials are our own.
        self.display_handle
            .insert_client(server_end, Arc::new(ClientState::default()))
            .expect("failed to insert IM relay client");
        let channel = im::relay::connect(client_end);
        self.loop_handle
            .insert_source(channel, |event, _, state| {
                if let smithay::reexports::calloop::channel::Event::Msg(event) = event {
                    state.handle_im_event(event);
                }
            })
            .expect("failed to register IM relay event source");
    }

    /// Detach a surface from every layout entry and the reverse index.
    fn detach_surface_from_layouts(&mut self, id: u64) {
        self.surface_outputs.remove(&id);
        for frame in self.frame_set.all_frames_mut() {
            frame.detach_surface(id);
        }
        for space in self.floating_spaces.values_mut() {
            for frame in &mut space.frames {
                frame.detach_surface(id);
            }
        }
    }

    /// Restore the focus invariants after a layout swap or frame removal:
    /// drop stale `selected_entry_id` / `focused_entry_id`, and zero
    /// `focused_frame_id` when its frame no longer exists.
    fn prune_dangling_focus(&mut self) {
        for frame in self.frame_set.all_frames_mut() {
            frame.validate_fullscreen();
            if let Some(sel) = frame.selected_entry_id
                && !frame.entries.iter().any(|entry| entry.id == sel)
            {
                frame.selected_entry_id = None;
            }
        }
        for space in self.floating_spaces.values_mut() {
            for frame in &mut space.frames {
                frame.validate_fullscreen();
                if let Some(sel) = frame.selected_entry_id
                    && !frame.entries.iter().any(|entry| entry.id == sel)
                {
                    frame.selected_entry_id = None;
                }
            }
        }
        if let Some(v) = self.focused_entry_id {
            let in_focused_frame = self
                .frame_location(self.focused_frame_id)
                .map(|location| location.frame)
                .is_some_and(|f| f.entries.iter().any(|entry| entry.id == v));
            if !in_focused_frame {
                self.focused_entry_id = None;
            }
        }
        if self.focused_frame_id != 0 && self.lookup_frame(self.focused_frame_id).is_none() {
            self.focused_frame_id = 0;
            self.focused_entry_id = None;
        }
    }

    /// If no frame is focused but some frame exists, use canonical output order.
    fn focus_any_frame_if_unfocused(&mut self, reason: &'static str) {
        if self.focused_frame_id == 0 {
            for output in &self.sorted_outputs {
                let name = output.name();
                if let Some(id) = self
                    .frame_set
                    .mapped_strip(&name)
                    .and_then(|strip| strip.frames.first())
                    .map(|frame| frame.surface_id)
                {
                    self.set_focus_frame(id, reason, false);
                    return;
                }
            }
        }
    }

    fn focus_mapped_frame_if_focused_output_unmapped(&mut self, output_name: &str) {
        let focused_output = self
            .frame_position(self.focused_surface_id())
            .map(|(focused_output, _)| focused_output);
        let focused_homeless = self.frame_set.is_homeless(self.focused_frame_id);
        if focused_output.as_deref() != Some(output_name) && !focused_homeless {
            return;
        }

        if let Some(id) = self
            .sorted_outputs
            .iter()
            .find_map(|output| self.active_frame_surface_id(&output.name()))
        {
            self.set_focus_frame(id, "output_disabled", false);
        } else {
            self.set_focus_state(0, None);
        }
    }

    /// Drop the Emacs frame `id` from whichever strip holds it.
    fn remove_emacs_frame_from_strip(&mut self, id: u64, active_close: bool) {
        self.frame_set.forget_frame_home(id);
        if self.frame_set.remove_homeless_frame(id) {
            return;
        }
        let Some((out, idx)) = self.lookup_frame(id) else {
            return;
        };
        let pre_entries = self.strip_entries(&out);
        let removed_floating = self
            .floating_spaces
            .get_mut(&out)
            .and_then(|space| space.remove_frame(id).map(|_| space.frames.is_empty()));
        if let Some(empty) = removed_floating {
            if empty {
                self.floating_spaces.remove(&out);
            }
            self.reconcile_output_entries(&out, &pre_entries);
            return;
        }
        let mut prev_active_id = None;
        if let Some(strip) = self.frame_set.mapped_strip_mut(&out) {
            prev_active_id = strip.active_frame().map(|f| f.surface_id);
            if active_close {
                strip.remove_active_frame_at(idx);
            } else {
                strip.remove_frame_at(idx);
            }
        }
        self.reconcile_output_entries(&out, &pre_entries);
        self.reconcile_active_for_output(&out, prev_active_id);
    }

    pub(crate) fn output_exists(&self, output: &Output) -> bool {
        self.output_state.contains_key(output)
    }

    /// Convert a `WlOutput` to an `Output` that EWM still tracks.
    pub(crate) fn output_from_resource(&self, wl_output: &WlOutput) -> Option<Output> {
        Output::from_resource(wl_output).filter(|output| self.output_exists(output))
    }

    /// Find a connected output by name, whether or not it is currently mapped.
    pub(crate) fn connected_output(&self, name: &str) -> Option<Output> {
        self.output_state
            .keys()
            .find(|output| output.name() == name)
            .cloned()
    }

    fn migrate_or_pool_output_frames(&mut self, output_name: &str, output: &Output) {
        let active_id = self.active_frame_surface_id(output_name);
        let home_identity = crate::utils::output_identity(output);
        self.frame_set
            .remember_last_active(&home_identity, active_id);

        if !self.migrate_output_frames_to_fallback(output_name, &home_identity) {
            let pre = self.surface_outputs_for(output_name);
            self.reconcile_output_membership(output_name, Some(output), &pre, &HashSet::new());
            self.leave_emacs_frames_on_output(output);
            if let Some(id) = active_id {
                self.queue_event(Event::FrameHidden { id });
            }
        }
        self.frame_set.pool_mapped_strip(output_name);
    }

    /// Unmap a connected output while keeping its head available for
    /// output-management so it can be re-enabled later.
    pub(crate) fn disable_output(&mut self, output: &Output) {
        let output_name = output.name();
        if self.space.output_geometry(output).is_none() {
            return;
        }

        self.migrate_or_pool_output_frames(&output_name, output);
        self.space.unmap_output(output);
        self.sync_sorted_outputs();
        self.refresh_cursor_outputs();
        self.refresh_primary_assignments();
        self.focus_mapped_frame_if_focused_output_unmapped(&output_name);
        self.recalculate_output_size();
        self.active_outputs_dirty = true;
        self.output_management_state.output_heads_changed = true;
        self.queue_redraw_all();
    }

    /// Clean up all Ewm state for a removed output.
    ///
    /// Called by backends after their own teardown (e.g. DRM surface removal).
    /// Handles: output state, lock check, screencasts, space, layouts,
    /// workspaces, working areas, and output size.
    pub fn remove_output(&mut self, output: &Output) {
        let output_name = output.name();

        // Cancel pending estimated-VBlank timers
        if let Some(output_state) = self.output_state.remove(output)
            && let RedrawState::WaitingForEstimatedVBlank(token)
            | RedrawState::WaitingForEstimatedVBlankAndQueued(token) = output_state.redraw_state
        {
            self.loop_handle.remove(token);
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

        self.migrate_or_pool_output_frames(&output_name, output);
        self.space.unmap_output(output);
        self.working_areas.remove(&output_name);

        self.sync_sorted_outputs();
        self.refresh_cursor_outputs();
        self.focus_mapped_frame_if_focused_output_unmapped(&output_name);

        self.screencopy_state.remove_output(output);

        // Prune dangling focus from the gone strip and hop to a survivor; I5.
        self.prune_dangling_focus();
        #[cfg(feature = "screencast")]
        self.refresh_mapped_cast_outputs();
        self.focus_any_frame_if_unfocused("output_removed");

        self.recalculate_output_size();
        self.send_output_disconnected(&output_name);
        self.output_management_state.output_heads_changed = true;

        info!("Output removed: {}", output_name);
    }

    /// Remove dead windows from the space.
    /// This replaces Space::refresh(). We manage output enter/leave explicitly
    /// rather than relying on automatic spatial overlap detection.
    pub fn cleanup_dead_windows(&mut self) -> bool {
        // Clean dead layout surfaces from id_windows
        let dead_ids: Vec<u64> = self
            .id_windows
            .iter()
            .filter(|(_, w)| !w.alive())
            .map(|(&id, _)| id)
            .collect();
        let removed_any = !dead_ids.is_empty();
        for id in dead_ids {
            if let Some(refocus_id) = self.handle_toplevel_destroyed_by_id(id) {
                self.set_focus_frame(refocus_id, "cleanup_dead_windows", true);
            }
        }

        self.cursor_manager.check_cursor_image_surface_alive();

        removed_any
    }

    /// Build a snapshot of all output states for the output management protocol.
    pub fn build_output_head_states(
        &self,
        output_infos: &[OutputInfo],
    ) -> HashMap<String, protocols::output_management::OutputHeadState> {
        use protocols::output_management::{OutputHeadState, OutputModeState};

        let mut states = HashMap::new();
        for info in output_infos {
            let output = self.mapped_output_by_name(&info.name);
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

    fn queue_surface_request(&mut self, surface: &WlSurface, event: impl FnOnce(u64) -> Event) {
        if let Some(id) = self.surface_id(surface) {
            self.queue_event(event(id));
        }
    }

    fn queue_minimize_request_for_surface(&mut self, surface: &WlSurface) {
        self.queue_surface_request(surface, |id| Event::Minimize { id });
    }

    fn set_fullscreen_for_surface(
        &mut self,
        surface: &WlSurface,
        output: Option<WlOutput>,
        fullscreen: bool,
    ) {
        if let Some(id) = self.surface_id(surface) {
            let output_name = output
                .and_then(|output| self.output_from_resource(&output))
                .map(|output| output.name());
            self.set_surface_fullscreen_on_output(id, output_name.as_deref(), fullscreen);
        }
    }

    fn queue_maximize_request_for_surface(&mut self, surface: &WlSurface) {
        self.queue_surface_request(surface, |id| Event::MaximizeRequest { id });
    }

    fn queue_unmaximize_request_for_surface(&mut self, surface: &WlSurface) {
        self.queue_surface_request(surface, |id| Event::UnmaximizeRequest { id });
    }

    /// Resolve an Emacs window entry id to its strip location and owning frame.
    fn frame_at_location(&self, location: &LayoutEntryLocation) -> Option<&strip::Frame> {
        if location.floating {
            self.floating_spaces
                .get(location.output_name)?
                .frames
                .get(location.frame_idx)
        } else {
            self.frame_set
                .mapped_strip(location.output_name)?
                .frames
                .get(location.frame_idx)
        }
    }

    fn lookup_entry(&self, entry_id: LayoutEntryId) -> Option<EntryLocation> {
        let location = self.layout_entry_for_id(entry_id)?;
        let frame = self.frame_at_location(&location)?;
        Some(EntryLocation {
            output: location.output_name.to_string(),
            frame_idx: location.frame_idx,
            floating: location.floating,
            frame_surface_id: frame.surface_id,
            surface_id: location.entry.surface_id(),
        })
    }

    fn frame_location(&self, frame_surface_id: u64) -> Option<FrameLocation<'_>> {
        for (output_name, strip) in self.frame_set.mapped_strips() {
            if let Some((frame_idx, frame)) = strip
                .frames
                .iter()
                .enumerate()
                .find(|(_, frame)| frame.surface_id == frame_surface_id)
            {
                return Some(FrameLocation {
                    output_name,
                    frame_idx,
                    strip: Some(strip),
                    frame,
                });
            }
        }
        for (output_name, space) in &self.floating_spaces {
            if let Some((frame_idx, frame)) = space
                .frames
                .iter()
                .enumerate()
                .find(|(_, frame)| frame.surface_id == frame_surface_id)
            {
                return Some(FrameLocation {
                    output_name,
                    frame_idx,
                    strip: None,
                    frame,
                });
            }
        }
        None
    }

    fn mapped_output_strips(&self) -> impl Iterator<Item = (&str, &strip::Strip)> {
        self.sorted_outputs.iter().filter_map(|output| {
            self.frame_set
                .mapped_strips()
                .get_key_value(&output.name())
                .map(|(name, strip)| (name.as_str(), strip))
        })
    }

    #[cfg(feature = "screencast")]
    pub(crate) fn introspect_windows(&self) -> HashMap<u64, dbus::WindowProperties> {
        let mut windows = HashMap::new();
        for (output_name, strip) in self.mapped_output_strips() {
            for (frame_idx, frame) in strip.frames.iter().enumerate() {
                let workspace_name =
                    crate::protocols::workspace::frame_workspace_name(frame, frame_idx);
                let frame_props = self
                    .surface_info
                    .get(&frame.surface_id)
                    .map(window_properties);
                for entry in &frame.entries {
                    let props = entry
                        .surface_id()
                        .and_then(|surface_id| self.surface_info.get(&surface_id))
                        .map(window_properties)
                        .or_else(|| frame_props.clone());
                    if let Some(mut props) = props {
                        props.title = layout_entry_window_title(
                            output_name,
                            &workspace_name,
                            entry,
                            &props.title,
                        );
                        windows.entry(entry.id.get()).or_insert(props);
                    }
                }
            }
        }
        windows
    }

    /// Tell portal pickers the introspected window list changed.
    fn flush_introspect_windows(&mut self) {
        #[cfg(feature = "screencast")]
        {
            if !std::mem::take(&mut self.introspect_windows_dirty) {
                return;
            }
            if let Some(dbus) = &self.dbus_servers {
                dbus.emit_windows_changed();
            }
        }
    }

    pub fn is_emacs_frame(&self, id: u64) -> bool {
        self.frame_location(id).is_some() || self.frame_set.is_homeless(id)
    }

    pub fn emacs_frame_output(&self, id: u64) -> Option<&str> {
        self.frame_location(id).map(|location| location.output_name)
    }

    pub fn emacs_frames_on<'a>(&'a self, output_name: &'a str) -> impl Iterator<Item = u64> + 'a {
        self.frame_set
            .mapped_strip(output_name)
            .into_iter()
            .flat_map(|s| s.frames.iter().map(|f| f.surface_id))
            .filter(|&id| id != 0)
    }

    /// Surface id receiving keyboard input.
    pub fn focused_surface_id(&self) -> u64 {
        if let Some(entry_id) = self.focused_entry_id {
            self.lookup_entry(entry_id)
                .and_then(|location| location.surface_id)
                .unwrap_or(self.focused_frame_id)
        } else {
            self.focused_frame_id
        }
    }

    pub fn set_surface_urgent(&mut self, surface_id: u64, urgent: bool) -> bool {
        if surface_id == 0 || (urgent && self.is_emacs_frame(surface_id)) {
            return false;
        }
        if urgent && self.focused_surface_id() == surface_id {
            return false;
        }

        if urgent {
            self.urgent_surfaces.insert(surface_id)
        } else {
            self.urgent_surfaces.remove(&surface_id)
        }
    }

    fn clear_focused_urgency(&mut self) {
        let focused = self.focused_surface_id();
        if focused != 0 {
            self.set_surface_urgent(focused, false);
        }
        if self.focused_frame_id != 0 {
            self.urgent_workspaces.remove(&self.focused_frame_id);
        }
    }

    pub fn mark_workspace_urgent(&mut self, frame_surface_id: Option<u64>) -> bool {
        let frame_surface_id = frame_surface_id.unwrap_or(self.focused_frame_id);
        if frame_surface_id == 0 || !self.is_emacs_frame(frame_surface_id) {
            return false;
        }
        if self.focused_frame_id == frame_surface_id {
            return false;
        }

        self.urgent_workspaces.insert(frame_surface_id)
    }

    fn set_focus_state(&mut self, frame_id: u64, entry_id: Option<LayoutEntryId>) {
        self.focused_frame_id = frame_id;
        self.focused_entry_id = entry_id;
    }

    /// Resolve a frame's xdg_toplevel `surface_id` to `(output_name, frame_idx)`.
    pub fn lookup_frame(&self, frame_surface_id: u64) -> Option<(String, usize)> {
        self.frame_location(frame_surface_id)
            .map(|location| (location.output_name.to_string(), location.frame_idx))
    }

    /// Frame surface_id whose focus id matches.
    pub fn frame_with_focus_id(&self, focus_id: FocusId) -> Option<u64> {
        self.mapped_output_strips()
            .flat_map(|(_, strip)| strip.frames.iter())
            .chain(
                self.floating_spaces
                    .values()
                    .flat_map(|space| space.frames.iter()),
            )
            .find(|f| f.focus_id == focus_id)
            .map(|f| f.surface_id)
    }

    /// Focus id assigned to the frame with this surface id.
    pub fn focus_id_of_frame(&self, frame_surface_id: u64) -> Option<FocusId> {
        self.frame_location(frame_surface_id)
            .map(|location| location.frame.focus_id)
    }

    /// Focus the layout entry identified by `entry_id`.
    pub fn set_focus_entry(&mut self, entry_id: LayoutEntryId, source: &str, notify_emacs: bool) {
        let Some(location) = self.lookup_entry(entry_id) else {
            return;
        };
        self.set_focus_entry_at(entry_id, location, source, notify_emacs);
    }

    fn set_focus_entry_at(
        &mut self,
        entry_id: LayoutEntryId,
        location: EntryLocation,
        source: &str,
        notify_emacs: bool,
    ) {
        module::record_focus(
            location.surface_id.unwrap_or(location.frame_surface_id),
            source,
            None,
        );
        if let Some(surface_id) = location.surface_id {
            self.flush_pending_title_event(surface_id);
        }
        self.set_focus_state(location.frame_surface_id, Some(entry_id));
        self.clear_focused_urgency();
        self.activate_frame_at(
            &location.output,
            location.floating,
            location.frame_surface_id,
            location.frame_idx,
        );
        if notify_emacs {
            self.queue_event(Event::Focus {
                focus_id: entry_id,
                x: None,
                y: None,
                pointer: false,
            });
        }
    }

    /// Focus an Emacs frame, routing keyboard to its selected entry's surface if one exists.
    pub fn set_focus_frame(&mut self, frame_surface_id: u64, source: &str, notify_emacs: bool) {
        let Some(location) = self.frame_location(frame_surface_id) else {
            debug!("set_focus_frame({source}): unknown frame {frame_surface_id}");
            return;
        };
        let output = location.output_name.to_string();
        if self.mapped_output_by_name(&output).is_none() {
            debug!("set_focus_frame({source}): output {output} not mapped");
            return;
        }
        let idx = location.frame_idx;
        let is_tiled = location.strip.is_some();
        let frame_focus_id = location.frame.focus_id;
        let selected_entry_id = location.frame.selected_entry_id;
        module::record_focus(frame_surface_id, source, None);
        if let Some(surface_id) = selected_entry_id.and_then(|entry_id| {
            self.lookup_entry(entry_id)
                .and_then(|location| location.surface_id)
        }) {
            self.flush_pending_title_event(surface_id);
        }
        self.set_focus_state(frame_surface_id, selected_entry_id);
        self.clear_focused_urgency();
        self.activate_frame_at(&output, !is_tiled, frame_surface_id, idx);
        if notify_emacs {
            self.queue_event(Event::Focus {
                focus_id: frame_focus_id,
                x: None,
                y: None,
                pointer: false,
            });
        }
    }

    /// Surface id of the Emacs frame whose strip-frame currently hosts
    /// `surface_id`, or `None` if the surface is not under any frame.
    pub fn frame_for_surface(&self, surface_id: u64) -> Option<u64> {
        self.frame_entry_for_surface(surface_id)
            .map(|(frame_id, _)| frame_id)
    }

    fn frame_entry_for_surface(&self, surface_id: u64) -> Option<(u64, LayoutEntryId)> {
        let location = self
            .focused_entry_for_surface(surface_id)
            .or_else(|| self.layout_entry_for_surface(surface_id, |_| true))?;
        let frame = self.frame_at_location(&location)?;
        Some((frame.surface_id, location.entry.id))
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
        if let Some(out) = self.get_focused_output()
            && let Some(strip) = self.frame_set.mapped_strip(&out)
        {
            let idx = strip.active_idx();
            if strip.frames.get(idx).is_some_and(|frame| {
                frame
                    .entries
                    .iter()
                    .any(|entry| entry.surface_id() == Some(surface_id))
            }) {
                return Some((out, idx));
            }
        }
        // Fall back to visible frames in canonical output order.
        self.mapped_output_strips()
            .find_map(|(name, strip)| {
                strip
                    .frames
                    .iter()
                    .position(|frame| {
                        frame
                            .entries
                            .iter()
                            .any(|entry| entry.surface_id() == Some(surface_id))
                    })
                    .map(|i| (name.to_string(), i))
            })
            .or_else(|| {
                self.floating_spaces.iter().find_map(|(name, space)| {
                    space
                        .frames
                        .iter()
                        .position(|frame| {
                            frame
                                .entries
                                .iter()
                                .any(|entry| entry.surface_id() == Some(surface_id))
                        })
                        .map(|i| (name.to_string(), i))
                })
            })
    }

    /// Surface id of the active Emacs frame on `output_name`.
    pub fn active_frame_surface_id(&self, output_name: &str) -> Option<u64> {
        let strip = self.frame_set.mapped_strip(output_name)?;
        strip.active_frame().map(|f| f.surface_id)
    }

    /// Active Emacs frame surface id per mapped output.
    fn active_frames_snapshot(&self) -> HashMap<String, u64> {
        self.sorted_outputs
            .iter()
            .filter_map(|o| {
                let name = o.name();
                self.active_frame_surface_id(&name).map(|id| (name, id))
            })
            .collect()
    }

    fn workspace_name_from_rename(name: String) -> Option<String> {
        let name = name.trim().to_string();
        (!name.is_empty()).then_some(name)
    }

    fn workspace_name_taken(&self, name: &str, except_frame: u64) -> bool {
        let name = name.to_lowercase();
        self.frame_set.all_frames().any(|frame| {
            frame.surface_id != except_frame
                && frame
                    .workspace_name
                    .as_deref()
                    .is_some_and(|existing| existing.to_lowercase() == name)
        })
    }

    /// Rename a workspace. Empty names clear the name. If `frame_surface_id`
    /// is absent, the currently focused frame is renamed.
    pub fn rename_workspace(&mut self, frame_surface_id: Option<u64>, name: String) -> bool {
        let frame_surface_id = frame_surface_id.unwrap_or(self.focused_frame_id);
        if frame_surface_id == 0 {
            return false;
        }

        let name = Self::workspace_name_from_rename(name);
        if let Some(name) = &name
            && self.workspace_name_taken(name, frame_surface_id)
        {
            warn!("rename_workspace: workspace name {:?} already exists", name,);
            return false;
        }

        let Some(frame) = self.frame_set.frame_mut(frame_surface_id) else {
            warn!(
                "rename_workspace: frame surface {} not found",
                frame_surface_id,
            );
            return false;
        };
        if frame.workspace_name == name {
            return false;
        }
        frame.workspace_name = name;
        self.introspect_windows_dirty = true;
        true
    }

    fn first_active_frame_surface_id(&self) -> Option<u64> {
        self.sorted_outputs
            .iter()
            .find_map(|output| self.active_frame_surface_id(&output.name()))
    }

    /// Activate a frame on OUTPUT, dispatching by placement kind.
    fn activate_frame_at(
        &mut self,
        output: &str,
        floating: bool,
        frame_surface_id: u64,
        frame_idx: usize,
    ) {
        if floating {
            self.activate_floating_frame(output, frame_surface_id);
        } else {
            self.activate_frame(output, frame_idx);
        }
    }

    /// Mark `new_idx` as active on `output_name`.
    fn activate_floating_frame(&mut self, output_name: &str, frame_surface_id: u64) {
        if let Some(space) = self.floating_spaces.get_mut(output_name) {
            space.activate_frame(frame_surface_id);
            if let Some(output) = self.find_mapped_output(output_name) {
                self.queue_redraw(&output);
            }
        }
    }

    pub fn activate_frame(&mut self, output_name: &str, new_idx: usize) {
        let Some(strip) = self.frame_set.mapped_strip_mut(output_name) else {
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
        let output = self.find_mapped_output(output_name);
        if let Some(output) = output {
            self.queue_redraw(&output);
        }
    }

    /// Where `output`'s content lands on screen under the overview zoom.
    pub fn overview_geometry(&self, output: &Output) -> overview::Geometry {
        let Some(progress) = self.overview.progress() else {
            return overview::Geometry::default();
        };
        let size = self
            .space
            .output_geometry(output)
            .map(|geo| geo.size.to_f64())
            .unwrap_or_default();
        let scale = output.current_scale().fractional_scale();
        let shift = self
            .frame_set
            .mapped_strip(&output.name())
            .map_or(0.0, |strip| strip.overview_shift)
            * progress;
        overview::Geometry::new(size, scale, self.overview.zoom(), shift)
    }

    /// Strip frame under an output-local content point, with the point's
    /// frame-relative position.
    fn overview_frame_under(
        &self,
        output: &Output,
        geometry: &overview::Geometry,
        in_content: Point<f64, Logical>,
    ) -> Option<(usize, Point<f64, Logical>)> {
        let working_area = self.get_working_area(output);
        let in_strip = in_content - working_area.loc.to_f64();
        let strip = self.frame_set.mapped_strip(&output.name())?;
        strip
            .frames_with_render_geo(
                working_area.size.to_f64(),
                geometry.strip_viewport(working_area),
            )
            .find(|(_, _, rect)| rect.contains(in_strip))
            .map(|(idx, _, rect)| (idx, in_strip - rect.loc))
    }

    /// Click in the open overview: a frame under `pos` becomes active, and a
    /// click anywhere on the zoomed output closes the overview.
    pub fn overview_click(&mut self, pos: Point<f64, Logical>) {
        let Some(output) = self.output_at(pos).cloned() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let pos_within_output = pos - output_geo.loc.to_f64();
        let geometry = self.overview_geometry(&output);
        let in_output = geometry.to_content(pos_within_output);
        // Hidden frames have slid up, so the click maps onto where they were.
        let slide = self.hide.current() * f64::from(output_geo.size.h);
        let in_content = in_output + Point::from((0., slide));
        let floating_frame = self
            .floating_spaces
            .get(&output.name())
            .and_then(|space| space.frame_under(in_content));
        if let Some((frame_id, rect)) = floating_frame {
            self.set_focus_frame_from_pointer(frame_id, "overview", in_content - rect.loc);
            self.overview_close();
        } else if let Some((idx, pos_in_frame)) =
            self.overview_frame_under(&output, &geometry, in_content)
        {
            self.overview_activate_frame(&output.name(), idx, pos_in_frame);
        } else if Rectangle::from_size(output_geo.size.to_f64()).contains(in_output) {
            self.overview_close();
        }
    }

    /// Activate frame `idx` and close; the strip snaps onto it and the zoom carries the slide.
    fn overview_activate_frame(
        &mut self,
        output_name: &str,
        idx: usize,
        pos_in_frame: Point<f64, Logical>,
    ) {
        self.bake_overview_shifts();
        let Some(strip) = self.frame_set.mapped_strip(output_name) else {
            return;
        };
        let Some(target) = strip.frames.get(idx).map(|frame| frame.surface_id) else {
            return;
        };
        let prev_view_pos = strip.view_pos();
        let zoom = self.overview.zoom();
        let progress = self.overview.progress().unwrap_or(0.0);
        self.set_focus_frame_from_pointer(target, "overview", pos_in_frame);
        if progress > 0.01
            && let Some(strip) = self.frame_set.mapped_strip_mut(output_name)
        {
            strip.view_offset = strip::ViewOffset::Static(0.0);
            strip.overview_shift = (strip.view_pos() - prev_view_pos) * zoom / progress;
        }
        self.with_overview(|overview| overview.close());
        self.queue_redraw_all();
    }

    /// Change the overview and tell Emacs when it opens or closes.
    fn with_overview<R>(&mut self, change: impl FnOnce(&mut overview::Overview) -> R) -> R {
        let was_open = self.overview.is_open();
        let result = change(&mut self.overview);
        let open = self.overview.is_open();
        if open != was_open {
            self.queue_event(Event::Overview { open });
        }
        result
    }

    pub fn overview_gesture_end(&mut self) -> bool {
        self.with_overview(|overview| overview.gesture_end())
    }

    /// Turn leftover overview shifts back into strip slides before the zoom changes course.
    fn bake_overview_shifts(&mut self) {
        let progress = self.overview.progress();
        let zoom = self.overview.zoom();
        for strip in self.frame_set.mapped_strips_mut() {
            let shift = mem::take(&mut strip.overview_shift);
            // A closed overview showed no shift, so there is nothing to carry over.
            if let Some(progress) = progress.filter(|_| shift != 0.0) {
                strip.view_offset.offset(-shift * progress / zoom);
                strip.animate_view_offset(0.0, strip::DEFAULT_ANIM_DURATION);
            }
        }
    }

    pub fn overview_toggle(&mut self) {
        if self.is_locked() {
            return;
        }
        self.bake_overview_shifts();
        let was_active = self.overview.is_active();
        self.with_overview(|overview| overview.toggle());
        if !was_active {
            self.set_inactive_frames_shown(true);
        }
        self.queue_redraw_all();
    }

    /// Thaw every frame while the overview shows them all, or refreeze the inactive ones.
    fn set_inactive_frames_shown(&mut self, shown: bool) {
        let ids: Vec<u64> = self
            .frame_set
            .mapped_strips()
            .values()
            .flat_map(|strip| {
                let active = strip.active_frame().map(|f| f.surface_id);
                strip
                    .frames
                    .iter()
                    .map(|f| f.surface_id)
                    .filter(move |id| Some(*id) != active)
            })
            .collect();
        for id in ids {
            self.queue_event(if shown {
                Event::FrameShown { id }
            } else {
                Event::FrameHidden { id }
            });
        }
    }

    pub fn overview_open(&mut self) -> bool {
        if self.overview.is_open() {
            return false;
        }
        self.overview_toggle();
        true
    }

    pub fn overview_close(&mut self) -> bool {
        if !self.overview.is_open() {
            return false;
        }
        self.overview_toggle();
        true
    }

    pub fn overview_gesture_begin(&mut self) {
        if self.is_locked() {
            return;
        }
        self.bake_overview_shifts();
        let was_active = self.overview.is_active();
        self.with_overview(|overview| overview.gesture_begin());
        if !was_active {
            self.set_inactive_frames_shown(true);
        }
        self.queue_redraw_all();
    }

    pub fn frames_hidden(&self) -> bool {
        self.hide.target() == 1.
    }

    /// Neither the overview nor the hide has moved the frames.
    pub fn frames_in_place(&self) -> bool {
        !self.overview.is_active() && self.hide.is_static() && !self.frames_hidden()
    }

    /// Slide every frame off its output or bring them back, and tell Emacs.
    pub fn hide_toggle(&mut self) {
        if self.is_locked() {
            return;
        }
        let hidden = !self.frames_hidden();
        self.hide = strip::ViewOffset::Animation(strip::ViewAnimation::new(
            self.animations_clock.clone(),
            self.hide.current(),
            if hidden { 1. } else { 0. },
            strip::DEFAULT_ANIM_DURATION,
        ));
        self.queue_event(Event::Hide { hidden });
        self.queue_redraw_all();
    }

    /// End a strip swipe on `output_name`: snap to the nearest frame and focus it.
    pub fn strip_gesture_end(&mut self, output_name: &str, working_w: f64) {
        let prev_active_id = self
            .frame_set
            .mapped_strip(output_name)
            .and_then(|s| s.active_frame())
            .map(|f| f.surface_id);
        let target = self.frame_set.mapped_strip_mut(output_name).and_then(|s| {
            s.gesture_end(working_w);
            s.frames.get(s.active_idx()).map(|f| f.surface_id)
        });
        self.reconcile_active_for_output(output_name, prev_active_id);
        // A swipe that snaps back changes nothing; don't refocus.
        let target = target.filter(|&id| Some(id) != prev_active_id);
        let output = self.find_mapped_output(output_name);
        if let (Some(id), Some(output)) = (target, &output) {
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
        if let Some(output) = output {
            self.queue_redraw(&output);
        }
    }

    /// Focus a frame reached with the pointer; Emacs then anchors mouse-follows-focus.
    fn set_focus_frame_from_pointer(
        &mut self,
        frame_surface_id: u64,
        source: &str,
        pos_in_frame: Point<f64, Logical>,
    ) {
        self.set_focus_frame(frame_surface_id, source, false);
        if let Some(focus_id) = self.focus_id_of_frame(frame_surface_id) {
            self.queue_event(Event::Focus {
                focus_id,
                x: Some(pos_in_frame.x),
                y: Some(pos_in_frame.y),
                pointer: true,
            });
        }
    }

    /// Global logical top-left of an Emacs frame's working area,
    /// driven by the strip column (frames aren't placed in `space`).
    pub fn emacs_frame_origin(
        &self,
        surface_id: u64,
    ) -> Option<smithay::utils::Point<i32, smithay::utils::Logical>> {
        let location = self.frame_location(surface_id)?;
        let output = self.mapped_output_by_name(location.output_name)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);

        if location.strip.is_none() {
            let (_, data) = self
                .floating_spaces
                .get(location.output_name)?
                .frame_with_data(surface_id)?;
            let loc = data.logical_pos();
            return Some(Point::from((
                output_geo.loc.x + loc.x as i32,
                output_geo.loc.y + loc.y as i32,
            )));
        }

        Some(
            Self::frame_rect(
                output_geo,
                working_area,
                location.strip?,
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
        // Rounded so popups and hits track a sliding frame.
        let frame_dx = strip.screen_x_of_frame(frame_idx).round() as i32;
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
        let focused = self.focused_entry_id?;
        self.layout_entry_for_surface(id, |entry| entry.id == focused)
    }

    pub(crate) fn layout_entry_for_id(
        &self,
        entry_id: LayoutEntryId,
    ) -> Option<LayoutEntryLocation<'_>> {
        for (output_name, strip) in self.mapped_output_strips() {
            if let Some((frame_idx, entry)) = strip
                .entries_with_frame()
                .find(|(_, entry)| entry.id == entry_id)
            {
                return Some(LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    floating: false,
                    entry,
                });
            }
        }
        for (output_name, space) in &self.floating_spaces {
            if let Some((frame_idx, entry)) = space
                .entries_with_frame()
                .find(|(_, entry)| entry.id == entry_id)
            {
                return Some(LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    floating: true,
                    entry,
                });
            }
        }
        None
    }

    fn layout_entry_for_surface(
        &self,
        id: u64,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryLocation<'_>> {
        for (output_name, strip) in self.mapped_output_strips() {
            if let Some((frame_idx, entry)) = strip
                .surface_entries_with_frame()
                .find(|(_, entry)| entry.surface_id() == Some(id) && predicate(entry))
            {
                return Some(LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    floating: false,
                    entry,
                });
            }
        }
        for (output_name, space) in &self.floating_spaces {
            if let Some((frame_idx, entry)) = space
                .surface_entries_with_frame()
                .find(|(_, entry)| entry.surface_id() == Some(id) && predicate(entry))
            {
                return Some(LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    floating: true,
                    entry,
                });
            }
        }
        None
    }

    /// Layout entry used for positioning a multi-view surface.
    fn preferred_entry_for_surface(&self, id: u64) -> Option<LayoutEntryLocation<'_>> {
        self.focused_entry_for_surface(id)
            .or_else(|| self.layout_entry_for_surface(id, LayoutEntry::primary))
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
    fn layout_entry_is_fullscreen(
        strip: &strip::Strip,
        frame_idx: usize,
        entry: &LayoutEntry,
    ) -> bool {
        strip
            .frames
            .get(frame_idx)
            .is_some_and(|frame| frame.entry_fullscreen(entry.id))
    }

    pub(crate) fn layout_entry_rect(
        &self,
        output_geo: Rectangle<i32, Logical>,
        working_area: Rectangle<i32, Logical>,
        strip: &strip::Strip,
        frame_idx: usize,
        entry: &LayoutEntry,
    ) -> Rectangle<i32, Logical> {
        let fullscreen = Self::layout_entry_is_fullscreen(strip, frame_idx, entry);
        if fullscreen {
            if entry.primary() {
                let window_size = entry
                    .surface_id()
                    .and_then(|surface_id| self.id_windows.get(&surface_id))
                    .map(|window| window.geometry().size)
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

        let frame_dx = strip.screen_x_of_frame(frame_idx).round() as i32;
        Rectangle::new(
            Point::from((
                output_geo.loc.x + working_area.loc.x + frame_dx + entry.x,
                output_geo.loc.y + working_area.loc.y + entry.y,
            )),
            Size::from((entry.w as i32, entry.h as i32)),
        )
    }

    fn entry_origin(&self, location: LayoutEntryLocation<'_>) -> Option<Point<i32, Logical>> {
        let output = self.mapped_output_by_name(location.output_name)?;
        let output_geo = self.space.output_geometry(output)?;
        if location.floating {
            let space = self.floating_spaces.get(location.output_name)?;
            let frame = space.frames.get(location.frame_idx)?;
            let (_, data) = space.frame_with_data(frame.surface_id)?;
            let loc = data.logical_pos();
            return Some(Point::from((
                output_geo.loc.x + loc.x as i32 + location.entry.x,
                output_geo.loc.y + loc.y as i32 + location.entry.y,
            )));
        }
        let working_area = self.get_working_area(output);
        let strip = self.frame_set.mapped_strip(location.output_name)?;
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

    fn layout_entry_location_fullscreen(&self, location: LayoutEntryLocation<'_>) -> bool {
        if location.floating {
            return self
                .floating_spaces
                .get(location.output_name)
                .and_then(|space| space.frames.get(location.frame_idx))
                .is_some_and(|frame| frame.entry_fullscreen(location.entry.id));
        }
        self.frame_set
            .mapped_strip(location.output_name)
            .is_some_and(|strip| {
                Self::layout_entry_is_fullscreen(strip, location.frame_idx, location.entry)
            })
    }

    /// On-screen origins of every layout view of `surface_id`, one per entry.
    pub fn surface_view_origins(&self, surface_id: u64) -> Vec<Point<i32, Logical>> {
        if !self.surface_outputs.contains_key(&surface_id) {
            return self.emacs_frame_origin(surface_id).into_iter().collect();
        }
        let mut origins = Vec::new();
        for (output_name, strip) in self.mapped_output_strips() {
            for (frame_idx, entry) in strip.surface_entries_with_frame() {
                if entry.surface_id() != Some(surface_id) {
                    continue;
                }
                let location = LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    floating: false,
                    entry,
                };
                if let Some(p) = self.entry_origin(location) {
                    origins.push(p);
                }
            }
        }
        for (output_name, space) in &self.floating_spaces {
            for (frame_idx, entry) in space.surface_entries_with_frame() {
                if entry.surface_id() != Some(surface_id) {
                    continue;
                }
                let location = LayoutEntryLocation {
                    output_name,
                    frame_idx,
                    floating: true,
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

    /// Floating space under `pos`: its output name, the space, and output-local `pos`.
    fn floating_space_at(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(String, &floating::FloatingSpace, Point<f64, Logical>)> {
        if !self.frames_in_place() {
            return None;
        }
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let output_name = output.name();
        let pos_within_output = pos - output_geo.loc.to_f64();
        let space = self.floating_spaces.get(&output_name)?;
        Some((output_name, space, pos_within_output))
    }

    pub(crate) fn floating_frame_under(&self, pos: Point<f64, Logical>) -> Option<(String, u64)> {
        let (output_name, space, pos_within_output) = self.floating_space_at(pos)?;
        let (frame_id, _) = space.frame_under(pos_within_output)?;
        Some((output_name, frame_id))
    }

    pub(crate) fn floating_resize_edges_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(String, u64, floating::ResizeEdge)> {
        let (output_name, space, pos_within_output) = self.floating_space_at(pos)?;
        let (frame_id, edges) = space.resize_edges_under(pos_within_output)?;
        Some((output_name, frame_id, edges))
    }

    fn layout_entry_under_matching(
        &self,
        pos: Point<f64, Logical>,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryHit<'_>> {
        // Zoomed or hidden content takes no surface hits.
        if !self.frames_in_place() {
            return None;
        }
        if let Some(hit) = self.floating_entry_under_matching(pos, &mut predicate) {
            return Some(hit);
        }
        // A floating frame occludes the strip; don't match a tiled entry beneath it.
        if self.floating_frame_under(pos).is_some() {
            return None;
        }
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);
        let strip = self.frame_set.mapped_strip(&output.name())?;
        self.tiled_entry_under_matching(strip, output_geo, working_area, pos, predicate)
    }

    /// Hit-test `pos` against floating frames' entries only (the overlay above the strip).
    fn floating_entry_under_matching(
        &self,
        pos: Point<f64, Logical>,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryHit<'_>> {
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let space = self.floating_spaces.get(&output.name())?;
        for (frame, frame_rect) in space.frames_with_render_geo() {
            for entry in frame.entries.iter() {
                if !predicate(entry) {
                    continue;
                }
                let fullscreen = frame.entry_fullscreen(entry.id);
                let rect = Rectangle::new(
                    Point::from((
                        output_geo.loc.x + frame_rect.loc.x as i32 + entry.x,
                        output_geo.loc.y + frame_rect.loc.y as i32 + entry.y,
                    )),
                    Size::from((entry.w as i32, entry.h as i32)),
                );
                if Self::rect_contains_f64(rect, pos) {
                    return Some(LayoutEntryHit {
                        entry,
                        fullscreen,
                        rect,
                        output_size: output_geo.size,
                    });
                }
            }
        }
        None
    }

    /// Hit-test `pos` against a tiled strip's entries only (no floating overlay).
    fn tiled_entry_under_matching<'a>(
        &self,
        strip: &'a strip::Strip,
        output_geo: Rectangle<i32, Logical>,
        working_area: Rectangle<i32, Logical>,
        pos: Point<f64, Logical>,
        mut predicate: impl FnMut(&LayoutEntry) -> bool,
    ) -> Option<LayoutEntryHit<'a>> {
        let active_idx = strip.active_idx();
        // Fullscreen primary clicks miss in the letterbox; inactive fullscreens render off-screen.
        for (frame_idx, entry) in strip.entries_with_frame() {
            if !predicate(entry) {
                continue;
            }
            let fullscreen = Self::layout_entry_is_fullscreen(strip, frame_idx, entry);
            if fullscreen && frame_idx != active_idx {
                continue;
            }
            let rect = self.layout_entry_rect(output_geo, working_area, strip, frame_idx, entry);
            if Self::rect_contains_f64(rect, pos) {
                return Some(LayoutEntryHit {
                    entry,
                    fullscreen,
                    rect,
                    output_size: output_geo.size,
                });
            }
        }
        None
    }

    /// Find the topmost layout entry containing `pos`, returning the entry
    /// and its global rectangle.
    fn layout_entry_under(&self, pos: Point<f64, Logical>) -> Option<LayoutEntryHit<'_>> {
        self.layout_entry_under_matching(pos, |_| true)
    }

    /// Find the topmost surface-backed layout entry containing `pos`.
    fn surface_layout_entry_under(&self, pos: Point<f64, Logical>) -> Option<LayoutEntryHit<'_>> {
        self.layout_entry_under_matching(pos, |entry| entry.surface_id().is_some())
    }

    /// The tiled entry whose rect contains a floating frame's center, for spatial reattach.
    pub fn tiled_entry_under_floating_center(
        &self,
        floating_frame_id: u64,
    ) -> Option<LayoutEntryId> {
        let (output_name, center) = self.floating_spaces.iter().find_map(|(name, space)| {
            space.frames_with_render_geo().find_map(|(frame, rect)| {
                (frame.surface_id == floating_frame_id).then(|| {
                    let c: Point<f64, Logical> = Point::from((
                        rect.loc.x + rect.size.w / 2.0,
                        rect.loc.y + rect.size.h / 2.0,
                    ));
                    (name.clone(), c)
                })
            })
        })?;
        let output = self.mapped_output_by_name(&output_name)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);
        let center = Point::from((
            output_geo.loc.x as f64 + center.x,
            output_geo.loc.y as f64 + center.y,
        ));
        let strip = self.frame_set.mapped_strip(&output_name)?;
        self.tiled_entry_under_matching(strip, output_geo, working_area, center, |_| true)
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

    /// Hit-test layout toplevel/subsurfaces under a global position; falls
    /// back to the frame's own surface when no entry hits. Popups are handled
    /// separately by `popup_surface_under`, so popup placement and z-order stay
    /// identical to rendering.
    pub fn layout_surface_under(&self, pos: Point<f64, Logical>) -> Option<SurfaceHit> {
        self.surface_layout_entry_under(pos)
            .and_then(|hit| self.surface_under_layout_entry(pos, hit))
            .or_else(|| self.surface_under_frame(pos))
    }

    fn surface_under_layout_entry(
        &self,
        pos: Point<f64, Logical>,
        hit: LayoutEntryHit<'_>,
    ) -> Option<SurfaceHit> {
        let entry = hit.entry;
        let window = self.id_windows.get(&entry.surface_id()?)?;
        let window_geo = window.geometry();
        let pointer_scale =
            Self::pointer_scale_for_entry(entry, hit.fullscreen, window_geo.size, hit.output_size);

        let pos_in_window = Point::from((
            (pos.x - hit.rect.loc.x as f64) * pointer_scale,
            (pos.y - hit.rect.loc.y as f64) * pointer_scale,
        ));
        let window_origin: Point<f64, Logical> =
            Point::from((pos.x - pos_in_window.x, pos.y - pos_in_window.y));

        Self::surface_under_non_popup_window(window, pos_in_window, window_origin)
    }

    /// Find the frame containing `pos`, returning the frame surface itself.
    fn surface_under_frame(&self, pos: Point<f64, Logical>) -> Option<SurfaceHit> {
        if !self.frames_in_place() {
            return None;
        }
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let working_area = self.get_working_area(output);
        if let Some(space) = self.floating_spaces.get(&output.name()) {
            for (frame, frame_rect) in space.frames_with_render_geo() {
                let rect = Rectangle::new(
                    Point::from((
                        output_geo.loc.x + frame_rect.loc.x as i32,
                        output_geo.loc.y + frame_rect.loc.y as i32,
                    )),
                    Size::from((frame_rect.size.w as i32, frame_rect.size.h as i32)),
                );
                if !Self::rect_contains_f64(rect, pos) {
                    continue;
                }
                let Some(window) = self.id_windows.get(&frame.surface_id) else {
                    continue;
                };
                let window_origin: Point<f64, Logical> =
                    Point::from((rect.loc.x as f64, rect.loc.y as f64));
                let pos_in_window = Point::from((pos.x - window_origin.x, pos.y - window_origin.y));
                if let Some(hit) =
                    Self::surface_under_non_popup_window(window, pos_in_window, window_origin)
                {
                    return Some(hit);
                }
            }
        }
        let strip = self.frame_set.mapped_strip(&output.name())?;

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
            if let Some(hit) =
                Self::surface_under_non_popup_window(window, pos_in_window, window_origin)
            {
                return Some(hit);
            }
        }

        None
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

    fn popup_surface_under(&self, pos: Point<f64, Logical>) -> Option<SurfaceHit> {
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

    /// Check if a surface is currently displaying as fullscreen on any output.
    pub fn is_surface_fullscreen(&self, id: u64) -> bool {
        self.mapped_output_strips()
            .map(|(_, strip)| strip)
            .any(|strip| {
                strip
                    .frames
                    .iter()
                    .any(|frame| frame.surface_fullscreen(id))
            })
            || self.floating_spaces.values().any(|space| {
                space
                    .frames
                    .iter()
                    .any(|frame| frame.surface_fullscreen(id))
            })
    }

    fn surface_has_primary_entry(&self, id: u64) -> bool {
        self.mapped_output_strips()
            .map(|(_, strip)| strip)
            .any(|strip| {
                strip
                    .active_surface_entries()
                    .any(|entry| entry.surface_id() == Some(id) && entry.primary())
            })
            || self.floating_spaces.values().any(|space| {
                space
                    .surface_entries()
                    .any(|entry| entry.surface_id() == Some(id) && entry.primary())
            })
    }

    fn configure_surface_fullscreen_state(&self, id: u64, fullscreen: bool) {
        let Some(window) = self.id_windows.get(&id) else {
            return;
        };
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let changed = toplevel.with_pending_state(|state| {
            let was_fullscreen = state.states.contains(XdgToplevelState::Fullscreen);
            if fullscreen {
                state.states.set(XdgToplevelState::Fullscreen);
                state.states.unset(XdgToplevelState::Maximized);
            } else {
                state.states.unset(XdgToplevelState::Fullscreen);
            }
            was_fullscreen != fullscreen
        });
        if changed {
            toplevel.send_configure();
        }
    }

    fn fullscreen_entry_id_for_surface(&self, id: u64) -> Option<LayoutEntryId> {
        self.focused_entry_for_surface(id)
            .or_else(|| self.layout_entry_for_surface(id, LayoutEntry::primary))
            .or_else(|| self.layout_entry_for_surface(id, |_| true))
            .map(|location| location.entry.id)
    }

    fn fullscreen_entry_id_for_surface_on_output(
        &self,
        id: u64,
        output_name: Option<&str>,
    ) -> Option<LayoutEntryId> {
        output_name
            .and_then(|name| self.frame_set.mapped_strip(name))
            .and_then(|strip| {
                strip
                    .surface_entries_with_frame()
                    .find(|(_, entry)| entry.surface_id() == Some(id))
                    .map(|(_, entry)| entry.id)
            })
            .or_else(|| {
                output_name
                    .and_then(|name| self.floating_spaces.get(name))
                    .and_then(|space| {
                        space
                            .surface_entries_with_frame()
                            .find(|(_, entry)| entry.surface_id() == Some(id))
                            .map(|(_, entry)| entry.id)
                    })
            })
            .or_else(|| self.fullscreen_entry_id_for_surface(id))
    }

    pub fn set_layout_entry_fullscreen(
        &mut self,
        entry_id: LayoutEntryId,
        fullscreen: bool,
    ) -> Option<u64> {
        let mut target_surface = None;
        let mut cleared_surfaces = Vec::new();

        for frame in self.frame_set.all_frames_mut() {
            target_surface = frame.entry_surface_id(entry_id);
            if target_surface.is_none() {
                continue;
            }
            if fullscreen {
                if let Some((previous, _next)) = frame.set_fullscreen_entry(entry_id) {
                    cleared_surfaces.extend(previous.map(|fullscreen| fullscreen.surface_id));
                }
            } else {
                cleared_surfaces.extend(
                    frame
                        .clear_fullscreen_entry(entry_id)
                        .map(|fullscreen| fullscreen.surface_id),
                );
            }
            break;
        }
        if target_surface.is_none() {
            'found_floating: for space in self.floating_spaces.values_mut() {
                for frame in &mut space.frames {
                    target_surface = frame.entry_surface_id(entry_id);
                    if target_surface.is_none() {
                        continue;
                    }
                    if fullscreen {
                        if let Some((previous, _next)) = frame.set_fullscreen_entry(entry_id) {
                            cleared_surfaces
                                .extend(previous.map(|fullscreen| fullscreen.surface_id));
                        }
                    } else {
                        cleared_surfaces.extend(
                            frame
                                .clear_fullscreen_entry(entry_id)
                                .map(|fullscreen| fullscreen.surface_id),
                        );
                    }
                    break 'found_floating;
                }
            }
        }

        let target_surface = target_surface?;
        self.refresh_primary_assignments();
        cleared_surfaces.sort_unstable();
        cleared_surfaces.dedup();
        for surface_id in cleared_surfaces {
            if !self.surface_has_primary_entry(surface_id) {
                self.configure_surface_fullscreen_state(surface_id, false);
            }
        }
        if !self.surface_has_primary_entry(target_surface) {
            self.configure_surface_fullscreen_state(target_surface, fullscreen);
        }
        Some(target_surface)
    }

    pub fn set_surface_fullscreen(&mut self, id: u64, fullscreen: bool) {
        self.set_surface_fullscreen_on_output(id, None, fullscreen);
    }

    pub fn set_surface_fullscreen_on_output(
        &mut self,
        id: u64,
        output_name: Option<&str>,
        fullscreen: bool,
    ) {
        if fullscreen {
            if let Some(entry_id) = self.fullscreen_entry_id_for_surface_on_output(id, output_name)
            {
                self.set_layout_entry_fullscreen(entry_id, true);
            } else {
                self.configure_surface_fullscreen_state(id, true);
            }
        } else {
            self.clear_surface_fullscreen(id);
        }
    }

    pub fn toggle_layout_entry_fullscreen(&mut self, entry_id: LayoutEntryId) {
        let fullscreen = self
            .layout_entry_for_id(entry_id)
            .is_some_and(|location| self.layout_entry_location_fullscreen(location));
        self.set_layout_entry_fullscreen(entry_id, !fullscreen);
    }

    pub fn toggle_surface_fullscreen(&mut self, id: u64) {
        if let Some(entry_id) = self.fullscreen_entry_id_for_surface(id) {
            self.toggle_layout_entry_fullscreen(entry_id);
        } else {
            self.configure_surface_fullscreen_state(id, true);
        }
    }

    fn clear_surface_fullscreen(&mut self, id: u64) {
        let mut cleared = false;
        for frame in self.frame_set.all_frames_mut() {
            cleared |= frame.clear_fullscreen_for_surface(id).is_some();
        }
        for space in self.floating_spaces.values_mut() {
            for frame in &mut space.frames {
                cleared |= frame.clear_fullscreen_for_surface(id).is_some();
            }
        }
        if cleared {
            self.refresh_primary_assignments();
        }
        if !self.surface_has_primary_entry(id) {
            self.configure_surface_fullscreen_state(id, false);
        }
    }

    /// True iff the active frame has a fullscreen entry covering the Top layer.
    pub fn render_above_top_layer(&self, output: &Output) -> bool {
        self.frames_in_place()
            && self
                .frame_set
                .mapped_strip(&output.name())
                .is_some_and(strip::Strip::active_frame_is_fullscreen)
    }

    /// Entry id of the layout entry under a point.
    pub fn layout_entry_id_under(&self, pos: Point<f64, Logical>) -> Option<LayoutEntryId> {
        self.layout_entry_under(pos).map(|hit| hit.entry.id)
    }

    /// Entry surface ids on `output_name`'s strip; empty if the strip is gone.
    fn strip_entries(&self, output_name: &str) -> HashSet<u64> {
        let mut entries: HashSet<u64> = self
            .frame_set
            .mapped_strip(output_name)
            .map(|strip| {
                strip
                    .surface_entries()
                    .filter_map(LayoutEntry::surface_id)
                    .collect()
            })
            .unwrap_or_default();
        if let Some(space) = self.floating_spaces.get(output_name) {
            entries.extend(space.surface_entries().filter_map(LayoutEntry::surface_id));
        }
        entries
    }

    fn surface_outputs_for(&self, output_name: &str) -> HashSet<u64> {
        self.surface_outputs
            .iter()
            .filter_map(|(&id, outputs)| outputs.contains(output_name).then_some(id))
            .collect()
    }

    fn set_preferred_scale_transform(&self, window: &Window, output: &Output) {
        let scale = output.current_scale();
        let transform = output.current_transform();
        window.with_surfaces(|surface, data| {
            crate::utils::send_scale_transform(surface, data, scale, transform);
        });
    }

    fn enter_output_for_window(&self, window: &Window, output: &Output) {
        self.set_preferred_scale_transform(window, output);
        if let Some(surface) = window.wl_surface() {
            output.enter(&surface);
        }
    }

    fn leave_output_for_window(&self, window: &Window, output: &Output) {
        if let Some(surface) = window.wl_surface() {
            output.leave(&surface);
        }
    }

    fn leave_emacs_frames_on_output(&self, output: &Output) {
        let output_name = output.name();
        for id in self.emacs_frames_on(&output_name) {
            if let Some(window) = self.id_windows.get(&id) {
                self.leave_output_for_window(window, output);
            }
        }
    }

    fn reconcile_output_membership(
        &mut self,
        output_name: &str,
        output: Option<&Output>,
        pre: &HashSet<u64>,
        post: &HashSet<u64>,
    ) {
        for &id in pre.difference(post) {
            if let (Some(out), Some(window)) = (output, self.id_windows.get(&id)) {
                self.leave_output_for_window(window, out);
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
            if let Some(out) = output {
                self.enter_output_for_window(window, out);
            }
        }
        for &id in post {
            self.surface_outputs
                .entry(id)
                .or_default()
                .insert(output_name.to_string());
        }
    }

    /// Diff `pre` (snapshot before strip mutation) against the current strip;
    /// fire `wl_surface.enter` / `leave` and sync `surface_outputs`.
    fn reconcile_output_entries(&mut self, output_name: &str, pre: &HashSet<u64>) {
        let output = self.find_mapped_output(output_name);
        let post = output
            .as_ref()
            .map(|_| self.strip_entries(output_name))
            .unwrap_or_default();
        self.reconcile_output_membership(output_name, output.as_ref(), pre, &post);
    }

    pub(crate) fn reconcile_mapped_output_entries(&mut self, output: &Output) {
        let output_name = output.name();
        let pre = self.surface_outputs_for(&output_name);
        let post = self.strip_entries(&output_name);
        self.reconcile_output_membership(&output_name, Some(output), &pre, &post);
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
                let previous = self.frame_location(id);
                let prev_output = previous.map(|location| location.output_name.to_string());
                if prev_output.as_deref() == Some(output_name) {
                    return None;
                }
                Some(FrameMigration {
                    id,
                    prev_output,
                    previous_frame: previous.map(|location| location.frame.clone()),
                })
            })
            .collect()
    }

    fn preserve_migrated_frame_state(frames: &mut [strip::Frame], migrations: &[FrameMigration]) {
        for frame in frames {
            let Some(previous_frame) = migrations
                .iter()
                .find(|migration| migration.id == frame.surface_id)
                .and_then(|migration| migration.previous_frame.as_ref())
            else {
                continue;
            };
            if frame.workspace_name.is_none() {
                frame.workspace_name = previous_frame.workspace_name.clone();
            }
            frame.preserve_compositor_view_state_from(previous_frame);
        }
    }

    fn output_layout_snapshot(&self, output_name: String) -> OutputLayoutSnapshot {
        let active_frame_id = self
            .frame_set
            .mapped_strip(&output_name)
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
            if let Some(prev_name) = &migration.prev_output
                && !output_names.iter().any(|name| name == prev_name)
            {
                output_names.push(prev_name.clone());
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
            if let Some(prev_strip) = self.frame_set.mapped_strip_mut(prev_name) {
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
            if was_active_in_prev && self.mapped_output_by_name(prev_name).is_some() {
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
            if let Some(prev_name) = migration.prev_output.as_deref()
                && let Some(prev_output) = self.mapped_output_by_name(prev_name)
            {
                self.leave_output_for_window(window, prev_output);
            }
            self.enter_output_for_window(window, target_output);
        }
    }

    fn move_frame_ids_between_outputs(
        &mut self,
        src_name: &str,
        dst_name: &str,
        frame_ids: &[u64],
    ) -> bool {
        if src_name == dst_name || frame_ids.is_empty() {
            return false;
        }

        let frame_ids: HashSet<u64> = frame_ids.iter().copied().collect();
        let pre_src = self.strip_entries(src_name);
        let pre_dst = self.strip_entries(dst_name);
        let prev_src_active = self.active_frame_surface_id(src_name);
        let prev_dst_active = self.active_frame_surface_id(dst_name);

        let mut moved = Vec::new();
        if let Some(src_strip) = self.frame_set.mapped_strip_mut(src_name) {
            let src_active_id = src_strip.active_frame().map(|frame| frame.surface_id);
            let kept;
            (moved, kept) = src_strip
                .frames
                .drain(..)
                .partition(|frame| frame_ids.contains(&frame.surface_id));
            src_strip.frames = kept;
            let active_idx = src_active_id
                .and_then(|id| {
                    src_strip
                        .frames
                        .iter()
                        .position(|frame| frame.surface_id == id)
                })
                .unwrap_or_else(|| src_strip.active_idx());
            src_strip.set_active(active_idx);
        }
        if moved.is_empty() {
            return false;
        }

        let moved_ids = moved
            .iter()
            .map(|frame| frame.surface_id)
            .collect::<Vec<_>>();

        let clock = self.animations_clock.clone();
        let dst_strip = self.frame_set.ensure_mapped_strip(dst_name, clock);
        let insert_at = dst_strip.frames.len();
        dst_strip.frames.append(&mut moved);
        if let Some(focused_idx) = moved_ids
            .iter()
            .position(|id| *id == self.focused_frame_id)
            .map(|idx| insert_at + idx)
        {
            dst_strip.set_active(focused_idx);
        } else if insert_at == 0 {
            dst_strip.set_active(0);
        }

        self.reconcile_output_entries(src_name, &pre_src);
        self.reconcile_output_entries(dst_name, &pre_dst);
        let dst_output = self.find_mapped_output(dst_name);
        if let Some(dst_output) = &dst_output {
            let migrations: Vec<FrameMigration> = moved_ids
                .iter()
                .map(|&id| FrameMigration {
                    id,
                    prev_output: Some(src_name.to_string()),
                    previous_frame: None,
                })
                .collect();
            self.send_frame_migration_output_events(dst_output, &migrations);
        }

        self.refresh_primary_assignments();
        if let Some(dst_output) = &dst_output {
            self.update_frames_for_working_area(dst_output);
        }
        self.prune_dangling_focus();
        self.focus_any_frame_if_unfocused("frame_set_move");
        if let Some(id) = prev_src_active
            && frame_ids.contains(&id)
        {
            self.queue_event(Event::FrameHidden { id });
        }
        self.reconcile_active_for_output(src_name, prev_src_active);
        self.reconcile_active_for_output(dst_name, prev_dst_active);
        for id in moved_ids {
            let home_output = self
                .frame_set
                .home_output(id)
                .unwrap_or(dst_name)
                .to_string();
            self.queue_event(Event::FrameMoved {
                id,
                output: dst_name.to_string(),
                home_output,
            });
        }

        true
    }

    fn migrate_output_frames_to_fallback(
        &mut self,
        output_name: &str,
        home_identity: &str,
    ) -> bool {
        let frame_ids = self
            .frame_set
            .mapped_strip(output_name)
            .map(|strip| {
                strip
                    .frames
                    .iter()
                    .map(|frame| frame.surface_id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.frame_set
            .remember_frame_homes(home_identity, frame_ids.iter().copied());
        let fallback = self
            .sorted_outputs
            .iter()
            .map(Output::name)
            .find(|name| name != output_name);
        fallback.is_some_and(|fallback| {
            self.move_frame_ids_between_outputs(output_name, &fallback, &frame_ids)
        })
    }

    fn restore_home_output_frames(&mut self, output_name: &str) -> bool {
        // Match frames by the reconnecting output's stable identity, not its
        // connector name, so a display that re-enumerated on a different
        // connector still reclaims its frames.
        let Some(home_identity) = self
            .mapped_output_by_name(output_name)
            .map(crate::utils::output_identity)
        else {
            return false;
        };
        let source_moves = self
            .frame_set
            .mapped_strips()
            .iter()
            .filter(|(src_name, _)| src_name.as_str() != output_name)
            .filter_map(|(src_name, strip)| {
                let frame_ids = strip
                    .frames
                    .iter()
                    .filter(|frame| {
                        self.frame_set
                            .home_output(frame.surface_id)
                            .is_some_and(|home| home == home_identity)
                    })
                    .map(|frame| frame.surface_id)
                    .collect::<Vec<_>>();
                (!frame_ids.is_empty()).then(|| (src_name.to_string(), frame_ids))
            })
            .collect::<Vec<_>>();

        let mut moved = false;
        for (src_name, frame_ids) in source_moves {
            moved |= self.move_frame_ids_between_outputs(&src_name, output_name, &frame_ids);
        }
        moved
    }

    /// Adopt every homeless frame onto a newly mapped output.
    fn adopt_homeless_frames(&mut self, dst_name: &str) -> bool {
        let mut moved = self.frame_set.take_homeless_frames();
        if moved.is_empty() {
            return false;
        }

        let pre_dst = self.strip_entries(dst_name);
        let prev_dst_active = self.active_frame_surface_id(dst_name);
        let moved_ids = moved
            .iter()
            .map(|frame| frame.surface_id)
            .collect::<Vec<_>>();

        let clock = self.animations_clock.clone();
        let dst_strip = self.frame_set.ensure_mapped_strip(dst_name, clock);
        let insert_at = dst_strip.frames.len();
        dst_strip.frames.append(&mut moved);
        if insert_at == 0 {
            dst_strip.set_active(0);
        }

        self.reconcile_output_entries(dst_name, &pre_dst);
        if let Some(dst_output) = self.find_mapped_output(dst_name) {
            let migrations: Vec<FrameMigration> = moved_ids
                .iter()
                .map(|&id| FrameMigration {
                    id,
                    prev_output: None,
                    previous_frame: None,
                })
                .collect();
            self.send_frame_migration_output_events(&dst_output, &migrations);
            self.update_frames_for_working_area(&dst_output);
        }
        self.refresh_primary_assignments();
        self.reconcile_active_for_output(dst_name, prev_dst_active);
        for id in moved_ids {
            let home_output = self
                .frame_set
                .home_output(id)
                .unwrap_or(dst_name)
                .to_string();
            self.queue_event(Event::FrameMoved {
                id,
                output: dst_name.to_string(),
                home_output,
            });
        }
        true
    }

    /// Activate the output's remembered active frame if it is present again.
    fn restore_last_active_frame(&mut self, output_name: &str) {
        let Some(identity) = self
            .mapped_output_by_name(output_name)
            .map(crate::utils::output_identity)
        else {
            return;
        };
        let Some(frame_id) = self.frame_set.take_last_active(&identity) else {
            return;
        };
        let prev_active_id = self.active_frame_surface_id(output_name);
        let focused_frame_id = self.focused_frame_id;
        let Some(strip) = self.frame_set.mapped_strip_mut(output_name) else {
            return;
        };
        // The focused frame stays active; focus continuity wins.
        if strip
            .frames
            .iter()
            .any(|frame| frame.surface_id == focused_frame_id)
        {
            return;
        }
        let Some(idx) = strip
            .frames
            .iter()
            .position(|frame| frame.surface_id == frame_id)
        else {
            return;
        };
        strip.set_active(idx);
        self.reconcile_active_for_output(output_name, prev_active_id);
    }

    /// Adopt homeless frames and pull homed frames back onto an output that
    /// just became mapped.
    fn attach_frames_for_mapped_output(&mut self, output: &Output, focus_reason: &'static str) {
        let output_name = output.name();
        let adopted = self.adopt_homeless_frames(&output_name);
        let restored = self.restore_home_output_frames(&output_name);
        if !adopted && !restored {
            self.reconcile_mapped_output_entries(output);
            self.reconcile_active_for_output(&output_name, None);
        }
        self.restore_last_active_frame(&output_name);
        if self.focused_frame_id == 0
            && let Some(id) = self.active_frame_surface_id(&output_name)
        {
            self.set_focus_frame(id, focus_reason, false);
        }
        self.focus_any_frame_if_unfocused(focus_reason);
    }

    /// Follow the focused frame's selected window after a merge, even if only its surface changed.
    fn sync_focus_after_layout_merge(&mut self, output_name: &str, focused_surface: u64) {
        let frame_id = self.focused_frame_id;
        let mut selected = None;

        let Some(strip) = self.frame_set.mapped_strip_mut(output_name) else {
            return;
        };
        if let Some(new_idx) = strip
            .frames
            .iter()
            .position(|frame| frame.surface_id == frame_id)
        {
            strip.set_active(new_idx);
            selected = Some(strip.frames[new_idx].selected_entry_id);
        } else if !strip.frames.is_empty() {
            let clamped = strip.active_idx().min(strip.frames.len() - 1);
            strip.set_active(clamped);
        }

        let Some(new_entry) = selected else {
            return;
        };
        let entry_changed = new_entry != self.focused_entry_id;
        self.set_focus_state(frame_id, new_entry);
        if entry_changed || self.focused_surface_id() != focused_surface {
            self.clear_focused_urgency();
            self.flush_pending_title_event(self.focused_surface_id());
            module::record_focus(self.focused_surface_id(), "layout_selected", None);
        }
    }

    fn sync_focus_after_floating_layout_merge(&mut self, output_name: &str, focused_surface: u64) {
        let frame_id = self.focused_frame_id;
        let mut selected = None;

        let Some(space) = self.floating_spaces.get_mut(output_name) else {
            return;
        };
        if let Some(idx) = space
            .frames
            .iter()
            .position(|frame| frame.surface_id == frame_id)
        {
            selected = Some(space.frames[idx].selected_entry_id);
            space.activate_frame(frame_id);
        }

        let Some(new_entry) = selected else {
            return;
        };
        let entry_changed = new_entry != self.focused_entry_id;
        self.set_focus_state(frame_id, new_entry);
        if entry_changed || self.focused_surface_id() != focused_surface {
            self.clear_focused_urgency();
            self.flush_pending_title_event(self.focused_surface_id());
            module::record_focus(self.focused_surface_id(), "floating_layout_selected", None);
        }
    }

    /// Apply a declarative per-output layout.
    ///
    /// Replaces the previous layout for the given output. Surfaces removed from
    /// this output get `wl_surface.leave`; new surfaces get `wl_surface.enter`
    /// and scale notification.
    pub fn apply_output_layout(&mut self, output_name: &str, mut frames: Vec<strip::Frame>) {
        // 1. Find the output
        let output = match self.find_mapped_output(output_name) {
            Some(o) => o,
            None => {
                warn!("apply_output_layout: output '{}' not found", output_name);
                return;
            }
        };

        // Only frames seeing their first home need the identity string; skip
        // building it in the steady state where every frame already has one.
        if frames
            .iter()
            .any(|f| f.surface_id != 0 && self.frame_set.home_output(f.surface_id).is_none())
        {
            let home_identity = crate::utils::output_identity(&output);
            self.frame_set
                .remember_frame_homes(&home_identity, frames.iter().map(|f| f.surface_id));
        }
        let migrations = self.collect_frame_migrations(output_name, &frames);
        for frame in &mut frames {
            frame.clear_compositor_view_state();
        }
        Self::preserve_migrated_frame_state(&mut frames, &migrations);
        // Capture affected outputs before the swap; reconciliation runs after
        // the merge against the post-merge strips so kept frames don't leak
        // `surface_outputs`, and active-frame event balance has a stable base.
        let snapshots = self.output_layout_snapshots(output_name, &migrations);
        self.remove_migrated_frames_from_sources(&migrations, &snapshots);

        // Omitted frames keep their chrome until Wayland destroy arrives, but
        // their layout entries are cleared by `merge_layout_frames`.
        let clock = self.animations_clock.clone();
        let focused_surface = self.focused_surface_id();
        let strip_entry = self.frame_set.ensure_mapped_strip(output_name, clock);
        strip_entry.merge_layout_frames(frames, module::has_pending_active_frame_close);
        self.sync_focus_after_layout_merge(output_name, focused_surface);

        self.reconcile_entry_snapshots(&snapshots);
        self.send_frame_migration_output_events(&output, &migrations);

        self.refresh_primary_assignments();
        self.update_frames_for_working_area(&output);
        self.prune_dangling_focus();
        // Covers the race where `apply_output_layout` runs before
        // `new_toplevel`'s auto-focus.
        self.focus_any_frame_if_unfocused("auto_focus_on_layout");
        self.focus_pending_activation_surfaces();

        debug!(
            "apply_output_layout: output '{}' with {} surface entries",
            output_name,
            self.strip_entries(output_name).len(),
        );

        for snapshot in snapshots {
            self.reconcile_active_for_output(&snapshot.output_name, snapshot.active_frame_id);
        }
    }

    /// Apply floating frame contents for an output; Rust keeps positions and stacking.
    pub fn apply_floating_layout(&mut self, output_name: &str, frames: Vec<strip::Frame>) {
        let pre = self.strip_entries(output_name);
        let focused_surface = self.focused_surface_id();
        let Some(space) = self.ensure_floating_space_for_output(output_name) else {
            warn!("apply_floating_layout: output '{}' not found", output_name);
            return;
        };
        space.merge_layout_frames(frames);
        self.sync_focus_after_floating_layout_merge(output_name, focused_surface);
        let post = self.strip_entries(output_name);
        let output = self.find_mapped_output(output_name);
        self.reconcile_output_membership(output_name, output.as_ref(), &pre, &post);
        self.refresh_primary_assignments();
        self.prune_dangling_focus();
        self.focus_pending_activation_surfaces();
        if let Some(output) = output {
            self.queue_redraw(&output);
        }
    }

    /// Run `f` on the floating space owning `id`, redrawing on change; false if none.
    fn with_floating_space(
        &mut self,
        id: u64,
        f: impl FnOnce(&mut floating::FloatingSpace) -> bool,
    ) -> bool {
        let Some(output_name) = self.emacs_frame_output(id).map(str::to_string) else {
            return false;
        };
        let Some(space) = self.floating_spaces.get_mut(&output_name) else {
            return false;
        };
        let changed = f(space);
        if changed && let Some(output) = self.find_mapped_output(&output_name) {
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
        let Some(output_name) = self.emacs_frame_output(id).map(str::to_string) else {
            return false;
        };
        let Some(space) = self.floating_spaces.get_mut(&output_name) else {
            return false;
        };
        let Some(size) = f(space) else {
            return false;
        };
        self.configure_floating_frame_size(id, size);
        if let Some(output) = self.find_mapped_output(&output_name) {
            self.queue_redraw(&output);
        }
        true
    }

    pub fn resize_floating_frame(&mut self, id: u64, dw: f64, dh: f64) -> bool {
        self.with_floating_resize(id, |space| space.resize_frame_by(id, Size::from((dw, dh))))
    }

    fn configure_floating_frame_size(&self, id: u64, size: Size<i32, Logical>) {
        let Some(window) = self.id_windows.get(&id) else {
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

    pub fn interactive_move_floating_frame_begin(&mut self, id: u64) -> bool {
        self.with_floating_space(id, |space| space.interactive_move_begin(id))
    }

    pub fn interactive_move_floating_frame_update(
        &mut self,
        id: u64,
        delta: Point<f64, Logical>,
    ) -> bool {
        self.with_floating_space(id, |space| space.interactive_move_update(id, delta))
    }

    pub fn interactive_move_floating_frame_end(&mut self, id: u64) {
        self.with_floating_space(id, |space| {
            space.interactive_move_end(Some(id));
            true
        });
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
    /// configure its toplevel.  Off-screen frames don't bid.
    fn refresh_primary_assignments(&mut self) {
        let output_sizes = self.output_sizes_by_name();
        let best_area = self.primary_entry_areas(&output_sizes);
        self.assign_primary_entries(&output_sizes, &best_area);
        self.refresh_mapped_cast_outputs();
        self.configure_primary_entries();
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
        fullscreen: bool,
    ) -> u64 {
        if fullscreen {
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
        for (oname, strip) in self.mapped_output_strips() {
            let active_idx = strip.active_idx();
            let Some(frame) = strip.frames.get(active_idx) else {
                continue;
            };
            for entry in frame.entries.iter().filter(|entry| entry.surface.is_some()) {
                let fullscreen = frame.entry_fullscreen(entry.id);
                let area = Self::layout_entry_area(output_sizes, oname, entry, fullscreen);
                let Some(surface_id) = entry.surface_id() else {
                    continue;
                };
                best_area
                    .entry(surface_id)
                    .and_modify(|a| *a = (*a).max(area))
                    .or_insert(area);
            }
        }
        for (oname, space) in &self.floating_spaces {
            for frame in &space.frames {
                for entry in frame.entries.iter().filter(|entry| entry.surface.is_some()) {
                    let fullscreen = frame.entry_fullscreen(entry.id);
                    let area = Self::layout_entry_area(output_sizes, oname, entry, fullscreen);
                    let Some(surface_id) = entry.surface_id() else {
                        continue;
                    };
                    best_area
                        .entry(surface_id)
                        .and_modify(|a| *a = (*a).max(area))
                        .or_insert(area);
                }
            }
        }
        best_area
    }

    fn assign_primary_entries(
        &mut self,
        output_sizes: &HashMap<String, (u64, u64)>,
        best_area: &HashMap<u64, u64>,
    ) {
        for frame in self.frame_set.all_frames_mut() {
            for entry in frame
                .entries
                .iter_mut()
                .filter(|entry| entry.surface.is_some())
            {
                entry.set_primary(false);
            }
        }
        for space in self.floating_spaces.values_mut() {
            for entry in space.surface_entries_mut() {
                entry.set_primary(false);
            }
        }

        let mut assigned: HashSet<u64> = HashSet::new();
        for output in &self.sorted_outputs {
            let oname = output.name();
            if let Some(strip) = self.frame_set.mapped_strip_mut(&oname) {
                let active_idx = strip.active_idx();
                let Some(frame) = strip.frames.get_mut(active_idx) else {
                    continue;
                };
                let fullscreen_entry_id = frame.fullscreen.map(|fullscreen| fullscreen.entry_id);
                for entry in frame
                    .entries
                    .iter_mut()
                    .filter(|entry| entry.surface.is_some())
                {
                    let Some(surface_id) = entry.surface_id() else {
                        continue;
                    };
                    let dominated = best_area.get(&surface_id).copied().unwrap_or(0);
                    let fullscreen = fullscreen_entry_id == Some(entry.id);
                    if Self::layout_entry_area(output_sizes, &oname, entry, fullscreen) == dominated
                        && assigned.insert(surface_id)
                    {
                        entry.set_primary(true);
                    }
                }
            }
            if let Some(space) = self.floating_spaces.get_mut(&oname) {
                for frame in &mut space.frames {
                    let fullscreen_entry_id =
                        frame.fullscreen.map(|fullscreen| fullscreen.entry_id);
                    for entry in frame
                        .entries
                        .iter_mut()
                        .filter(|entry| entry.surface.is_some())
                    {
                        let Some(surface_id) = entry.surface_id() else {
                            continue;
                        };
                        let dominated = best_area.get(&surface_id).copied().unwrap_or(0);
                        let fullscreen = fullscreen_entry_id == Some(entry.id);
                        if Self::layout_entry_area(output_sizes, &oname, entry, fullscreen)
                            == dominated
                            && assigned.insert(surface_id)
                        {
                            entry.set_primary(true);
                        }
                    }
                }
            }
        }
    }

    fn refresh_mapped_cast_outputs(&mut self) {
        #[cfg(feature = "screencast")]
        {
            self.introspect_windows_dirty = true;
            let mut seen = HashSet::new();
            self.casting.mapped_cast_output.clear();
            for output in &self.sorted_outputs {
                let Some(strip) = self.frame_set.mapped_strip(&output.name()) else {
                    continue;
                };
                for entry in strip.entries() {
                    seen.insert(entry.id);
                    self.casting
                        .mapped_cast_output
                        .insert(entry.id, output.clone());
                }
            }
            let sessions_to_stop: Vec<screencasting::CastSessionId> = self
                .casting
                .casts
                .iter()
                .filter_map(|cast| match cast.target {
                    dbus::CastTarget::LayoutEntry { entry_id } if !seen.contains(&entry_id) => {
                        Some(cast.session_id)
                    }
                    _ => None,
                })
                .collect();
            for session_id in sessions_to_stop {
                info!(%session_id, "stopping layout-entry cast (view removed)");
                self.stop_cast(session_id);
            }
        }
    }

    fn configure_primary_entries(&self) {
        for output in &self.sorted_outputs {
            let Some(strip) = self.frame_set.mapped_strip(&output.name()) else {
                continue;
            };
            let active_idx = strip.active_idx();
            let Some(frame) = strip.frames.get(active_idx) else {
                continue;
            };
            for entry in frame.entries.iter().filter(|entry| entry.primary()) {
                self.configure_primary(output, frame.entry_fullscreen(entry.id), entry);
            }
        }
        for output in &self.sorted_outputs {
            let oname = output.name();
            let Some(space) = self.floating_spaces.get(&oname) else {
                continue;
            };
            for frame in &space.frames {
                for entry in frame.entries.iter().filter(|entry| entry.primary()) {
                    self.configure_primary(output, frame.entry_fullscreen(entry.id), entry);
                }
            }
        }
    }

    fn configure_primary(&self, output: &Output, fullscreen: bool, entry: &LayoutEntry) {
        let Some(surface_id) = entry.surface_id() else {
            return;
        };
        let Some(window) = self.id_windows.get(&surface_id) else {
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
        let new_size = if fullscreen {
            let s = crate::utils::output_size(output);
            (s.w as i32, s.h as i32).into()
        } else {
            (entry.w as i32, entry.h as i32).into()
        };
        let changed = toplevel.with_pending_state(|state| {
            let was_fullscreen = state.states.contains(XdgToplevelState::Fullscreen);
            let was_maximized = state.states.contains(XdgToplevelState::Maximized);
            let changed = state.size != Some(new_size)
                || was_fullscreen != fullscreen
                || was_maximized == fullscreen;
            state.size = Some(new_size);
            if fullscreen {
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

    fn pointer_is_on_output(&self, output: &Output) -> bool {
        let pos: Point<f64, Logical> = self.cursor_location().into();
        self.space
            .output_under(pos)
            .any(|candidate| candidate == output)
    }

    /// Current pointer location, read from Smithay's pointer handle.
    /// This is the single source of truth, updated by `pointer.motion()`,
    /// `pointer.set_location()`, and `cursor_position_hint`.
    pub fn pointer_location(&self) -> (f64, f64) {
        let pos = self.pointer.current_location();
        (pos.x, pos.y)
    }

    /// Where the cursor is drawn: the tablet tool while one is in use, else the pointer.
    pub fn cursor_location(&self) -> (f64, f64) {
        match self.tablet_cursor_location {
            Some(pos) => (pos.x, pos.y),
            None => self.pointer_location(),
        }
    }

    /// Hand the drawn cursor back to the pointer after real pointer input.
    pub fn clear_tablet_cursor(&mut self) {
        if self.tablet_cursor_location.take().is_some() {
            self.queue_redraw_for_pointer();
        }
    }

    /// Queue a redraw for outputs overlapping the cursor.
    /// Redraws the output the pointer is currently on, plus the previous
    /// output if the pointer crossed a boundary.
    pub fn queue_redraw_for_pointer(&mut self) {
        self.refresh_cursor_outputs();

        let pos = smithay::utils::Point::from(self.cursor_location());
        let current = self.output_at(pos).cloned();
        let prev = self.pointer_output.take();

        if let Some(ref prev) = prev
            && current.as_ref() != Some(prev)
        {
            self.queue_redraw(prev);
        }
        if let Some(ref output) = current {
            self.queue_redraw(output);
        }
        self.pointer_output = current;
    }

    /// Refresh output enter/leave, scale, and transform for transient pointer surfaces.
    ///
    /// Cursor and DnD surfaces are not owned by a frame, layer, or lock surface, so
    /// they need explicit output tracking based on the current pointer position.
    pub fn refresh_cursor_outputs(&self) {
        if self.cursor_hide.is_hidden {
            return;
        }

        tracy_span!("Ewm::refresh_cursor_outputs");

        let pointer_pos: Point<f64, Logical> = self.cursor_location().into();

        match self.cursor_manager.cursor_image() {
            CursorImageStatus::Surface(surface) => {
                let hotspot = smithay::wayland::compositor::with_states(surface, |states| {
                    states
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .hotspot
                });

                let surface_pos = pointer_pos.to_i32_round() - hotspot;
                let bbox = bbox_from_surface_tree(surface, surface_pos);

                let dnd = self
                    .dnd_icon
                    .as_ref()
                    .map(|icon| &icon.surface)
                    .map(|surface| (surface, bbox_from_surface_tree(surface, surface_pos)));

                let mut cursor_scale = 1.0;
                let mut cursor_transform = Transform::Normal;
                let mut dnd_scale = 1.0;
                let mut dnd_transform = Transform::Normal;
                for output in &self.sorted_outputs {
                    let Some(geo) = self.space.output_geometry(output) else {
                        continue;
                    };

                    if let Some(mut overlap) = geo.intersection(bbox) {
                        overlap.loc -= surface_pos;
                        cursor_scale =
                            f64::max(cursor_scale, output.current_scale().fractional_scale());
                        cursor_transform = output.current_transform();
                        output_update(output, Some(overlap), surface);
                    } else {
                        output_update(output, None, surface);
                    }

                    if let Some((surface, bbox)) = dnd {
                        if let Some(mut overlap) = geo.intersection(bbox) {
                            overlap.loc -= surface_pos;
                            dnd_scale =
                                f64::max(dnd_scale, output.current_scale().fractional_scale());
                            dnd_transform = output.current_transform();
                            output_update(output, Some(overlap), surface);
                        } else {
                            output_update(output, None, surface);
                        }
                    }
                }

                smithay::wayland::compositor::with_states(surface, |data| {
                    crate::utils::send_scale_transform(
                        surface,
                        data,
                        output::Scale::Fractional(cursor_scale),
                        cursor_transform,
                    );
                });
                if let Some((surface, _)) = dnd {
                    smithay::wayland::compositor::with_states(surface, |data| {
                        crate::utils::send_scale_transform(
                            surface,
                            data,
                            output::Scale::Fractional(dnd_scale),
                            dnd_transform,
                        );
                    });
                }
            }
            cursor_image => {
                let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) else {
                    return;
                };

                let icon = if let CursorImageStatus::Named(icon) = cursor_image {
                    *icon
                } else {
                    Default::default()
                };

                let mut dnd_scale = 1.0;
                let mut dnd_transform = Transform::Normal;
                for output in &self.sorted_outputs {
                    let Some(geo) = self.space.output_geometry(output) else {
                        continue;
                    };

                    let output_scale = output.current_scale().integer_scale();
                    let cursor = self
                        .cursor_manager
                        .get_cursor_with_name(icon, output_scale)
                        .unwrap_or_else(|| self.cursor_manager.get_default_cursor(output_scale));
                    let hotspot =
                        cursor::XCursor::hotspot(&cursor.frames()[0]).to_logical(output_scale);

                    let surface_pos = pointer_pos.to_i32_round() - hotspot;
                    let bbox = bbox_from_surface_tree(surface, surface_pos);

                    if let Some(mut overlap) = geo.intersection(bbox) {
                        overlap.loc -= surface_pos;
                        dnd_scale = f64::max(dnd_scale, output.current_scale().fractional_scale());
                        dnd_transform = output.current_transform();
                        output_update(output, Some(overlap), surface);
                    } else {
                        output_update(output, None, surface);
                    }
                }

                smithay::wayland::compositor::with_states(surface, |data| {
                    crate::utils::send_scale_transform(
                        surface,
                        data,
                        output::Scale::Fractional(dnd_scale),
                        dnd_transform,
                    );
                });
            }
        }
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
            // A second locker would be refused, and spawning it reaps the first.
            IdleAction::RunCommand(_) if self.lock_client_is_live() => self.deactivate_monitors(),
            IdleAction::RunCommand(cmd) => {
                match std::process::Command::new("sh").arg("-c").arg(cmd).spawn() {
                    Ok(child) => {
                        info!("Idle command spawned: {}", cmd);
                        // Reap the child the lock guard spared.
                        if let Some(mut previous) = self.idle_timeout.child_process.replace(child) {
                            let _ = previous.kill();
                            let _ = previous.wait();
                        }
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

        // A pen in use moves constantly; hiding would just flicker.
        if self.tablet_cursor_location.is_some() {
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
        !self.frames_hidden()
            && self
                .frame_set
                .mapped_strips()
                .values()
                .any(strip::Strip::active_frame_is_fullscreen)
    }

    /// Kill idle child process (e.g., on lid close).
    pub fn kill_idle_child(&mut self) {
        // Killing the lock client leaves the session blanked for good.
        if self.lock_client_is_live() {
            return;
        }
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
            .find(|(_, state)| state.redraw_state.is_render_queued())
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
        let needs_redraw = |s: &OutputState| s.redraw_state.is_render_queued();
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
        let was_active = self.overview.is_active();
        self.overview.advance();
        if was_active && !self.overview.is_active() {
            self.set_inactive_frames_shown(false);
        }
        self.hide.advance();
        if let Some(strip) = self.frame_set.mapped_strip_mut(&output.name()) {
            strip.advance();
        }
        if let Some(space) = self.floating_spaces.get_mut(&output.name()) {
            space.advance();
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
            debug_assert!(state.redraw_state.is_render_queued());
            state.frame_clock.next_presentation_time()
        };

        // Sweep finished animations so render-path flag reflects live ones.
        self.advance_animations(output);

        // Refresh protocol state before rendering
        self.refresh_foreign_toplevel();
        let output_infos = backend.output_info_list(&self.sorted_outputs);
        self.refresh_output_management(&output_infos);

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
            state.unfinished_animations_remain =
                self.overview.is_animating() || self.hide.is_animation_ongoing();
        }
        if let Some(strip) = self.frame_set.mapped_strip(&output.name()) {
            let any_open_fade = strip
                .surface_entries()
                .filter_map(LayoutEntry::surface_id)
                .any(|surface_id| self.surface_open_anims.contains_key(&surface_id))
                || strip
                    .frames
                    .iter()
                    .any(|f| self.surface_open_anims.contains_key(&f.surface_id));
            if (strip.animations_ongoing() || any_open_fade)
                && let Some(state) = self.output_state.get_mut(output)
            {
                state.unfinished_animations_remain = true;
            }
        }
        if let Some(space) = self.floating_spaces.get(&output.name()) {
            let any_open_fade = space
                .surface_entries()
                .filter_map(LayoutEntry::surface_id)
                .any(|surface_id| self.surface_open_anims.contains_key(&surface_id))
                || space
                    .frames
                    .iter()
                    .any(|f| self.surface_open_anims.contains_key(&f.surface_id));
            if (space.animations_ongoing() || any_open_fade)
                && let Some(state) = self.output_state.get_mut(output)
            {
                state.unfinished_animations_remain = true;
            }
        }
        if !self.cursor_hide.is_hidden
            && self.pointer_is_on_output(output)
            && self
                .cursor_manager
                .is_current_cursor_animated(output.current_scale().integer_scale())
            && let Some(state) = self.output_state.get_mut(output)
        {
            state.unfinished_animations_remain = true;
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
        if let Some(output_state) = self.output_state.get(output)
            && let Some(ref lock_surface) = output_state.lock_surface
        {
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

        if let CursorImageStatus::Surface(surface) = self.cursor_manager.cursor_image() {
            with_surface_tree_downward(
                surface,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |surface, states, _| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        states,
                        None,
                        render_element_states,
                        default_primary_scanout_output_compare,
                    );
                },
                |_, _, _| true,
            );
        }

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
                        default_primary_scanout_output_compare,
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

        if let Some(output_state) = self.output_state.get(output)
            && let Some(ref lock_surface) = output_state.lock_surface
        {
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

        if let CursorImageStatus::Surface(surface) = self.cursor_manager.cursor_image() {
            send_dmabuf_feedback_surface_tree(
                surface,
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
        if let Some(output_state) = self.output_state.get(output)
            && let Some(ref lock_surface) = output_state.lock_surface
        {
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

        if let CursorImageStatus::Surface(surface) = self.cursor_manager.cursor_image() {
            take_presentation_feedback_surface_tree(
                surface,
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

            if let Some((last_output, last_sequence)) = &*last_sent_at
                && last_output == output
                && *last_sequence == sequence
            {
                return None;
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

        if let Some(output_state) = self.output_state.get(output)
            && let Some(ref lock_surface) = output_state.lock_surface
        {
            send_frames_surface_tree(
                lock_surface.wl_surface(),
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                &should_send,
            );
        }

        if let Some(icon) = self.dnd_icon.as_ref() {
            send_frames_surface_tree(
                &icon.surface,
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                &should_send,
            );
        }

        if let CursorImageStatus::Surface(surface) = self.cursor_manager.cursor_image() {
            send_frames_surface_tree(
                surface,
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
    fn fallback_hidden_emacs_ids(&self) -> HashSet<u64> {
        let visible: HashSet<u64> = self
            .mapped_output_strips()
            .filter_map(|(_, strip)| strip.active_frame().map(|frame| frame.surface_id))
            .chain(
                self.floating_spaces
                    .values()
                    .flat_map(|space| space.frames.iter().map(|frame| frame.surface_id)),
            )
            // Hidden frames have slid off their outputs too.
            .filter(|_| !self.frames_hidden())
            .collect();
        self.frame_set
            .all_frames()
            .map(|frame| frame.surface_id)
            .chain(
                self.floating_spaces
                    .values()
                    .flat_map(|space| space.frames.iter().map(|frame| frame.surface_id)),
            )
            .filter(|id| !visible.contains(id))
            .collect()
    }

    pub fn send_frame_callbacks_on_fallback_timer(&self) {
        // Off-screen emacs frames mustn't be woken; their pgtk client would
        // redisplay on the emacs main thread for nothing.
        let hidden_emacs_ids = self.fallback_hidden_emacs_ids();

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

        if let Some(icon) = self.dnd_icon.as_ref() {
            let output = self.first_output().unwrap_or(&output);
            send_frames_surface_tree(
                &icon.surface,
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                |_, _| None,
            );
        }

        if let CursorImageStatus::Surface(surface) = self.cursor_manager.cursor_image() {
            let output = self.first_output().unwrap_or(&output);
            send_frames_surface_tree(
                surface,
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
                    id,
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
    pub fn refresh_output_management(&mut self, output_infos: &[OutputInfo]) {
        if !self.output_management_state.output_heads_changed {
            return;
        }
        self.output_management_state.output_heads_changed = false;
        let new_state = self.build_output_head_states(output_infos);
        self.output_management_state.notify_changes(new_state);
    }

    /// Output name for a surface, preferring the focused layout entry when a
    /// surface appears on multiple outputs.
    fn surface_output_name(&self, surface_id: u64) -> Option<&str> {
        if self.surface_outputs.contains_key(&surface_id) {
            if let Some(location) = self.focused_entry_for_surface(surface_id) {
                return Some(location.output_name);
            }
            if let Some(name) = self.canonical_surface_output_name(surface_id) {
                return Some(name);
            }
        }

        self.emacs_frame_output(surface_id)
    }

    fn canonical_surface_output_name(&self, surface_id: u64) -> Option<&str> {
        let output_names = self.surface_outputs.get(&surface_id)?;
        self.sorted_outputs
            .iter()
            .find_map(|output| {
                let name = output.name();
                output_names.get(name.as_str()).map(String::as_str)
            })
            .or_else(|| output_names.iter().map(String::as_str).min())
    }

    /// Find the output for a surface (returns Output object)
    fn find_surface_output(&self, surface_id: u64) -> Option<smithay::output::Output> {
        let output_name = self.surface_output_name(surface_id)?;
        self.find_mapped_output(output_name)
    }

    /// Set the Emacs process PID for client identification
    pub fn set_emacs_pid(&mut self, pid: u32) {
        info!("Tracking Emacs PID: {}", pid);
        self.emacs_pid = Some(pid);
    }

    /// Check if focus is on an Emacs surface (for key interception decisions)
    pub fn is_focus_on_emacs(&self) -> bool {
        self.is_emacs_frame(self.focused_surface_id())
    }

    pub(crate) fn is_keyboard_focus_on_emacs(&self) -> bool {
        self.keyboard_focus
            .as_ref()
            .and_then(|surface| self.surface_id(surface))
            .is_some_and(|id| self.is_emacs_frame(id))
    }

    pub(crate) fn begin_key_hold(&mut self, keycode: u32, owner: HeldKeyOwner) {
        self.im_repeat.begin_hold(keycode, owner);
    }

    pub(crate) fn held_key_owner(&self, keycode: u32) -> Option<HeldKeyOwner> {
        self.im_repeat.held_owner(keycode)
    }

    pub(crate) fn end_key_hold(&mut self, keycode: u32) {
        self.im_repeat.end_hold(&self.loop_handle, keycode);
    }

    pub(crate) fn configure_keyboard_repeat(&mut self, rate: i32, delay: i32) {
        self.im_repeat
            .configure_repeat(&self.loop_handle, rate, delay);
    }

    pub(crate) fn start_text_input_repeat(&mut self, key: TextInputKey) {
        self.im_repeat
            .start_text_input_repeat(&self.loop_handle, key);
    }

    pub(crate) fn start_command_repeat(&mut self, keycode: u32, key: String) {
        self.im_repeat
            .start_command_repeat(&self.loop_handle, keycode, key);
    }

    pub(crate) fn cancel_command_repeat(&mut self) {
        self.im_repeat.cancel_command_repeat(&self.loop_handle);
    }

    fn take_intercept_repeat_for_timer(&mut self, keycode: u32) -> Option<InterceptRepeat> {
        self.im_repeat.take_due_repeat(keycode)
    }

    fn reschedule_intercept_repeat(&mut self, keycode: u32) {
        self.im_repeat
            .reschedule_if_active(&self.loop_handle, keycode);
    }

    pub(crate) fn emacs_surface_for_focused_output(&self) -> Option<WlSurface> {
        self.get_emacs_surface_for_focused_output()
            .and_then(|id| self.id_windows.get(&id))
            .and_then(|window| window.wl_surface())
            .map(|surface| surface.into_owned())
    }

    fn emacs_keyboard_redirect_focus(&self) -> Option<WlSurface> {
        if !self.im_repeat.has_keyboard_redirect_hold() && !module::get_keyboard_capture() {
            return None;
        }

        self.emacs_surface_for_focused_output().or_else(|| {
            self.keyboard_focus
                .as_ref()
                .filter(|surface| {
                    self.surface_id(surface)
                        .is_some_and(|id| self.is_emacs_frame(id))
                })
                .cloned()
        })
    }

    /// Focus a surface by id; resolves to either a layout entry or an Emacs frame and
    /// delegates to the canonical mutator (`set_focus_entry` / `set_focus_frame`).
    /// Callers with a known kind should call those directly.
    pub fn set_focus(&mut self, id: u64, notify_emacs: bool, source: &str) -> bool {
        let (frame_id, entry_id) = self.resolve_focus_target(id);
        if let Some(entry_id) = entry_id {
            self.set_focus_entry(entry_id, source, notify_emacs);
            true
        } else if frame_id != 0 {
            if self.is_emacs_frame(frame_id) {
                self.set_focus_frame(frame_id, source, notify_emacs);
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    fn resolve_focus_target(&self, id: u64) -> (u64, Option<LayoutEntryId>) {
        if self.is_emacs_frame(id) {
            return (id, None);
        }
        if let Some((frame_id, entry_id)) = self.frame_entry_for_surface(id) {
            (frame_id, Some(entry_id))
        } else {
            (id, None)
        }
    }

    pub fn focus_target_ready(&self, id: u64) -> bool {
        let (frame_id, entry_id) = self.resolve_focus_target(id);
        entry_id.is_some() || (frame_id != 0 && self.is_emacs_frame(frame_id))
    }

    /// Focus the surface, or ask Emacs to show it when no window does.
    pub fn activate_or_defer_surface(&mut self, id: u64, notify_emacs: bool, source: &str) {
        if self.set_focus(id, notify_emacs, source) {
            self.pending_activation_surfaces.remove(&id);
        } else {
            self.pending_activation_surfaces.insert(id);
            if notify_emacs {
                self.queue_event(Event::ActivateSurface { id });
            }
        }
    }

    fn focus_pending_activation_surfaces(&mut self) {
        let mut ready: Vec<_> = self
            .pending_activation_surfaces
            .iter()
            .copied()
            .filter(|&id| self.focus_target_ready(id))
            .collect();
        ready.sort_unstable();

        for id in ready {
            self.pending_activation_surfaces.remove(&id);
            self.set_focus(id, true, "xdg_activation_pending");
        }
    }

    /// Focus the entity under `pos`; returns `(frame_id, entry_id)`.
    /// Hit-test `pos` and focus the resulting layout entry or frame. Callers retain
    /// control over emitting the `Focus` event (for ffm coordinates), so this
    /// passes `notify_emacs=false` to the canonical mutators.
    pub fn focus_under(
        &mut self,
        pos: Point<f64, Logical>,
        source: &str,
    ) -> Option<(u64, Option<LayoutEntryId>)> {
        let (frame_id, entry_id) = if let Some(entry_id) = self.layout_entry_id_under(pos) {
            let location = self.lookup_entry(entry_id)?;
            let frame_id = location.frame_surface_id;
            self.set_focus_entry_at(entry_id, location, source, false);
            (frame_id, Some(entry_id))
        } else {
            let (surface, _) = self.surface_under_point(pos)?;
            let sid = self.surface_id(&surface)?;
            let (fid, resolved_entry_id) = self.resolve_focus_target(sid);
            if let Some(entry_id) = resolved_entry_id {
                self.set_focus_entry(entry_id, source, false);
            } else if fid != 0 {
                self.set_focus_frame(fid, source, false);
            } else {
                return None;
            }
            (fid, resolved_entry_id)
        };
        Some((frame_id, entry_id))
    }

    pub fn focus_follows_mouse(&mut self, under: &Option<SurfaceHit>, pos: Point<f64, Logical>) {
        let Some((_, origin)) = under else { return };
        let Some((frame_id, entry_id)) = self.focus_under(pos, "pointer_motion") else {
            return;
        };
        if let Some(focus_id) = entry_id.or_else(|| self.focus_id_of_frame(frame_id)) {
            self.queue_event(Event::Focus {
                focus_id,
                x: Some(pos.x - origin.x),
                y: Some(pos.y - origin.y),
                pointer: true,
            });
        }
    }

    /// Update on-demand layer shell keyboard focus.
    /// If the surface has OnDemand keyboard interactivity, set it as on-demand focus.
    /// Otherwise, clear on-demand focus.
    pub fn focus_layer_surface_if_on_demand(&mut self, surface: Option<DesktopLayerSurface>) {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        if let Some(surface) = surface
            && surface.cached_state().keyboard_interactivity == KeyboardInteractivity::OnDemand
        {
            if self.layer_shell_on_demand_focus.as_ref() != Some(&surface) {
                self.layer_shell_on_demand_focus = Some(surface);
            }
            return;
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

            map.layers_on(layer).find_map(|surface| {
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

            map.layers_on(layer).find_map(|surface| {
                if surface == on_demand {
                    Some(surface.wl_surface().clone())
                } else {
                    None
                }
            })
        };

        // Check all outputs (typically just one for EWM)
        for output in &self.sorted_outputs {
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
        module::push_event(event);
    }

    pub(crate) fn record_emacs_keyboard_activity(&mut self) {
        self.last_emacs_keyboard_activity = Instant::now();
    }

    fn queue_title_update(
        &mut self,
        id: u64,
        app: String,
        title: String,
        first_title: bool,
        app_changed: bool,
    ) {
        if self.is_emacs_frame(id) {
            self.discard_pending_title_event(id);
            return;
        }

        // Pre-map titles feed placement rules, so they are never debounced.
        if first_title
            || app_changed
            || self.unmapped_surfaces.contains(&id)
            || self.focused_surface_id() == id
        {
            self.discard_pending_title_event(id);
            self.queue_event(Event::Title { id, app, title });
            return;
        }

        self.pending_title_events
            .entry(id)
            .and_modify(|pending| {
                pending.app = app.clone();
                pending.title = title.clone();
            })
            .or_insert_with(|| PendingTitleEvent {
                app,
                title,
                queued_at: Instant::now(),
            });
        self.ensure_title_flush_timer(TITLE_EVENT_IDLE_FLUSH_DELAY);
    }

    fn ensure_title_flush_timer(&mut self, duration: Duration) {
        if self.title_flush_timer_token.is_some() {
            return;
        }

        match self
            .loop_handle
            .insert_source(Timer::from_duration(duration), |_, _, state| {
                state.ewm.on_title_flush_timer();
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            }) {
            Ok(token) => self.title_flush_timer_token = Some(token),
            Err(err) => warn!("Failed to insert title flush timer: {:?}", err),
        }
    }

    fn cancel_title_flush_timer(&mut self) {
        if let Some(token) = self.title_flush_timer_token.take() {
            self.loop_handle.remove(token);
        }
    }

    fn discard_pending_title_event(&mut self, id: u64) {
        self.pending_title_events.remove(&id);
        if self.pending_title_events.is_empty() {
            self.cancel_title_flush_timer();
        }
    }

    fn flush_pending_title_event(&mut self, id: u64) {
        if let Some(pending) = self.pending_title_events.remove(&id) {
            self.queue_event(Event::Title {
                id,
                app: pending.app,
                title: pending.title,
            });
        }
        if self.pending_title_events.is_empty() {
            self.cancel_title_flush_timer();
        }
    }

    fn flush_pending_title_events(&mut self) {
        self.cancel_title_flush_timer();

        let mut pending = self.pending_title_events.drain().collect::<Vec<_>>();
        pending.sort_by_key(|(id, _)| *id);
        for (id, pending) in pending {
            self.queue_event(Event::Title {
                id,
                app: pending.app,
                title: pending.title,
            });
        }
    }

    fn on_title_flush_timer(&mut self) {
        self.title_flush_timer_token = None;

        let Some(oldest) = self
            .pending_title_events
            .values()
            .map(|pending| pending.queued_at)
            .min()
        else {
            return;
        };

        if self.is_focus_on_emacs() {
            let idle_elapsed = self.last_emacs_keyboard_activity.elapsed();
            let stale_elapsed = oldest.elapsed();
            if idle_elapsed < TITLE_EVENT_IDLE_FLUSH_DELAY && stale_elapsed < TITLE_EVENT_MAX_STALE
            {
                let idle_remaining = TITLE_EVENT_IDLE_FLUSH_DELAY - idle_elapsed;
                let stale_remaining = TITLE_EVENT_MAX_STALE - stale_elapsed;
                self.ensure_title_flush_timer(idle_remaining.min(stale_remaining));
                return;
            }
        }

        self.flush_pending_title_events();
    }

    /// Emit FrameShown/FrameHidden when an output's strip-active changes.
    /// Caller snapshots `prev_active_id` before the mutation; the helper
    /// compares against the post-mutation state.  No stored state.
    pub(crate) fn reconcile_active_for_output(
        &mut self,
        output_name: &str,
        prev_active_id: Option<u64>,
    ) {
        let Some(strip) = self.frame_set.mapped_strip(output_name) else {
            return;
        };
        let new_active_id = strip.active_frame().map(|f| f.surface_id);
        if prev_active_id == new_active_id {
            return;
        }
        if let Some(id) = prev_active_id {
            // Skip if the frame was destroyed -- the lisp side already handled it.
            // The overview keeps every frame visible.
            if !self.overview.is_active() && strip.frames.iter().any(|f| f.surface_id == id) {
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
    /// disable->enable gap are queued in `im_text_input` and drained on the
    /// next `ImEvent::Activated` for the same surface.
    pub fn update_text_input_focus(&self, surface: Option<&WlSurface>, surface_id: Option<u64>) {
        let is_emacs = surface_id.is_some_and(|id| self.is_emacs_frame(id));
        im::text_input::update_focus(&self.seat, surface, is_emacs);
    }

    fn active_text_input_surface_id(&self) -> Option<u64> {
        im::text_input::active_surface_id(&self.seat, |surface| self.surface_id(surface))
    }

    fn commit_active_text_input(&self, text: String) {
        im::text_input::commit_active(&self.seat, text)
    }

    fn replace_active_text_input(&self, before: u32, after: u32, text: String) -> bool {
        im::text_input::replace_active(&self.seat, before, after, text)
    }

    fn deliver_replace_ack(&mut self, surface_id: u64, r: im::text_input::Replacement) {
        if self.replace_active_text_input(r.before, r.after, r.text) {
            self.queue_event(Event::TextInputReplaced { surface_id });
        }
    }

    /// Get surface ID from a WlSurface
    pub fn surface_id(&self, surface: &WlSurface) -> Option<u64> {
        self.id_windows.iter().find_map(|(&id, window)| {
            window
                .wl_surface()
                .and_then(|ws| (*ws == *surface).then_some(id))
        })
    }

    /// Find the Output at a global logical position.
    fn output_at(&self, pos: Point<f64, Logical>) -> Option<&Output> {
        self.space
            .output_under(pos)
            .next()
            .or_else(|| self.first_output())
    }

    pub(crate) fn first_output(&self) -> Option<&Output> {
        self.sorted_outputs.first()
    }

    /// Find the Output whose geometry strictly contains `pos`.
    ///
    /// Unlike [`Self::output_at`], returns `None` when `pos` lies in dead
    /// space between or outside outputs instead of falling back to an
    /// arbitrary output.
    fn output_containing(&self, pos: Point<f64, Logical>) -> Option<&Output> {
        self.space.output_under(pos).next()
    }

    /// True when the strip on the output under `pos` is mid-animation.
    pub fn is_strip_animating_at(&self, pos: Point<f64, Logical>) -> bool {
        let Some(output) = self.output_containing(pos) else {
            return false;
        };
        self.frame_set
            .mapped_strip(&output.name())
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
        target: Point<f64, Logical>,
        previous: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
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
        let Some(output) = self.first_output() else {
            return target;
        };
        let Some(geom) = self.space.output_geometry(output) else {
            return target;
        };
        crate::utils::center(geom).to_f64()
    }

    fn layer_hit_under(
        &self,
        output: &Output,
        layer: Layer,
        pos_within_output: Point<f64, Logical>,
        output_pos: Point<i32, Logical>,
    ) -> Option<(DesktopLayerSurface, SurfaceHit)> {
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
                return Some((layer_surface.clone(), (surface, global_pos)));
            }
        }
        None
    }

    /// Check layer surfaces on a specific layer for a surface under the point.
    /// `pos_within_output` is the point relative to the output origin.
    /// Returns the WlSurface and its location in global coordinates.
    fn layer_surface_under(
        &self,
        output: &Output,
        layer: Layer,
        pos_within_output: Point<f64, Logical>,
        output_pos: Point<i32, Logical>,
    ) -> Option<SurfaceHit> {
        self.layer_hit_under(output, layer, pos_within_output, output_pos)
            .map(|(_, hit)| hit)
    }

    /// Find the layer surface (desktop type) under a point.
    /// Used for click-to-focus on layer surfaces with OnDemand keyboard interactivity.
    pub fn layer_under_point(&self, pos: Point<f64, Logical>) -> Option<DesktopLayerSurface> {
        let output = self.output_at(pos)?;
        let output_geo = self.space.output_geometry(output)?;
        let pos_within_output = pos - output_geo.loc.to_f64();
        let above_top_layer = self.render_above_top_layer(output);

        // When fullscreen, only Overlay receives input above the fullscreen surface.
        // Top/Bottom/Background are visually behind it and must not intercept clicks.
        let layers: &[Layer] = if above_top_layer {
            &[Layer::Overlay]
        } else if self.overview.is_active() {
            // Bottom and background are zoomed away behind the overview backdrop.
            &[Layer::Overlay, Layer::Top]
        } else {
            &[Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background]
        };
        for &layer in layers {
            if let Some((layer_surface, _)) =
                self.layer_hit_under(output, layer, pos_within_output, output_geo.loc)
            {
                return Some(layer_surface);
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
    pub fn surface_under_point(&self, pos: Point<f64, Logical>) -> Option<SurfaceHit> {
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
            if !above_top_layer
                && let Some(result) =
                    self.layer_surface_under(output, Layer::Top, pos_within_output, geo.loc)
            {
                return Some(result);
            }
        }

        // 3. Popups, matching render insertion above layout surfaces and frames.
        if let Some(result) = self.popup_surface_under(pos) {
            return Some(result);
        }

        // 4. Layout surfaces (entries + frame's own surface)
        if let Some(result) = self.layout_surface_under(pos) {
            return Some(result);
        }

        // 5. Deferred layers (Top deferred behind fullscreen, then Bottom/Background)
        if let (Some(output), Some(geo)) = (output, output_geo) {
            let pos_within_output = pos - geo.loc.to_f64();

            if above_top_layer
                && let Some(result) =
                    self.layer_surface_under(output, Layer::Top, pos_within_output, geo.loc)
            {
                return Some(result);
            }

            // Bottom and background layers are zoomed away behind the overview backdrop.
            if self.overview.is_active() {
                return None;
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

    pub(crate) fn pointer_target_at(&self, pos: Point<f64, Logical>) -> Option<SurfaceHit> {
        if self.is_locked() {
            self.lock_surface_focus()
                .map(|surface| (surface, (0.0, 0.0).into()))
        } else {
            self.surface_under_point(pos)
        }
    }

    fn pointer_focus_surface(focus: &Option<SurfaceHit>) -> Option<&WlSurface> {
        focus.as_ref().map(|(surface, _)| surface)
    }

    /// Click, move and resize grabs pin delivery; DnD and popup grabs hit-test.
    fn grab_pins_pointer_focus(&self) -> bool {
        self.pointer
            .with_grab(|_, grab| {
                grab.is::<ClickGrab<State>>()
                    || grab.is::<frame_click_grab::FrameClickGrab>()
                    || grab.is::<floating_move_grab::MoveGrab>()
                    || grab.is::<floating_resize_grab::ResizeGrab>()
            })
            .unwrap_or(false)
    }

    /// Emacs tracks a drag source and `pos` is over another client's surface.
    pub(crate) fn drag_source_over_foreign(
        &self,
        frame: &WlSurface,
        pos: Point<f64, Logical>,
    ) -> bool {
        self.emacs_drag_source
            && self.surface_under_point(pos).is_some_and(|(surface, _)| {
                surface.client().map(|c| c.id()) != frame.client().map(|c| c.id())
            })
    }

    pub(crate) fn update_pointer_focus_for_motion(
        &mut self,
        pos: Point<f64, Logical>,
    ) -> (Option<SurfaceHit>, bool) {
        let grabbed = self.pointer.is_grabbed();
        if self.grab_pins_pointer_focus() {
            return (self.pointer_focus.clone(), false);
        }
        let under = self.pointer_target_at(pos);
        let focus_changed =
            Self::pointer_focus_surface(&self.pointer_focus) != Self::pointer_focus_surface(&under);
        self.pointer_focus = under.clone();

        let entry_id = self.layout_entry_id_under(pos);
        let entry_changed = mem::replace(&mut self.pointer_entry_id, entry_id) != entry_id;

        // Don't retarget keyboard focus while grabbed; the grab owns it.
        if !grabbed
            && self.focus_follows_mouse_mode
            && (focus_changed || entry_changed)
            && !self.is_strip_animating_at(pos)
        {
            self.focus_follows_mouse(&under, pos);
        }

        (under, focus_changed)
    }

    /// Get the output where the focused frame is located
    pub(crate) fn get_focused_output(&self) -> Option<String> {
        if self.focused_frame_id == 0 {
            return None;
        }
        self.emacs_frame_output(self.focused_frame_id)
            .map(str::to_string)
    }

    /// Get the output under the cursor position
    fn output_under_cursor(&self) -> Option<String> {
        use smithay::utils::Point;
        let (px, py) = self.cursor_location();
        let cursor_point = Point::from((px as i32, py as i32));

        for output in &self.sorted_outputs {
            if let Some(geo) = self.space.output_geometry(output)
                && geo.contains(cursor_point)
            {
                return Some(output.name());
            }
        }
        None
    }

    /// Output for surface placement and keyboard nav: focused > cursor > first.
    pub fn active_output(&self) -> Option<String> {
        self.get_focused_output()
            .or_else(|| self.output_under_cursor())
            .or_else(|| self.first_output().map(|o| o.name()))
    }

    /// Find a mapped output by name.
    fn mapped_output_by_name(&self, name: &str) -> Option<&Output> {
        self.sorted_outputs
            .iter()
            .find(|output| output.name() == name)
    }

    /// Find a mapped output by name.
    pub(crate) fn find_mapped_output(&self, name: &str) -> Option<Output> {
        self.mapped_output_by_name(name).cloned()
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
        let cur_center = crate::utils::center(cur);

        self.space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o).map(|g| (o, g)))
            .filter_map(|(output, geo)| {
                if !geo.overlaps(band) {
                    return None;
                }
                let center = crate::utils::center(geo);
                let distance = if horizontal {
                    i64::from(sign) * (i64::from(center.x) - i64::from(cur_center.x))
                } else {
                    i64::from(sign) * (i64::from(center.y) - i64::from(cur_center.y))
                };
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
        let cur = match self.find_mapped_output(&cur_name) {
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

    fn active_outputs_snapshot(&self) -> Vec<module::ActiveOutput> {
        self.sorted_outputs
            .iter()
            .filter_map(|output| {
                let geo = self.space.output_geometry(output)?;
                let wa = self.working_areas.get(&output.name());
                let x = geo.loc.x + wa.map_or(0, |r| r.loc.x);
                let y = geo.loc.y + wa.map_or(0, |r| r.loc.y);
                Some(module::ActiveOutput {
                    name: output.name(),
                    origin: (x, y),
                })
            })
            .collect()
    }

    fn frame_origins_snapshot(&self) -> Vec<module::FrameOrigin> {
        let mut origins = Vec::new();

        // Floating frames carry their own logical position.
        for (name, space) in &self.floating_spaces {
            let Some(output) = self.mapped_output_by_name(name) else {
                continue;
            };
            let Some(output_geo) = self.space.output_geometry(output) else {
                continue;
            };
            for frame in &space.frames {
                if frame.surface_id == 0 {
                    continue;
                }
                let Some((_, data)) = space.frame_with_data(frame.surface_id) else {
                    continue;
                };
                let loc = data.logical_pos();
                origins.push(module::FrameOrigin {
                    surface_id: frame.surface_id,
                    origin: (
                        output_geo.loc.x + loc.x as i32,
                        output_geo.loc.y + loc.y as i32,
                    ),
                });
            }
        }

        origins
    }

    /// Immediately sync active_outputs into shared state so that Emacs sees
    /// up-to-date output lists when it handles events queued after this call.
    pub fn flush_active_outputs(&mut self) {
        let mut shared = module::shared_state().lock().unwrap();
        shared.active_outputs = self.active_outputs_snapshot();
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

    fn sync_sorted_outputs(&mut self) {
        let mut outputs = self.space.outputs().cloned().collect::<Vec<_>>();

        let space = &self.space;
        outputs.sort_by(|a, b| {
            let a_geo = space.output_geometry(a).unwrap();
            let b_geo = space.output_geometry(b).unwrap();
            (a_geo.loc.x, a_geo.loc.y, a.name()).cmp(&(b_geo.loc.x, b_geo.loc.y, b.name()))
        });
        self.sorted_outputs = outputs;
    }

    /// Register a newly connected output.
    ///
    /// Called by backends after hardware setup (DRM surface, virtual output) and
    /// after the output is mapped in the space. Handles all backend-agnostic
    /// bookkeeping: sorted outputs, size recalculation, event to Emacs,
    /// initial working area, and output management state.
    pub fn add_output(&mut self, output: &Output, info: OutputInfo) {
        let output_name = info.name.clone();

        self.sync_sorted_outputs();
        self.refresh_cursor_outputs();
        self.recalculate_output_size();

        self.send_output_detected(info);

        // Send initial working area (full output initially, before any panels)
        let working_area = self.get_working_area(output);
        self.working_areas.insert(output_name.clone(), working_area);
        self.queue_event(Event::WorkingArea {
            output: output_name.clone(),
            x: working_area.loc.x,
            y: working_area.loc.y,
            width: working_area.size.w,
            height: working_area.size.h,
        });

        if self.space.output_geometry(output).is_some() {
            self.attach_frames_for_mapped_output(output, "output_added");
            self.introspect_windows_dirty = true;
        }
        self.output_management_state.output_heads_changed = true;
    }

    /// Handle output configuration changes (mode, scale, transform, position).
    ///
    /// Called by backends after applying hardware state changes. Handles all
    /// backend-agnostic bookkeeping: scale notification, lock buffer resize,
    /// screencast size, recalculation, working areas, event to Emacs, and
    /// output management state.
    pub fn output_config_changed(&mut self, output: &Output, became_mapped: bool) {
        let output_name = output.name();
        let current_mode = output.current_mode().unwrap_or(smithay::output::Mode {
            size: (0, 0).into(),
            refresh: 0,
        });
        let current_scale = output.current_scale().fractional_scale();
        let current_transform = output.current_transform();
        let current_geo = self.space.output_geometry(output).unwrap_or_default();
        self.sync_sorted_outputs();

        // Notify existing surfaces of scale/transform change
        self.send_scale_transform_to_output_surfaces(output);
        self.refresh_cursor_outputs();

        // Resize lock buffer and reconfigure lock surface for new output size
        let output_size = crate::utils::output_size(output);
        let is_locked = self.is_locked();
        if let Some(state) = self.output_state.get_mut(output) {
            state.resize_lock_buffer((output_size.w as i32, output_size.h as i32));
            state.resize_backdrops((output_size.w as i32, output_size.h as i32));
            if is_locked && let Some(lock_surface) = &state.lock_surface {
                configure_lock_surface(lock_surface, output);
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
        let working_area_changed = self.check_working_area_change(output);
        if !working_area_changed {
            // Always refresh Emacs frames: the output position may have changed
            // even when the working area (relative to the output) didn't.
            self.update_frames_for_working_area(output);
        }
        self.refresh_primary_assignments();
        self.queue_redraw_all();
        if became_mapped {
            self.attach_frames_for_mapped_output(output, "output_enabled");
        } else {
            self.restore_home_output_frames(&output_name);
        }

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
    /// Iterates windows and layer surfaces on the given output, sending both
    /// integer and fractional scale via `send_scale_transform`. Layout
    /// migration uses `enter_output_for_window`, which sets scale before enter.
    pub fn send_scale_transform_to_output_surfaces(&self, output: &Output) {
        let scale = output.current_scale();
        let transform = output.current_transform();

        // Notify declared surfaces on this output
        if let Some(strip) = self.frame_set.mapped_strip(&output.name()) {
            for surface_id in strip.surface_entries().filter_map(LayoutEntry::surface_id) {
                if let Some(window) = self.id_windows.get(&surface_id) {
                    self.set_preferred_scale_transform(window, output);
                }
            }
        }

        // Emacs frames aren't in `space`; notify them via the strip.
        for id in self.emacs_frames_on(&output.name()) {
            if let Some(window) = self.id_windows.get(&id) {
                self.set_preferred_scale_transform(window, output);
            }
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

    fn working_area_f64(&self, output: &Output) -> Rectangle<f64, smithay::utils::Logical> {
        self.get_working_area(output).to_f64()
    }

    fn ensure_floating_space_for_output(
        &mut self,
        output_name: &str,
    ) -> Option<&mut floating::FloatingSpace> {
        let output = self.mapped_output_by_name(output_name)?.clone();
        let working_area = self.working_area_f64(&output);
        let clock = self.animations_clock.clone();
        let space = self
            .floating_spaces
            .entry(output_name.to_string())
            .or_insert_with(|| floating::FloatingSpace::new(working_area, clock));
        space.update_config(working_area);
        Some(space)
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

        if let Some(space) = self.floating_spaces.get_mut(&output_name) {
            space.update_config(working_area.to_f64());
        }

        self.queue_redraw(output);
    }

    /// Check and update working area for an output, sending event if changed.
    ///
    /// Returns true when the working area changed and Emacs frames were resized.
    pub fn check_working_area_change(&mut self, output: &Output) -> bool {
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

        changed
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
    pub fn debug_state(&self, output_infos: &[OutputInfo]) -> serde_json::Value {
        let mut id_window_keys: Vec<u64> = self.id_windows.keys().copied().collect();
        id_window_keys.sort_unstable();
        let mut urgent_surfaces: Vec<u64> = self.urgent_surfaces.iter().copied().collect();
        urgent_surfaces.sort_unstable();
        let mut urgent_workspaces: Vec<u64> = self.urgent_workspaces.iter().copied().collect();
        urgent_workspaces.sort_unstable();
        let emacs_surfaces: HashMap<u64, String> = self
            .frame_set
            .mapped_strips()
            .iter()
            .flat_map(|(out, s)| {
                s.frames
                    .iter()
                    .map(move |f| (f.surface_id, out.to_string()))
            })
            .chain(self.floating_spaces.iter().flat_map(|(out, space)| {
                space
                    .frames
                    .iter()
                    .map(move |f| (f.surface_id, out.to_string()))
            }))
            .collect();
        let cursor_pos: Point<f64, Logical> = self.cursor_location().into();
        let pointer_output = self.output_at(cursor_pos);
        let cursor_scale = pointer_output
            .map(|output| output.current_scale().integer_scale())
            .unwrap_or(1);
        let cursor_output = pointer_output.map(|output| {
            serde_json::json!({
                "name": output.name(),
                "scale": output.current_scale().fractional_scale(),
                "integer_scale": output.current_scale().integer_scale(),
            })
        });

        let held_intercept_keycodes = self.im_repeat.held_debug();
        let (keyboard_repeat_rate, keyboard_repeat_delay) = self.im_repeat.repeat_config_debug();

        serde_json::json!({
            "surfaces": &self.surface_info,
            "emacs_surfaces": emacs_surfaces,
            "output_strips": &self.frame_set.mapped_strips(),
            "overview": {
                "open": self.overview.is_open(),
                "progress": self.overview.progress(),
                "zoom": self.overview.zoom(),
            },
            "homeless_frames": &self.frame_set.homeless_frames(),
            "floating_spaces": &self.floating_spaces,
            "surface_outputs": &self.surface_outputs,
            "focused_surface_id": self.focused_surface_id(),
            "focused_frame_id": self.focused_frame_id,
            "focused_entry_id": self.focused_entry_id,
            "id_windows": id_window_keys,
            "urgent_surfaces": urgent_surfaces,
            "urgent_workspaces": urgent_workspaces,
            "outputs": output_infos,
            "working_areas": self.get_working_areas_info(),
            "layer_surfaces": self.get_layer_surfaces_info(),
            "pointer_location": self.pointer_location(),
            "tablet_cursor_location": self.tablet_cursor_location.map(|p| (p.x, p.y)),
            "cursor": {
                "output": cursor_output,
                "image": self.cursor_manager.debug_state(cursor_scale),
            },
            "intercepted_keys": module::get_intercepted_keys(),
            "emacs_pid": self.emacs_pid,
            "text_input_intercept": self.text_input_intercept,
            "text_input_active": self.im_text_input.is_active(),
            "held_intercept_keycodes": held_intercept_keycodes,
            "intercept_repeat": self.im_repeat.repeat_debug(),
            "keyboard_repeat": {
                "rate": keyboard_repeat_rate,
                "delay": keyboard_repeat_delay,
            },
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
            "keyboard_capture": module::get_keyboard_capture(),
            "keyboard_capture_holders": module::keyboard_capture_holders_debug(),
            "debug_mode": module::DEBUG_MODE.load(std::sync::atomic::Ordering::Relaxed),
            "pending_commands": module::peek_commands(),
            "focus_history": module::get_focus_history(),
        })
    }

    fn layer_for_root(&self, root: &WlSurface) -> Option<(Output, DesktopLayerSurface)> {
        self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            let layer = map.layer_for_surface(root, WindowSurfaceType::TOPLEVEL)?;
            Some((output.clone(), layer.clone()))
        })
    }

    fn output_for_root(&self, root: &WlSurface) -> Option<Output> {
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

    fn is_floating_frame(&self, id: u64) -> bool {
        self.floating_spaces
            .values()
            .any(|space| space.has_frame(id))
    }

    /// Tiled host for an app surface: parent's frame, else focused, else active; never floating.
    fn resolve_app_host_frame(&self, parent_id: Option<u64>, output: Option<&str>) -> Option<u64> {
        parent_id
            .and_then(|pid| self.frame_for_surface(pid))
            .filter(|&id| !self.is_floating_frame(id))
            .or_else(|| {
                output.and_then(|o| {
                    (self.focused_frame_id != 0
                        && !self.is_floating_frame(self.focused_frame_id)
                        && self.emacs_frame_output(self.focused_frame_id) == Some(o))
                    .then_some(self.focused_frame_id)
                })
            })
            .or_else(|| output.and_then(|o| self.active_frame_surface_id(o)))
    }

    /// Handle new toplevel surface from XdgShellHandler.
    pub fn handle_new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_surface_id;
        self.next_surface_id += 1;

        let identity = client_identity(surface.wl_surface());
        let client_pid = identity.as_ref().map_or(0, |i| i.credentials.pid);
        // Only Emacs connects to the listening socket from this process.
        let is_emacs = self.emacs_pid.is_some_and(|pid| client_pid == pid as i32);
        if is_emacs {
            info!("Surface {} is an Emacs surface", id);
        } else {
            self.unmapped_surfaces.insert(id);
        }

        let app = smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
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
        let frame_output = pending_frame.as_ref().map(|pending| pending.output.clone());
        let frame_is_floating = pending_frame
            .as_ref()
            .is_some_and(|pending| pending.floating);
        let frame_pos = pending_frame.as_ref().and_then(|pending| pending.pos);
        let target_output = frame_output.clone().or_else(|| self.active_output());

        surface.with_pending_state(|state| {
            if is_emacs && frame_is_floating {
                // Let Emacs drive the size; the frame fits itself to its content.
                state.size = None;
            } else {
                state.size = Some(self.output_size);
                state.states.set(XdgToplevelState::Maximized);
            }
            state.states.set(XdgToplevelState::Activated);
        });
        surface.send_configure();

        let window = Window::new_wayland_window(surface);
        self.window_ids.insert(window.clone(), id);
        self.id_windows.insert(id, window.clone());

        debug!(
            "new_toplevel id={} is_emacs={} pending_output={:?} floating={} target_output={:?} pending_queue_after_take={:?}",
            id,
            is_emacs,
            frame_output,
            frame_is_floating,
            target_output,
            module::peek_pending_frame_outputs()
        );

        if is_emacs {
            // Eager-insert into the target strip so `is_emacs_frame(id)`,
            // `set_focus_frame(id)`, etc. work immediately -- before Emacs's
            // first layout-refresh arrives.  Layout-refresh later updates fields
            // (name, width, surfaces, selected_entry_id) via MERGE semantics.
            if let Some(output_name) = target_output.as_deref() {
                if frame_is_floating {
                    if let Some(space) = self.ensure_floating_space_for_output(output_name) {
                        let mut frame = crate::strip::Frame::new(id);
                        frame.width = floating::DEFAULT_FRAME_WIDTH;
                        frame.height = floating::DEFAULT_FRAME_HEIGHT;
                        frame.floating_pos = frame_pos.map(|(x, y)| {
                            floating::Data::logical_to_size_frac_in_working_area(
                                space.working_area(),
                                Point::from((x, y)),
                            )
                        });
                        space.add_frame(frame);
                    }
                } else {
                    let clock = self.animations_clock.clone();
                    let strip = self.frame_set.ensure_mapped_strip(output_name, clock);
                    strip.frames.push(crate::strip::Frame::new(id));
                }
            }
        }

        // Associate window with target output for scale detection.
        // Output association is managed explicitly (not via space.refresh()).
        // We send scale info and enter the output before the client's first commit
        // so it knows the correct scale to render at.
        let scale_output = target_output
            .as_ref()
            .and_then(|name| self.mapped_output_by_name(name))
            .or_else(|| self.first_output())
            .cloned();
        if let Some(output) = scale_output {
            // Direct output enter, bypassing SpaceElement::output_enter which would
            // trigger output_update -> leave for uncommitted surfaces.
            self.enter_output_for_window(&window, &output);
        }

        // Resize Emacs frames to fill their working area
        if let Some(ref output_name) = frame_output.filter(|_| !frame_is_floating)
            && let Some(working_area) = self
                .mapped_output_by_name(output_name)
                .map(|o| self.get_working_area(o))
            && let Some(t) = window.toplevel()
        {
            t.with_pending_state(|state| {
                state.size = Some(working_area.size);
            });
            t.send_configure();
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

        // App surfaces resolve their host frame at map; `New` only creates the buffer.
        self.queue_event(Event::New {
            id,
            app: app.clone(),
            output: target_output.clone().filter(|_| is_emacs),
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
            self.queue_event(if frame_is_floating || is_active {
                Event::FrameShown { id }
            } else {
                Event::FrameHidden { id }
            });
        }
        // Sent after New so the lisp side has assigned ewm-surface-id by the
        // time the Focus event is processed.
        if auto_focus {
            info!("Auto-focusing initial Emacs surface {}", id);
            if let Some(focus_id) = self.focus_id_of_frame(id) {
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
            target_output.as_deref().unwrap_or("unknown")
        );
    }

    /// Handle toplevel destroyed from XdgShellHandler.
    pub fn handle_toplevel_destroyed(&mut self, surface: ToplevelSurface) -> Option<u64> {
        let Some(id) = self.surface_id(surface.wl_surface()) else {
            // Every toplevel gets an id in handle_new_toplevel.
            warn!("destroyed toplevel missing from id_windows");
            return None;
        };
        self.handle_toplevel_destroyed_by_id(id)
    }

    /// Drop a toplevel by surface id; same cleanup as the surface-typed entry
    /// point, but reachable from integration tests without a real toplevel.
    pub fn handle_toplevel_destroyed_by_id(&mut self, id: u64) -> Option<u64> {
        let was_focused = self.focused_frame_id == id;
        let close_was_active = module::take_pending_active_frame_close(id);
        let active_close = was_focused || close_was_active;
        let output = self.surface_output_name(id).map(str::to_owned);

        if let Some(window) = self.id_windows.remove(&id) {
            self.window_ids.remove(&window);
        }
        self.urgent_surfaces.remove(&id);
        self.urgent_workspaces.remove(&id);
        self.pending_activation_surfaces.remove(&id);
        self.flush_pending_title_event(id);
        self.surface_info.remove(&id);
        let was_emacs = self.is_emacs_frame(id);
        self.detach_surface_from_layouts(id);
        self.remove_emacs_frame_from_strip(id, active_close);
        self.prune_dangling_focus();
        self.queue_event(Event::Close { id });
        debug!(
            "Toplevel {} destroyed was_focused={} close_was_active={} was_emacs={} output={:?}",
            id, was_focused, close_was_active, was_emacs, output
        );

        if was_emacs {
            self.refresh_mapped_cast_outputs();
        }

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
                if let Err(e) = ClientState::insert(&mut state.ewm.display_handle, client) {
                    warn!("Failed to insert client: {}", e);
                }
            })
            .expect("Failed to init wayland socket source");

        register_display_source(event_loop, display);

        Ok(socket_name)
    }

    fn check_surface_info_changes(&mut self, surface: &WlSurface) {
        let Some(id) = self.surface_id(surface) else {
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
        let first_title = !title.is_empty() && cached.is_none_or(|info| info.title.is_empty());
        let app_changed = cached.is_some_and(|info| info.app_id != app_id);

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
            self.introspect_windows_dirty = true;
            self.queue_title_update(id, app_id, title, first_title, app_changed);
        }
    }

    /// Handle commit for layer surfaces. Returns true if this was a layer surface.
    /// Handle layer shell surface commit.
    pub fn handle_layer_surface_commit(&mut self, surface: &WlSurface) -> bool {
        use smithay::wayland::shell::wlr_layer::LayerSurfaceData;

        let root_surface = surface_root(surface);
        let Some((output, layer)) = self.layer_for_root(&root_surface) else {
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

            if initial_configure_sent {
                if crate::utils::is_mapped(surface) {
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
                let scale = output.current_scale();
                let transform = output.current_transform();
                smithay::wayland::compositor::with_states(surface, |data| {
                    crate::utils::send_scale_transform(surface, data, scale, transform);
                });
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
#[derive(Clone, Debug)]
struct ClientIdentity {
    credentials: Credentials,
    comm: Option<String>,
}

impl ClientIdentity {
    fn from_credentials(credentials: Credentials) -> Self {
        Self {
            credentials,
            comm: client_process_name(credentials),
        }
    }

    fn describe(&self) -> String {
        format!(
            "pid={} uid={} gid={} comm={}",
            self.credentials.pid,
            self.credentials.uid,
            self.credentials.gid,
            self.comm.as_deref().unwrap_or("<unknown>")
        )
    }
}

fn client_process_name(credentials: Credentials) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{}/comm", credentials.pid))
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

#[derive(Default)]
pub struct ClientState {
    pub compositor: CompositorClientState,
    /// Whether this is the portal backend from the Mutter service channel.
    portal: bool,
    identity: OnceLock<ClientIdentity>,
}

impl ClientState {
    pub(crate) fn portal() -> Arc<Self> {
        Arc::new(Self {
            portal: true,
            ..Default::default()
        })
    }

    /// Insert a listener-accepted STREAM and record its peer credentials.
    pub(crate) fn insert(
        handle: &mut DisplayHandle,
        stream: UnixStream,
    ) -> std::io::Result<Client> {
        let state = Arc::new(ClientState::default());
        let client = handle.insert_client(stream, state.clone())?;
        match handle.backend_handle().get_client_credentials(client.id()) {
            Ok(credentials) => state.set_credentials(credentials),
            Err(e) => debug!(
                "Failed to get credentials for client {:?}: {}",
                client.id(),
                e
            ),
        }
        Ok(client)
    }

    fn set_credentials(&self, credentials: Credentials) {
        let _ = self
            .identity
            .set(ClientIdentity::from_credentials(credentials));
    }

    fn identity_description(&self) -> String {
        match self.identity.get() {
            Some(identity) => identity.describe(),
            None if self.portal => "portal".into(),
            None => "pid=<unknown> uid=<unknown> gid=<unknown> comm=<unknown>".into(),
        }
    }
}
impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        info!("Client connected: {:?}", client_id);
    }
    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        info!(
            "Client disconnected: {:?}, reason: {:?}, {}",
            client_id,
            reason,
            self.identity_description()
        );
    }
}

/// The state attached to CLIENT at insertion.
fn client_state(client: &Client) -> &ClientState {
    client
        .get_data::<ClientState>()
        .expect("ClientState inserted at connection time")
}

/// Whether SURFACE belongs to the portal backend.
fn is_portal_surface(surface: &WlSurface) -> bool {
    surface
        .client()
        .is_some_and(|client| client_state(&client).portal)
}

/// The identity recorded when SURFACE's client connected, if any.
fn client_identity(surface: &WlSurface) -> Option<ClientIdentity> {
    client_state(&surface.client()?).identity.get().cloned()
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

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client_state(client).compositor
    }

    fn new_subsurface(&mut self, surface: &WlSurface, parent: &WlSurface) {
        let root = surface_root(parent);
        if let Some(output) = self.ewm.output_for_root(&root) {
            let scale = output.current_scale();
            let transform = output.current_transform();
            smithay::wayland::compositor::with_states(surface, |data| {
                crate::utils::send_scale_transform(surface, data, scale, transform);
            });
        }
    }

    fn commit(&mut self, surface: &WlSurface) {
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // Queue early import for DRM backend (processed in main loop)
        self.ewm.pending_early_imports.push(surface.clone());

        // Cursor surface: apply role buffer offset to the hotspot and redraw.
        let root = surface_root(surface);
        if matches!(
            self.ewm.cursor_manager.cursor_image(),
            CursorImageStatus::Surface(cursor_surface) if cursor_surface == &root
        ) {
            if surface == &root {
                smithay::wayland::compositor::with_states(surface, |states| {
                    let cursor_image_attributes = states.data_map.get::<CursorImageSurfaceData>();

                    if let Some(mut cursor_image_attributes) =
                        cursor_image_attributes.map(|attrs| attrs.lock().unwrap())
                    {
                        let buffer_delta = states
                            .cached_state
                            .get::<smithay::wayland::compositor::SurfaceAttributes>()
                            .current()
                            .buffer_delta
                            .take();
                        if let Some(buffer_delta) = buffer_delta {
                            cursor_image_attributes.hotspot -= buffer_delta;
                        }
                    }
                });
            }
            self.ewm.queue_redraw_all();
            return;
        }

        // DnD icon surface: update offset from buffer delta and redraw.
        if let Some(icon) = &self.ewm.dnd_icon
            && icon.surface == root
        {
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

        // Surface type dispatch order:
        // 1. Layer surfaces  2. Popups  3. Windows  4. Lock surfaces
        // When adding new surface types, add a branch here.

        // 1. Handle layer surface commits
        if self.ewm.handle_layer_surface_commit(surface) {
            return;
        }

        // 2. Handle popup commits
        if self.ewm.handle_popup_commit(surface) {
            return;
        }

        // Early return for sync subsurfaces - parent commit will handle them
        if is_sync_subsurface(surface) {
            return;
        }

        let root_surface = surface_root(surface);

        // 3. Find the window that owns this root surface
        if let Some(id) = self.ewm.surface_id(&root_surface) {
            if let Some(window) = self.ewm.id_windows.get(&id) {
                window.on_commit();
            }
            // Title before Mapped, so placement rules see a title set on the mapping commit.
            self.ewm.check_surface_info_changes(surface);

            // unmapped->mapped transition: seed the open-fade once. The
            // 16ms delay holds alpha at 0 for one vblank so the empty/white
            // initial buffer some clients ship is rendered invisibly.
            if crate::utils::is_mapped(&root_surface) && self.ewm.unmapped_surfaces.remove(&id) {
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

                // Placement is final at map: min/max hints are known, so decide once.
                let (open_floating, parent) = self
                    .ewm
                    .id_windows
                    .get(&id)
                    .and_then(|window| window.toplevel())
                    .map(|toplevel| (compute_open_floating(toplevel), toplevel.parent()))
                    .unwrap_or((false, None));
                let parent_id = parent.and_then(|wl| self.ewm.surface_id(&wl));
                let output = self.ewm.active_output();
                let frame_surface_id = self
                    .ewm
                    .resolve_app_host_frame(parent_id, output.as_deref());
                // Size hint for floating placement, dropped when it fills the working area.
                let working_area = output
                    .as_deref()
                    .and_then(|name| self.ewm.mapped_output_by_name(name))
                    .map(|o| self.ewm.get_working_area(o));
                let (width, height) = self
                    .ewm
                    .id_windows
                    .get(&id)
                    .and_then(natural_open_size)
                    .filter(|s| working_area.is_none_or(|wa| s.w < wa.size.w && s.h < wa.size.h))
                    .map(|s| (s.w as f64, s.h as f64))
                    .unzip();
                self.ewm.queue_event(Event::Mapped {
                    id,
                    open_floating,
                    output,
                    frame_surface_id,
                    width,
                    height,
                });
            }

            self.ewm.queue_redraw_for_surface(id);
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

// Shared memory
impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.ewm.shm_state
    }
}

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
        let surface_id = focused.and_then(|s| self.ewm.surface_id(s));
        self.ewm.update_text_input_focus(focused, surface_id);
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

// Virtual Pointer (zwlr-virtual-pointer-v1: lets clients inject pointer motion,
// buttons and axis, routed through the normal input handlers).
impl VirtualPointerHandler for State {
    fn virtual_pointer_manager_state(&mut self) -> &mut VirtualPointerManagerState {
        &mut self.ewm.virtual_pointer_state
    }

    fn on_virtual_pointer_motion(&mut self, event: VirtualPointerMotionEvent) {
        crate::input::handle_pointer_motion::<VirtualPointerInputBackend>(self, event);
        self.ewm.queue_redraw_for_pointer();
    }

    fn on_virtual_pointer_motion_absolute(&mut self, event: VirtualPointerMotionAbsoluteEvent) {
        crate::input::handle_pointer_motion_absolute::<VirtualPointerInputBackend>(self, event);
        self.ewm.queue_redraw_for_pointer();
    }

    fn on_virtual_pointer_button(&mut self, event: VirtualPointerButtonEvent) {
        crate::input::handle_pointer_button::<VirtualPointerInputBackend>(self, event);
    }

    fn on_virtual_pointer_axis(&mut self, event: VirtualPointerAxisEvent) {
        crate::input::handle_pointer_axis::<VirtualPointerInputBackend>(self, event);
    }
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

    fn fullscreen_request(&mut self, surface: ToplevelSurface, output: Option<WlOutput>) {
        self.ewm
            .set_fullscreen_for_surface(surface.wl_surface(), output, true);
        self.refresh_pointer_after_layout();
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.ewm
            .set_fullscreen_for_surface(surface.wl_surface(), None, false);
        self.refresh_pointer_after_layout();
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

// KDE Decoration, the only kind GTK understands
impl KdeDecorationHandler for State {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.ewm.kde_decoration_state
    }

    fn new_decoration(&mut self, _surface: &WlSurface, decoration: &OrgKdeKwinServerDecoration) {
        decoration.mode(KdeDecorationMode::Server);
    }

    fn request_mode(
        &mut self,
        _surface: &WlSurface,
        decoration: &OrgKdeKwinServerDecoration,
        _mode: WEnum<KdeDecorationMode>,
    ) {
        decoration.mode(KdeDecorationMode::Server);
    }
}

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
            self.ewm.output_from_resource(wl_output)
        } else {
            let name = self.ewm.active_output();
            name.and_then(|n| self.ewm.find_mapped_output(&n))
        };

        let Some(output) = output else {
            warn!("No output for new layer surface, closing");
            surface.send_close();
            return;
        };

        let wl_surface = surface.wl_surface().clone();
        self.ewm.unmapped_layer_surfaces.insert(wl_surface.clone());

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
        // Defer: event sends from resource destructors abort on a dying client.
        self.ewm.loop_handle.insert_idle(move |state| {
            let ewm = &mut state.ewm;
            let wl_surface = surface.wl_surface();
            ewm.unmapped_layer_surfaces.remove(wl_surface);

            let Some((output, layer)) = ewm.layer_for_root(wl_surface) else {
                return;
            };
            // Clear on-demand focus if it was this layer surface
            if ewm.layer_shell_on_demand_focus.as_ref() == Some(&layer) {
                ewm.layer_shell_on_demand_focus = None;
            }

            let mut map = layer_map_for_output(&output);
            map.unmap_layer(&layer);
            // Re-arrange after unmapping to recalculate exclusive zones
            map.arrange();
            drop(map);

            // Check for working area expansion (panel removed)
            ewm.check_working_area_change(&output);

            ewm.queue_redraw(&output);
            info!("Layer surface destroyed");
        });
    }

    fn new_popup(
        &mut self,
        _parent: smithay::wayland::shell::wlr_layer::LayerSurface,
        popup: smithay::wayland::shell::xdg::PopupSurface,
    ) {
        self.ewm.unconstrain_popup(&popup);
        if let Err(err) = self.ewm.popups.track_popup(PopupKind::Xdg(popup)) {
            warn!("error tracking popup: {err:?}");
        }
    }
}

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
        use std::time::Duration;
        const TOKEN_TIMEOUT: Duration = Duration::from_secs(10);

        debug!(
            "xdg_activation: request_activation called for surface {:?}",
            surface.id()
        );

        if token_data.timestamp.elapsed() < TOKEN_TIMEOUT {
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
                } else if self.ewm.is_emacs_frame(id) {
                    debug!("xdg_activation: ignored for Emacs frame {}", id);
                } else {
                    self.ewm
                        .activate_or_defer_surface(id, true, "xdg_activation");
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

impl XdgForeignHandler for State {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.ewm.xdg_foreign_state
    }
}

impl XdgSystemBellHandler for State {
    fn ring(&mut self, surface: Option<WlSurface>) {
        let id = surface.and_then(|surface| self.ewm.surface_id(&surface));
        // Emacs rang this itself; echoing it back would loop.
        if id.is_some_and(|id| self.ewm.is_emacs_frame(id)) {
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
            .is_some_and(|id| self.ewm.is_emacs_frame(id))
    }

    fn activate(&mut self, wl_surface: WlSurface) {
        if let Some(id) = self.ewm.surface_id(&wl_surface) {
            self.ewm
                .activate_or_defer_surface(id, true, "foreign_toplevel");
            info!("Foreign toplevel: activated surface {}", id);
        }
    }

    fn close(&mut self, wl_surface: WlSurface) {
        if let Some(id) = self.ewm.surface_id(&wl_surface) {
            let Some(window) = self.ewm.id_windows.get(&id) else {
                return;
            };
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_close();
                info!("Foreign toplevel: sent close request");
            }
        }
    }

    fn set_fullscreen(&mut self, wl_surface: WlSurface, wl_output: Option<WlOutput>) {
        self.ewm
            .set_fullscreen_for_surface(&wl_surface, wl_output, true);
        self.refresh_pointer_after_layout();
    }

    fn unset_fullscreen(&mut self, wl_surface: WlSurface) {
        self.ewm
            .set_fullscreen_for_surface(&wl_surface, None, false);
        self.refresh_pointer_after_layout();
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

    fn activate_workspace(&mut self, output: String, frame_index: usize) {
        self.ewm.queue_event(Event::ActivateWorkspace {
            output,
            frame_index,
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
            && self.ewm.id_windows.contains_key(&id)
        {
            info!("Restoring focus to surface {} after unlock", id);
            self.ewm.set_focus(id, false, "unlock");
        }
        self.sync_keyboard_focus();

        self.ewm.queue_redraw_all();

        // Restart idle timer after unlock
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

    /// Check whether a live client holds the session lock, or is taking it.
    pub fn lock_client_is_live(&self) -> bool {
        match &self.lock_state {
            LockState::Unlocked => false,
            LockState::Locking(locker) => locker.ext_session_lock().is_alive(),
            LockState::Locked(lock) => lock.is_alive(),
        }
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

    fn lock_surface_output(&self) -> Option<&Output> {
        self.output_under_cursor()
            .and_then(|name| self.mapped_output_by_name(&name))
            .or_else(|| self.first_output())
    }

    /// Get the lock surface for keyboard focus when locked.
    pub fn lock_surface_focus(&self) -> Option<WlSurface> {
        let output = self.lock_surface_output()?;
        self.output_state
            .get(output)?
            .lock_surface
            .as_ref()
            .map(|s| s.wl_surface().clone())
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
        // A grab or constraint owns the pointer; a warp would teleport it.
        if pointer.is_grabbed() || input::active_constraint_for_focus(self, &pointer).is_some() {
            return;
        }
        let pos = Point::from((x, y));
        self.ewm.pointer_focus = self.ewm.pointer_target_at(pos);
        let under = self.ewm.pointer_focus.clone();
        self.send_pointer_motion(pos, under);
        self.ewm.queue_redraw_for_pointer();
    }

    /// Re-compute `pointer_focus` from the current pointer location and send
    /// a motion event so Smithay's internal pointer focus is up to date.
    /// Called before constraint activation to ensure focus is synced.
    fn refresh_pointer_focus(&mut self) {
        if self.ewm.grab_pins_pointer_focus() {
            return;
        }
        let (x, y) = self.ewm.pointer_location();
        let pos: Point<f64, Logical> = (x, y).into();

        let under = self.ewm.pointer_target_at(pos);

        if self.ewm.pointer_focus == under {
            return;
        }

        self.ewm.pointer_focus = under.clone();
        self.send_pointer_motion(pos, under);
    }

    fn refresh_pointer_after_layout(&mut self) {
        self.refresh_pointer_after_scene_change(false);
    }

    fn refresh_pointer_after_scene_change(&mut self, skip_strip_animation: bool) -> bool {
        if self.ewm.grab_pins_pointer_focus() {
            return false;
        }
        let (x, y) = self.ewm.pointer_location();
        let pos: Point<f64, Logical> = (x, y).into();

        if skip_strip_animation && !self.ewm.is_locked() && self.ewm.is_strip_animating_at(pos) {
            return false;
        }

        let next_entry_id = self.ewm.layout_entry_id_under(pos);
        let under = self.ewm.pointer_target_at(pos);
        if self.ewm.pointer_entry_id == next_entry_id && self.ewm.pointer_focus == under {
            return false;
        }

        self.ewm.pointer_entry_id = next_entry_id;
        self.ewm.pointer_focus = under.clone();
        self.send_pointer_motion(pos, under);
        self.maybe_activate_pointer_constraint();
        self.ewm.queue_redraw_for_pointer();
        true
    }

    fn send_pointer_motion(&mut self, pos: Point<f64, Logical>, under: Option<SurfaceHit>) {
        let pointer = self.ewm.pointer.clone();
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
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
        let output = self.ewm.find_mapped_output(output_name);
        let old_geo = output
            .as_ref()
            .and_then(|o| self.ewm.space.output_geometry(o));

        self.backend.apply_output_config(&mut self.ewm, output_name);

        if let (Some(output), Some(old_geo)) = (&output, old_geo) {
            let point = Point::from((pos.x as i32, pos.y as i32));
            if old_geo.contains(point)
                && let Some(new_geo) = self.ewm.space.output_geometry(output)
                && old_geo != new_geo
            {
                let rel_x = (pos.x - old_geo.loc.x as f64) / old_geo.size.w as f64;
                let rel_y = (pos.y - old_geo.loc.y as f64) / old_geo.size.h as f64;
                let new_x = new_geo.loc.x as f64 + rel_x * new_geo.size.w as f64;
                let new_y = new_geo.loc.y as f64 + rel_y * new_geo.size.h as f64;
                self.warp_pointer(new_x, new_y);
            }
        }
    }

    /// Center the pointer on the first output. Called once during startup so the
    /// cursor doesn't begin at (0, 0).
    pub fn center_pointer_on_first_output(&mut self) {
        let Some(output) = self.ewm.first_output().cloned() else {
            return;
        };
        let Some(geo) = self.ewm.space.output_geometry(&output) else {
            return;
        };
        let center = crate::utils::center(geo).to_f64();
        self.warp_pointer(center.x, center.y);
    }

    /// Synchronize Wayland keyboard focus with current state.
    ///
    /// Called unconditionally every frame, and before key forwarding in
    /// input handlers. Derives the correct focus from layer shell
    /// interactivity and focused_surface_id. Only calls keyboard.set_focus()
    /// when the resolved focus differs from current.
    ///
    /// Never call from resource destructors: sends abort on a dying client.
    pub fn sync_keyboard_focus(&mut self) {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        let locked = self.ewm.is_locked();
        let lock_surface = if locked {
            self.ewm.lock_surface_focus()
        } else {
            None
        };

        if !locked && let Some(surface) = &self.ewm.layer_shell_on_demand_focus {
            let good = surface.alive()
                && surface.cached_state().keyboard_interactivity == KeyboardInteractivity::OnDemand;
            if !good {
                self.ewm.layer_shell_on_demand_focus = None;
            }
        }

        let layer_focus = if locked {
            None
        } else {
            self.ewm.resolve_layer_keyboard_focus()
        };
        let ewm = &self.ewm;
        let new_focus = resolve_keyboard_focus_target(locked, lock_surface, layer_focus, || {
            if let Some(surface) = ewm.emacs_keyboard_redirect_focus() {
                return Some(surface);
            }
            let id = ewm.focused_surface_id();
            ewm.id_windows
                .get(&id)
                .and_then(|w| w.wl_surface())
                .map(|s| s.into_owned())
        });

        // A popup grab owns the keyboard; driving focus here would fight it and
        // dismiss the menu.  `refresh_popup_grab` clears the grab on dismissal.
        if self.ewm.popup_grab.is_none() && self.ewm.keyboard_focus != new_focus {
            self.ewm.keyboard_focus = new_focus.clone();
            let keyboard = self.ewm.keyboard.clone();
            keyboard.set_focus(self, new_focus, SERIAL_COUNTER.next_serial());
        }
    }

    /// Drop the tracked popup grab once it has ended.
    pub fn refresh_popup_grab(&mut self) {
        let Some(grab) = &self.ewm.popup_grab else {
            return;
        };
        // A DnD started from the menu takes over the pointer grab.
        let superseded = self
            .ewm
            .pointer
            .with_grab(|_, g| !g.is::<PopupPointerGrab<State>>())
            .unwrap_or(false);
        if grab.has_ended() || superseded {
            self.ewm.popup_grab = None;
        }
    }

    pub(crate) fn send_intercept_key_to_emacs(
        &mut self,
        keycode: u32,
        key_state: KeyState,
        time: InputTime,
        mods_changed: bool,
    ) {
        let keyboard = self.ewm.keyboard.clone();
        self.sync_keyboard_focus();

        if !self.ewm.is_keyboard_focus_on_emacs() {
            return;
        }

        let saved_layout = self.ewm.xkb_current_layout;
        if saved_layout != 0 {
            keyboard.with_xkb_state(self, |mut context| {
                context.set_layout(smithay::input::keyboard::Layout(0));
            });
        }

        keyboard.input_forward(
            self,
            keycode.into(),
            key_state,
            SERIAL_COUNTER.next_serial(),
            time,
            mods_changed,
        );

        if saved_layout != 0 {
            keyboard.with_xkb_state(self, |mut context| {
                context.set_layout(smithay::input::keyboard::Layout(saved_layout as u32));
            });
        }
    }

    /// Forward back to the client a key Emacs declined to translate.
    fn forward_text_input_key(
        &mut self,
        surface_id: u64,
        keycode: u32,
        ctrl: bool,
        alt: bool,
        shift: bool,
        logo: bool,
    ) {
        if self.ewm.active_text_input_surface_id() != Some(surface_id) {
            return;
        }
        let keyboard = self.ewm.keyboard.clone();
        self.sync_keyboard_focus();
        crate::input::forward_with_modifiers(self, &keyboard, (ctrl, alt, shift, logo), |state| {
            for key_state in [KeyState::Pressed, KeyState::Released] {
                keyboard.input_forward(
                    state,
                    keycode.into(),
                    key_state,
                    SERIAL_COUNTER.next_serial(),
                    InputTime::now(),
                    false,
                );
            }
        });
    }

    pub(crate) fn on_intercept_repeat_timer(&mut self, keycode: u32) {
        let Some(repeat) = self.ewm.take_intercept_repeat_for_timer(keycode) else {
            return;
        };
        if self.ewm.held_key_owner(keycode).is_none() {
            return;
        }

        self.ewm.notify_activity();
        self.ewm.reset_idle_timer();

        match repeat {
            InterceptRepeat::TextInput(TextInputKey {
                keycode,
                keysym,
                utf8,
                surface_id,
                ctrl,
                alt,
                shift,
                logo,
            }) => {
                self.ewm.queue_event(Event::Key {
                    keycode,
                    keysym,
                    utf8,
                    surface_id,
                    ctrl,
                    alt,
                    shift,
                    logo,
                });
            }
            InterceptRepeat::Command { key, .. } => {
                if module::get_keyboard_capture() {
                    self.ewm.cancel_command_repeat();
                    return;
                }
                self.ewm.queue_event(Event::InterceptedCommand { key });
            }
        }

        self.ewm.reschedule_intercept_repeat(keycode);
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
        self.ewm.popups.cleanup();
        self.refresh_popup_grab();
        self.sync_keyboard_focus();

        self.ewm.refresh_idle_inhibit();
        self.ewm.refresh_cursor_outputs();

        // Refresh workspace protocol state (pull model: diff source of truth vs mirrors).
        crate::protocols::workspace::refresh::<State>(
            &mut self.ewm.workspace_state,
            self.ewm.frame_set.mapped_strips(),
            &self.ewm.urgent_surfaces,
            &self.ewm.urgent_workspaces,
            &self.ewm.sorted_outputs,
        );

        // Process pending early imports
        let pending_imports: Vec<_> = std::mem::take(&mut self.ewm.pending_early_imports);
        for surface in pending_imports {
            self.backend.early_import(&surface);
        }

        self.refresh_pointer_after_scene_change(true);

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
            self.refresh_pointer_after_scene_change(true);
            if is_focused {
                rendered_focused = true;
            }
        }

        // Clean up dead elements from space.
        // Output enter/leave is managed explicitly in handle_new_toplevel and
        // the Layout command, not via automatic spatial overlap detection.
        if self.ewm.cleanup_dead_windows() {
            self.sync_keyboard_focus();
        }

        // Update shared state snapshot for Emacs to read synchronously
        self.update_shared_state();
        self.ewm.flush_introspect_windows();

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
        let frame_origins = self.ewm.frame_origins_snapshot();
        let active_frames = self.ewm.active_frames_snapshot();
        let mut shared = module::shared_state().lock().unwrap();
        shared.focused_surface_id = self.ewm.focused_surface_id();
        shared.focused_frame_id = self.ewm.focused_frame_id;
        shared.pointer_location = self.ewm.pointer_location();
        shared.frame_origins = frame_origins;
        shared.active_frames = active_frames;
    }

    /// Handle a module command (from Emacs via dynamic module).
    pub(crate) fn handle_module_command(&mut self, cmd: module::ModuleCommand) {
        tracy_span!("handle_module_command");

        use module::ModuleCommand;
        match cmd {
            ModuleCommand::Close { id } => {
                if let Some(window) = self.ewm.id_windows.get(&id)
                    && let Some(toplevel) = window.toplevel()
                {
                    toplevel.send_close();
                    info!("Close surface {} (sent close request)", id);
                }
            }
            ModuleCommand::FocusOutputDirection { dx, dy } => {
                self.ewm.focus_output_direction(dx, dy);
            }
            ModuleCommand::RenameWorkspace {
                frame_surface_id,
                name,
            } => {
                self.ewm.rename_workspace(frame_surface_id, name);
            }
            ModuleCommand::MarkWorkspaceUrgent { frame_surface_id } => {
                self.ewm.mark_workspace_urgent(frame_surface_id);
            }
            ModuleCommand::FocusTarget { focus_id } => {
                if self.ewm.lookup_entry(focus_id).is_some() {
                    self.ewm.set_focus_entry(focus_id, "lisp_request", false);
                } else if let Some(frame_id) = self.ewm.frame_with_focus_id(focus_id) {
                    self.ewm.set_focus_frame(frame_id, "lisp_request", false);
                }
            }
            ModuleCommand::WarpPointer { x, y } => {
                // Don't warp while a pen is in use.
                if self.ewm.tablet_cursor_location.is_none() {
                    self.warp_pointer(x, y);
                }
            }
            ModuleCommand::ConfigureOutput {
                name,
                config:
                    module::OutputConfig {
                        x,
                        y,
                        width,
                        height,
                        refresh,
                        custom,
                        modeline,
                        scale,
                        transform,
                        enabled,
                    },
            } => {
                let current_output = self.ewm.find_mapped_output(&name);
                let current_mode = current_output
                    .as_ref()
                    .and_then(|output| output.current_mode());
                let current_pos = current_output
                    .as_ref()
                    .and_then(|output| self.ewm.space.output_geometry(output))
                    .map(|geo| (geo.loc.x, geo.loc.y))
                    .unwrap_or((0, 0));

                // Update stored config (merge with existing)
                let config = self.ewm.output_config.entry(name.clone()).or_default();
                if width.is_some() || height.is_some() || refresh.is_some() {
                    // Fall back to current mode for unspecified parameters
                    let w = width
                        .unwrap_or_else(|| current_mode.map(|mode| mode.size.w).unwrap_or(1920));
                    let h = height
                        .unwrap_or_else(|| current_mode.map(|mode| mode.size.h).unwrap_or(1080));
                    config.mode = Some(output_mode::Mode {
                        custom,
                        mode: output_mode::ConfiguredMode {
                            width: w as u16,
                            height: h as u16,
                            refresh,
                        },
                    });
                }
                if modeline.is_some() {
                    config.modeline = modeline;
                }
                if x.is_some() || y.is_some() {
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
                let active_surface_id = self.ewm.active_text_input_surface_id();
                match self
                    .ewm
                    .im_text_input
                    .commit(active_surface_id, surface_id, text)
                {
                    im::text_input::CommitResult::Deliver(text) => {
                        self.ewm.commit_active_text_input(text);
                    }
                    im::text_input::CommitResult::Queued => {
                        debug!("ImCommit: queued (surface_id={surface_id})");
                    }
                    im::text_input::CommitResult::DroppedStale => {
                        debug!("ImCommit: dropped stale commit (surface_id={surface_id})");
                    }
                }
            }
            ModuleCommand::TextInputReplace { text, surface_id } => {
                let active_surface_id = self.ewm.active_text_input_surface_id();
                match self
                    .ewm
                    .im_text_input
                    .replace(active_surface_id, surface_id, text)
                {
                    im::text_input::ReplaceResult::Deliver(r) => {
                        self.ewm.deliver_replace_ack(surface_id, r);
                    }
                    im::text_input::ReplaceResult::Queued => {
                        debug!("TextInputReplace: queued (surface_id={surface_id})");
                    }
                    im::text_input::ReplaceResult::DroppedStale => {
                        debug!("TextInputReplace: dropped stale replace (surface_id={surface_id})");
                        self.ewm
                            .queue_event(Event::TextInputReplaceDropped { surface_id });
                    }
                }
            }
            ModuleCommand::TextInputForwardKey {
                surface_id,
                keycode,
                ctrl,
                alt,
                shift,
                logo,
            } => {
                self.forward_text_input_key(surface_id, keycode, ctrl, alt, shift, logo);
            }
            ModuleCommand::TextInputIntercept { enabled } => {
                if self.ewm.text_input_intercept != enabled {
                    info!("Text input intercept: {}", enabled);
                    self.ewm.text_input_intercept = enabled;
                }
            }
            ModuleCommand::KeyboardCaptureChanged { active } => {
                trace!("Keyboard capture changed: active={}", active);
                if active {
                    self.ewm.cancel_command_repeat();
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
            ModuleCommand::GetDebugState(reply) => {
                let outputs = self.backend.output_info_list(&self.ewm.sorted_outputs);
                let state = elisp::serde::to_value(self.ewm.debug_state(&outputs))
                    .unwrap_or(elisp::Value::NIL);
                let _ = reply.send(state);
            }
            ModuleCommand::EntryUnderFloatingCenter {
                floating_frame_id,
                reply,
            } => {
                let entry = self
                    .ewm
                    .tiled_entry_under_floating_center(floating_frame_id);
                let _ = reply.send(entry);
            }
            ModuleCommand::CreateActivationToken(reply) => {
                let (token_ref, _) = self.ewm.activation_state.create_external_token(None);
                let token = token_ref.clone();
                let token_str = token.as_str().to_string();
                debug!("Created activation token for Emacs: {}", token_str);
                if reply.send(token_str).is_err() {
                    // Emacs gave up before we replied; the token would otherwise leak.
                    self.ewm.activation_state.remove_token(&token);
                }
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
                self.refresh_pointer_after_layout();
            }
            ModuleCommand::FloatingLayout { output, frames } => {
                self.ewm.apply_floating_layout(&output, frames);
                self.refresh_pointer_after_layout();
            }
            ModuleCommand::MoveFloatingFrame { id, dx, dy } => {
                self.ewm.move_floating_frame(id, dx, dy);
                self.refresh_pointer_after_layout();
            }
            ModuleCommand::ResizeFloatingFrame { id, dw, dh } => {
                self.ewm.resize_floating_frame(id, dw, dh);
                self.refresh_pointer_after_layout();
            }
            ModuleCommand::ToggleFullscreen { entry_id } => {
                self.ewm.toggle_layout_entry_fullscreen(entry_id);
                self.refresh_pointer_after_layout();
            }
            ModuleCommand::ConfigureInput { configs } => {
                info!("Input config updated: {} entries", configs.len());

                // Extract keyboard settings before storing configs (avoids borrow conflict).
                let kb_repeat = input::effective_keyboard_repeat(&configs);
                let kb_xkb = configs
                    .iter()
                    .find(|c| {
                        c.device.is_none()
                            && c.device_type == Some(crate::input::DeviceType::Keyboard)
                    })
                    .and_then(|kb| {
                        kb.xkb_layouts.clone().map(|layouts| {
                            (layouts, kb.xkb_variants.clone(), kb.xkb_options.clone())
                        })
                    });

                self.ewm.input_configs = configs;
                self.backend
                    .reapply_libinput_config(&self.ewm.input_configs);

                // Apply the same effective repeat settings to Wayland clients
                // and intercepted command/text-input repeat.
                let (rate, delay) = kb_repeat;
                let keyboard = self.ewm.keyboard.clone();
                keyboard.change_repeat_info(rate, delay);
                self.ewm.configure_keyboard_repeat(rate, delay);
                info!("Keyboard repeat: rate={}, delay={}", rate, delay);

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
            ModuleCommand::ConfigureCursor { config } => {
                self.ewm.cursor_manager.reload(&config.theme, config.size);
                self.backend.clear_cursor_texture_cache();
                let env_vars = config.env_vars();
                self.backend.import_environment(&env_vars);
                self.ewm.queue_event(Event::Environment { vars: env_vars });
                self.ewm.queue_redraw_for_pointer();
            }
            ModuleCommand::ConfigureFocusFollowsMouse { state: setting } => {
                self.ewm.focus_follows_mouse_mode = setting;
            }
            ModuleCommand::SetDragSource { active } => {
                self.ewm.emacs_drag_source = active;
            }
            ModuleCommand::ConfigureUnfocusedAlpha { alpha } => {
                self.ewm.unfocused_alpha = alpha.clamp(0.0, 1.0);
            }
            ModuleCommand::ConfigureBlur { config } => {
                if self.ewm.blur_config != config {
                    self.ewm.blur_config = config;
                    self.ewm.queue_redraw_all();
                }
            }
            ModuleCommand::ConfigureAnimations { enabled } => {
                self.ewm.animations_clock.set_complete_instantly(!enabled);
            }
            ModuleCommand::Overview { action } => {
                match action {
                    module::OverviewAction::Toggle => self.ewm.overview_toggle(),
                    module::OverviewAction::Open => {
                        self.ewm.overview_open();
                    }
                    module::OverviewAction::Close => {
                        self.ewm.overview_close();
                    }
                }
                self.refresh_pointer_after_scene_change(false);
            }
            ModuleCommand::HideToggle => {
                self.ewm.hide_toggle();
                self.refresh_pointer_after_scene_change(false);
            }
            ModuleCommand::ConfigureOverview {
                zoom,
                backdrop_color,
            } => {
                self.ewm.overview.set_zoom(zoom);
                self.ewm.overview.backdrop_color = backdrop_color;
                self.ewm.queue_redraw_all();
            }
            ModuleCommand::SetIdleInhibited { inhibited } => {
                self.ewm.manual_idle_inhibited = inhibited;
            }

            ModuleCommand::Initialized => {
                // Force-resend keyboard.enter; PGTK silently dropped the
                // initial one (sent before its first surface roundtrip).
                info!("Client initialized; replaying keyboard focus");
                // set_focus() sends nothing when the target is unchanged, so
                // drop the protocol focus or the replay is a no-op.
                let keyboard = self.ewm.keyboard.clone();
                keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
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
                    if let Ok(text) = String::from_utf8(buf)
                        && !text.is_empty()
                    {
                        let _ = tx.send(Event::SelectionChanged { text });
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
            drm.lid_supported = true;
            if drm.lid_closed == is_closed {
                return;
            }
            drm.lid_closed = is_closed;
            drm.on_lid_state_changed(&mut self.ewm);
        }
    }

    /// Handle one IM relay activate/deactivate event. Delivered by the calloop
    /// channel so `is_active()` and the Emacs intercept round-trip land before
    /// the next key, not one dispatch late.
    pub fn handle_im_event(&mut self, event: im::relay::ImEvent) {
        match event {
            im::relay::ImEvent::Activated => {
                if self.ewm.im_text_input.activate() {
                    self.ewm.queue_event(Event::TextInputActivated);
                }

                // Drain commits queued during the disable->enable gap.
                let active_surface_id = self.ewm.active_text_input_surface_id();
                if let Some(text) = self
                    .ewm
                    .im_text_input
                    .drain_pending_for_active_surface(active_surface_id)
                {
                    debug!("ImCommit: draining queued commit(s)");
                    self.ewm.commit_active_text_input(text);
                }
            }
            im::relay::ImEvent::Deactivated => {
                if self.ewm.im_text_input.deactivate() {
                    self.ewm.queue_event(Event::TextInputDeactivated);
                }
                crate::module::set_text_input_surrounding(None);
            }
            im::relay::ImEvent::SurroundingText {
                text,
                cursor,
                anchor,
            } => {
                let active_surface_id = self.ewm.active_text_input_surface_id();
                self.ewm.im_text_input.set_surrounding(
                    active_surface_id,
                    text.len() as u32,
                    cursor,
                    anchor,
                );
                // Publish for `ewm-edit` to pull, rather than pushing per keystroke.
                crate::module::set_text_input_surrounding(active_surface_id.map(|surface_id| {
                    crate::module::SurroundingSnapshot {
                        surface_id,
                        text,
                        cursor,
                        anchor,
                    }
                }));
                // A finish queued during the refocus gap now has fresh lengths.
                let drained = self
                    .ewm
                    .im_text_input
                    .drain_pending_replace(active_surface_id);
                if let Some((surface_id, r)) = drained.deliver {
                    debug!("TextInputReplace: draining queued replace (surface_id={surface_id})");
                    self.ewm.deliver_replace_ack(surface_id, r);
                }
                for surface_id in drained.dropped {
                    debug!(
                        "TextInputReplace: dropped stale queued replace (surface_id={surface_id})"
                    );
                    self.ewm
                        .queue_event(Event::TextInputReplaceDropped { surface_id });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const HEAD_A: &str = "HEAD-A";
    const HEAD_B: &str = "HEAD-B";
    const HEAD_C: &str = "HEAD-C";
    const HEAD_D: &str = "HEAD-D";
    const PROP_HEADS: [&str; 4] = [HEAD_A, HEAD_B, HEAD_C, HEAD_D];
    const PROP_SURFACE_OUTPUTS: [&str; 6] = [HEAD_A, HEAD_B, HEAD_C, HEAD_D, "HEAD-X", "HEAD-Y"];

    fn nz(raw: u64) -> LayoutEntryId {
        std::num::NonZeroU64::new(raw).unwrap()
    }

    #[cfg(feature = "screencast")]
    #[test]
    fn layout_entry_window_title_uses_workspace_and_entry_name() {
        let named = LayoutEntry::emacs_window(nz(1), "*scratch*".to_string(), 0, 0, 100, 100);
        assert_eq!(
            layout_entry_window_title("eDP-1", "2", &named, "ignored"),
            "eDP-1 | 2 | *scratch*"
        );

        let unnamed = LayoutEntry::emacs_window(nz(2), String::new(), 0, 0, 100, 100);
        assert_eq!(
            layout_entry_window_title("HDMI-A-1", "1", &unnamed, "fallback"),
            "HDMI-A-1 | 1 | fallback"
        );
    }

    fn frame_with_entry(
        frame_surface_id: u64,
        entry_id: LayoutEntryId,
        surface_id: u64,
    ) -> strip::Frame {
        let mut frame = strip::Frame::new(frame_surface_id);
        frame.width = 400.0;
        frame.selected_entry_id = Some(entry_id);
        frame.entries.push(LayoutEntry::surface(
            entry_id,
            "entry".to_string(),
            surface_id,
            0,
            0,
            400,
            400,
        ));
        frame
    }

    fn frame_with_width(surface_id: u64, width: f64) -> strip::Frame {
        let mut frame = strip::Frame::new(surface_id);
        frame.width = width;
        frame
    }

    #[test]
    fn new_toplevel_skips_focused_floating_frame_for_tiled_host() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);

        fix.ewm()
            .apply_output_layout(HEAD_A, vec![frame_with_width(100, 800.0)]);

        let floating_entry_id = nz(3001);
        let floating_entry_surface_id = 9000;
        fix.ewm().apply_floating_layout(
            HEAD_A,
            vec![frame_with_entry(
                200,
                floating_entry_id,
                floating_entry_surface_id,
            )],
        );
        fix.ewm()
            .set_focus_entry(floating_entry_id, "test_floating_entry", false);

        assert_eq!(fix.ewm_ref().focused_frame_id, 200);
        assert_eq!(fix.ewm_ref().focused_entry_id, Some(floating_entry_id));
        assert_eq!(
            fix.ewm_ref().focused_surface_id(),
            floating_entry_surface_id
        );

        fix.enable_event_capture();
        let mut client = fix.new_client().unwrap();
        fix.roundtrip(&mut client);
        let toplevel = fix.open_toplevel(&mut client, "app", Size::from((240, 160)));
        let app_surface_id = fix.find_surface_id(&client, &toplevel.surface).unwrap();
        let placement = fix
            .drain_events()
            .into_iter()
            .find_map(|event| match event {
                Event::Mapped {
                    id,
                    frame_surface_id,
                    ..
                } if id == app_surface_id => frame_surface_id,
                _ => None,
            });

        // Frame 200 is floating and focused, but a new surface must land on the
        // tiled frame (100), not clobber the floating overlay.
        assert_eq!(placement, Some(100));
    }

    #[test]
    fn tiled_entry_under_floating_center_hits_active_window() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        // Tiled frame whose single entry spans the whole working area.
        let mut tiled = strip::Frame::new(100);
        tiled.width = 1600.0;
        tiled.selected_entry_id = Some(nz(1));
        tiled.entries.push(LayoutEntry::surface(
            nz(1),
            "app".to_string(),
            5000,
            0,
            0,
            1600,
            900,
        ));
        fix.ewm().apply_output_layout(HEAD_A, vec![tiled]);
        fix.ewm()
            .apply_floating_layout(HEAD_A, vec![frame_with_entry(200, nz(3001), 9000)]);

        // The floating frame (200) sits over the tiled entry -> that entry wins.
        assert_eq!(
            fix.ewm_ref().tiled_entry_under_floating_center(200),
            Some(nz(1))
        );
        // Unknown floating id -> nothing.
        assert_eq!(fix.ewm_ref().tiled_entry_under_floating_center(4242), None);
    }

    #[test]
    fn floating_frame_occludes_the_strip_when_hit_testing() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);

        // Tiled entry spanning the whole output (the background frame).
        let tiled_entry = nz(1);
        let mut tiled = strip::Frame::new(100);
        tiled.width = 1600.0;
        tiled.selected_entry_id = Some(tiled_entry);
        tiled.entries.push(LayoutEntry::surface(
            tiled_entry,
            "app".to_string(),
            5000,
            0,
            0,
            1600,
            900,
        ));
        fix.ewm().apply_output_layout(HEAD_A, vec![tiled]);

        // Floating frame (400x480, centered at (600,210)) over the tiled entry;
        // its 400x400 entry leaves an 80px strip of the frame's own surface.
        let floating_entry = nz(3001);
        fix.ewm()
            .apply_floating_layout(HEAD_A, vec![frame_with_entry(200, floating_entry, 9000)]);

        let over_entry = Point::from((800.0, 400.0)); // inside the floating entry
        // The frame's own surface below its entry: the modeline and echo area --
        // the only region where focus fell through to the strip.
        let modeline = Point::from((800.0, 620.0));
        let echo_area = Point::from((800.0, 685.0));
        let outside = Point::from((100.0, 100.0)); // tiled area beside the float

        let ewm = fix.ewm_ref();
        let on_float = Some((HEAD_A.to_string(), 200));
        // Scene sanity: the anchors are where the geometry above puts them.
        assert_eq!(ewm.floating_frame_under(over_entry), on_float);
        assert_eq!(ewm.floating_frame_under(modeline), on_float);
        assert_eq!(ewm.floating_frame_under(echo_area), on_float);
        assert!(ewm.floating_frame_under(outside).is_none());

        // The modeline and echo area occlude the tiled entry beneath them.
        assert_eq!(ewm.layout_entry_id_under(modeline), None);
        assert_eq!(ewm.layout_entry_id_under(echo_area), None);
        // The floating entry still resolves; the tiled entry still wins beside the float.
        assert_eq!(ewm.layout_entry_id_under(over_entry), Some(floating_entry));
        assert_eq!(ewm.layout_entry_id_under(outside), Some(tiled_entry));

        // No point over the floating frame ever hit-tests to the tiled entry beneath.
        proptest!(|(x in 0i32..1600, y in 0i32..900)| {
            let p = Point::from((x as f64, y as f64));
            if ewm.floating_frame_under(p).is_some() {
                prop_assert_ne!(ewm.layout_entry_id_under(p), Some(tiled_entry));
            }
        });
    }

    fn output_names(outputs: &[OutputInfo]) -> Vec<String> {
        outputs.iter().map(|output| output.name.clone()).collect()
    }

    #[test]
    fn output_reports_track_backend_lifecycle() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        fix.add_output(HEAD_B, 1280, 720);

        assert_eq!(
            output_names(&fix.output_info_list()),
            vec![HEAD_A.to_string(), HEAD_B.to_string()]
        );

        fix.ewm().output_config.insert(
            HEAD_A.to_string(),
            OutputConfig {
                position: Some((320, 40)),
                scale: Some(1.5),
                ..Default::default()
            },
        );
        fix.apply_output_config(HEAD_A);

        let outputs = fix.output_info_list();
        let output = outputs.iter().find(|output| output.name == HEAD_A).unwrap();
        assert_eq!((output.x, output.y), (320, 40));
        assert!((output.scale - backend::closest_representable_scale(1.5)).abs() < f64::EPSILON);

        fix.remove_output(HEAD_B);

        assert_eq!(
            output_names(&fix.output_info_list()),
            vec![HEAD_A.to_string()]
        );
    }

    fn strip_ids(fix: &testing::Fixture, name: &str) -> Vec<u64> {
        fix.ewm_ref()
            .frame_set
            .mapped_strip(name)
            .map(|strip| strip.frames.iter().map(|f| f.surface_id).collect())
            .unwrap_or_default()
    }

    #[test]
    fn frames_survive_losing_every_output() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        fix.add_output(HEAD_B, 1280, 720);
        fix.ewm()
            .frame_set
            .insert_mapped_strip(HEAD_A.to_string(), strip_with_frame_ids(vec![1, 2], 1));
        fix.ewm()
            .frame_set
            .insert_mapped_strip(HEAD_B.to_string(), strip_with_frame_ids(vec![3, 4], 1));
        fix.ewm().set_focus_frame(4, "test", false);

        // Lid close: HEAD_A's frames migrate to HEAD_B.
        fix.remove_output(HEAD_A);
        assert_eq!(strip_ids(&fix, HEAD_B), vec![3, 4, 1, 2]);

        // Undock with the lid closed: no fallback, all frames go homeless.
        fix.remove_output(HEAD_B);
        assert!(fix.ewm_ref().frame_set.mapped_strips().is_empty());
        let homeless: Vec<u64> = fix
            .ewm_ref()
            .frame_set
            .homeless_frames()
            .iter()
            .map(|f| f.surface_id)
            .collect();
        assert_eq!(homeless, vec![3, 4, 1, 2]);
        assert_eq!(fix.ewm_ref().focused_frame_id, 0);

        // Lid open: the first output to map adopts every homeless frame.
        fix.add_output(HEAD_A, 1600, 900);
        assert_eq!(strip_ids(&fix, HEAD_A), vec![3, 4, 1, 2]);
        assert!(fix.ewm_ref().frame_set.homeless_frames().is_empty());
        assert_eq!(fix.ewm_ref().active_frame_surface_id(HEAD_A), Some(2));
        assert_eq!(fix.ewm_ref().focused_frame_id, 2);

        // Dock again: HEAD_B's frames return home with their active restored.
        fix.add_output(HEAD_B, 1280, 720);
        assert_eq!(strip_ids(&fix, HEAD_A), vec![1, 2]);
        assert_eq!(strip_ids(&fix, HEAD_B), vec![3, 4]);
        assert_eq!(fix.ewm_ref().active_frame_surface_id(HEAD_B), Some(4));
    }

    #[test]
    fn cursor_config_change_queues_pointer_output_redraw() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        fix.add_output(HEAD_B, 1280, 720);
        fix.warp_pointer(10.0, 10.0);
        for state in fix.ewm().output_state.values_mut() {
            state.redraw_state = RedrawState::Idle;
        }

        fix.handle_module_command(crate::module::ModuleCommand::ConfigureCursor {
            config: cursor::CursorConfig::new("ConfiguredCursorTheme", 24),
        });

        let redraws = fix
            .ewm_ref()
            .output_state
            .iter()
            .filter(|(_, state)| state.redraw_state.is_render_queued())
            .map(|(output, _)| output.name())
            .collect::<Vec<_>>();
        assert_eq!(redraws, vec![HEAD_A.to_string()]);
    }

    fn add_prop_outputs(fix: &mut testing::Fixture) {
        for name in PROP_HEADS {
            fix.add_output(name, 1600, 900);
        }
    }

    fn order_from_keys(keys: &[u8; 4]) -> Vec<usize> {
        let mut order = (0..PROP_HEADS.len()).collect::<Vec<_>>();
        order.sort_by_key(|&idx| (keys[idx], idx));
        order
    }

    fn set_sorted_output_order(fix: &mut testing::Fixture, order: &[usize]) {
        fix.ewm().sorted_outputs.sort_by_key(|output| {
            order
                .iter()
                .position(|&idx| PROP_HEADS[idx] == output.name())
                .unwrap_or(usize::MAX)
        });
    }

    fn strip_with_frame_ids(ids: Vec<u64>, active_idx: usize) -> strip::Strip {
        let mut strip = strip::Strip::new(strip::AnimationsClock::default());
        strip.frames = ids.into_iter().map(strip::Frame::new).collect();
        if !strip.frames.is_empty() {
            strip.set_active(active_idx % strip.frames.len());
        }
        strip
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn frame_fallbacks_follow_sorted_output_order(
            order_keys in any::<[u8; 4]>(),
            frame_counts in any::<[u8; 4]>(),
            active_indices in any::<[u8; 4]>(),
        ) {
            prop_assume!(frame_counts.iter().filter(|count| **count % 4 != 0).count() >= 2);

            let mut fix = testing::Fixture::new().unwrap();
            add_prop_outputs(&mut fix);
            let order = order_from_keys(&order_keys);
            set_sorted_output_order(&mut fix, &order);
            prop_assert_eq!(
                fix.ewm_ref()
                    .output_at(Point::from((-10_000.0, -10_000.0)))
                    .map(|output| output.name()),
                Some(PROP_HEADS[order[0]].to_string())
            );
            prop_assert_eq!(
                fix.ewm_ref()
                    .active_outputs_snapshot()
                    .into_iter()
                    .map(|output| output.name)
                    .collect::<Vec<_>>(),
                order
                    .iter()
                    .map(|idx| PROP_HEADS[*idx].to_string())
                    .collect::<Vec<_>>()
            );

            for (idx, name) in PROP_HEADS.iter().enumerate() {
                let count = usize::from(frame_counts[idx] % 4);
                if count == 0 {
                    continue;
                }
                let base = 1_000 + idx as u64 * 100;
                let ids = (0..count).map(|frame_idx| base + frame_idx as u64).collect();
                let strip = strip_with_frame_ids(ids, usize::from(active_indices[idx]));
                fix.ewm().frame_set.insert_mapped_strip((*name).to_string(), strip);
            }

            let first_output = order
                .iter()
                .copied()
                .find(|idx| frame_counts[*idx] % 4 != 0)
                .unwrap();
            let first_frame_id = 1_000 + first_output as u64 * 100;
            let first_active_id = first_frame_id
                + u64::from(active_indices[first_output] % (frame_counts[first_output] % 4));

            prop_assert_eq!(
                fix.ewm_ref().first_active_frame_surface_id(),
                Some(first_active_id)
            );

            fix.ewm().focused_frame_id = 0;
            fix.ewm().focus_any_frame_if_unfocused("test");
            prop_assert_eq!(fix.ewm_ref().focused_frame_id, first_frame_id);

            let disabled_name = PROP_HEADS[first_output];
            fix.ewm()
                .sorted_outputs
                .retain(|output| output.name() != disabled_name);
            let fallback_output = order
                .iter()
                .copied()
                .find(|idx| *idx != first_output && frame_counts[*idx] % 4 != 0)
                .unwrap();
            let fallback_active_id = 1_000
                + fallback_output as u64 * 100
                + u64::from(active_indices[fallback_output] % (frame_counts[fallback_output] % 4));

            fix.ewm()
                .focus_mapped_frame_if_focused_output_unmapped(disabled_name);
            prop_assert_eq!(fix.ewm_ref().focused_frame_id, fallback_active_id);
        }

        #[test]
        fn surface_output_name_prefers_focus_then_sorted_output_order(
            order_keys in any::<[u8; 4]>(),
            membership in 1u8..64,
            focused_idx in 0usize..=4,
        ) {
            let mut fix = testing::Fixture::new().unwrap();
            add_prop_outputs(&mut fix);
            let order = order_from_keys(&order_keys);
            set_sorted_output_order(&mut fix, &order);

            let surface_id = 9_000;
            let selected = PROP_SURFACE_OUTPUTS
                .iter()
                .enumerate()
                .filter_map(|(idx, name)| ((membership & (1 << idx)) != 0).then_some(*name))
                .collect::<Vec<_>>();
            fix.ewm().surface_outputs.insert(
                surface_id,
                selected.iter().map(|name| (*name).to_string()).collect(),
            );

            for (idx, name) in PROP_HEADS.iter().enumerate() {
                if selected.contains(name) {
                    let mut strip = strip::Strip::new(strip::AnimationsClock::default());
                    let frame = frame_with_entry(
                        2_000 + idx as u64,
                        nz(3_000 + idx as u64),
                        surface_id,
                    );
                    strip.frames.push(frame);
                    fix.ewm().frame_set.insert_mapped_strip((*name).to_string(), strip);
                }
            }

            let focused = if focused_idx < PROP_HEADS.len() {
                let name = PROP_HEADS[focused_idx];
                selected.contains(&name).then_some(name)
            } else {
                None
            };
            if focused.is_some() {
                fix.ewm().focused_entry_id = Some(nz(3_000 + focused_idx as u64));
                fix.ewm().focused_frame_id = 2_000 + focused_idx as u64;
            }

            let ordered = order
                .iter()
                .map(|idx| PROP_HEADS[*idx])
                .find(|name| selected.contains(name));
            let lexical = selected.iter().copied().min();
            let expected = focused.or(ordered).or(lexical);

            prop_assert_eq!(fix.ewm_ref().surface_output_name(surface_id), expected);
            prop_assert_eq!(
                fix.ewm_ref()
                    .layout_entry_for_surface(surface_id, |_| true)
                    .map(|location| location.output_name),
                ordered,
            );
            prop_assert_eq!(
                fix.ewm_ref().frame_position(surface_id),
                focused.or(ordered).map(|name| (name.to_string(), 0)),
            );
            prop_assert_eq!(
                fix.ewm_ref().frame_for_surface(surface_id),
                focused.or(ordered).map(|name| {
                    2_000 + PROP_HEADS.iter().position(|candidate| candidate == &name).unwrap() as u64
                }),
            );
        }

        #[test]
        fn primary_entry_ties_follow_sorted_output_order(
            order_keys in any::<[u8; 4]>(),
            membership in 1u8..16,
            entry_w in 1u32..800,
            entry_h in 1u32..800,
        ) {
            let mut fix = testing::Fixture::new().unwrap();
            add_prop_outputs(&mut fix);
            let order = order_from_keys(&order_keys);
            set_sorted_output_order(&mut fix, &order);

            let surface_id = 10_000;
            for (idx, name) in PROP_HEADS.iter().enumerate() {
                if membership & (1 << idx) == 0 {
                    continue;
                }
                let mut strip = strip::Strip::new(strip::AnimationsClock::default());
                let surface_entry_id = nz(5_000 + idx as u64);
                let plain_entry_id = nz(6_000 + idx as u64);
                let mut frame = frame_with_entry(
                    4_000 + idx as u64,
                    surface_entry_id,
                    surface_id,
                );
                frame.entries[0].w = entry_w;
                frame.entries[0].h = entry_h;
                frame.entries.push(LayoutEntry::emacs_window(
                    plain_entry_id,
                    "plain".to_string(),
                    entry_w as i32,
                    0,
                    entry_w,
                    entry_h,
                ));
                strip.frames.push(frame);
                fix.ewm().frame_set.insert_mapped_strip((*name).to_string(), strip);
            }

            let output_sizes = PROP_HEADS
                .iter()
                .map(|name| ((*name).to_string(), (100, 100)))
                .collect::<HashMap<_, _>>();
            let best_area = fix.ewm_ref().primary_entry_areas(&output_sizes);
            fix.ewm().assign_primary_entries(&output_sizes, &best_area);

            let expected = order
                .iter()
                .map(|idx| PROP_HEADS[*idx])
                .find(|name| {
                    let idx = PROP_HEADS.iter().position(|candidate| candidate == name).unwrap();
                    membership & (1 << idx) != 0
                })
                .unwrap();
            let primary = PROP_HEADS
                .iter()
                .copied()
                .filter(|name| {
                    fix.ewm_ref().frame_set.mapped_strip(name)
                        .is_some_and(|strip| {
                            strip.surface_entries().any(|entry| {
                                entry.surface_id() == Some(surface_id) && entry.primary()
                            })
                        })
                })
                .collect::<Vec<_>>();

            prop_assert_eq!(primary, vec![expected]);

            #[cfg(feature = "screencast")]
            {
                fix.ewm().refresh_mapped_cast_outputs();
                for (idx, name) in PROP_HEADS.iter().enumerate() {
                    if membership & (1 << idx) == 0 {
                        continue;
                    }

                    for entry_id in [nz(5_000 + idx as u64), nz(6_000 + idx as u64)] {
                        prop_assert_eq!(
                            fix.ewm_ref()
                                .casting
                                .mapped_cast_output
                                .get(&entry_id)
                                .map(Output::name),
                            Some((*name).to_string()),
                        );
                        prop_assert_eq!(
                            fix.ewm_ref()
                                .layout_entry_cast_params(entry_id)
                                .map(|(size, _)| size),
                            Some(Size::from((entry_w as i32, entry_h as i32))),
                        );
                    }
                }

                let removed_idx = order
                    .iter()
                    .copied()
                    .find(|idx| membership & (1 << idx) != 0)
                    .unwrap();
                let removed_name = PROP_HEADS[removed_idx];
                if let Some(strip) = fix.ewm().frame_set.mapped_strip_mut(removed_name)
                    && let Some(frame) = strip.frames.first_mut() {
                        frame.entries.clear();
                    }
                fix.ewm().refresh_mapped_cast_outputs();
                prop_assert!(!fix
                    .ewm_ref()
                    .casting
                    .mapped_cast_output
                    .contains_key(&nz(5_000 + removed_idx as u64)));
                prop_assert!(!fix
                    .ewm_ref()
                    .casting
                    .mapped_cast_output
                    .contains_key(&nz(6_000 + removed_idx as u64)));
            }
        }

        #[test]
        fn lock_focus_output_prefers_cursor_then_sorted_output_order(
            order_keys in any::<[u8; 4]>(),
            cursor_idx in 0usize..4,
        ) {
            let mut fix = testing::Fixture::new().unwrap();
            add_prop_outputs(&mut fix);
            let order = order_from_keys(&order_keys);
            set_sorted_output_order(&mut fix, &order);

            let cursor_name = PROP_HEADS[cursor_idx];
            let cursor_output = fix.ewm_ref().find_mapped_output(cursor_name).unwrap();
            let cursor_geo = fix.ewm_ref().space.output_geometry(&cursor_output).unwrap();
            let cursor_center = crate::utils::center(cursor_geo).to_f64();
            fix.warp_pointer(cursor_center.x, cursor_center.y);
            prop_assert_eq!(
                fix.ewm_ref().lock_surface_output().map(|output| output.name()),
                Some(cursor_name.to_string()),
            );

            fix.warp_pointer(-10_000.0, -10_000.0);
            prop_assert_eq!(
                fix.ewm_ref().lock_surface_output().map(|output| output.name()),
                Some(PROP_HEADS[order[0]].to_string()),
            );
        }
    }

    #[test]
    fn hidden_surface_activation_asks_emacs_to_show_it() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.enable_event_capture();

        fix.ewm().activate_or_defer_surface(42, true, "test");

        assert!(fix.ewm_ref().pending_activation_surfaces.contains(&42));
        assert!(matches!(
            fix.drain_events().as_slice(),
            [Event::ActivateSurface { id: 42 }]
        ));
    }

    #[test]
    fn surface_request_helpers_queue_expected_events_and_update_fullscreen() {
        use smithay::wayland::seat::WaylandFocus as _;

        let mut fix = testing::Fixture::new().unwrap();
        let mut client = fix.new_client().unwrap();
        fix.roundtrip(&mut client);
        fix.add_output(HEAD_A, 1600, 900);

        let toplevel = fix.open_toplevel(&mut client, "requests", Size::from((400, 400)));
        let surface_id = fix.find_surface_id(&client, &toplevel.surface).unwrap();
        fix.ewm()
            .apply_output_layout(HEAD_A, vec![frame_with_entry(100, nz(3001), surface_id)]);

        let server_surface = fix
            .ewm_ref()
            .id_windows
            .get(&surface_id)
            .and_then(|window| window.wl_surface())
            .unwrap()
            .into_owned();

        fix.enable_event_capture();
        fix.ewm()
            .queue_minimize_request_for_surface(&server_surface);
        fix.ewm()
            .set_fullscreen_for_surface(&server_surface, None, true);
        assert!(fix.ewm_ref().is_surface_fullscreen(surface_id));
        fix.ewm()
            .set_fullscreen_for_surface(&server_surface, None, false);
        assert!(!fix.ewm_ref().is_surface_fullscreen(surface_id));
        fix.ewm()
            .queue_maximize_request_for_surface(&server_surface);
        fix.ewm()
            .queue_unmaximize_request_for_surface(&server_surface);

        let events = fix.drain_events();
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], Event::Minimize { id } if *id == surface_id));
        assert!(matches!(&events[1], Event::MaximizeRequest { id } if *id == surface_id));
        assert!(matches!(&events[2], Event::UnmaximizeRequest { id } if *id == surface_id));
    }

    #[test]
    fn system_bell_forwards_toplevel_id() {
        let mut fix = testing::Fixture::new().unwrap();
        let mut client = fix.new_client().unwrap();
        fix.roundtrip(&mut client);
        fix.add_output(HEAD_A, 1600, 900);

        let toplevel = fix.open_toplevel(&mut client, "bell", Size::from((400, 400)));
        let surface_id = fix.find_surface_id(&client, &toplevel.surface).unwrap();

        fix.enable_event_capture();
        client.ring_bell(Some(&toplevel.surface));
        client.ring_bell(None);
        fix.roundtrip(&mut client);

        let events = fix.drain_events();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::Bell { id: Some(id) } if *id == surface_id));
        assert!(matches!(&events[1], Event::Bell { id: None }));
    }

    #[test]
    fn background_title_updates_coalesce_to_latest() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.enable_event_capture();

        fix.ewm()
            .queue_title_update(42, "app".to_string(), "one".to_string(), false, false);
        fix.ewm()
            .queue_title_update(42, "app".to_string(), "two".to_string(), false, false);

        assert!(fix.drain_events().is_empty());

        fix.ewm().flush_pending_title_events();
        let events = fix.drain_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::Title { id, app, title }
                if *id == 42 && app == "app" && title == "two"));
    }

    #[test]
    fn focused_title_update_is_immediate() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        let entry_id = nz(3001);
        let surface_id = 42;

        let mut strip = strip::Strip::new(strip::AnimationsClock::default());
        strip
            .frames
            .push(frame_with_entry(100, entry_id, surface_id));
        fix.ewm()
            .frame_set
            .insert_mapped_strip(HEAD_A.to_string(), strip);
        fix.ewm().focused_frame_id = 100;
        fix.ewm().focused_entry_id = Some(entry_id);
        fix.enable_event_capture();

        fix.ewm().queue_title_update(
            surface_id,
            "app".to_string(),
            "focused".to_string(),
            false,
            false,
        );

        let events = fix.drain_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::Title { id, app, title }
                if *id == surface_id && app == "app" && title == "focused"));
    }

    #[test]
    fn fallback_frame_callbacks_hide_unmapped_output_frames() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        fix.add_output(HEAD_B, 1600, 900);
        fix.ewm()
            .frame_set
            .insert_mapped_strip(HEAD_A.to_string(), strip_with_frame_ids(vec![100, 101], 0));
        fix.ewm()
            .frame_set
            .insert_mapped_strip(HEAD_B.to_string(), strip_with_frame_ids(vec![200], 0));

        fix.ewm().output_config.insert(
            HEAD_B.to_string(),
            OutputConfig {
                enabled: false,
                ..Default::default()
            },
        );
        fix.apply_output_config(HEAD_B);

        let hidden = fix.ewm_ref().fallback_hidden_emacs_ids();
        assert!(!hidden.contains(&100));
        assert!(hidden.contains(&101));
        assert!(hidden.contains(&200));
    }

    #[test]
    fn close_flushes_pending_title_before_close_event() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.enable_event_capture();

        fix.ewm()
            .queue_title_update(42, "app".to_string(), "final".to_string(), false, false);
        assert!(fix.drain_events().is_empty());

        fix.ewm().handle_toplevel_destroyed_by_id(42);
        let events = fix.drain_events();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::Title { id, app, title }
                if *id == 42 && app == "app" && title == "final"));
        assert!(matches!(&events[1], Event::Close { id } if *id == 42));
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

    #[test]
    fn emacs_frame_origin_uses_floating_position() {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        fix.ewm()
            .apply_floating_layout(HEAD_A, vec![frame_with_width(200, 640.0)]);

        assert_eq!(
            fix.ewm_ref().emacs_frame_origin(200),
            Some(Point::from((480, 210))),
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        // Mid-slide, popups and hits round the frame's position like the renderer.
        #[test]
        fn emacs_frame_origin_rounds_a_sliding_frame(offset in -2000f64..=2000.) {
            let mut fix = testing::Fixture::new().unwrap();
            fix.add_output(HEAD_A, 1600, 900);
            fix.ewm().apply_output_layout(
                HEAD_A,
                vec![frame_with_width(100, 400.0), frame_with_width(101, 400.0)],
            );
            let strip = fix.ewm().frame_set.mapped_strip_mut(HEAD_A).unwrap();
            strip.view_offset = strip::ViewOffset::Static(offset);
            let expected = (strip.column_x(1) - strip.view_pos()).round() as i32;

            prop_assert_eq!(
                fix.ewm_ref().emacs_frame_origin(101),
                Some(Point::from((expected, 0))),
            );
        }
    }
}

// Emacs dynamic module initialization
emacs::plugin_is_GPL_compatible! {}

#[emacs::module(name = "ewm-core", defun_prefix = "ewm", mod_in_name = false)]
fn init(_: &emacs::Env) -> emacs::Result<()> {
    Ok(())
}
