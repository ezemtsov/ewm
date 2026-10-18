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

pub mod animation;
pub mod backend;
pub mod closing_frame;
pub mod cursor;
#[cfg(feature = "screencast")]
pub mod dbus;
pub mod dbus_login1;
pub mod dbus_screensaver;
pub mod event;
pub mod floating;
mod floating_shadow;
pub mod frame_clock;
pub mod gtk;
pub mod handlers;
pub mod im;
pub mod input;
pub mod layout;
mod module;
pub mod output_mode;
pub mod overview;
pub mod protocols;
pub mod render;
mod render_helpers;
pub mod resize_fade;
#[cfg(feature = "screencast")]
pub mod screencasting;
mod shadow_style;
pub mod snapshot;
pub mod strip;
pub mod surface;
pub mod texture;
pub mod tracy;
pub mod utils;
pub mod vblank_throttle;
pub mod xwayland;

pub use module::OverviewAction;
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

use crate::handlers::{XDG_ACTIVATION_TOKEN_TIMEOUT, configure_lock_surface};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem;
use std::os::unix::io::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub use backend::{Backend, DrmBackendState, HeadlessBackend};
pub use event::{Event, OutputInfo, OutputMode};
pub use layout::{Layout, Monitor, Placement};
use im::repeat::{HeldKeyOwner, InterceptRepeat, InterceptRepeatState};
use serde::{Deserialize, Serialize};
use smithay::backend::input::{InputTime, KeyState};
use smithay::backend::renderer::element::solid::SolidColorBuffer;
use smithay::backend::renderer::element::utils::select_dmabuf_feedback;
use smithay::backend::renderer::element::{
    default_primary_scanout_output_compare, RenderElementStates,
};
use smithay::desktop::utils::{
    bbox_from_surface_tree, output_update, send_dmabuf_feedback_surface_tree,
    send_frames_surface_tree, surface_presentation_feedback_flags_from_states,
    surface_primary_scanout_output, take_presentation_feedback_surface_tree,
    update_surface_primary_scanout_output, OutputPresentationFeedback,
};
use smithay::desktop::{
    layer_map_for_output,
    LayerSurface as DesktopLayerSurface, PopupGrab, PopupManager,
    PopupPointerGrab, Space, Window, WindowSurfaceType,
};
use smithay::input::keyboard::xkb::keysyms;
use smithay::input::keyboard::{KeyboardHandle, ModifiersState};
use smithay::input::pointer::{
    ClickGrab, CursorImageStatus, CursorImageSurfaceData, PointerHandle,
};
use smithay::input::{Seat, SeatState};
use smithay::output::{self, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::Timer;
use smithay::reexports::calloop::{
    Interest, LoopHandle, LoopSignal, Mode as CalloopMode, PostAction, RegistrationToken,
};
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState;
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeDefaultMode;
use smithay::reexports::wayland_server::backend::{
    ClientData, ClientId, Credentials, DisconnectReason,
};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::utils::{IsAlive, Logical, Point, Rectangle, Size, Transform, SERIAL_COUNTER};
use smithay::wayland::background_effect::BackgroundEffectState;
use smithay::wayland::compositor::{
    get_parent, with_surface_tree_downward, CompositorClientState,
    CompositorState, SurfaceData, TraversalAction,
};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{DmabufState};
use smithay::wayland::fractional_scale::{FractionalScaleManagerState};
use smithay::wayland::idle_inhibit::{IdleInhibitManagerState};
use smithay::wayland::idle_notify::{IdleNotifierState};
use smithay::wayland::input_method::{
    InputMethodManagerState,
};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::pointer_constraints::{
    with_pointer_constraint, PointerConstraintsState,
};
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::data_device::{
    request_data_device_client_selection, set_data_device_selection,
    DataDeviceState, };
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionState,
};
use smithay::wayland::selection::wlr_data_control::{DataControlState};
use smithay::wayland::session_lock::{
    LockSurface, SessionLockManagerState, SessionLocker,
};
use smithay::wayland::shell::kde::decoration::{KdeDecorationState};
use smithay::wayland::shell::wlr_layer::{Layer, WlrLayerShellState};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationState};
use smithay::wayland::shell::xdg::{
    ToplevelSurface, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmState};
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::tablet_manager::TabletManagerState;
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::xdg_activation::{
    XdgActivationState,
};
use smithay::wayland::xdg_foreign::{XdgForeignState};
use smithay::wayland::xdg_system_bell::{XdgSystemBellState};
use tracing::{debug, error, info, trace, warn};

use crate::protocols::foreign_toplevel::{
    ForeignToplevelManagerState, WindowInfo,
};
use crate::protocols::output_management::{OutputManagementState};
use crate::protocols::screencopy::{ScreencopyManagerState};
use crate::protocols::virtual_pointer::{
    VirtualPointerManagerState, };
use crate::protocols::workspace::{WorkspaceManagerState};

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
    /// Panel exclusive zones, painted in the fill colour.
    pub bands: [SolidColorBuffer; 4],
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
            bands: Default::default(),
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
    /// The view its frame shows fullscreen, marked by `Layout::reconcile_fullscreen`.
    #[serde(default)]
    pub fullscreen: bool,
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
                fullscreen: false,
            }),
        }
    }

    pub fn surface_id(&self) -> Option<u64> {
        self.surface.as_ref().map(|surface| surface.surface_id)
    }

    pub fn rect(&self) -> Rectangle<i32, Logical> {
        Rectangle::new(
            (self.x, self.y).into(),
            (self.w as i32, self.h as i32).into(),
        )
    }

    pub fn primary(&self) -> bool {
        self.surface.as_ref().is_some_and(|surface| surface.primary)
    }

    pub fn set_primary(&mut self, primary: bool) {
        if let Some(surface) = &mut self.surface {
            surface.primary = primary;
        }
    }

    pub fn fullscreen(&self) -> bool {
        self.surface
            .as_ref()
            .is_some_and(|surface| surface.fullscreen)
    }

    pub fn set_fullscreen(&mut self, fullscreen: bool) {
        if let Some(surface) = &mut self.surface {
            surface.fullscreen = fullscreen;
        }
    }
}

struct LayoutEntryHit<'a> {
    entry: &'a LayoutEntry,
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
fn window_properties(surface: &surface::SurfaceState) -> dbus::WindowProperties {
    dbus::WindowProperties {
        title: surface.title.clone(),
        app_id: surface.app_id.clone(),
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
    /// Applies only while this surface has focus, or everywhere when `None`.
    pub surface: Option<u64>,
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
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InterceptDispatch {
    #[default]
    Keyboard,
    Command,
    /// Run in the compositor, Emacs told afterwards.
    Action(Action),
}

/// What a key or a module call changes in the compositor alone, Emacs told by events.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Focus the strip frame `delta` slots away, counting from the shown frame under a float.
    FocusFrame { delta: i32 },
    /// Focus the `n`th strip frame, counting from 1.
    FocusNthFrame { n: usize },
    /// Focus frame `id`, as a workspace client asks.
    ActivateFrame { id: u64 },
    /// Focus the active frame on the output in direction `(dx, dy)`.
    FocusOutput { dx: i32, dy: i32 },
    /// Move the focused strip frame `delta` slots.
    MoveFrame { delta: i32 },
    /// Fullscreen the focused view's surface, or bring it back.
    ToggleFullscreen,
    /// Open, close or toggle the overview.
    Overview(module::OverviewAction),
    /// Slide every frame off its output, or back.
    HideToggle,
    /// Run `command` detached from the compositor.
    Spawn { command: Vec<String> },
}

/// DnD icon surface attached to the pointer during a drag operation.
#[derive(Debug)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

const TITLE_EVENT_IDLE_FLUSH_DELAY: Duration = Duration::from_millis(500);
const TITLE_EVENT_MAX_STALE: Duration = Duration::from_millis(1000);
/// Critically damped and brisk, the one spring every slide runs on.
pub fn slide_spring() -> animation::SpringParams {
    animation::SpringParams::new(1., 800., 0.0001)
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
    pub tablet_cursor_location: Option<Point<f64, Logical>>,

    // Surface tracking
    next_surface_id: u64,
    title_flush_timer_token: Option<RegistrationToken>,
    last_emacs_keyboard_activity: Instant,
    /// Where every Emacs frame is: a monitor per mapped output, parked frames when none is.
    pub layout: Layout,
    /// Clock every `Animation` reads, frozen at the target presentation time per redraw.
    pub animations_clock: animation::Clock,
    /// Zoomed-out strip view shared by all outputs.
    pub overview: overview::Overview,
    /// Frames' slide off the top of every output: 0 in place, 1 fully off.
    pub hide: strip::ViewOffset,
    /// Every toplevel's compositor-side state, from creation to destruction.
    pub surfaces: HashMap<u64, surface::SurfaceState>,

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
    /// Keys for Emacs waiting until it reads the Focus they followed.
    pub(crate) deferred_keys: VecDeque<input::DeferredKey>,
    pub(crate) im_repeat: InterceptRepeatState,
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
    /// Connected libinput devices, for re-applying configuration on change
    pub libinput_devices: std::collections::HashSet<smithay::reexports::input::Device>,

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
    pub text_input_provider: im::TextInputProvider,
    // Relay client and its event source, connected while Emacs is the provider.
    im_relay: Option<(Client, RegistrationToken)>,

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

        // Input method manager (the relay or an external input method such as fcitx5)
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
        loop_handle_clone
            .insert_source(
                Timer::from_duration(XDG_ACTIVATION_TOKEN_TIMEOUT),
                |_, _, state| {
                    state.ewm.activation_state.retain_tokens(|_, token_data| {
                        token_data.timestamp.elapsed() < XDG_ACTIVATION_TOKEN_TIMEOUT
                    });
                    smithay::reexports::calloop::timer::TimeoutAction::ToDuration(
                        XDG_ACTIVATION_TOKEN_TIMEOUT,
                    )
                },
            )
            .expect("failed to register activation token timer");

        let animations_clock = animation::Clock::default();

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
            tablet_cursor_location: None,
            next_surface_id: 1,
            title_flush_timer_token: None,
            last_emacs_keyboard_activity: Instant::now(),
            layout: Layout::new(animations_clock.clone()),
            overview: overview::Overview::new(animations_clock.clone()),
            hide: strip::ViewOffset::default(),
            animations_clock,
            surfaces: HashMap::new(),
            output_size: Size::from((0, 0)),
            sorted_outputs: Vec::new(),
            output_config: HashMap::new(),
            pointer_focus: None,
            pointer_output: None,
            focused_frame_id: 0,
            focused_entry_id: None,
            keyboard_focus: None,
            deferred_keys: VecDeque::new(),
            im_repeat: InterceptRepeatState::new(
                input::DEFAULT_KEYBOARD_REPEAT_RATE,
                input::DEFAULT_KEYBOARD_REPEAT_DELAY,
            ),
            introspect_windows_dirty: false,
            focus_follows_mouse_mode: false,
            emacs_drag_source: false,
            unfocused_alpha: 1.0,
            blur_config: shadow_style::Blur::default(),
            pointer_entry_id: None,
            input_configs: Vec::new(),
            libinput_devices: std::collections::HashSet::new(),
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
            text_input_provider: im::TextInputProvider::default(),
            im_relay: None,
            popups: PopupManager::default(),
            popup_grab: None,
            dnd_icon: None,
            pending_drop: None,
            layer_shell_state,
            unmapped_layer_surfaces: std::collections::HashSet::new(),
            layer_shell_on_demand_focus: None,
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
    fn connect_im_relay(&mut self) {
        if self.im_relay.is_some() {
            return;
        }
        let (server_end, client_end) =
            UnixStream::pair().expect("failed to create IM relay socketpair");
        // Socketpair peer credentials are our own.
        let client = self
            .display_handle
            .insert_client(server_end, Arc::new(ClientState::default()))
            .expect("failed to insert IM relay client");
        let channel = im::relay::connect(client_end);
        let source = self
            .loop_handle
            .insert_source(channel, |event, _, state| {
                if let smithay::reexports::calloop::channel::Event::Msg(event) = event {
                    state.handle_im_event(event);
                }
            })
            .expect("failed to register IM relay event source");
        self.im_relay = Some((client, source));
    }

    /// Disconnect the relay so an external input method can take the seat.
    fn disconnect_im_relay(&mut self) {
        let Some((client, source)) = self.im_relay.take() else {
            return;
        };
        self.loop_handle.remove(source);
        self.display_handle
            .backend_handle()
            .kill_client(client.id(), DisconnectReason::ConnectionClosed);
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

    /// Unmap a connected output while keeping its head available for
    /// output-management so it can be re-enabled later.
    pub(crate) fn disable_output(&mut self, output: &Output) {
        if self.space.output_geometry(output).is_none() {
            return;
        }

        self.detach_output(output);
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

        // The output goes first, then the frames it took with it.
        self.queue_event(Event::OutputDisconnected {
            name: output_name.clone(),
        });
        self.detach_output(output);
        self.screencopy_state.remove_output(output);

        info!("Output removed: {}", output_name);
    }

    /// Remove dead windows from the space.
    /// This replaces Space::refresh(). We manage output enter/leave explicitly
    /// rather than relying on automatic spatial overlap detection.
    pub fn cleanup_dead_windows(&mut self) -> bool {
        let dead_ids: Vec<u64> = self
            .windows()
            .filter(|(_, window)| !window.alive())
            .map(|(id, _)| id)
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
            let output = self.layout.output(&info.name);
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

    fn queue_maximize_request_for_surface(&mut self, surface: &WlSurface) {
        self.queue_surface_request(surface, |id| Event::MaximizeRequest { id });
    }

    fn queue_unmaximize_request_for_surface(&mut self, surface: &WlSurface) {
        self.queue_surface_request(surface, |id| Event::UnmaximizeRequest { id });
    }

    #[cfg(feature = "screencast")]
    pub(crate) fn introspect_windows(&self) -> HashMap<u64, dbus::WindowProperties> {
        let mut windows = HashMap::new();
        for monitor in self.mapped_monitors() {
            let output_name = monitor.name();
            for (frame_idx, frame) in monitor.strip.frames().iter().enumerate() {
                let workspace_name =
                    crate::protocols::workspace::frame_workspace_name(frame, frame_idx);
                let frame_props = self.surfaces.get(&frame.surface_id).map(window_properties);
                for entry in &frame.entries {
                    let props = entry
                        .surface_id()
                        .and_then(|surface_id| self.surfaces.get(&surface_id))
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

    /// Surface id receiving keyboard input.
    pub fn focused_surface_id(&self) -> u64 {
        self.focused_entry_id
            .and_then(|entry_id| self.layout.entry_surface_id(entry_id))
            .unwrap_or(self.focused_frame_id)
    }

    pub fn set_surface_urgent(&mut self, surface_id: u64, urgent: bool) -> bool {
        if surface_id == 0 || (urgent && self.layout.has_frame(surface_id)) {
            return false;
        }
        if urgent && self.focused_surface_id() == surface_id {
            return false;
        }

        self.surfaces
            .get_mut(&surface_id)
            .is_some_and(|surface| std::mem::replace(&mut surface.urgent, urgent) != urgent)
    }

    fn clear_focused_urgency(&mut self) {
        for id in [self.focused_surface_id(), self.focused_frame_id] {
            if let Some(surface) = self.surfaces.get_mut(&id) {
                surface.urgent = false;
            }
        }
    }

    pub fn mark_workspace_urgent(&mut self, frame_surface_id: Option<u64>) -> bool {
        let frame_surface_id = frame_surface_id.unwrap_or(self.focused_frame_id);
        if frame_surface_id == 0 || !self.layout.has_frame(frame_surface_id) {
            return false;
        }
        if self.focused_frame_id == frame_surface_id {
            return false;
        }

        self.surfaces
            .get_mut(&frame_surface_id)
            .is_some_and(|frame| !std::mem::replace(&mut frame.urgent, true))
    }

    fn set_focus_state(&mut self, frame_id: u64, entry_id: Option<LayoutEntryId>) {
        self.focused_frame_id = frame_id;
        self.focused_entry_id = entry_id;
    }

    /// Focus the layout entry identified by `entry_id`. Returns the frame focused.
    pub fn set_focus_entry(
        &mut self,
        entry_id: LayoutEntryId,
        source: &str,
        notify_emacs: bool,
    ) -> Option<u64> {
        let location = self.layout.entry_location(entry_id)?;
        let frame_id = location.frame.surface_id;
        module::record_focus(
            location.entry.surface_id().unwrap_or(frame_id),
            source,
            None,
        );
        self.commit_focus(frame_id, Some(entry_id), entry_id, notify_emacs);
        Some(self.focused_frame_id)
    }

    /// Focus an Emacs frame, routing keyboard to its selected entry's surface if one exists.
    pub fn set_focus_frame(
        &mut self,
        frame_surface_id: u64,
        source: &str,
        notify_emacs: bool,
    ) -> Option<FocusId> {
        let Some(location) = self.layout.frame_location(frame_surface_id) else {
            debug!("set_focus_frame({source}): unknown frame {frame_surface_id}");
            return None;
        };
        let (focus_id, selected) = (location.frame.focus_id, location.frame.selected_entry_id);
        module::record_focus(frame_surface_id, source, None);
        Some(self.commit_focus(frame_surface_id, selected, focus_id, notify_emacs))
    }

    /// Focus `frame_id` with `entry_id` selected, then tell Emacs about `focus_id`.
    fn commit_focus(
        &mut self,
        frame_id: u64,
        entry_id: Option<LayoutEntryId>,
        focus_id: FocusId,
        notify_emacs: bool,
    ) -> FocusId {
        // A fullscreen float takes the focus meant for its frame.
        let (frame_id, entry_id, focus_id) = match self.layout.fullscreen_float_over(frame_id) {
            Some((float_id, entry_id)) => (float_id, Some(entry_id), entry_id),
            None => (frame_id, entry_id, focus_id),
        };
        if let Some(surface_id) =
            entry_id.and_then(|entry_id| self.layout.entry_surface_id(entry_id))
        {
            self.flush_pending_title_event(surface_id);
        }
        self.set_focus_state(frame_id, entry_id);
        self.clear_focused_urgency();
        self.activate_frame(frame_id);
        if notify_emacs {
            self.queue_event(Event::Focus {
                focus_id,
                x: None,
                y: None,
                pointer: false,
            });
        }
        focus_id
    }

    fn workspace_name_from_rename(name: String) -> Option<String> {
        let name = name.trim().to_string();
        (!name.is_empty()).then_some(name)
    }

    fn workspace_name_taken(&self, name: &str, except_frame: u64) -> bool {
        let name = name.to_lowercase();
        self.layout.all_frames().any(|frame| {
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

        let Some(frame) = self.layout.frame_mut(frame_surface_id) else {
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
            .layout
            .strip(output)
            .map_or(0.0, strip::Strip::overview_shift)
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
        let strip = self.layout.strip(output)?;
        strip
            .frames_with_render_geo(
                working_area.size.to_f64(),
                geometry.strip_viewport(working_area),
            )
            .find(|(_, _, rect, _)| rect.contains(in_strip))
            .map(|(idx, _, rect, _)| (idx, in_strip - rect.loc))
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
        let floating_frame = self.layout.monitor_of(&output).and_then(|monitor| {
            monitor
                .floats_with_render_geo()
                .find(|(_, rect)| rect.contains(in_content))
                .map(|(frame, rect)| (frame.surface_id, rect))
        });
        if let Some((frame_id, rect)) = floating_frame {
            self.set_focus_frame_from_pointer(frame_id, "overview", in_content - rect.loc);
            self.overview_close();
        } else if let Some((idx, pos_in_frame)) =
            self.overview_frame_under(&output, &geometry, in_content)
        {
            self.overview_activate_frame(&output, idx, pos_in_frame);
        } else if Rectangle::from_size(output_geo.size.to_f64()).contains(in_output) {
            self.overview_close();
        }
    }

    /// Activate frame `idx` and close; the strip snaps onto it and the zoom carries the slide.
    fn overview_activate_frame(
        &mut self,
        output: &Output,
        idx: usize,
        pos_in_frame: Point<f64, Logical>,
    ) {
        self.bake_overview_shifts();
        let Some(strip) = self.layout.strip(output) else {
            return;
        };
        let Some(target) = strip.frames().get(idx).map(|frame| frame.surface_id) else {
            return;
        };
        let prev_view_pos = strip.view_pos();
        let zoom = self.overview.zoom();
        let progress = self.overview.progress().unwrap_or(0.0);
        self.set_focus_frame_from_pointer(target, "overview", pos_in_frame);
        if progress > 0.01
            && let Some(strip) = self.layout.strip_mut(output)
        {
            strip.carry_slide_as_overview_shift(prev_view_pos, zoom, progress);
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
        for strip in self.layout.strips_mut() {
            strip.bake_overview_shift(progress, zoom);
        }
    }

    /// Run overview `action`.
    pub(crate) fn overview(&mut self, action: module::OverviewAction) {
        match action {
            module::OverviewAction::Toggle => self.overview_toggle(),
            module::OverviewAction::Open => {
                self.overview_open();
            }
            module::OverviewAction::Close => {
                self.overview_close();
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
        let ids: Vec<u64> = self.layout.inactive_frame_ids().collect();
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
        self.hide.animate_to(
            self.animations_clock.clone(),
            if hidden { 1. } else { 0. },
            0.,
            f64::EPSILON,
        );
        self.queue_event(Event::Hide { hidden });
        self.queue_redraw_all();
    }

    /// End a strip swipe on `output`: snap to the nearest frame and focus it.
    pub fn strip_gesture_end(&mut self, output: &Output, working_w: f64) {
        let prev_active_id = self.layout.active_frame_id(output);
        let target = self.layout.strip_mut(output).and_then(|s| {
            s.gesture_end(working_w);
            s.active_frame_id()
        });
        self.reconcile_active_for_output(output, prev_active_id);
        // A swipe that snaps back changes nothing; don't refocus.
        let target = target.filter(|&id| Some(id) != prev_active_id);
        if let Some(id) = target {
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
        self.queue_redraw(output);
    }

    /// Focus a frame reached with the pointer; Emacs then anchors mouse-follows-focus.
    fn set_focus_frame_from_pointer(
        &mut self,
        frame_surface_id: u64,
        source: &str,
        pos_in_frame: Point<f64, Logical>,
    ) {
        if let Some(focus_id) = self.set_focus_frame(frame_surface_id, source, false) {
            self.queue_event(Event::Focus {
                focus_id,
                x: Some(pos_in_frame.x),
                y: Some(pos_in_frame.y),
                pointer: true,
            });
        }
    }

    fn refresh_mapped_cast_outputs(&mut self) {
        #[cfg(feature = "screencast")]
        {
            let live = || self.layout.entry_outputs();
            let current = &self.casting.mapped_cast_output;
            let changed = live().count() != current.len()
                || live().any(|(id, output)| current.get(&id) != Some(output));
            if changed {
                self.casting.mapped_cast_output =
                    live().map(|(id, output)| (id, output.clone())).collect();
                self.introspect_windows_dirty = true;
            }
            let sessions_to_stop: Vec<screencasting::CastSessionId> = self
                .casting
                .casts
                .iter()
                .filter_map(|cast| match cast.target {
                    dbus::CastTarget::LayoutEntry { entry_id }
                        if !self.casting.mapped_cast_output.contains_key(&entry_id) =>
                    {
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
        let outputs: Vec<Output> = self
            .layout
            .monitors()
            .iter()
            .filter(|monitor| self.surface_on_output(id, monitor.name()))
            .map(|monitor| monitor.output().clone())
            .collect();
        if outputs.is_empty() {
            return self.queue_redraw_all();
        }
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

    /// Monitors off. Redraws keep settling, the backend is skipped.
    pub fn deactivate_monitors(&mut self) {
        if self.monitors_active {
            info!("Monitors deactivated");
            self.monitors_active = false;
        }
    }

    /// Monitors on. The next queued frame re-enables each CRTC.
    pub fn activate_monitors(&mut self) {
        if !self.monitors_active {
            info!("Monitors activated");
            self.monitors_active = true;
            self.queue_redraw_all();
        }
    }

    /// Surfaces some output shows, none with the frames hidden.
    fn shown_surface_ids(&self) -> HashSet<u64> {
        if self.frames_hidden() {
            return HashSet::new();
        }
        self.layout.shown_surface_ids(self.overview.is_active())
    }

    /// Tell each placed toplevel whether some output shows it. Unplaced ones keep drawing
    /// their first buffer.
    pub fn refresh_suspended(&mut self) {
        let shown = self.shown_surface_ids();
        for (id, surface) in &mut self.surfaces {
            let suspended = surface.placed && !shown.contains(id);
            if surface.suspended == suspended {
                continue;
            }
            surface.suspended = suspended;
            let Some(toplevel) = surface.window.toplevel() else {
                continue;
            };
            toplevel.with_pending_state(|state| {
                if suspended {
                    state.states.set(XdgToplevelState::Suspended);
                } else {
                    state.states.unset(XdgToplevelState::Suspended);
                }
            });
            toplevel.send_pending_configure();
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
            if went_idle && !state.ewm.monitors_active {
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
    /// Combines four sources of idle inhibition:
    /// 1. **Manual toggle**: `ewm-idle-inhibit-mode` in Emacs.
    /// 2. **FDO ScreenSaver D-Bus**: Firefox, Chrome, and other apps use
    ///    `org.freedesktop.ScreenSaver.Inhibit()` to prevent screensaver.
    /// 3. **Wayland idle-inhibit protocol**: Video players create `zwp_idle_inhibitor_v1` on their
    ///    surface.
    /// 4. **Fullscreen heuristic**: A fullscreen surface that is currently focused is treated as an
    ///    idle inhibitor (covers video players and games that don't implement the protocol).
    ///
    /// For Wayland protocol inhibitors, only visible surfaces count
    /// (checked via `surface_primary_scanout_output`). This prevents a
    /// minimized/hidden app from keeping the screen awake.
    ///
    /// While the session is locked only the lock surface is visible, so only its
    /// own protocol inhibitor counts and the other sources wait for the unlock.
    ///
    /// The result is propagated to both the ext-idle-notify protocol (for
    /// swayidle) and the native idle timeout (monitor blanking/command).
    pub fn refresh_idle_inhibit(&mut self) {
        use smithay::wayland::compositor::with_states;

        // Clean up dead surfaces
        self.idle_inhibiting_surfaces.retain(|s| s.is_alive());

        let is_inhibited = (!self.is_locked()
            && (self.manual_idle_inhibited
                || self
                    .is_fdo_idle_inhibited
                    .load(std::sync::atomic::Ordering::SeqCst)))
            || self.fullscreen_surface_inhibits_idle()
            || self.idle_inhibiting_surfaces.iter().any(|surface| {
                with_states(surface, |states| {
                    surface_primary_scanout_output(surface, states).is_some()
                })
            });

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
        !self.is_locked() && !self.frames_hidden() && self.layout.any_active_fullscreen()
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
    fn next_queued_redraw(&self, prefer: Option<&Output>) -> Option<Output> {
        let needs_redraw = |s: &OutputState| s.redraw_state.is_render_queued();
        if let Some(prefer) = prefer
            && needs_redraw(self.output_state.get(prefer)?)
        {
            return Some(prefer.clone());
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
        if let Some(monitor) = self.layout.monitor_of_mut(output) {
            monitor.advance();
        }
        for surface in self.surfaces.values_mut() {
            surface.advance();
        }
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

        self.animations_clock
            .set_presentation_time(target_presentation_time);

        // Sweep finished animations so render-path flag reflects live ones.
        self.advance_animations(output);

        // Refresh protocol state before rendering
        self.refresh_foreign_toplevel();
        if self.output_management_state.output_heads_changed {
            let output_infos = backend.output_info_list(&self.sorted_outputs);
            self.refresh_output_management(&output_infos);
        }

        // Inactive monitors have no framebuffer and a disabled output keeps
        // its surface off, so neither shows anything. Skip the backend and
        // let them satisfy the lock as if rendered.
        let dark = !self.monitors_active || self.space.output_geometry(output).is_none();
        let res = if dark {
            backend::RenderResult::Skipped
        } else {
            backend.render(self, output, target_presentation_time)
        };

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
            if res != backend::RenderResult::Skipped || dark {
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
        let name = output.name();
        let any_fade = self
            .surfaces
            .iter()
            .any(|(&id, surface)| surface.is_animating() && self.surface_on_output(id, &name));
        let layout_animating = self
            .layout
            .monitor_of(output)
            .is_some_and(Monitor::animations_ongoing);
        if let Some(state) = self.output_state.get_mut(output)
            && (any_fade || layout_animating)
        {
            state.unfinished_animations_remain = true;
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
        if res != backend::RenderResult::Skipped || dark {
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
        for (_, window) in self.windows() {
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
        for (_, window) in self.windows() {
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
        for (_, window) in self.windows() {
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

        for (_, window) in self.windows() {
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
        let shown = self.shown_surface_ids();
        self.layout
            .all_frames()
            .map(|frame| frame.surface_id)
            .filter(|id| !shown.contains(id))
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

        for (id, window) in self.windows() {
            if hidden_emacs_ids.contains(&id) {
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

        let focused = self.focused_surface_id();
        let windows: Vec<WindowInfo> = self
            .surfaces
            .iter()
            .filter_map(|(&id, surface)| {
                let wl_surface = surface.window.wl_surface()?.into_owned();
                let output = self.find_surface_output(id);
                Some(WindowInfo {
                    id,
                    surface: wl_surface,
                    title: if surface.title.is_empty() {
                        None
                    } else {
                        Some(surface.title.clone())
                    },
                    app_id: Some(surface.app_id.clone()),
                    output,
                    is_focused: focused == id,
                    is_fullscreen: surface.fullscreen,
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

    /// The monitor showing surface `id`: its focused view's, else the first in
    /// output order, else the one holding it as an Emacs frame.
    fn surface_monitor(&self, id: u64) -> Option<&Monitor> {
        if let Some(location) = self.focused_entry_for_surface(id) {
            return Some(location.monitor);
        }
        let outputs = &self.surfaces.get(&id)?.outputs;
        self.mapped_monitors()
            .find(|monitor| outputs.contains(monitor.name()))
    }

    /// The output showing surface `id`, owned.
    pub(crate) fn find_surface_output(&self, id: u64) -> Option<Output> {
        self.surface_monitor(id)
            .map(|monitor| monitor.output().clone())
    }

    /// Set the Emacs process PID for client identification
    pub fn set_emacs_pid(&mut self, pid: u32) {
        info!("Tracking Emacs PID: {}", pid);
        self.emacs_pid = Some(pid);
    }

    /// Check if focus is on an Emacs surface (for key interception decisions)
    pub fn is_focus_on_emacs(&self) -> bool {
        self.layout.has_frame(self.focused_surface_id())
    }

    pub(crate) fn is_keyboard_focus_on_emacs(&self) -> bool {
        self.keyboard_focus
            .as_ref()
            .is_some_and(|surface| self.is_emacs_frame_surface(surface))
    }

    pub(crate) fn is_emacs_frame_surface(&self, surface: &WlSurface) -> bool {
        self.surface_id(surface)
            .is_some_and(|id| self.layout.has_frame(id))
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

    pub(crate) fn start_repeat(&mut self, repeat: InterceptRepeat) {
        self.im_repeat.start_repeat(&self.loop_handle, repeat);
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

    /// The focused Emacs frame's surface, for keyboard redirects.
    pub(crate) fn focused_emacs_frame_surface(&self) -> Option<WlSurface> {
        let id = self.focused_frame_id;
        if id == 0 || !self.layout.has_frame(id) {
            return None;
        }
        self.window(id)?
            .wl_surface()
            .map(|surface| surface.into_owned())
    }

    fn emacs_keyboard_redirect_focus(&self) -> Option<WlSurface> {
        if !self.im_repeat.has_keyboard_redirect_hold() && !module::get_keyboard_capture() {
            return None;
        }

        self.focused_emacs_frame_surface().or_else(|| {
            self.keyboard_focus
                .as_ref()
                .filter(|surface| self.is_emacs_frame_surface(surface))
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
            if self.layout.has_frame(frame_id) {
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
        if self.layout.has_frame(id) {
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
        entry_id.is_some() || (frame_id != 0 && self.layout.has_frame(frame_id))
    }

    /// Focus the surface, or ask Emacs to show it again when no window does.
    pub fn activate_or_defer_surface(&mut self, id: u64, source: &str) {
        let focused = self.set_focus(id, true, source);
        if focused {
            self.layer_shell_on_demand_focus = None;
        }
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        surface.pending_activation = (!focused).then(Instant::now);
        // Mapped asks Emacs to show an unplaced surface.
        if !focused && surface.placed {
            self.queue_event(Event::ActivateSurface { id });
        }
    }

    fn focus_pending_activation_surfaces(&mut self) {
        for surface in self.surfaces.values_mut() {
            if !surface.activation_pending() {
                surface.pending_activation = None;
            }
        }
        let mut ready: Vec<u64> = self
            .pending_activations()
            .filter(|&id| self.focus_target_ready(id))
            .collect();
        ready.sort_unstable();

        for id in ready {
            if let Some(surface) = self.surfaces.get_mut(&id) {
                surface.pending_activation = None;
            }
            self.set_focus(id, true, "xdg_activation_pending");
            self.layer_shell_on_demand_focus = None;
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
            let frame_id = self.set_focus_entry(entry_id, source, false)?;
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
        if let Some(focus_id) = entry_id.or_else(|| self.layout.focus_id_of_frame(frame_id)) {
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
            if focused == 0 || !self.surfaces.contains_key(&focused) {
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
        if self.layout.has_frame(id) {
            self.discard_pending_title_event(id);
            return;
        }

        // Pre-map titles feed placement rules, so they are never debounced.
        if first_title
            || app_changed
            || self
                .surfaces
                .get(&id)
                .is_some_and(|surface| !surface.mapped)
            || self.focused_surface_id() == id
        {
            self.discard_pending_title_event(id);
            self.queue_event(Event::Title { id, app, title });
            return;
        }

        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        let queued_at = surface
            .pending_title
            .as_ref()
            .map_or_else(Instant::now, |pending| pending.queued_at);
        surface.pending_title = Some(surface::PendingTitle {
            app,
            title,
            queued_at,
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

    /// Drop the flush timer once no title is pending.
    fn cancel_title_flush_timer(&mut self) {
        let Some(token) = self.title_flush_timer_token else {
            return;
        };
        if self
            .surfaces
            .values()
            .any(|surface| surface.pending_title.is_some())
        {
            return;
        }
        self.title_flush_timer_token = None;
        self.loop_handle.remove(token);
    }

    fn discard_pending_title_event(&mut self, id: u64) {
        if let Some(surface) = self.surfaces.get_mut(&id) {
            surface.pending_title = None;
        }
        self.cancel_title_flush_timer();
    }

    fn flush_pending_title_event(&mut self, id: u64) {
        if let Some(pending) = self
            .surfaces
            .get_mut(&id)
            .and_then(|surface| surface.pending_title.take())
        {
            self.queue_event(Event::Title {
                id,
                app: pending.app,
                title: pending.title,
            });
        }
        self.cancel_title_flush_timer();
    }

    fn flush_pending_title_events(&mut self) {
        let mut pending: Vec<(u64, surface::PendingTitle)> = self
            .surfaces
            .iter_mut()
            .filter_map(|(&id, surface)| Some((id, surface.pending_title.take()?)))
            .collect();
        pending.sort_by_key(|(id, _)| *id);
        for (id, pending) in pending {
            self.queue_event(Event::Title {
                id,
                app: pending.app,
                title: pending.title,
            });
        }
        self.cancel_title_flush_timer();
    }

    fn on_title_flush_timer(&mut self) {
        self.title_flush_timer_token = None;

        let Some(oldest) = self
            .surfaces
            .values()
            .filter_map(|surface| Some(surface.pending_title.as_ref()?.queued_at))
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

    /// Update text_input focus for input method support.
    ///
    /// With Emacs as the provider, Emacs surfaces clear text_input like None
    /// does (leave + set_focus(None)), causing the client to disable. Commits
    /// that arrive during the resulting disable->enable gap are queued in
    /// `im_text_input` and drained on the next `ImEvent::Activated` for the
    /// same surface. An external input method serves Emacs frames too.
    pub fn update_text_input_focus(&self, surface: Option<&WlSurface>) {
        let surface = match self.text_input_provider {
            im::TextInputProvider::Emacs => {
                surface.filter(|surface| !self.is_emacs_frame_surface(surface))
            }
            im::TextInputProvider::External => surface,
        };
        im::text_input::update_focus(&self.seat, surface);
    }

    fn active_text_input_surface_id(&self) -> Option<u64> {
        im::text_input::active_surface_id(&self.seat, |surface| self.surface_id(surface))
    }

    fn commit_active_text_input(&self, text: String) {
        im::text_input::commit_active(&self.seat, text)
    }

    fn preedit_active_text_input(&self, text: String) {
        im::text_input::preedit_active(&self.seat, text)
    }

    fn replace_active_text_input(&self, before: u32, after: u32, text: String) -> bool {
        im::text_input::replace_active(&self.seat, before, after, text)
    }

    fn deliver_replace_ack(&mut self, surface_id: u64, r: im::text_input::Replacement) {
        if self.replace_active_text_input(r.before, r.after, r.text) {
            self.queue_event(Event::TextInputReplaced { surface_id });
        }
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
                    || grab.is::<input::frame_click_grab::FrameClickGrab>()
                    || grab.is::<input::floating_move_grab::MoveGrab>()
                    || grab.is::<input::floating_resize_grab::ResizeGrab>()
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
            && !self.is_animating_at(pos)
        {
            self.focus_follows_mouse(&under, pos);
        }

        (under, focus_changed)
    }

    /// The output showing the focused frame.
    pub(crate) fn focused_output(&self) -> Option<&Output> {
        self.layout
            .frame_location(self.focused_frame_id)
            .map(|location| location.monitor.output())
    }

    pub(crate) fn output_under_cursor(&self) -> Option<&Output> {
        self.output_containing(self.cursor_location().into())
    }

    /// Output for surface placement and keyboard nav: focused, else under the cursor, else first.
    pub fn active_output(&self) -> Option<&Output> {
        self.focused_output()
            .or_else(|| self.output_under_cursor())
            .or_else(|| self.first_output())
    }

    /// The mapped output called `name`, owned.
    pub(crate) fn find_mapped_output(&self, name: &str) -> Option<Output> {
        self.layout.output(name).cloned()
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
        let Some(cur) = self.active_output().cloned() else {
            return;
        };
        let target = self.output_in_direction(&cur, dx, dy);
        let Some(target) = target else { return };
        let Some(id) = self.layout.active_frame_id(&target) else {
            return;
        };
        self.set_focus_frame(id, "focus_output_direction", true);
    }

    /// Focus strip frame `pick(anchor)`, anchored at the focused frame, else at the shown one.
    pub(crate) fn focus_strip_frame(&mut self, pick: impl FnOnce(usize) -> Option<usize>) {
        let focused = self.focused_frame_id;
        let target = self.layout.frame_location(focused).and_then(|location| {
            let strip = &location.monitor.strip;
            let anchor = if location.floating {
                Some(strip.active_idx())
            } else {
                strip.index_of(focused)
            };
            strip.frame_ids().nth(pick(anchor?)?)
        });
        if let Some(id) = target {
            self.set_focus_frame(id, "action", true);
        }
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
        self.mapped_monitors()
            .filter_map(|monitor| {
                let geo = self.space.output_geometry(monitor.output())?;
                let working_area = monitor.working_area;
                Some(module::ActiveOutput {
                    name: monitor.name().to_string(),
                    origin: (
                        geo.loc.x + working_area.loc.x,
                        geo.loc.y + working_area.loc.y,
                    ),
                })
            })
            .collect()
    }

    /// Map `output` at `position` and give it a monitor.
    pub(crate) fn map_output(&mut self, output: &Output, position: Point<i32, Logical>) {
        self.space.map_output(output, position);
        output.change_current_state(None, None, None, Some(position));
        let working_area = self.get_working_area(output);
        self.layout.add_output(output.clone(), working_area);
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

        self.queue_event(Event::OutputDetected(info));

        // Send initial working area (full output initially, before any panels)
        let working_area = self.get_working_area(output);
        self.queue_event(Event::WorkingArea {
            output: output_name.clone(),
            x: working_area.loc.x,
            y: working_area.loc.y,
            width: working_area.size.w,
            height: working_area.size.h,
        });

        if self.space.output_geometry(output).is_some() {
            self.attach_output(output, "output_added");
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
        if let Some(strip) = self.layout.strip_mut(output) {
            strip.update_config(current_scale);
        }

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
        let working_area_changed = self.check_working_area_change(output);
        if !working_area_changed {
            // Always refresh Emacs frames: the output position may have changed
            // even when the working area (relative to the output) didn't.
            self.update_frames_for_working_area(output);
        }
        self.refresh_primary_assignments();
        self.queue_redraw_all();
        if became_mapped {
            self.attach_output(output, "output_enabled");
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
        // Enabled by config, nothing else asks Emacs for the frames it lacks.
        if became_mapped {
            self.queue_event(Event::OutputsComplete);
        }

        self.output_management_state.output_heads_changed = true;
    }

    /// Send `output`'s scale and transform to every window and layer surface on it.
    pub fn send_scale_transform_to_output_surfaces(&self, output: &Output) {
        let scale = output.current_scale();
        let transform = output.current_transform();

        let name = output.name();
        for surface in self.surfaces.values() {
            if surface.outputs.contains(&name) {
                crate::utils::set_preferred_scale_transform(&surface.window, output);
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

    /// Update Emacs frames to fit within the working area of an output.
    /// Called when layer surface exclusive zones change.
    pub fn update_frames_for_working_area(&mut self, output: &Output) {
        let working_area = self.get_working_area(output);

        for id in self.layout.strip_frame_ids(output) {
            let Some(window) = self.window(id) else {
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

        if let Some(space) = self.layout.floating_mut(output) {
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

        let Some(monitor) = self.layout.monitor_of_mut(output) else {
            return false;
        };
        let changed = mem::replace(&mut monitor.working_area, working_area) != working_area;
        monitor.set_view(working_area);
        monitor.resized |= changed;

        if changed {
            info!(
                "Working area for {} changed: {}x{}+{}+{}",
                output_name,
                working_area.size.w,
                working_area.size.h,
                working_area.loc.x,
                working_area.loc.y
            );

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
        self.layout
            .monitors()
            .iter()
            .map(|monitor| {
                let rect = monitor.working_area;
                crate::event::WorkingAreaInfo {
                    output: monitor.name().to_string(),
                    x: rect.loc.x,
                    y: rect.loc.y,
                    width: rect.size.w,
                    height: rect.size.h,
                }
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
        let surfaces: HashMap<u64, serde_json::Value> = self
            .surfaces
            .iter()
            .map(|(&id, surface)| {
                let info = serde_json::json!({ "app_id": surface.app_id, "title": surface.title });
                (id, info)
            })
            .collect();
        let (mut urgent_workspaces, mut urgent_surfaces): (Vec<u64>, Vec<u64>) = self
            .surfaces
            .iter()
            .filter(|(_, surface)| surface.urgent)
            .map(|(&id, _)| id)
            .partition(|&id| self.layout.has_frame(id));
        urgent_surfaces.sort_unstable();
        urgent_workspaces.sort_unstable();
        let emacs_surfaces = self.layout.frame_outputs_snapshot();
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
            "surfaces": surfaces,
            "emacs_surfaces": emacs_surfaces,
            "output_strips": self.layout.strips().collect::<HashMap<_, _>>(),
            "overview": {
                "open": self.overview.is_open(),
                "progress": self.overview.progress(),
                "zoom": self.overview.zoom(),
            },
            "homeless_frames": &self.layout.homeless().tiled,
            "homeless_floating_frames": &self.layout.homeless().floating,
            "floating_spaces": self
                .layout
                .monitors()
                .iter()
                .map(|monitor| (monitor.name(), &monitor.floating))
                .collect::<HashMap<_, _>>(),
            "surface_outputs": self.surface_outputs(),
            "focused_surface_id": self.focused_surface_id(),
            "focused_frame_id": self.focused_frame_id,
            "focused_entry_id": self.focused_entry_id,
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
            "intercepted_keys": *module::intercepted_keys().read().unwrap(),
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

    /// Tiled host for an app surface: parent's frame, else focused, else active, never a float.
    fn resolve_app_host_frame(
        &self,
        parent_id: Option<u64>,
        output: Option<&Output>,
    ) -> Option<u64> {
        let tiled_frame_on = |id: u64, output: Option<&Output>| {
            self.layout
                .frame_location(id)
                .filter(|location| !location.floating)
                .filter(|location| output.is_none_or(|o| location.monitor.output() == o))
                .map(|_| id)
        };
        parent_id
            .and_then(|pid| self.frame_entry_for_surface(pid))
            .and_then(|(id, _)| tiled_frame_on(id, None))
            .or_else(|| output.and_then(|o| tiled_frame_on(self.focused_frame_id, Some(o))))
            .or_else(|| output.and_then(|o| self.layout.active_frame_id(o)))
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
                // Let Emacs drive the size; the frame fits itself to its content.
                state.size = None;
            } else {
                state.size = Some(self.output_size);
                state.states.set(XdgToplevelState::Maximized);
            }
            state.states.set(XdgToplevelState::Activated);
        });
        surface.send_configure();

        let record = surface::SurfaceState::new(id, surface, is_emacs, app.clone());
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
            layout::enter_output_for_window(&window, output);
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

        // Auto-focus the first Emacs surface so keyboard input works without
        // a pointer event. Keyboard.enter is replayed on Initialized command;
        // PGTK drops enters sent before its surface's first roundtrip.
        let auto_focus = is_emacs && self.focused_surface_id() == 0;
        if auto_focus {
            self.set_focus_frame(id, "initial_surface", false);
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
        // Sent after New so the lisp side has assigned ewm-surface-id by the
        // time the Focus event is processed.
        if auto_focus {
            info!("Auto-focusing initial Emacs surface {}", id);
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
        let was_focused = self.focused_frame_id == id;
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
            let refocus = output
                .as_ref()
                .and_then(|output| self.layout.active_frame_id(output))
                .or_else(|| {
                    self.pointer_output
                        .as_ref()
                        .and_then(|output| self.layout.active_frame_id(output))
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

    fn check_surface_info_changes(&mut self, wl_surface: &WlSurface) {
        let Some(id) = self.surface_id(wl_surface) else {
            return;
        };

        let (app_id, title) = smithay::wayland::compositor::with_states(wl_surface, |states| {
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

        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        let changed = surface.app_id != app_id || surface.title != title;
        if !changed || (app_id.is_empty() && title.is_empty()) {
            return;
        }
        let first_title = !title.is_empty() && surface.title.is_empty();
        let app_changed = surface.app_id != app_id;
        if module::DEBUG_MODE.load(std::sync::atomic::Ordering::Relaxed) {
            debug!(
                "Surface {} info changed: app='{}' title='{}'",
                id, app_id, title
            );
        }
        surface.app_id = app_id.clone();
        surface.title = title.clone();
        self.introspect_windows_dirty = true;
        self.queue_title_update(id, app_id, title, first_title, app_changed);
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
        self.output_under_cursor().or_else(|| self.first_output())
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
    /// Set surface `id` fullscreen or back.
    pub(crate) fn set_surface_fullscreen(&mut self, id: u64, fullscreen: bool) {
        self.ewm.set_surface_fullscreen(id, fullscreen);
        self.refresh_pointer_after_layout();
    }

    /// A client asked for fullscreen `on`. Every request is answered with a configure.
    pub(crate) fn request_fullscreen(&mut self, surface: &WlSurface, on: bool) {
        let Some(id) = self.ewm.surface_id(surface) else {
            return;
        };
        let changed = self.ewm.pending_fullscreen(id) != on;
        self.set_surface_fullscreen(id, on);
        if !changed && let Some(toplevel) = self.ewm.window(id).and_then(|window| window.toplevel())
        {
            toplevel.send_configure();
        }
    }

    /// Warp the pointer to an absolute position, notifying clients.
    /// Does NOT activate pointer constraints (programmatic warps from Emacs
    /// during layout changes should not trap the pointer).
    pub fn warp_pointer(&mut self, x: f64, y: f64) {
        let pointer = self.ewm.pointer.clone();
        // A grab or constraint owns the pointer; a warp would teleport it.
        if pointer.is_grabbed() || self.active_constraint_for_focus(&pointer).is_some() {
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

    pub(crate) fn refresh_pointer_after_layout(&mut self) {
        self.refresh_pointer_after_scene_change(false);
    }

    fn refresh_pointer_after_scene_change(&mut self, skip_strip_animation: bool) -> bool {
        if self.ewm.grab_pins_pointer_focus() {
            return false;
        }
        let (x, y) = self.ewm.pointer_location();
        let pos: Point<f64, Logical> = (x, y).into();

        if skip_strip_animation && !self.ewm.is_locked() && self.ewm.is_animating_at(pos) {
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

        // A popup grab owns the keyboard until refresh_popup_grab clears it
        if self.ewm.popup_grab.is_some() {
            return;
        }

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
            ewm.window(id)
                .and_then(|w| w.wl_surface())
                .map(|s| s.into_owned())
        });

        if self.ewm.keyboard_focus != new_focus {
            self.flush_deferred_keys(new_focus.as_ref());
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
        self.tap_key(keycode, (ctrl, alt, shift, logo));
    }

    /// Deliver `repeat` once, on the press and again on each repeat tick.
    fn deliver_intercept(&mut self, repeat: InterceptRepeat) {
        match repeat {
            InterceptRepeat::TextInput(key) => self.ewm.queue_event(Event::Key(key)),
            InterceptRepeat::Command { key, .. } => {
                self.ewm.queue_event(Event::InterceptedCommand { key })
            }
            InterceptRepeat::Action { action, .. } => self.do_action(action),
        }
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

        if matches!(repeat, InterceptRepeat::Command { .. }) && module::get_keyboard_capture() {
            self.ewm.cancel_command_repeat();
            return;
        }
        self.deliver_intercept(repeat);
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
        self.ewm.refresh_suspended();

        // Refresh workspace protocol state (pull model: diff source of truth vs mirrors).
        crate::protocols::workspace::refresh::<State>(
            &mut self.ewm.workspace_state,
            &self.ewm.layout,
            &|id| {
                self.ewm
                    .surfaces
                    .get(&id)
                    .is_some_and(|surface| surface.urgent)
            },
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
        let focused = self.ewm.focused_output().cloned();
        let mut rendered_focused = false;

        while let Some(output) = self.ewm.next_queued_redraw(focused.as_ref()) {
            let is_focused = focused.as_ref() == Some(&output);

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

        self.update_shared_state();
        self.ewm.flush_introspect_windows();

        // Flush Wayland clients
        if let Err(e) = self.ewm.display_handle.flush_clients() {
            tracing::warn!("Failed to flush Wayland clients: {e}");
        }

        // Clear the time so it's fetched afresh next iteration.
        self.ewm.animations_clock.clear();
        self.ewm.notified_activity_this_iteration = false;
    }

    /// Publish this tick's state to Emacs, then its events.
    fn update_shared_state(&self) {
        {
            let mut shared = module::shared_state().lock().unwrap();
            shared.focused_surface_id = self.ewm.focused_surface_id();
            shared.focused_frame_id = self.ewm.focused_frame_id;
            shared.pointer_location = self.ewm.pointer_location();
            shared.active_outputs = self.ewm.active_outputs_snapshot();
            shared.frame_origins = self.ewm.frame_origins_snapshot();
            shared.active_frames = self.ewm.layout.active_frames_snapshot();
            shared.strip_frames = self.ewm.layout.strip_frames_snapshot();
            shared.frame_outputs = self.ewm.layout.frame_outputs_snapshot();
        }
        module::flush_events();
    }

    /// Handle a module command (from Emacs via dynamic module).
    pub(crate) fn handle_module_command(&mut self, cmd: module::ModuleCommand) {
        tracy_span!("handle_module_command");

        use module::ModuleCommand;
        match cmd {
            ModuleCommand::Close { id } => {
                if let Some(window) = self.ewm.window(id)
                    && let Some(toplevel) = window.toplevel()
                {
                    toplevel.send_close();
                    info!("Close surface {} (sent close request)", id);
                }
            }
            ModuleCommand::Action { action } => self.do_action(action),
            ModuleCommand::ReplayDeferredKeys => self.replay_deferred_keys(),
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
                if self.ewm.layout.entry_location(focus_id).is_some() {
                    self.ewm.set_focus_entry(focus_id, "lisp_request", false);
                } else if let Some(frame_id) = self.ewm.layout.frame_with_focus_id(focus_id) {
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
            ModuleCommand::ImPreedit { text, surface_id } => {
                if self.ewm.active_text_input_surface_id() == Some(surface_id) {
                    self.ewm.preedit_active_text_input(text);
                }
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
            ModuleCommand::SendKey { surface_id, keys } => {
                if surface_id.is_none_or(|id| id == self.ewm.focused_surface_id()) {
                    for key in keys {
                        let Some(code) = key
                            .key
                            .keysym()
                            .and_then(|k| self.ewm.keyboard.keycode_for_keysym(k))
                        else {
                            continue;
                        };
                        let mods = (key.ctrl, key.alt, key.shift, key.logo);
                        self.tap_key(code.raw(), mods);
                    }
                }
            }
            ModuleCommand::TextInputIntercept { enabled } => {
                if self.ewm.text_input_intercept != enabled {
                    info!("Text input intercept: {}", enabled);
                    self.ewm.text_input_intercept = enabled;
                }
            }
            ModuleCommand::ConfigureTextInput { provider } => {
                self.set_text_input_provider(provider);
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
            ModuleCommand::Layout { frames } => {
                let dropped = strip::dropped_frame_ids(self.ewm.layout.placed_frames(), &frames);
                self.store_close_snapshots(&dropped);
                self.apply_layout(frames);
                self.refresh_pointer_after_layout();
            }
            ModuleCommand::MoveFrame { id, delta } => {
                self.ewm.move_frame(id, delta);
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
                if let Some(id) = self.ewm.layout.entry_surface_id(entry_id) {
                    let on = !self.ewm.pending_fullscreen(id);
                    self.set_surface_fullscreen(id, on);
                }
            }
            ModuleCommand::SetFullscreen { entry_id, on } => {
                if let Some(id) = self.ewm.layout.entry_surface_id(entry_id) {
                    self.set_surface_fullscreen(id, on);
                }
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
                for mut device in self.ewm.libinput_devices.iter().cloned() {
                    input::apply_libinput_settings(&mut device, &self.ewm.input_configs);
                }

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
            ModuleCommand::ConfigureOverview {
                zoom,
                backdrop_color,
            } => {
                self.ewm.overview.set_zoom(zoom);
                self.ewm.overview.backdrop_color = backdrop_color;
                self.ewm.queue_redraw_all();
            }
            ModuleCommand::ConfigureBackground { color } => {
                self.ewm.overview.background_color = color;
                self.ewm.queue_redraw_all();
            }
            ModuleCommand::SetIdleInhibited { inhibited } => {
                self.ewm.manual_idle_inhibited = inhibited;
            }
            ModuleCommand::PowerOffMonitors => {
                self.ewm.deactivate_monitors();
                self.backend.clear_all_surfaces();
            }
            ModuleCommand::PowerOnMonitors => self.ewm.activate_monitors(),

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

    /// Hand the seat's input method slot to the relay or to an external client.
    fn set_text_input_provider(&mut self, provider: im::TextInputProvider) {
        self.ewm.text_input_provider = provider;
        match provider {
            im::TextInputProvider::Emacs => self.ewm.connect_im_relay(),
            im::TextInputProvider::External => {
                self.ewm.disconnect_im_relay();
                self.handle_im_event(im::relay::ImEvent::Deactivated);
            }
        }
        // Emacs frames join or leave text-input focus with the provider.
        self.ewm
            .update_text_input_focus(self.ewm.keyboard_focus.as_ref());
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
mod tests;

// Emacs dynamic module initialization
emacs::plugin_is_GPL_compatible! {}

#[emacs::module(name = "ewm-core", defun_prefix = "ewm", mod_in_name = false)]
fn init(_: &emacs::Env) -> emacs::Result<()> {
    Ok(())
}
