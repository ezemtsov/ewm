//! Emacs dynamic module interface for EWM
//!
//! # Design Invariants
//!
//! 1. **Thread safety**: The compositor runs in a separate thread from Emacs. Communication uses
//!    queues, pipes, and shared state:
//!    - `COMMAND_QUEUE`: Emacs -> Compositor commands (layouts, focus, etc.)
//!    - `EVENT_WRITER`: Compositor -> Emacs events via pipe fd (open_channel)
//!    - `SHARED_STATE`: Compositor -> Emacs snapshot (focus, pointer, outputs)
//!    - `KEYBOARD_CAPTURE_ACTIVE`: derived from tokened Emacs input leases
//!
//! 2. **No blocking**: Module functions called from Emacs must never block. They push to queues and
//!    return immediately. The compositor processes queues on its event loop. Exception:
//!    request/reply defuns (`create_activation_token`, `get_debug_state_module`) block on a
//!    one-shot channel until the compositor processes the command on its next tick.
//!
//! 3. **Synchronous state for races**: Some state must be synchronous to avoid race conditions:
//!    - `PENDING_FRAME_OUTPUTS`: Must be read atomically with surface creation
//!    - `INTERCEPTED_KEYS`: Must be available before first key event
//!
//! 4. **Focus debugging**: `FOCUS_HISTORY` records the last 20 focus changes with source tracking.
//!    Essential for diagnosing focus races where the compositor and Emacs disagree about which
//!    surface has focus.
//!
//! # Why This Design
//!
//! Emacs dynamic modules run in the Emacs main thread. The compositor must run
//! in its own thread to process Wayland events without blocking Emacs. This
//! creates a producer-consumer relationship:
//!
//! - Emacs produces: layout commands, focus requests, key interception config
//! - Compositor produces: new surface events, title changes, focus notifications
//!
//! State that Emacs needs to read synchronously (focus ID, pointer location,
//! output offsets) is collected into a single `SharedState` mutex, updated once
//! per compositor tick in `update_shared_state()`. This gives consistent
//! snapshots without scattered atomics.

use anyhow::anyhow;
use emacs::{define_errors, defun, use_functions, use_symbols, Env, IntoLisp, Result, Value};

// Cached Lisp symbols used in hot-path defuns (interning on every call is wasteful).
use_symbols! {
    // Boolean sentinel: Emacs uses :false to pass an explicit false through the module interface,
    // since nil is indistinguishable from "argument absent".
    kw_false => ":false"
    // output_layout_module / intercept_keys_module plist keys
    kw_x           => ":x"
    kw_y           => ":y"
    kw_w           => ":w"
    kw_h           => ":h"
    kw_fullscreen  => ":fullscreen"
    kw_key         => ":key"
    kw_description => ":description"
    kw_ctrl        => ":ctrl"
    kw_alt         => ":alt"
    kw_shift       => ":shift"
    kw_super       => ":super"
    kw_emulate     => ":emulate"
    kw_dispatch    => ":dispatch"
    kw_repeat      => ":repeat"
    kw_name       => ":name"
    kw_width      => ":width"
    kw_entries    => ":entries"
    kw_surface_id => ":surface-id"
    kw_entry_id    => ":entry-id"
    kw_focus_id    => ":focus-id"
    kw_selected_entry_id => ":selected-entry-id"
    // configure_output_module: marker for "not provided" (distinct from nil)
    kw_unset  => ":unset"
    // configure_input_module plist property names
    kw_device          => ":device"
    kw_type            => ":type"
    kw_natural_scroll  => ":natural-scroll"
    kw_tap             => ":tap"
    kw_dwt             => ":dwt"
    kw_left_handed     => ":left-handed"
    kw_middle_emulation => ":middle-emulation"
    kw_accel_speed     => ":accel-speed"
    kw_accel_profile   => ":accel-profile"
    kw_click_method    => ":click-method"
    kw_scroll_method   => ":scroll-method"
    kw_tap_button_map  => ":tap-button-map"
    kw_repeat_delay    => ":repeat-delay"
    kw_repeat_rate     => ":repeat-rate"
    kw_xkb_layouts     => ":xkb-layouts"
    kw_xkb_variants    => ":xkb-variants"
    kw_xkb_options     => ":xkb-options"
}

// Cached Lisp function subrs used in hot-path defuns (interning string on every call is wasteful).
use_functions! {
    plist_get   => "plist-get"
    length
    aref
    car
    cadr
    cddr
    symbol_name => "symbol-name"
}

define_errors! {
    ewm_input_error        "EWM input configuration error"
    ewm_input_invalid_value "Invalid value for input config key" (ewm_input_error)
    ewm_input_unknown_key  "Unknown input config key"            (ewm_input_error)
}

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use smithay::input::keyboard::{keysyms, xkb};
use smithay::reexports::calloop::LoopSignal;

use crate::cursor::CursorConfig;
use crate::strip::Frame;
pub use crate::utils::reply::Reply;
use crate::utils::reply::{reply_channel, ReplyError};
use crate::{EmulatedKey, InterceptDispatch, InterceptedKey, LayoutEntry};

/// Extract a number from a Lisp value as f64, accepting both int and float.
fn value_as_f64(val: Value<'_>) -> Result<f64> {
    val.into_rust::<f64>()
        .or_else(|_| val.into_rust::<i64>().map(|i| i as f64))
}

fn nonzero_u64_from_i64(id: i64) -> Option<NonZeroU64> {
    u64::try_from(id).ok().and_then(NonZeroU64::new)
}

fn optional_nonzero_u64(val: Value<'_>) -> Result<Option<NonZeroU64>> {
    if val.is_not_nil() {
        Ok(nonzero_u64_from_i64(val.into_rust::<i64>()?))
    } else {
        Ok(None)
    }
}

fn parse_optional_input_enum<T>(
    env: &Env,
    key_name: &str,
    val: Value<'_>,
    parse: impl FnOnce(&str) -> Option<T>,
    expected: &str,
) -> Result<Option<T>> {
    if val.is_not_nil() {
        let s: String = val.into_rust()?;
        if let Some(parsed) = parse(&s) {
            return Ok(Some(parsed));
        }
        env.signal(
            ewm_input_invalid_value,
            (format!("Invalid {key_name} \"{s}\", expected: {expected}"),),
        )?;
    }
    Ok(None)
}

// Shared State (read by Emacs, written by compositor)

/// Compositor state snapshot, updated once per tick in `update_shared_state()`.
/// Only contains fields that Emacs reads synchronously via per-field defuns.
#[derive(Default)]
pub struct ActiveOutput {
    pub name: String,
    pub origin: (i32, i32),
}

#[derive(Default)]
pub struct SharedState {
    pub focused_surface_id: u64,
    pub focused_frame_id: u64,
    pub pointer_location: (f64, f64),
    pub active_outputs: Vec<ActiveOutput>,
}

static SHARED_STATE: OnceLock<Mutex<SharedState>> = OnceLock::new();

pub fn shared_state() -> &'static Mutex<SharedState> {
    SHARED_STATE.get_or_init(|| Mutex::new(SharedState::default()))
}

/// Pending frame-to-output assignments (synchronous to avoid race with surface creation)
static PENDING_FRAME_OUTPUTS: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

fn pending_frame_outputs() -> &'static Mutex<VecDeque<String>> {
    PENDING_FRAME_OUTPUTS.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Take the next pending frame output (called by compositor when creating surfaces).
pub fn take_pending_frame_output() -> Option<String> {
    pending_frame_outputs().lock().unwrap().pop_front()
}

/// Get pending frame outputs for state dump.
pub fn peek_pending_frame_outputs() -> Vec<String> {
    pending_frame_outputs()
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect()
}

/// Frame closes initiated for the selected Emacs frame.
///
/// This is synchronous for the same reason as `PENDING_FRAME_OUTPUTS`: a pgtk
/// delete can relay Emacs's MRU focus before the Wayland destroy reaches us,
/// so the destroy handler needs the close intent without waiting for the
/// command queue.
static PENDING_ACTIVE_FRAME_CLOSES: OnceLock<Mutex<BTreeSet<u64>>> = OnceLock::new();

fn pending_active_frame_closes() -> &'static Mutex<BTreeSet<u64>> {
    PENDING_ACTIVE_FRAME_CLOSES.get_or_init(|| Mutex::new(BTreeSet::new()))
}

pub(crate) fn mark_pending_active_frame_close(id: u64) {
    pending_active_frame_closes().lock().unwrap().insert(id);
}

pub(crate) fn take_pending_active_frame_close(id: u64) -> bool {
    pending_active_frame_closes().lock().unwrap().remove(&id)
}

pub(crate) fn has_pending_active_frame_close(id: u64) -> bool {
    pending_active_frame_closes().lock().unwrap().contains(&id)
}

pub(crate) fn peek_pending_active_frame_closes() -> Vec<u64> {
    pending_active_frame_closes()
        .lock()
        .unwrap()
        .iter()
        .copied()
        .collect()
}

/// Intercepted keys (synchronous to avoid race during startup)
static INTERCEPTED_KEYS: OnceLock<RwLock<Vec<InterceptedKey>>> = OnceLock::new();

fn intercepted_keys() -> &'static RwLock<Vec<InterceptedKey>> {
    INTERCEPTED_KEYS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Get intercepted keys (called by compositor during input handling).
pub fn get_intercepted_keys() -> Vec<InterceptedKey> {
    intercepted_keys().read().unwrap().clone()
}

#[doc(hidden)]
pub fn set_intercepted_keys_for_test(keys: Vec<InterceptedKey>) {
    *intercepted_keys().write().unwrap() = keys;
}

/// Emulated keys (synchronous, like intercepted keys)
static EMULATED_KEYS: OnceLock<RwLock<Vec<EmulatedKey>>> = OnceLock::new();

fn emulated_keys() -> &'static RwLock<Vec<EmulatedKey>> {
    EMULATED_KEYS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Get emulated keys (called by compositor during input handling).
pub fn get_emulated_keys() -> Vec<EmulatedKey> {
    emulated_keys().read().unwrap().clone()
}

#[doc(hidden)]
pub fn set_emulated_keys_for_test(keys: Vec<EmulatedKey>) {
    *emulated_keys().write().unwrap() = keys;
}

/// Emacs currently needs Wayland keyboard focus for command-loop input.
///
/// Capture is represented as tokened leases so cleanup from one input context
/// cannot clear capture that another still owns.
static KEYBOARD_CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_CAPTURE_STATE: OnceLock<Mutex<KeyboardCaptureState>> = OnceLock::new();

#[derive(Debug, Default)]
struct KeyboardCaptureState {
    next_token: u64,
    leases: BTreeMap<u64, String>,
    redirect_active: bool,
}

impl KeyboardCaptureState {
    fn begin(&mut self, reason: String) -> u64 {
        let token = self.allocate_token();
        self.leases.insert(token, reason);
        token
    }

    fn end(&mut self, token: u64) -> bool {
        self.leases.remove(&token).is_some()
    }

    fn begin_redirect(&mut self) {
        self.redirect_active = true;
    }

    fn clear_redirect(&mut self) -> bool {
        std::mem::take(&mut self.redirect_active)
    }

    fn is_active(&self) -> bool {
        self.redirect_active || !self.leases.is_empty()
    }

    fn clear(&mut self) {
        self.leases.clear();
        self.redirect_active = false;
    }

    fn debug_holders(&self) -> Vec<(u64, String)> {
        self.redirect_active
            .then(|| (0, "keyboard-redirect".to_string()))
            .into_iter()
            .chain(
                self.leases
                    .iter()
                    .map(|(&token, reason)| (token, reason.clone())),
            )
            .collect()
    }

    fn allocate_token(&mut self) -> u64 {
        loop {
            self.next_token = self.next_token.wrapping_add(1);
            if self.next_token == 0 {
                self.next_token = 1;
            }
            if !self.leases.contains_key(&self.next_token) {
                return self.next_token;
            }
        }
    }
}

fn keyboard_capture_state() -> &'static Mutex<KeyboardCaptureState> {
    KEYBOARD_CAPTURE_STATE.get_or_init(|| Mutex::new(KeyboardCaptureState::default()))
}

fn sync_keyboard_capture_active(state: &KeyboardCaptureState) {
    KEYBOARD_CAPTURE_ACTIVE.store(state.is_active(), Ordering::Release);
}

fn update_keyboard_capture_state<T>(f: impl FnOnce(&mut KeyboardCaptureState) -> T) -> T {
    let mut state = keyboard_capture_state().lock().unwrap();
    let result = f(&mut state);
    sync_keyboard_capture_active(&state);
    result
}

pub fn begin_keyboard_capture(reason: impl Into<String>) -> u64 {
    update_keyboard_capture_state(|state| state.begin(reason.into()))
}

pub fn end_keyboard_capture(token: u64) -> bool {
    update_keyboard_capture_state(|state| state.end(token))
}

pub fn clear_keyboard_capture_holders() {
    update_keyboard_capture_state(KeyboardCaptureState::clear);
}

pub fn keyboard_capture_holders_debug() -> Vec<(u64, String)> {
    keyboard_capture_state().lock().unwrap().debug_holders()
}

pub fn begin_keyboard_redirect_capture() {
    update_keyboard_capture_state(KeyboardCaptureState::begin_redirect);
}

pub fn clear_keyboard_redirect_capture() -> bool {
    update_keyboard_capture_state(KeyboardCaptureState::clear_redirect)
}

pub fn get_keyboard_capture() -> bool {
    KEYBOARD_CAPTURE_ACTIVE.load(Ordering::Acquire)
}

// Per-field accessors: thin defuns for hot-path reads.
// Each locks SharedState briefly and returns a single value, no alist construction.

/// Query focused surface ID (called by Emacs on every post-command-hook).
#[defun]
fn get_focused_id() -> Result<i64> {
    Ok(shared_state().lock().unwrap().focused_surface_id as i64)
}

/// Query the focused Emacs frame's surface ID.
#[defun]
fn get_focused_frame_id() -> Result<i64> {
    Ok(shared_state().lock().unwrap().focused_frame_id as i64)
}

/// Return a list of active output names (e.g., ("eDP-1" "DP-1")).
#[defun]
fn get_active_outputs<'a>(env: &'a Env) -> Result<Value<'a>> {
    let state = shared_state().lock().unwrap();
    let mut result: Value<'a> = ().into_lisp(env)?;
    for output in state.active_outputs.iter().rev() {
        result = env.cons(output.name.as_str(), result)?;
    }
    Ok(result)
}

/// Query output origin (called by Emacs for mouse-follows-focus pointer warping).
/// Returns (x . y) cons cell, or nil if output not found.
#[defun]
fn get_output_origin<'a>(env: &'a Env, name: String) -> Result<Value<'a>> {
    let state = shared_state().lock().unwrap();
    match state
        .active_outputs
        .iter()
        .find(|output| output.name == name)
    {
        Some(output) => env.cons(output.origin.0 as i64, output.origin.1 as i64),
        None => ().into_lisp(env),
    }
}

/// Query pointer location (called by Emacs for mouse-follows-focus).
/// Returns (x . y) cons cell in compositor coordinates.
#[defun]
fn get_pointer_location<'a>(env: &'a Env) -> Result<Value<'a>> {
    let (x, y) = shared_state().lock().unwrap().pointer_location;
    env.cons(x, y)
}

// Module Commands (Emacs -> Compositor)

/// Commands sent from Emacs to the compositor via the module interface.
#[derive(Debug)]
pub enum ModuleCommand {
    Close {
        id: u64,
    },
    WarpPointer {
        x: f64,
        y: f64,
    },
    Screenshot {
        path: Option<String>,
    },
    ConfigureOutput {
        name: String,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<i32>,
        height: Option<i32>,
        refresh: Option<i32>,
        scale: Option<f64>,
        transform: Option<i32>,
        enabled: Option<bool>,
    },
    ImCommit {
        text: String,
        surface_id: u64,
    },
    TextInputIntercept {
        enabled: bool,
    },
    KeyboardCaptureChanged {
        active: bool,
    },
    SwitchLayout {
        layout: String,
    },
    GetLayouts,
    /// Request verbose state dump for debugging (ewm-show-state).
    /// Compositor sends the JSON string back via the reply channel.
    GetDebugState(Reply<String>),
    /// Request activation token creation. Compositor sends the token string back via the reply
    /// channel.
    CreateActivationToken(Reply<String>),
    /// Set clipboard selection from Emacs
    SetSelection {
        text: String,
    },
    /// Per-output frame strip.  Pure geometry; focus is shifted only via `FocusTarget'.
    OutputLayout {
        output: String,
        frames: Vec<Frame>,
    },
    /// Toggle compositor-owned fullscreen for a layout view.
    ToggleFullscreen {
        entry_id: crate::LayoutEntryId,
    },
    /// Configure input devices (unified)
    ConfigureInput {
        configs: Vec<crate::input::InputConfigEntry>,
    },
    /// Configure native idle timeout
    ConfigureIdle {
        timeout_secs: Option<u64>,
        action: String,
    },
    /// Configure cursor auto-hide (inactivity timeout + hide-on-typing)
    ConfigureCursorHide {
        timeout_secs: Option<u64>,
        hide_when_typing: bool,
    },
    /// Configure cursor theme and size.
    ConfigureCursor {
        config: CursorConfig,
    },
    /// Configure focus follows mouse
    ConfigureFocusFollowsMouse {
        state: bool,
    },
    /// Focus the active frame on the output adjacent to the current one.
    /// `dx`/`dy` are -1/+1 along one axis; the other axis is 0.
    FocusOutputDirection {
        dx: i32,
        dy: i32,
    },
    /// Rename a workspace. If `frame_surface_id` is absent, the compositor's
    /// currently focused frame is renamed.
    RenameWorkspace {
        frame_surface_id: Option<u64>,
        name: String,
    },
    /// Mark a workspace as requiring attention. If `frame_surface_id` is absent,
    /// the compositor's currently focused frame is used.
    MarkWorkspaceUrgent {
        frame_surface_id: Option<u64>,
    },
    /// Sole channel for Emacs-initiated focus; bypasses xdg_activation.
    FocusTarget {
        focus_id: std::num::NonZeroU64,
    },
    /// Set alpha multiplier for unfocused toplevels (1.0 = disabled).
    ConfigureUnfocusedAlpha {
        alpha: f32,
    },
    /// Flip the `AnimationsClock` shared by every `ViewAnimation`.
    ConfigureAnimations {
        enabled: bool,
    },
    /// Set manual idle inhibition (from Emacs)
    SetIdleInhibited {
        inhibited: bool,
    },
    /// Client init done; force-resend wl_keyboard.enter (PGTK drops the
    /// pre-roundtrip one).
    Initialized,
}

/// Command queue shared between Emacs thread and compositor
static COMMAND_QUEUE: OnceLock<Mutex<Vec<ModuleCommand>>> = OnceLock::new();

fn command_queue() -> &'static Mutex<Vec<ModuleCommand>> {
    COMMAND_QUEUE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Drain all pending commands from the queue.
/// Called by the compositor in its main loop.
pub fn drain_commands() -> Vec<ModuleCommand> {
    command_queue().lock().unwrap().drain(..).collect()
}

/// Peek at pending commands without draining (for state dump)
pub fn peek_commands() -> Vec<String> {
    command_queue()
        .lock()
        .unwrap()
        .iter()
        .map(|cmd| format!("{:?}", cmd))
        .collect()
}

/// Push a command to the queue and wake the compositor.
fn push_command(cmd: ModuleCommand) {
    command_queue().lock().unwrap().push(cmd);
    // Wake the event loop so it processes the command
    if let Some(signal) = LOOP_SIGNAL.get() {
        signal.wakeup();
    }
}

fn push_keyboard_capture_changed() {
    // Snapshot the edge at enqueue time. A begin+end pair can be queued before
    // the compositor drains commands; the begin still needs to cancel repeat.
    push_command(ModuleCommand::KeyboardCaptureChanged {
        active: get_keyboard_capture(),
    });
}

/// Flag to request compositor shutdown from Emacs thread
pub static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Debug mode flag - when enabled, more verbose logging is output
pub static DEBUG_MODE: AtomicBool = AtomicBool::new(false);

/// Event loop signal for waking the compositor from Emacs thread
pub static LOOP_SIGNAL: OnceLock<LoopSignal> = OnceLock::new();

/// Request an XDG activation token from the compositor.
/// Returns the token string for use as `XDG_ACTIVATION_TOKEN`, or nil on timeout.
#[defun]
fn create_activation_token() -> Result<Option<String>> {
    Ok(request_reply(
        ModuleCommand::CreateActivationToken,
        Duration::from_millis(200),
        "activation token",
    ))
}

/// Push a request-reply command with a one-shot reply slot and block on the
/// reply.  Fast-fails with `None` if the compositor isn't running so callers
/// don't burn the full timeout when there's no one to answer.
fn request_reply<F>(make_cmd: F, timeout: Duration, label: &str) -> Option<String>
where
    F: FnOnce(Reply<String>) -> ModuleCommand,
{
    if !is_running() {
        return None;
    }
    let (reply, receiver) = reply_channel();
    push_command(make_cmd(reply));
    match receiver.recv_timeout(timeout) {
        Ok(value) => Some(value),
        Err(ReplyError::Timeout) => {
            tracing::warn!("Timeout waiting for {}", label);
            None
        }
        Err(ReplyError::Disconnected) => {
            tracing::warn!("Compositor dropped reply for {} without sending", label);
            None
        }
    }
}

// Focus History (for debugging focus issues)

/// Maximum focus history entries
const FOCUS_HISTORY_SIZE: usize = 20;

/// A single focus event for the history
#[derive(Clone, Debug, serde::Serialize)]
pub struct FocusEvent {
    /// Timestamp (monotonic counter)
    pub seq: usize,
    /// Surface ID that received focus
    pub surface_id: u64,
    /// Source of the focus change
    pub source: String,
    /// Additional context
    pub context: Option<String>,
}

/// Focus history shared between compositor and Emacs
static FOCUS_HISTORY: OnceLock<Mutex<VecDeque<FocusEvent>>> = OnceLock::new();
static FOCUS_SEQ: AtomicUsize = AtomicUsize::new(0);

fn focus_history() -> &'static Mutex<VecDeque<FocusEvent>> {
    FOCUS_HISTORY.get_or_init(|| Mutex::new(VecDeque::with_capacity(FOCUS_HISTORY_SIZE)))
}

/// Record a focus change (called by compositor)
pub fn record_focus(surface_id: u64, source: &str, context: Option<&str>) {
    let seq = FOCUS_SEQ.fetch_add(1, Ordering::Relaxed);
    let event = FocusEvent {
        seq,
        surface_id,
        source: source.to_string(),
        context: context.map(|s| s.to_string()),
    };

    if DEBUG_MODE.load(Ordering::Relaxed) {
        tracing::debug!("Focus #{}: {} -> {} {:?}", seq, source, surface_id, context);
    }

    let mut history = focus_history().lock().unwrap();
    if history.len() >= FOCUS_HISTORY_SIZE {
        history.pop_front();
    }
    history.push_back(event);
}

/// Get focus history as JSON (for state dump)
pub fn get_focus_history() -> Vec<FocusEvent> {
    focus_history().lock().unwrap().iter().cloned().collect()
}

// Event Pipe (compositor -> Emacs via open_channel fd)

/// Pipe writer for sending events to Emacs.  Set by `init_event_channel`.
static EVENT_WRITER: Mutex<Option<Box<dyn std::io::Write + Send>>> = Mutex::new(None);

/// Initialize the event channel from a pipe process.
/// Called from Elisp before starting the compositor thread.
#[defun]
fn init_event_channel(env: &Env, pipe_process: Value<'_>) -> Result<()> {
    let writer = env.open_channel(pipe_process)?;
    *EVENT_WRITER.lock().unwrap() = Some(Box::new(writer));
    Ok(())
}

/// Write `event` as one JSON line to the Emacs pipe.
pub(crate) fn write_event(event: &crate::event::Event) {
    use std::io::Write;
    let mut buf = serde_json::to_vec(event).unwrap_or_default();
    buf.push(b'\n');
    let mut guard = EVENT_WRITER.lock().unwrap();
    if let Some(w) = guard.as_mut() {
        let _ = w.write_all(&buf);
    }
}

/// Test function - returns a greeting
#[defun]
fn hello() -> Result<String> {
    Ok("Hello from EWM compositor!".to_string())
}

/// Return the module version
#[defun]
fn version() -> Result<String> {
    Ok(env!("CARGO_PKG_VERSION").to_string())
}

/// Install a CSS string on the host Emacs's default GTK screen.
#[defun]
fn gtk_set_style(css: String) -> Result<()> {
    crate::gtk::set_style(&css)
}

/// Suspend GdkWindow paint updates for a frame's GtkWindow (window-id string).
#[defun]
fn gtk_freeze_frame_updates(window_id: String) -> Result<()> {
    let ptr: usize = window_id
        .parse()
        .map_err(|_| anyhow!("invalid window-id: {window_id}"))?;
    crate::gtk::set_updates_frozen(ptr, true)
}

/// Resume GdkWindow paint updates; flushes any queued expose.
#[defun]
fn gtk_thaw_frame_updates(window_id: String) -> Result<()> {
    let ptr: usize = window_id
        .parse()
        .map_err(|_| anyhow!("invalid window-id: {window_id}"))?;
    crate::gtk::set_updates_frozen(ptr, false)
}

// Compositor state
struct CompositorState {
    thread: Option<JoinHandle<()>>,
}

static COMPOSITOR: OnceLock<Mutex<CompositorState>> = OnceLock::new();

fn compositor_state() -> &'static Mutex<CompositorState> {
    COMPOSITOR.get_or_init(|| Mutex::new(CompositorState { thread: None }))
}

/// Initialize logging to journald.
/// Filter controlled by RUST_LOG env var (default: ewm=debug,smithay=warn).
/// View logs with: journalctl --user -t ewm -f
fn init_logging() {
    use std::sync::Once;

    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    static INIT_LOG: Once = Once::new();
    INIT_LOG.call_once(|| {
        let default_filter = "ewm=debug,smithay=warn";
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

        // Try journald first, fall back to stderr
        if let Ok(journald) = tracing_journald::layer() {
            tracing_subscriber::registry()
                .with(filter)
                .with(journald.with_syslog_identifier("ewm".to_string()))
                .init();
        } else {
            // Fallback for systems without journald
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    });
}

fn cursor_config(theme: Option<String>, size: i64) -> Result<CursorConfig> {
    if theme.as_ref().is_some_and(|theme| theme.is_empty()) {
        return Err(anyhow!("cursor theme must not be empty").into());
    }

    let size = u8::try_from(size)
        .ok()
        .filter(|size| *size > 0)
        .ok_or_else(|| anyhow!("cursor size must be in 1..=255"))?;

    Ok(CursorConfig::resolve(theme, size))
}

/// Start the compositor in a background thread.
/// Must be called from a TTY (not inside another compositor).
/// Returns t if started successfully, nil if already running.
#[defun]
fn start(cursor_theme: Option<String>, cursor_size: i64) -> Result<bool> {
    use crate::backend::drm::run_drm;

    init_logging();
    let cursor_config = cursor_config(cursor_theme, cursor_size)?;

    let mut state = compositor_state().lock().unwrap();

    // Check if already running
    if state.thread.as_ref().is_some_and(|t| !t.is_finished()) {
        tracing::warn!("Compositor already running");
        return Ok(false);
    }

    // Reset stop flag
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    clear_keyboard_capture_holders();

    // Spawn compositor thread - frames are created via output_detected events
    // (Emacs receives events and creates frames with ewm--create-frame-for-output)
    let handle = thread::spawn(move || {
        tracing::info!("Compositor thread starting");

        // Catch panics so they don't crash Emacs
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_drm(cursor_config)));

        match result {
            Ok(Ok(())) => {
                tracing::info!("Compositor thread exiting normally");
            }
            Ok(Err(e)) => {
                tracing::error!("Compositor error: {}", e);
            }
            Err(panic) => {
                let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "Unknown panic".to_string()
                };
                tracing::error!("Compositor panicked: {}", msg);
            }
        }
    });

    state.thread = Some(handle);
    tracing::info!("Compositor started");
    Ok(true)
}

/// Stop the compositor gracefully.
/// Returns t if stop was requested, nil if compositor wasn't running.
#[defun]
fn stop() -> Result<bool> {
    let state = compositor_state().lock().unwrap();

    if state.thread.as_ref().is_none_or(|t| t.is_finished()) {
        tracing::info!("Compositor not running");
        return Ok(false);
    }

    tracing::info!("Requesting compositor stop");
    STOP_REQUESTED.store(true, Ordering::SeqCst);

    // Wake the event loop so it sees the stop request
    if let Some(signal) = LOOP_SIGNAL.get() {
        signal.stop();
    }

    Ok(true)
}

/// Check if compositor is running.
#[defun]
fn running() -> Result<bool> {
    Ok(is_running())
}

fn is_running() -> bool {
    compositor_state()
        .lock()
        .unwrap()
        .thread
        .as_ref()
        .is_some_and(|t| !t.is_finished())
}

/// Get the Wayland display socket name (if compositor is running).
#[defun]
fn socket() -> Result<Option<String>> {
    Ok(std::env::var("EWM_WAYLAND_DISPLAY").ok())
}

// Module Command Functions (direct Emacs -> Compositor)

/// Set declarative layout for an output (module mode).
///
/// OUTPUT is the output name. FRAMES is a vector of plists describing the
/// strip of Emacs frames left-to-right. Each frame plist has:
///   :name NAME :surface-id ID :focus-id N :selected-entry-id N|nil
///   :width FLOAT :entries [ENTRY ...]
/// where each ENTRY is a plist `:entry-id :name :x :y :w :h` with optional
/// `:surface-id`.
/// Entry coordinates are relative to the owning frame's working area.
#[defun]
fn output_layout_module(env: &Env, output: String, frames: Value<'_>) -> Result<()> {
    let frames_len: i64 = env.call(length, (frames,))?.into_rust()?;
    let mut parsed_frames = Vec::with_capacity(frames_len as usize);

    for i in 0..frames_len {
        let frame = env.call(aref, (frames, i))?;

        let name: String = env
            .call(plist_get, (frame, kw_name))?
            .into_rust()
            .unwrap_or_default();

        let surface_id = env
            .call(plist_get, (frame, kw_surface_id))?
            .into_rust::<i64>()
            .unwrap_or(0) as u64;

        let focus_id = match optional_nonzero_u64(env.call(plist_get, (frame, kw_focus_id))?)? {
            Some(v) => v,
            None => {
                tracing::warn!("frame {} layout dropped: missing focus_id", surface_id);
                continue;
            }
        };

        let width = value_as_f64(env.call(plist_get, (frame, kw_width))?).unwrap_or(0.0);

        let selected_entry_id =
            optional_nonzero_u64(env.call(plist_get, (frame, kw_selected_entry_id))?)?;

        let entries_val = env.call(plist_get, (frame, kw_entries))?;
        let entries_len: i64 = if entries_val.is_not_nil() {
            env.call(length, (entries_val,))?.into_rust()?
        } else {
            0
        };
        let mut entries = Vec::with_capacity(entries_len as usize);
        for j in 0..entries_len {
            let entry = env.call(aref, (entries_val, j))?;

            let name: String = env
                .call(plist_get, (entry, kw_name))?
                .into_rust()
                .unwrap_or_default();

            let entry_id = match optional_nonzero_u64(env.call(plist_get, (entry, kw_entry_id))?)? {
                Some(v) => v,
                None => {
                    tracing::warn!("layout entry dropped: missing entry_id");
                    continue;
                }
            };
            let x: i64 = env.call(plist_get, (entry, kw_x))?.into_rust()?;
            let y: i64 = env.call(plist_get, (entry, kw_y))?.into_rust()?;
            let w: i64 = env.call(plist_get, (entry, kw_w))?.into_rust()?;
            let h: i64 = env.call(plist_get, (entry, kw_h))?.into_rust()?;
            let surface_id_val = env.call(plist_get, (entry, kw_surface_id))?;
            let surface_id = if surface_id_val.is_not_nil() {
                surface_id_val.into_rust::<i64>().unwrap_or(0) as u64
            } else {
                0
            };

            entries.push(if surface_id == 0 {
                LayoutEntry::emacs_window(entry_id, name, x as i32, y as i32, w as u32, h as u32)
            } else {
                LayoutEntry::surface(
                    entry_id, name, surface_id, x as i32, y as i32, w as u32, h as u32,
                )
            });
        }

        parsed_frames.push(Frame {
            name,
            workspace_name: None,
            surface_id,
            focus_id,
            width,
            entries,
            fullscreen: None,
            selected_entry_id,
            move_anim: None,
        });
    }

    push_command(ModuleCommand::OutputLayout {
        output,
        frames: parsed_frames,
    });
    Ok(())
}

/// Request surface to close (module mode).
#[defun]
fn close_module(id: i64) -> Result<()> {
    push_command(ModuleCommand::Close { id: id as u64 });
    Ok(())
}

/// Toggle compositor-owned fullscreen for ENTRY-ID.
#[defun]
fn toggle_fullscreen_module(entry_id: i64) -> Result<()> {
    if let Some(entry_id) = std::num::NonZeroU64::new(entry_id.max(0) as u64) {
        push_command(ModuleCommand::ToggleFullscreen { entry_id });
    }
    Ok(())
}

/// Focus the active frame on the output adjacent to the current one.
/// DX/DY are -1/+1 along one axis; the other axis is 0.
#[defun]
fn focus_output_direction_module(dx: i64, dy: i64) -> Result<()> {
    push_command(ModuleCommand::FocusOutputDirection {
        dx: dx as i32,
        dy: dy as i32,
    });
    Ok(())
}

/// Rename FRAME-SURFACE-ID's workspace, or the focused workspace when nil.
/// Empty names clear the workspace name.
#[defun]
fn workspace_rename_module(name: String, frame_surface_id: Option<i64>) -> Result<()> {
    let frame_surface_id = frame_surface_id.and_then(|id| (id > 0).then_some(id as u64));
    push_command(ModuleCommand::RenameWorkspace {
        frame_surface_id,
        name,
    });
    Ok(())
}

/// Mark FRAME-SURFACE-ID's workspace as urgent, or the focused workspace when nil.
#[defun]
fn workspace_mark_urgent_module(frame_surface_id: Option<i64>) -> Result<()> {
    let frame_surface_id = frame_surface_id.and_then(|id| (id > 0).then_some(id as u64));
    push_command(ModuleCommand::MarkWorkspaceUrgent { frame_surface_id });
    Ok(())
}

/// Sole focus channel from Lisp. FOCUS-ID names a frame chrome or layout entry.
#[defun]
fn focus_target_module(focus_id: i64) -> Result<()> {
    let Some(focus_id) = std::num::NonZeroU64::new(focus_id as u64) else {
        tracing::warn!("focus_target_module: ignored 0 focus_id");
        return Ok(());
    };
    push_command(ModuleCommand::FocusTarget { focus_id });
    Ok(())
}

/// Notify compositor that the client has finished its own initialization.
#[defun]
fn notify_initialized_module() -> Result<()> {
    push_command(ModuleCommand::Initialized);
    Ok(())
}

/// Warp pointer to absolute position (module mode).
#[defun]
fn warp_pointer_module(x: Value<'_>, y: Value<'_>) -> Result<()> {
    push_command(ModuleCommand::WarpPointer {
        x: value_as_f64(x)?,
        y: value_as_f64(y)?,
    });
    Ok(())
}

/// Take a screenshot (module mode).
#[defun]
fn screenshot_module(path: Option<String>) -> Result<()> {
    push_command(ModuleCommand::Screenshot { path });
    Ok(())
}

/// Prepare next frame for output (synchronous to avoid race with surface creation).
#[defun]
fn prepare_frame_module(output: String) -> Result<()> {
    pending_frame_outputs()
        .lock()
        .unwrap()
        .push_back(output.clone());
    tracing::info!("Prepared frame for output {}", output);
    Ok(())
}

/// Mark a selected Emacs frame as about to close.
#[defun]
fn prepare_frame_close_module(id: i64) -> Result<()> {
    if id <= 0 {
        tracing::warn!("prepare_frame_close_module: ignored invalid id {id}");
        return Ok(());
    }
    let id = id as u64;
    mark_pending_active_frame_close(id);
    tracing::debug!("Prepared frame close for {id}");
    Ok(())
}

/// Configure output (module mode).
/// ENABLED should be t, nil, or omitted.
/// SCALE is a float (e.g. 1.5). TRANSFORM is an integer (0=Normal, 1=90, 2=180, 3=270, 4=Flipped,
/// 5=Flipped90, 6=Flipped180, 7=Flipped270).
#[defun]
#[allow(clippy::too_many_arguments)]
fn configure_output_module(
    name: String,
    x: Option<i64>,
    y: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
    refresh: Option<i64>,
    scale: Value<'_>,
    transform: Option<i64>,
    enabled: Value<'_>,
) -> Result<()> {
    let scale = if scale.is_not_nil() {
        Some(value_as_f64(scale)?)
    } else {
        None
    };
    // Convert enabled value: t -> Some(true), nil -> Some(false), unspecified -> None
    // We use a special marker to detect "not provided" vs nil
    let enabled_opt = if enabled.is_not_nil() {
        // Check if it's :unset (our marker for "not provided")
        if enabled == *kw_unset {
            None
        } else {
            Some(true)
        }
    } else {
        Some(false)
    };

    push_command(ModuleCommand::ConfigureOutput {
        name,
        x: x.map(|v| v as i32),
        y: y.map(|v| v as i32),
        width: width.map(|v| v as i32),
        height: height.map(|v| v as i32),
        refresh: refresh.map(|v| v as i32),
        scale,
        transform: transform.map(|v| v as i32),
        enabled: enabled_opt,
    });
    Ok(())
}

/// Emacs key names that don't round-trip through simple transformations.
/// Emacs lowercases and replaces underscores with hyphens, but also compounds
/// words (e.g., XKB "ISO_Left_Tab" -> Emacs "iso-lefttab"), losing word boundaries.
const EMACS_TO_XKB_NAMES: &[(&str, &str)] = &[
    ("iso-lefttab", "ISO_Left_Tab"),
    ("iso-move-line-up", "ISO_Move_Line_Up"),
    ("iso-move-line-down", "ISO_Move_Line_Down"),
    ("iso-partial-line-up", "ISO_Partial_Line_Up"),
    ("iso-partial-line-down", "ISO_Partial_Line_Down"),
    ("iso-partial-space-left", "ISO_Partial_Space_Left"),
    ("iso-partial-space-right", "ISO_Partial_Space_Right"),
    ("iso-set-margin-left", "ISO_Set_Margin_Left"),
    ("iso-set-margin-right", "ISO_Set_Margin_Right"),
    ("iso-release-margin-left", "ISO_Release_Margin_Left"),
    ("iso-release-margin-right", "ISO_Release_Margin_Right"),
    ("iso-release-both-margins", "ISO_Release_Both_Margins"),
    ("iso-fast-cursor-left", "ISO_Fast_Cursor_Left"),
    ("iso-fast-cursor-right", "ISO_Fast_Cursor_Right"),
    ("iso-fast-cursor-up", "ISO_Fast_Cursor_Up"),
    ("iso-fast-cursor-down", "ISO_Fast_Cursor_Down"),
    ("iso-continuous-underline", "ISO_Continuous_Underline"),
    ("iso-discontinuous-underline", "ISO_Discontinuous_Underline"),
    ("iso-emphasize", "ISO_Emphasize"),
    ("iso-center-object", "ISO_Center_Object"),
    ("iso-enter", "ISO_Enter"),
];

/// Resolve an Emacs key name to an XKB keysym.
/// Handles: case differences (Emacs "left" -> XKB "Left"),
/// hyphens vs underscores (Emacs "kp-add" -> XKB "KP_Add"),
/// XF86 prefix (Emacs "AudioMute" -> XKB "XF86AudioMute"),
/// and compound-word lossy names via fallback table.
fn resolve_keysym_from_name(name: &str) -> xkb::Keysym {
    let no_symbol: xkb::Keysym = keysyms::KEY_NoSymbol.into();

    // Try the name as-is (case-insensitive)
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
    if sym != no_symbol {
        return sym;
    }

    // Try with hyphens replaced by underscores (Emacs convention -> XKB convention)
    if name.contains('-') {
        let underscored = name.replace('-', "_");
        let sym = xkb::keysym_from_name(&underscored, xkb::KEYSYM_CASE_INSENSITIVE);
        if sym != no_symbol {
            return sym;
        }
    }

    // Try with XF86 prefix (Emacs strips it for media keys)
    let xf86_name = format!("XF86{}", name);
    let sym = xkb::keysym_from_name(&xf86_name, xkb::KEYSYM_CASE_INSENSITIVE);
    if sym != no_symbol {
        return sym;
    }

    // Fallback: Emacs compounds words in some key names, losing word boundaries.
    // Use a static table for these lossy mappings.
    if let Some((_, xkb_name)) = EMACS_TO_XKB_NAMES.iter().find(|(emacs, _)| *emacs == name) {
        return xkb::keysym_from_name(xkb_name, xkb::KEYSYM_NO_FLAGS);
    }

    no_symbol
}

/// Set intercepted and emulated keys (module mode).
/// KEYS is a vector of plists with :key :description :ctrl :alt :shift
/// :super :fullscreen :dispatch keys.
/// Keys with :emulate "ctrl" (etc.) go into the emulated set; the rest are intercepted.
#[defun]
fn intercept_keys_module(env: &Env, keys: Value<'_>) -> Result<()> {
    let mut parsed_keys = Vec::new();
    let mut parsed_emulated = Vec::new();

    let len: i64 = env.call(length, (keys,))?.into_rust()?;

    for i in 0..len {
        let key_spec = env.call(aref, (keys, i))?;

        // Resolve keysym from Emacs key description using libxkbcommon.
        // Emacs passes either an integer (Unicode codepoint) or a string (key name).
        let key_val = env.call(plist_get, (key_spec, kw_key))?;
        let description_val = env.call(plist_get, (key_spec, kw_description))?;
        let key_description = if let Ok(description) = description_val.into_rust::<String>() {
            description
        } else if let Ok(codepoint) = key_val.into_rust::<i64>() {
            char::from_u32(codepoint as u32)
                .map(|c| c.to_string())
                .unwrap_or_else(|| format!("U+{:X}", codepoint))
        } else if let Ok(name) = key_val.into_rust::<String>() {
            format!("<{name}>")
        } else {
            String::new()
        };
        let keysym = if let Ok(codepoint) = key_val.into_rust::<i64>() {
            xkb::utf32_to_keysym(codepoint as u32)
        } else if let Ok(name) = key_val.into_rust::<String>() {
            resolve_keysym_from_name(&name)
        } else {
            continue; // Skip invalid keys
        };

        if keysym == keysyms::KEY_NoSymbol.into() {
            let desc = if let Ok(v) = key_val.into_rust::<i64>() {
                format!("codepoint {}", v)
            } else if let Ok(v) = key_val.into_rust::<String>() {
                format!("name {:?}", v)
            } else {
                "unknown".to_string()
            };
            tracing::warn!("Could not resolve keysym for key {}, skipping", desc);
            continue;
        }

        // Helper to check if a value is truthy (not nil and not :false)
        let is_true = |v: Value| -> bool { v.is_not_nil() && v != *kw_false };

        // Extract modifier flags
        let ctrl_val = env.call(plist_get, (key_spec, kw_ctrl))?;
        let alt_val = env.call(plist_get, (key_spec, kw_alt))?;
        let shift_val = env.call(plist_get, (key_spec, kw_shift))?;
        let super_val = env.call(plist_get, (key_spec, kw_super))?;
        let allow_fullscreen_val = env.call(plist_get, (key_spec, kw_fullscreen))?;
        let dispatch_val = env.call(plist_get, (key_spec, kw_dispatch))?;
        let repeat_val = env.call(plist_get, (key_spec, kw_repeat))?;
        let dispatch = if let Ok(dispatch) = dispatch_val.into_rust::<String>() {
            match dispatch.as_str() {
                "command" => InterceptDispatch::Command,
                "keyboard" => InterceptDispatch::Keyboard,
                other => {
                    tracing::warn!("Unknown intercept dispatch {:?}, using keyboard", other);
                    InterceptDispatch::Keyboard
                }
            }
        } else {
            InterceptDispatch::Keyboard
        };

        // Check :emulate flag: keys with this go to emulated set
        let emulate_val = env.call(plist_get, (key_spec, kw_emulate))?;
        if let Ok(target_mod) = emulate_val.into_rust::<String>() {
            parsed_emulated.push(EmulatedKey {
                keysym: keysym.raw(),
                ctrl: is_true(ctrl_val),
                alt: is_true(alt_val),
                shift: is_true(shift_val),
                logo: is_true(super_val),
                target_mod,
            });
        } else {
            parsed_keys.push(InterceptedKey {
                keysym: keysym.raw(),
                key: key_description,
                ctrl: is_true(ctrl_val),
                alt: is_true(alt_val),
                shift: is_true(shift_val),
                logo: is_true(super_val),
                allow_fullscreen: is_true(allow_fullscreen_val),
                dispatch,
                repeat: is_true(repeat_val),
            });
        }
    }

    *intercepted_keys().write().unwrap() = parsed_keys;
    *emulated_keys().write().unwrap() = parsed_emulated;
    tracing::info!(
        "Intercepted keys set ({} intercept, {} emulate)",
        intercepted_keys().read().unwrap().len(),
        emulated_keys().read().unwrap().len(),
    );
    Ok(())
}

/// Commit text to a client text field.
/// SURFACE-ID identifies the target surface, used for queuing commits
/// that arrive while the client is in a disable->enable gap.
#[defun]
fn im_commit_module(text: String, surface_id: i64) -> Result<()> {
    push_command(ModuleCommand::ImCommit {
        text,
        surface_id: surface_id as u64,
    });
    Ok(())
}

/// Enable/disable text input interception (module mode).
/// ENABLED should be t or nil.
#[defun]
fn text_input_intercept_module(enabled: Value<'_>) -> Result<()> {
    push_command(ModuleCommand::TextInputIntercept {
        enabled: enabled.is_not_nil() && enabled != *kw_false,
    });
    Ok(())
}

/// Begin an Emacs keyboard-capture lease and return its token.
#[defun]
fn keyboard_capture_begin_module(reason: String) -> Result<i64> {
    let token = begin_keyboard_capture(reason);
    push_keyboard_capture_changed();
    Ok(token as i64)
}

/// End a keyboard-capture lease. Stale tokens are ignored.
#[defun]
fn keyboard_capture_end_module(token: i64) -> Result<bool> {
    if token <= 0 {
        return Ok(false);
    }
    let token = token as u64;
    let removed = end_keyboard_capture(token);
    if removed {
        push_keyboard_capture_changed();
    }
    Ok(removed)
}

/// Clear compositor-owned keyboard capture created by a keyboard redirect.
#[defun]
fn keyboard_redirect_capture_clear_module() -> Result<bool> {
    let removed = clear_keyboard_redirect_capture();
    if removed {
        push_keyboard_capture_changed();
    }
    Ok(removed)
}

/// Switch to named XKB layout (module mode).
#[defun]
fn switch_layout_module(layout: String) -> Result<()> {
    push_command(ModuleCommand::SwitchLayout { layout });
    Ok(())
}

/// Get current XKB layouts (module mode).
#[defun]
fn get_layouts_module() -> Result<()> {
    push_command(ModuleCommand::GetLayouts);
    Ok(())
}

/// Request verbose compositor state dump for debugging (module mode).
/// Blocks until the compositor replies; returns the JSON string, or nil on timeout.
#[defun]
fn get_debug_state_module() -> Result<Option<String>> {
    Ok(request_reply(
        ModuleCommand::GetDebugState,
        Duration::from_millis(200),
        "compositor state dump",
    ))
}

/// Set clipboard selection from Emacs (module mode).
#[defun]
fn set_selection_module(text: String) -> Result<()> {
    push_command(ModuleCommand::SetSelection { text });
    Ok(())
}

/// Configure native idle timeout (module mode).
/// TIMEOUT is seconds of inactivity (nil to disable).
/// ACTION is "blank" for monitor off, or a shell command string.
#[defun]
fn configure_idle_module(timeout: Option<i64>, action: Option<String>) -> Result<()> {
    let timeout_secs = timeout.and_then(|t| if t > 0 { Some(t as u64) } else { None });
    push_command(ModuleCommand::ConfigureIdle {
        timeout_secs,
        action: action.unwrap_or_else(|| "blank".to_string()),
    });
    Ok(())
}

/// Configure cursor auto-hide (module mode).
/// TIMEOUT is seconds of pointer inactivity before hiding (nil to disable).
/// HIDE-WHEN-TYPING (t/nil) hides the cursor on key press; pointer motion restores it.
#[defun]
fn configure_cursor_hide_module(timeout: Option<i64>, hide_when_typing: Value<'_>) -> Result<()> {
    let timeout_secs = timeout.and_then(|t| if t > 0 { Some(t as u64) } else { None });
    push_command(ModuleCommand::ConfigureCursorHide {
        timeout_secs,
        hide_when_typing: hide_when_typing.is_not_nil(),
    });
    Ok(())
}

/// Configure cursor theme and size.
#[defun]
fn configure_cursor_module(theme: Option<String>, size: i64) -> Result<()> {
    let config = cursor_config(theme, size)?;
    push_command(ModuleCommand::ConfigureCursor { config });
    Ok(())
}

/// Configure focus follows mouse (module mode).
/// STATE is the state of focus follows mouse mode (nil to disable).
#[defun]
fn set_focus_follows_mouse(state: Value<'_>) -> Result<()> {
    push_command(ModuleCommand::ConfigureFocusFollowsMouse {
        state: state.is_not_nil(),
    });
    Ok(())
}

/// Set alpha multiplier for unfocused toplevels (module mode).
/// ALPHA is clamped to 0.0..=1.0; 1.0 disables the effect.
#[defun]
fn set_unfocused_alpha(alpha: f64) -> Result<()> {
    let alpha = alpha.clamp(0.0, 1.0) as f32;
    push_command(ModuleCommand::ConfigureUnfocusedAlpha { alpha });
    Ok(())
}

/// Toggle cross-frame slide / open-fade animations (module mode).
/// When ENABLED is nil, in-flight animations snap to target and new ones
/// skip the easing.
#[defun]
fn set_animations_enabled(enabled: Value<'_>) -> Result<()> {
    push_command(ModuleCommand::ConfigureAnimations {
        enabled: enabled.is_not_nil(),
    });
    Ok(())
}

/// Set manual idle inhibition (module mode).
/// When INHIBITED is non-nil, idle timeout and idle notifications are
/// suppressed regardless of other inhibition sources.  When nil, manual
/// inhibition is lifted (other sources like D-Bus or Wayland protocol
/// may still inhibit idle).
#[defun]
fn set_idle_inhibited(inhibited: Value<'_>) -> Result<()> {
    push_command(ModuleCommand::SetIdleInhibited {
        inhibited: inhibited.is_not_nil(),
    });
    Ok(())
}

/// Configure input devices (module mode).
/// CONFIGS is a vector of plists, each with :device :type and setting properties.
#[defun]
fn configure_input_module(env: &Env, configs: Value<'_>) -> Result<()> {
    use crate::input;

    let len: i64 = env.call(length, (configs,))?.into_rust()?;
    let mut entries = Vec::new();

    for i in 0..len {
        let plist = env.call(aref, (configs, i))?;
        let mut entry = input::InputConfigEntry::default();

        // Walk plist key/value pairs. Compare the key symbol Value directly against
        // cached GlobalRefs, avoiding symbol-name -> String -> match on every iteration.
        let mut rest = plist;
        while rest.is_not_nil() {
            let key = env.call(car, (rest,))?;
            let val = env.call(cadr, (rest,))?;

            if key == *kw_device {
                if val.is_not_nil() {
                    entry.device = Some(val.into_rust()?);
                }
            } else if key == *kw_type {
                if let Some(v) = parse_optional_input_enum(
                    env,
                    ":type",
                    val,
                    input::DeviceType::parse,
                    input::DEVICE_TYPE_VALUES,
                )? {
                    entry.device_type = Some(v);
                }
            } else if key == *kw_natural_scroll {
                entry.natural_scroll = Some(val.is_not_nil());
            } else if key == *kw_tap {
                entry.tap = Some(val.is_not_nil());
            } else if key == *kw_dwt {
                entry.dwt = Some(val.is_not_nil());
            } else if key == *kw_left_handed {
                entry.left_handed = Some(val.is_not_nil());
            } else if key == *kw_middle_emulation {
                entry.middle_emulation = Some(val.is_not_nil());
            } else if key == *kw_accel_speed {
                if val.is_not_nil() {
                    entry.accel_speed = Some(value_as_f64(val)?);
                }
            } else if key == *kw_accel_profile {
                if let Some(v) = parse_optional_input_enum(
                    env,
                    ":accel-profile",
                    val,
                    input::parse_accel_profile,
                    input::ACCEL_PROFILE_VALUES,
                )? {
                    entry.accel_profile = Some(v);
                }
            } else if key == *kw_click_method {
                if let Some(v) = parse_optional_input_enum(
                    env,
                    ":click-method",
                    val,
                    input::parse_click_method,
                    input::CLICK_METHOD_VALUES,
                )? {
                    entry.click_method = Some(v);
                }
            } else if key == *kw_scroll_method {
                if let Some(v) = parse_optional_input_enum(
                    env,
                    ":scroll-method",
                    val,
                    input::parse_scroll_method,
                    input::SCROLL_METHOD_VALUES,
                )? {
                    entry.scroll_method = Some(v);
                }
            } else if key == *kw_tap_button_map {
                if let Some(v) = parse_optional_input_enum(
                    env,
                    ":tap-button-map",
                    val,
                    input::parse_tap_button_map,
                    input::TAP_BUTTON_MAP_VALUES,
                )? {
                    entry.tap_button_map = Some(v);
                }
            } else if key == *kw_repeat_delay {
                if val.is_not_nil() {
                    entry.repeat_delay = Some(val.into_rust::<i64>()? as i32);
                }
            } else if key == *kw_repeat_rate {
                if val.is_not_nil() {
                    entry.repeat_rate = Some(val.into_rust::<i64>()? as i32);
                }
            } else if key == *kw_xkb_layouts {
                if val.is_not_nil() {
                    entry.xkb_layouts = Some(val.into_rust()?);
                }
            } else if key == *kw_xkb_variants {
                if val.is_not_nil() {
                    entry.xkb_variants = Some(val.into_rust()?);
                }
            } else if key == *kw_xkb_options {
                if val.is_not_nil() {
                    entry.xkb_options = Some(val.into_rust()?);
                }
            } else {
                let key_name: String = env.call(symbol_name, (key,))?.into_rust()?;
                env.signal(
                    ewm_input_unknown_key,
                    (format!("Unknown input config key {key_name}"),),
                )?;
            }

            rest = env.call(cddr, (rest,))?;
        }

        entries.push(entry);
    }

    push_command(ModuleCommand::ConfigureInput { configs: entries });
    Ok(())
}

/// Toggle debug mode for verbose logging.
/// Returns new debug mode state (t or nil).
#[defun]
fn debug_mode_module(enabled: Option<Value<'_>>) -> Result<bool> {
    let new_state = match enabled {
        Some(v) => v.is_not_nil(),
        None => !DEBUG_MODE.load(Ordering::Relaxed),
    };
    DEBUG_MODE.store(new_state, Ordering::Relaxed);
    if new_state {
        tracing::info!("Debug mode ENABLED - verbose logging active");
    } else {
        tracing::info!("Debug mode DISABLED");
    }
    Ok(new_state)
}

/// Check if debug mode is enabled.
#[defun]
fn debug_mode_p() -> Result<bool> {
    Ok(DEBUG_MODE.load(Ordering::Relaxed))
}

/// List installed XDG desktop applications.
/// Returns an alist of (name . commandline) for apps that have a command.
/// Runs synchronously in the Emacs thread (GIO just reads .desktop files).
#[defun]
fn list_xdg_apps<'a>(env: &'a Env) -> Result<Value<'a>> {
    use gio::prelude::*;

    let mut result: Value<'a> = ().into_lisp(env)?;

    for app in gio::AppInfo::all() {
        if !app.should_show() {
            continue;
        }

        let name = app.name();
        let commandline = match app.commandline() {
            Some(c) => c.to_string_lossy().to_string(),
            None => continue,
        };

        let pair = env.cons(name.as_str(), commandline.as_str())?;
        result = env.cons(pair, result)?;
    }

    result = env.call("nreverse", (result,))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{nonzero_u64_from_i64, KeyboardCaptureState};

    #[test]
    fn nonzero_u64_from_i64_rejects_zero_and_negative_values() {
        assert_eq!(nonzero_u64_from_i64(-1), None);
        assert_eq!(nonzero_u64_from_i64(0), None);
        assert_eq!(nonzero_u64_from_i64(42).map(|id| id.get()), Some(42));
    }

    #[test]
    fn stale_keyboard_capture_end_keeps_newer_lease() {
        let mut state = KeyboardCaptureState::default();
        let old = state.begin("prefix".to_string());
        let new = state.begin("minibuffer".to_string());

        assert!(state.end(old));
        assert!(state.is_active());
        assert_eq!(state.debug_holders(), vec![(new, "minibuffer".to_string())]);
    }

    #[test]
    fn keyboard_capture_is_inactive_after_last_lease_ends() {
        let mut state = KeyboardCaptureState::default();
        let token = state.begin("read-key".to_string());

        assert!(state.is_active());
        assert!(state.end(token));
        assert!(!state.is_active());
    }

    #[test]
    fn ending_unknown_keyboard_capture_token_is_noop() {
        let mut state = KeyboardCaptureState::default();
        let token = state.begin("transient".to_string());

        assert!(!state.end(token + 1));
        assert!(state.is_active());
        assert_eq!(
            state.debug_holders(),
            vec![(token, "transient".to_string())]
        );
    }

    #[test]
    fn redirect_and_tokened_capture_clear_independently() {
        let mut state = KeyboardCaptureState::default();
        let token = state.begin("read-key".to_string());

        state.begin_redirect();
        assert!(state.clear_redirect());
        assert!(state.is_active());
        assert_eq!(state.debug_holders(), vec![(token, "read-key".to_string())]);

        state.begin_redirect();
        assert!(state.end(token));
        assert!(state.is_active());
        assert_eq!(
            state.debug_holders(),
            vec![(0, "keyboard-redirect".to_string())]
        );
    }

    #[test]
    fn duplicate_redirect_capture_is_a_noop() {
        let mut state = KeyboardCaptureState::default();

        state.begin_redirect();
        state.begin_redirect();
        assert_eq!(
            state.debug_holders(),
            vec![(0, "keyboard-redirect".to_string())]
        );
    }
}
