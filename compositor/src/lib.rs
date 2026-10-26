//! EWM - Emacs Wayland Manager
//!
//! Wayland compositor core library.
//!
//! # Design Invariants
//!
//! 1. **Surfaces and views are distinct**: A Wayland surface is a client-owned object, a view is
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
//! 5. **Emacs owns policy, Rust preserves invariants**: Emacs decides layout and focus intent. Rust
//!    applies those decisions, handles Wayland protocol state, and repairs local state when async
//!    ordering would otherwise break invariants.
//!
//! 6. **Rendering state is per-output**: Each output advances independently through its redraw
//!    state machine.

pub mod animation;
pub mod backend;
pub mod config;
pub mod cursor;
#[cfg(feature = "screencast")]
pub mod dbus;
pub mod dbus_login1;
pub mod dbus_screensaver;
pub mod event;
mod focus;
pub mod frame_clock;
pub mod gtk;
pub mod handlers;
mod idle;
pub mod im;
pub mod input;
pub mod layout;
mod module;
mod output;
pub mod output_mode;
pub mod protocols;
pub mod render;
mod render_helpers;
pub mod rubber_band;
#[cfg(feature = "screencast")]
pub mod screencasting;
pub mod surface;
mod title;
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

/// Whether `connector_name` is a built-in laptop panel: eDP, LVDS or DSI.
pub fn is_laptop_panel(connector_name: &str) -> bool {
    matches!(connector_name.get(..4), Some("eDP-" | "LVDS" | "DSI-"))
}

use crate::handlers::XDG_ACTIVATION_TOKEN_TIMEOUT;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem;
use std::os::unix::io::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub use backend::{Backend, DrmBackendState, HeadlessBackend};
pub use event::{Event, OutputInfo, OutputMode};
use layout::overview;
use render_helpers::floating_shadow;

pub use focus::{FocusSource, Notify};
pub use handlers::{LockRenderState, LockState};
pub use idle::{CursorHideState, IdleAction, IdleTimeoutState};
pub use input::binding::{Action, InterceptDispatch, InterceptedKey, Trigger};
pub use layout::entry::{FocusId, LayoutEntry, LayoutEntryId, LayoutEntrySurface};
pub use layout::{Layout, Monitor, Placement};
use im::repeat::{HeldKeyOwner, InterceptRepeat, InterceptRepeatState};
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
use smithay::input::keyboard::KeyboardHandle;
use smithay::input::pointer::{
    ClickGrab, CursorImageStatus, CursorImageSurfaceData, PointerHandle,
};
use smithay::input::{Seat, SeatState};
use smithay::output::{Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::Timer;
use smithay::reexports::calloop::{
    Interest, LoopHandle, LoopSignal, Mode as CalloopMode, PostAction, RegistrationToken,
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState;
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeDefaultMode;
use smithay::reexports::wayland_server::backend::{
    ClientData, ClientId, Credentials, DisconnectReason, GlobalId,
};
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
use smithay::wayland::pointer_gestures::PointerGesturesState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::data_device::{
    request_data_device_client_selection, set_data_device_selection,
    DataDeviceState, };
use smithay::wayland::selection::ext_data_control::DataControlState as ExtDataControlState;
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionState,
};
use smithay::wayland::selection::wlr_data_control::DataControlState as WlrDataControlState;
use smithay::wayland::session_lock::{
    LockSurface, SessionLockManagerState,
};
use smithay::wayland::shell::kde::decoration::{KdeDecorationState};
use smithay::wayland::shell::wlr_layer::{Layer, WlrLayerShellState};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationState};
use smithay::wayland::shell::xdg::XdgShellState;
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
use tracing::{debug, info, warn};

use crate::protocols::foreign_toplevel::{ForeignToplevelManagerState, WindowInfo};
use crate::protocols::output_management::OutputManagementState;
use crate::protocols::screencopy::ScreencopyManagerState;
use crate::protocols::virtual_pointer::VirtualPointerManagerState;
use crate::protocols::workspace::WorkspaceManagerState;

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

/// Desired output configuration (from Emacs).
/// Stored per output name. Looked up on connect and config changes.
#[derive(Debug, Clone)]
pub struct OutputConfig {
    /// Desired video mode (None = use preferred/auto)
    pub mode: Option<output_mode::Mode>,
    /// Explicit DRM modeline. Takes precedence over `mode` when set
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
    /// The wl_output global, advertised while the output is mapped.
    pub global: Option<GlobalId>,
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
            global: None,
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

type LayoutEntryHit<'a> = (&'a LayoutEntry, Rectangle<i32, Logical>);

pub(crate) type SurfaceHit = (WlSurface, Point<f64, Logical>);

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

/// DnD icon surface attached to the pointer during a drag operation.
#[derive(Debug)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

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
    pub xdg_decoration_state: XdgDecorationState,
    pub kde_decoration_state: KdeDecorationState,
    pub background_effect_state: BackgroundEffectState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub wlr_data_control_state: WlrDataControlState,
    pub ext_data_control_state: ExtDataControlState,
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
    pub hide: layout::transition::Transition,
    /// Every toplevel's compositor-side state, from creation to destruction.
    pub surfaces: HashMap<u64, surface::SurfaceState>,

    // Output
    pub output_size: Size<i32, Logical>,
    /// Mapped compositor outputs in deterministic spatial order.
    pub sorted_outputs: Vec<Output>,
    /// Desired output configuration, keyed by output name.
    /// Looked up when outputs connect. Updated by Emacs commands.
    pub output_config: HashMap<String, OutputConfig>,

    // Input
    /// Surface under the pointer + its global origin, for constraint checks.
    pub pointer_focus: Option<SurfaceHit>,
    /// The touchpad swipe in flight.
    pub swipe: input::Swipe,
    /// Scroll travel toward the next bound tick.
    pub scroll_trackers: input::ScrollTrackers,
    /// Last output the cursor was drawn on, for per-output cursor redraw.
    pointer_output: Option<Output>,
    pub keyboard_focus: Option<WlSurface>,
    /// Keys for Emacs waiting until it reads the Focus they followed.
    pub(crate) deferred_keys: VecDeque<input::DeferredKey>,
    pub(crate) im_repeat: InterceptRepeatState,
    /// Introspected window list changed. Announce it next tick.
    pub introspect_windows_dirty: bool,
    /// Every option Emacs sets.
    pub config: config::Config,
    /// Emacs is tracking a drag source (`track-mouse' is `drag-source').
    pub emacs_drag_source: bool,
    /// UID of the layout entry currently under the pointer (for FFM view detection).
    pub pointer_entry_id: Option<LayoutEntryId>,

    /// Connected libinput devices, for re-applying configuration on change
    pub libinput_devices: std::collections::HashSet<smithay::reexports::input::Device>,

    // PID that marks Emacs-owned wl_surfaces in `new_toplevel`. Whether a surface is an Emacs
    // frame derives from strip membership.
    pub emacs_pid: Option<u32>,

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
    pub output_manager_state: OutputManagerState,

    // Text input state (provides zwp_text_input_v3 protocol)
    pub text_input_state: TextInputManagerState,

    // Input method state (provides zwp_input_method_v2 protocol)
    pub input_method_state: InputMethodManagerState,

    // Virtual keyboard state (zwp-virtual-keyboard-v1 protocol)
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

    // Active popup (menu) grab. Suppresses EWM's own focus routing while live.
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

    /// How far toward sepia the outputs are, fading to its target.
    sepia: animation::Animation,

    /// Kept awake behind a closed lid: panel off, clients suspended.
    pub unattended: bool,

    // D-Bus ScreenSaver connection (must be kept alive for the interface to work)
    pub dbus_screensaver_conn: Option<zbus::blocking::Connection>,

    // Gamma control state (wlr-gamma-control-unstable-v1 protocol)
    pub gamma_control_state: crate::protocols::gamma_control::GammaControlManagerState,

    // Fractional scale protocol (wp-fractional-scale-v1)
    pub fractional_scale_state: FractionalScaleManagerState,

    // Viewporter protocol (wp-viewporter, required for fractional scale clients)
    pub viewporter_state: ViewporterState,

    // Presentation time protocol (wp-presentation-time)
    pub presentation_state: smithay::wayland::presentation::PresentationState,

    // Relative pointer protocol (zwp-relative-pointer-v1)
    pub relative_pointer_state: RelativePointerManagerState,

    // Pointer constraints protocol (zwp-pointer-constraints-v1)
    pub pointer_constraints_state: PointerConstraintsState,

    // Pointer gestures protocol (zwp-pointer-gestures-v1)
    pub pointer_gestures_state: PointerGesturesState,

    // Cursor shape protocol (wp-cursor-shape-v1)
    pub cursor_shape_manager_state: CursorShapeManagerState,

    // Tablet protocol (zwp-tablet-v2)
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
    // on a dying client). Defer event-sending cleanup via insert_idle.
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

impl Ewm {
    pub fn new(
        display_handle: DisplayHandle,
        loop_handle: LoopHandle<'static, State>,
        is_drm: bool,
        cursor: config::Cursor,
    ) -> Self {
        let resolved = cursor.resolve();
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
        let wlr_data_control_state = WlrDataControlState::new::<State, _>(
            &display_handle,
            Some(&primary_selection_state),
            |_| true,
        );
        let ext_data_control_state = ExtDataControlState::new::<State, _>(
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

        // Initialize pointer gestures protocol (zwp-pointer-gestures-v1)
        let pointer_gestures_state = PointerGesturesState::new::<State>(&display_handle);

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
        let sepia = animation::Animation::ease(animations_clock.clone(), 0., 0., Duration::ZERO);

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
            wlr_data_control_state,
            ext_data_control_state,
            seat,
            pointer,
            keyboard,
            tablet_cursor_location: None,
            next_surface_id: 1,
            title_flush_timer_token: None,
            last_emacs_keyboard_activity: Instant::now(),
            layout: Layout::new(animations_clock.clone()),
            overview: overview::Overview::new(animations_clock.clone()),
            hide: layout::transition::Transition::new(animations_clock.clone()),
            animations_clock,
            surfaces: HashMap::new(),
            output_size: Size::from((0, 0)),
            sorted_outputs: Vec::new(),
            output_config: HashMap::new(),
            pointer_focus: None,
            pointer_output: None,
            keyboard_focus: None,
            deferred_keys: VecDeque::new(),
            im_repeat: InterceptRepeatState::new(
                input::DEFAULT_KEYBOARD_REPEAT_RATE,
                input::DEFAULT_KEYBOARD_REPEAT_DELAY,
            ),
            introspect_windows_dirty: false,
            config: config::Config {
                cursor,
                ..Default::default()
            },
            emacs_drag_source: false,
            pointer_entry_id: None,
            libinput_devices: std::collections::HashSet::new(),
            emacs_pid: None,
            swipe: input::Swipe::default(),
            scroll_trackers: input::ScrollTrackers::default(),
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
            sepia,
            unattended: false,
            dbus_screensaver_conn: None,
            gamma_control_state,
            fractional_scale_state,
            viewporter_state,
            presentation_state,
            relative_pointer_state,
            pointer_constraints_state,
            pointer_gestures_state,
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
            cursor_manager: cursor::CursorManager::new(&resolved.theme, resolved.size),
            captured_events: None,
            worker_event_tx,
        }
    }

    /// Drop windows whose surface died, refocusing when the focused one went. Returns whether any
    /// did. Output enter and leave are explicit, nothing watches spatial overlap.
    pub fn cleanup_dead_windows(&mut self) -> bool {
        let dead_ids: Vec<u64> = self
            .windows()
            .filter(|(_, window)| !window.alive())
            .map(|(id, _)| id)
            .collect();
        let removed_any = !dead_ids.is_empty();
        for id in dead_ids {
            if let Some(refocus_id) = self.handle_toplevel_destroyed_by_id(id) {
                self.set_focus_frame(refocus_id, FocusSource::CleanupDeadWindows, Notify::Emacs);
            }
        }

        self.cursor_manager.check_cursor_image_surface_alive();

        removed_any
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
        let Some(frame_surface_id) = frame_surface_id.or_else(|| self.focused_frame_id()) else {
            return false;
        };

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
        let dnd_surface = self.dnd_icon.as_ref().map(|icon| &icon.surface);

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
                self.place_pointer_surface(surface, |_| surface_pos);
                if let Some(dnd) = dnd_surface {
                    self.place_pointer_surface(dnd, |_| surface_pos);
                }
            }
            cursor_image => {
                let Some(dnd) = dnd_surface else {
                    return;
                };
                let icon = if let CursorImageStatus::Named(icon) = cursor_image {
                    *icon
                } else {
                    Default::default()
                };
                // The named cursor has a hotspot per output scale.
                self.place_pointer_surface(dnd, |output| {
                    let output_scale = output.current_scale().integer_scale();
                    let cursor = self
                        .cursor_manager
                        .get_cursor_with_name(icon, output_scale)
                        .unwrap_or_else(|| self.cursor_manager.get_default_cursor(output_scale));
                    pointer_pos.to_i32_round()
                        - cursor::XCursor::hotspot(&cursor.frames()[0]).to_logical(output_scale)
                });
            }
        }
    }

    /// Enter `surface` on the outputs it overlaps at `pos_on(output)`, leave the others, and send
    /// it the largest overlapping scale with the last transform.
    fn place_pointer_surface(
        &self,
        surface: &WlSurface,
        pos_on: impl Fn(&Output) -> Point<i32, Logical>,
    ) {
        let mut scale = 1.0;
        let mut transform = Transform::Normal;
        for output in &self.sorted_outputs {
            let Some(geo) = self.layout.output_rect(output) else {
                continue;
            };
            let surface_pos = pos_on(output);
            let bbox = bbox_from_surface_tree(surface, surface_pos);
            if let Some(mut overlap) = geo.intersection(bbox) {
                overlap.loc -= surface_pos;
                scale = f64::max(scale, output.current_scale().fractional_scale());
                transform = output.current_transform();
                output_update(output, Some(overlap), surface);
            } else {
                output_update(output, None, surface);
            }
        }
        smithay::wayland::compositor::with_states(surface, |data| {
            crate::utils::send_scale_transform(surface, data, Scale::Fractional(scale), transform);
        });
    }

    /// Surfaces some output shows, none while unattended or with the frames hidden.
    fn shown_surface_ids(&self) -> HashSet<u64> {
        if self.unattended || self.frames_hidden() {
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

    /// Redraw `output` once: advance animations, refresh the protocol mirrors, render, then send
    /// frame callbacks and serve the casts.
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
        let dark = !self.monitors_active || self.layout.output_rect(output).is_none();
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
                self.overview.is_animating() || self.hide.is_animating() || !self.sepia.is_done();
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
        // Unattended, the fallback timer alone paces every surface.
        if self.unattended {
            return;
        }

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
        // Off-screen emacs frames mustn't be woken. Their pgtk client would
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
                    is_focused: focused == Some(id),
                    is_fullscreen: surface.fullscreen,
                })
            })
            .collect();

        self.foreign_toplevel_state.refresh::<State>(windows);
    }

    /// Set the Emacs process PID for client identification
    pub fn set_emacs_pid(&mut self, pid: u32) {
        info!("Tracking Emacs PID: {}", pid);
        self.emacs_pid = Some(pid);
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
        let output_geo = self.layout.output_rect(output)?;
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
        let output_geo = output.and_then(|o| self.layout.output_rect(o));
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

    /// Click, move and resize grabs pin delivery. DnD and popup grabs hit-test.
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

        // Don't retarget keyboard focus while grabbed. The grab owns it.
        if !grabbed
            && self.config.focus_follows_mouse
            && (focus_changed || entry_changed)
            && !self.is_animating_at(pos)
        {
            self.focus_follows_mouse(&under, pos);
        }

        (under, focus_changed)
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
            "focused_frame_id": self.focused_frame_id(),
            "focused_entry_id": self.focused_entry_id(),
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

    pub fn init_wayland_listener(
        display: Display<State>,
        event_loop: &LoopHandle<State>,
    ) -> anyhow::Result<std::ffi::OsString> {
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
        // A grab or constraint owns the pointer. A warp would teleport it.
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
        let old_geo = output.as_ref().and_then(|o| self.ewm.layout.output_rect(o));

        self.backend.apply_output_config(&mut self.ewm, output_name);

        if let (Some(output), Some(old_geo)) = (&output, old_geo) {
            let point = Point::from((pos.x as i32, pos.y as i32));
            if old_geo.contains(point)
                && let Some(new_geo) = self.ewm.layout.output_rect(output)
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
        let Some(geo) = self.ewm.layout.output_rect(&output) else {
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
            ewm.window(ewm.focused_surface_id()?)
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

    /// Deliver `repeat` once, on the press and again on each repeat tick.
    fn deliver_intercept(&mut self, repeat: InterceptRepeat) {
        match repeat {
            InterceptRepeat::TextInput(key) => self.ewm.queue_event(Event::Key(key)),
            InterceptRepeat::Bound { bound, .. } => self.run_bound(bound),
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

        if repeat.owner() == HeldKeyOwner::Command && module::get_keyboard_capture() {
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
        self.ewm.refresh_urgency();

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
            // The module defuns report 0 for no focus.
            shared.focused_surface_id = self.ewm.focused_surface_id().unwrap_or(0);
            shared.focused_frame_id = self.ewm.focused_frame_id().unwrap_or(0);
            shared.pointer_location = self.ewm.pointer_location();
            shared.active_outputs = self.ewm.active_outputs_snapshot();
            shared.frame_origins = self.ewm.frame_origins_snapshot();
            shared.active_frames = self.ewm.layout.active_frames_snapshot();
            shared.strip_frames = self.ewm.layout.strip_frames_snapshot();
            shared.frame_outputs = self.ewm.layout.frame_outputs_snapshot();
        }
        module::flush_events();
    }

    /// Offer `text` as the clipboard selection on behalf of Emacs.
    pub(crate) fn set_selection(&mut self, text: String) {
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
}

#[cfg(test)]
mod tests;

// Emacs dynamic module initialization
emacs::plugin_is_GPL_compatible! {}

#[emacs::module(name = "ewm-core", defun_prefix = "ewm", mod_in_name = false)]
fn init(_: &emacs::Env) -> emacs::Result<()> {
    Ok(())
}
