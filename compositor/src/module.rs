//! Emacs dynamic module interface for EWM
//!
//! # Design Invariants
//!
//! 1. **Thread safety**: The compositor runs in a separate thread from Emacs. Communication uses
//!    queues, pipes, and shared state:
//!    - `COMMAND_QUEUE`: Emacs -> Compositor commands (layouts, focus, etc.)
//!    - `EVENT_CHANNEL`: Compositor -> Emacs events, drainable only after the tick publishes
//!      `SHARED_STATE`. The pipe fd (open_channel) only carries a wakeup byte
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

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::anyhow;
use elisp::Value as Data;
use elisp_emacs::Lisp;
use emacs::{Env, Result, Value, defun};
use serde::Deserialize;
use smithay::input::keyboard::{keysyms, xkb};
use smithay::reexports::calloop::LoopSignal;
use tracing::{debug, info, trace};

use crate::config::{self, Config};
use crate::im::TextInputProvider;
use crate::input::SwipeDirection;
use crate::layout::Placement;
use crate::layout::strip::{self, Frame};
pub use crate::utils::reply::Reply;
use crate::utils::reply::{ReplyError, reply_channel};
use crate::{
    Event, FocusId, FocusSource, InterceptDispatch, InterceptedKey, LayoutEntry, LayoutEntryId,
    Notify, State, Trigger, tracy_span,
};

// Shared State (read by Emacs, written by compositor)

/// Compositor state snapshot, updated once per tick in `update_shared_state()`.
/// Only contains fields that Emacs reads synchronously via per-field defuns.
#[derive(Default)]
pub struct ActiveOutput {
    pub name: String,
    pub origin: (i32, i32),
}

#[derive(Default)]
pub struct FrameOrigin {
    pub surface_id: u64,
    pub origin: (i32, i32),
}

#[derive(Default)]
pub struct SharedState {
    pub focused_surface_id: u64,
    pub focused_frame_id: u64,
    pub pointer_location: (f64, f64),
    pub active_outputs: Vec<ActiveOutput>,
    pub frame_origins: Vec<FrameOrigin>,
    pub active_frames: HashMap<String, u64>,
    pub strip_frames: HashMap<String, Vec<u64>>,
    pub frame_outputs: HashMap<u64, String>,
}

static SHARED_STATE: OnceLock<Mutex<SharedState>> = OnceLock::new();

pub fn shared_state() -> &'static Mutex<SharedState> {
    SHARED_STATE.get_or_init(|| Mutex::new(SharedState::default()))
}

#[derive(Clone)]
pub struct SurroundingSnapshot {
    pub surface_id: u64,
    pub text: String,
    pub cursor: u32,
    pub anchor: u32,
}

/// Pulled synchronously by `ewm-edit` instead of pushed per keystroke.
static TEXT_INPUT_SURROUNDING: OnceLock<Mutex<Option<SurroundingSnapshot>>> = OnceLock::new();

fn text_input_surrounding() -> &'static Mutex<Option<SurroundingSnapshot>> {
    TEXT_INPUT_SURROUNDING.get_or_init(|| Mutex::new(None))
}

pub fn set_text_input_surrounding(snapshot: Option<SurroundingSnapshot>) {
    *text_input_surrounding().lock().unwrap() = snapshot;
}

/// Pending frame-to-output assignments (synchronous to avoid race with surface creation)
#[derive(Debug, Clone)]
pub struct PendingFrame {
    pub output: String,
    pub placement: Placement,
    /// Emacs's creation counter, echoed in `Event::New` to pair the surface with its frame.
    pub seq: u64,
}

static PENDING_FRAMES: OnceLock<Mutex<VecDeque<PendingFrame>>> = OnceLock::new();

fn pending_frames() -> &'static Mutex<VecDeque<PendingFrame>> {
    PENDING_FRAMES.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Queue FRAME for the next Emacs toplevel.
pub(crate) fn prepare_frame(frame: PendingFrame) {
    pending_frames().lock().unwrap().push_back(frame);
}

/// Take the next pending frame output (called by compositor when creating surfaces).
pub fn take_pending_frame() -> Option<PendingFrame> {
    pending_frames().lock().unwrap().pop_front()
}

/// Get pending frame outputs for state dump.
pub fn peek_pending_frame_outputs() -> Vec<String> {
    pending_frames()
        .lock()
        .unwrap()
        .iter()
        .map(|pending| {
            if matches!(pending.placement, Placement::Floating { .. }) {
                format!("{}:floating", pending.output)
            } else {
                pending.output.clone()
            }
        })
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

pub(crate) fn intercepted_keys() -> &'static RwLock<Vec<InterceptedKey>> {
    INTERCEPTED_KEYS.get_or_init(|| RwLock::new(Vec::new()))
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
fn get_active_outputs() -> Result<Lisp<Vec<String>>> {
    let state = shared_state().lock().unwrap();
    let names = state.active_outputs.iter().map(|o| o.name.clone());
    Ok(Lisp(names.collect()))
}

/// `(x . y)`.
fn point((x, y): (i32, i32)) -> Data {
    Data::cons(Data::Int(x.into()), Data::Int(y.into()))
}

/// Query output origin (called by Emacs for mouse-follows-focus pointer warping).
/// Returns (x . y) cons cell, or nil if output not found.
#[defun]
fn get_output_origin(name: String) -> Result<Lisp<Data>> {
    let state = shared_state().lock().unwrap();
    let output = state.active_outputs.iter().find(|o| o.name == name);
    Ok(Lisp(output.map_or(Data::NIL, |o| point(o.origin))))
}

/// Surface id of the strip frame shown on output NAME, or nil.
#[defun]
fn get_active_frame_id(name: String) -> Result<Option<i64>> {
    let state = shared_state().lock().unwrap();
    Ok(state.active_frames.get(&name).map(|id| *id as i64))
}

/// Surface ids of output NAME's strip frames, left to right.
#[defun]
fn get_strip_frame_ids(name: String) -> Result<Lisp<Vec<u64>>> {
    let state = shared_state().lock().unwrap();
    Ok(Lisp(
        state.strip_frames.get(&name).cloned().unwrap_or_default(),
    ))
}

/// Output name of the Emacs frame with SURFACE-ID, or nil while it is unplaced.
#[defun]
fn get_frame_output(surface_id: i64) -> Result<Option<String>> {
    let state = shared_state().lock().unwrap();
    Ok(u64::try_from(surface_id)
        .ok()
        .and_then(|id| state.frame_outputs.get(&id).cloned()))
}

/// Query a floating Emacs frame's origin for mouse-follows-focus pointer warping.
/// Returns (x . y) cons cell, or nil if the frame is not found.
#[defun]
fn get_frame_origin(surface_id: i64) -> Result<Lisp<Data>> {
    let state = shared_state().lock().unwrap();
    let frame = state
        .frame_origins
        .iter()
        .find(|f| u64::try_from(surface_id) == Ok(f.surface_id));
    Ok(Lisp(frame.map_or(Data::NIL, |f| point(f.origin))))
}

/// Query pointer location (called by Emacs for mouse-follows-focus).
/// Returns (x . y) cons cell in compositor coordinates.
#[defun]
fn get_pointer_location() -> Result<Lisp<Data>> {
    let (x, y) = shared_state().lock().unwrap().pointer_location;
    Ok(Lisp(Data::cons(Data::Float(x), Data::Float(y))))
}

// Module Commands (Emacs -> Compositor)

/// Commands sent from Emacs to the compositor via the module interface.
#[derive(Debug)]
pub enum ModuleCommand {
    Action {
        action: crate::Action,
    },
    /// Emacs read a Focus, so the keys deferred behind it can go.
    ReplayDeferredKeys,
    Close {
        id: u64,
    },
    WarpPointer {
        x: f64,
        y: f64,
    },
    ConfigureOutput {
        name: String,
        config: OutputConfig,
    },
    ImCommit {
        text: String,
        surface_id: u64,
    },
    ImPreedit {
        text: String,
        surface_id: u64,
    },
    TextInputReplace {
        text: String,
        surface_id: u64,
    },
    TextInputForwardKey {
        surface_id: u64,
        keycode: u32,
        ctrl: bool,
        alt: bool,
        shift: bool,
        logo: bool,
    },
    TextInputIntercept {
        enabled: bool,
    },
    ConfigureTextInput {
        provider: TextInputProvider,
    },
    SendKey {
        surface_id: Option<u64>,
        keys: Vec<ChordSpec>,
    },
    KeyboardCaptureChanged {
        active: bool,
    },
    SwitchLayout {
        layout: String,
    },
    GetLayouts,
    /// Request verbose state dump for debugging (ewm-show-state).
    /// Compositor sends the state as Lisp data back via the reply channel.
    GetDebugState(Reply<Data>),
    /// Query the tiled entry id under a floating frame's center.
    EntryUnderFloatingCenter {
        floating_frame_id: u64,
        reply: Reply<Option<LayoutEntryId>>,
    },
    /// Request activation token creation. Compositor sends the token string back via the reply
    /// channel.
    CreateActivationToken(Reply<String>),
    /// Set clipboard selection from Emacs
    SetSelection {
        text: String,
    },
    /// Contents of every Emacs frame. Nothing moves, and focus changes only via `FocusTarget'.
    Layout {
        frames: Vec<Frame>,
    },
    /// Move an Emacs frame `delta` slots along its strip.
    MoveFrame {
        id: u64,
        delta: i32,
    },
    /// Move a floating Emacs frame by a logical-pixel delta.
    MoveFloatingFrame {
        id: u64,
        dx: f64,
        dy: f64,
    },
    /// Resize a floating Emacs frame by a logical-pixel delta.
    ResizeFloatingFrame {
        id: u64,
        dw: f64,
        dh: f64,
    },
    /// Toggle compositor-owned fullscreen for a layout view.
    ToggleFullscreen {
        entry_id: LayoutEntryId,
    },
    SetFullscreen {
        entry_id: LayoutEntryId,
        on: bool,
    },
    /// Every option Emacs sets, applied where it changed.
    Configure {
        config: Config,
    },
    /// Mirror whether Emacs' `track-mouse' is `drag-source'.
    SetDragSource {
        active: bool,
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
    /// Sole channel for Emacs-initiated focus. Bypasses xdg_activation.
    FocusTarget {
        focus_id: NonZeroU64,
    },
    /// Set manual idle inhibition (from Emacs)
    SetIdleInhibited {
        inhibited: bool,
    },
    /// Monitors off until a waking input, unlock or `PowerOnMonitors`
    PowerOffMonitors,
    /// Monitors back on
    PowerOnMonitors,
    /// Client init done. Force-resend wl_keyboard.enter (PGTK drops the
    /// pre-roundtrip one).
    Initialized,
}

impl State {
    /// Handle a module command (from Emacs via dynamic module).
    pub(crate) fn handle_module_command(&mut self, cmd: ModuleCommand) {
        tracy_span!("handle_module_command");
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
                    self.ewm
                        .set_focus_entry(focus_id, FocusSource::LispRequest, Notify::None);
                } else if let Some(frame_id) = self.ewm.layout.frame_with_focus_id(focus_id) {
                    self.ewm
                        .set_focus_frame(frame_id, FocusSource::LispRequest, Notify::None);
                }
            }
            ModuleCommand::WarpPointer { x, y } => {
                // Don't warp while a pen is in use.
                if self.ewm.tablet_cursor_location.is_none() {
                    self.warp_pointer(x, y);
                }
            }
            ModuleCommand::ConfigureOutput { name, config } => self.configure_output(name, config),
            ModuleCommand::ImPreedit { text, surface_id } => {
                if self.ewm.active_text_input_surface_id() == Some(surface_id) {
                    self.ewm.preedit_active_text_input(text);
                }
            }
            ModuleCommand::ImCommit { text, surface_id } => self.ewm.im_commit(text, surface_id),
            ModuleCommand::TextInputReplace { text, surface_id } => {
                self.ewm.text_input_replace(text, surface_id);
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
            ModuleCommand::SendKey { surface_id, keys } => self.send_keys(surface_id, keys),
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
            ModuleCommand::SwitchLayout { layout } => self.switch_xkb_layout(layout),
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
                    // Emacs gave up before we replied. The token would otherwise leak.
                    self.ewm.activation_state.remove_token(&token);
                }
            }
            ModuleCommand::SetSelection { text } => self.set_selection(text),
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
            ModuleCommand::Configure { config } => self.apply_config(config),
            ModuleCommand::SetDragSource { active } => {
                self.ewm.emacs_drag_source = active;
            }
            ModuleCommand::SetIdleInhibited { inhibited } => {
                self.ewm.manual_idle_inhibited = inhibited;
            }
            ModuleCommand::PowerOffMonitors => {
                self.ewm.deactivate_monitors();
                self.backend.clear_all_surfaces();
            }
            ModuleCommand::PowerOnMonitors => self.ewm.activate_monitors(),
            ModuleCommand::Initialized => self.replay_keyboard_focus(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverviewAction {
    Toggle,
    Open,
    Close,
}

/// Command queue shared between Emacs thread and compositor
static COMMAND_QUEUE: OnceLock<Mutex<Vec<ModuleCommand>>> = OnceLock::new();

fn command_queue() -> &'static Mutex<Vec<ModuleCommand>> {
    COMMAND_QUEUE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Take every command Emacs queued, once per compositor tick.
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
    // the compositor drains commands. The begin still needs to cancel repeat.
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
/// reply. Fast-fails with `None` if the compositor isn't running so callers
/// don't burn the full timeout when there's no one to answer.
fn request_reply<T, F>(make_cmd: F, timeout: Duration, label: &str) -> Option<T>
where
    F: FnOnce(Reply<T>) -> ModuleCommand,
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
    pub source: FocusSource,
}

/// Focus history shared between compositor and Emacs
static FOCUS_HISTORY: OnceLock<Mutex<VecDeque<FocusEvent>>> = OnceLock::new();
static FOCUS_SEQ: AtomicUsize = AtomicUsize::new(0);

fn focus_history() -> &'static Mutex<VecDeque<FocusEvent>> {
    FOCUS_HISTORY.get_or_init(|| Mutex::new(VecDeque::with_capacity(FOCUS_HISTORY_SIZE)))
}

/// Record a focus change (called by compositor)
pub fn record_focus(surface_id: u64, source: FocusSource) {
    let seq = FOCUS_SEQ.fetch_add(1, Ordering::Relaxed);
    let event = FocusEvent {
        seq,
        surface_id,
        source,
    };

    if DEBUG_MODE.load(Ordering::Relaxed) {
        tracing::debug!("Focus #{}: {:?} -> {}", seq, source, surface_id);
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

// Event channel (compositor -> Emacs): a queue drained by `drain_events`, the pipe as wakeup.

struct EventChannel {
    /// Staged until `flush_events`.
    pending: VecDeque<crate::event::Event>,
    queue: VecDeque<crate::event::Event>,
    wakeup: Box<dyn std::io::Write + Send>,
    /// Coalescing may empty the queue without consuming its wakeup.
    notified: bool,
}

impl EventChannel {
    fn new(wakeup: Box<dyn std::io::Write + Send>) -> Self {
        Self {
            pending: VecDeque::new(),
            queue: VecDeque::new(),
            wakeup,
            notified: false,
        }
    }

    /// Queue `event`, dropping the unread events it supersedes.
    fn push(&mut self, event: crate::event::Event) {
        self.pending.retain(|earlier| !event.supersedes(earlier));
        self.queue.retain(|earlier| !event.supersedes(earlier));
        self.pending.push_back(event);
    }

    /// Publish staged events, sending a wakeup only when none is outstanding.
    fn flush(&mut self) {
        use std::io::Write;
        self.queue.append(&mut self.pending);
        if !self.queue.is_empty() && !self.notified {
            match self.wakeup.write_all(b"\0") {
                Ok(()) => self.notified = true,
                // A full pipe already has a wakeup for the reader.
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => self.notified = true,
                Err(err) => tracing::warn!("event wakeup failed: {err}"),
            }
        }
    }

    fn take(&mut self) -> Vec<crate::event::Event> {
        self.notified = false;
        self.queue.drain(..).collect()
    }

    /// Whether a Focus Emacs has not read is staged or queued.
    fn focus_unread(&self) -> bool {
        self.pending
            .iter()
            .chain(self.queue.iter())
            .any(|event| matches!(event, crate::event::Event::Focus { .. }))
    }
}

/// Set by `init_event_channel`, before the compositor thread starts.
static EVENT_CHANNEL: Mutex<Option<EventChannel>> = Mutex::new(None);

/// Register the pipe process whose filter drains events with `drain_events`.
#[defun]
fn init_event_channel(env: &Env, pipe_process: Value<'_>) -> Result<()> {
    let writer = env.open_channel(pipe_process)?;
    // open_channel hides the writer behind impl Write. It is a PipeWriter, the only way to its fd.
    let pipe = (&writer as &dyn std::any::Any)
        .downcast_ref::<std::io::PipeWriter>()
        .ok_or_else(|| anyhow!("emacs-rs event channel is not a PipeWriter"))?;
    crate::utils::set_nonblocking(pipe)?;
    install_event_channel(Box::new(writer));
    Ok(())
}

pub(crate) fn install_event_channel(wakeup: Box<dyn std::io::Write + Send>) {
    *EVENT_CHANNEL.lock().unwrap() = Some(EventChannel::new(wakeup));
}

pub(crate) fn clear_event_channel() {
    *EVENT_CHANNEL.lock().unwrap() = None;
}

/// Whether Emacs still has a Focus to read.
pub(crate) fn focus_unread() -> bool {
    EVENT_CHANNEL
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(EventChannel::focus_unread)
}

/// Queue `event` for Emacs.
pub(crate) fn push_event(event: crate::event::Event) {
    if let Some(channel) = EVENT_CHANNEL.lock().unwrap().as_mut() {
        channel.push(event);
    }
}

/// Hand this tick's events to Emacs, waking the pipe filter when it was idle.
pub(crate) fn flush_events() {
    if let Some(channel) = EVENT_CHANNEL.lock().unwrap().as_mut() {
        channel.flush();
    }
}

/// The queued events, oldest first, as Lisp data.
#[defun]
fn drain_events() -> Result<Lisp<Vec<crate::event::Event>>> {
    Ok(Lisp(take_events()))
}

/// Take the queued events, releasing the keys that waited on a Focus among them.
pub(crate) fn take_events() -> Vec<crate::event::Event> {
    let events: Vec<crate::event::Event> = match EVENT_CHANNEL.lock().unwrap().as_mut() {
        Some(channel) => channel.take(),
        None => Vec::new(),
    };
    if events
        .iter()
        .any(|event| matches!(event, crate::event::Event::Focus { .. }))
    {
        push_command(ModuleCommand::ReplayDeferredKeys);
    }
    events
}

/// A greeting, to check the module loaded.
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

/// Resume GdkWindow paint updates. Flushes any queued expose.
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
/// Filter controlled by RUST_LOG env var (default: ewm=info,smithay=warn).
/// View logs with: journalctl --user -t ewm -f
fn init_logging() {
    use std::sync::Once;

    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::prelude::*;

    static INIT_LOG: Once = Once::new();
    INIT_LOG.call_once(|| {
        let default_filter = "ewm=info,smithay=warn";
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

/// The cursor THEME and SIZE Lisp asks for, checked.
fn cursor_config(theme: Option<String>, size: i64) -> Result<config::Cursor> {
    if theme.as_ref().is_some_and(|theme| theme.is_empty()) {
        return Err(anyhow!("cursor theme must not be empty"));
    }

    let size = u8::try_from(size)
        .ok()
        .filter(|size| *size > 0)
        .ok_or_else(|| anyhow!("cursor size must be in 1..=255"))?;

    Ok(config::Cursor {
        theme,
        size: Some(size),
    })
}

/// Start the compositor in a background thread.
/// Must be called from a TTY (not inside another compositor).
/// Returns t if started successfully, nil if already running.
#[defun]
fn start(cursor_theme: Option<String>, cursor_size: i64) -> Result<bool> {
    use crate::backend::drm::run_drm;

    init_logging();
    let cursor = cursor_config(cursor_theme, cursor_size)?;

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
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_drm(cursor)));

        match result {
            Ok(Ok(())) => {
                tracing::info!("Compositor thread exiting normally");
            }
            Ok(Err(e)) => {
                tracing::error!("Compositor error: {:#}", e);
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

/// One Emacs window of a frame as Lisp spells it. `surface-id` names the view it shows.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct EntrySpec {
    entry_id: LayoutEntryId,
    name: String,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    surface_id: Option<u64>,
}

/// One Emacs frame as Lisp spells it. Entries are relative to its working area.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct FrameSpec {
    name: String,
    surface_id: u64,
    #[serde(default)]
    owner: crate::layout::strip::Owner,
    focus_id: Option<FocusId>,
    selected_entry_id: Option<LayoutEntryId>,
    width: f64,
    height: f64,
    entries: Vec<EntrySpec>,
}

impl FrameSpec {
    /// `None` drops a frame whose focus id is still in flight.
    fn frame(self) -> Option<Frame> {
        let Some(focus_id) = self.focus_id else {
            tracing::warn!("frame {} layout dropped: missing focus_id", self.surface_id);
            return None;
        };
        let entries = self
            .entries
            .into_iter()
            .map(|e| match e.surface_id.filter(|&id| id != 0) {
                Some(sid) => LayoutEntry::surface(e.entry_id, e.name, sid, e.x, e.y, e.w, e.h),
                None => LayoutEntry::emacs_window(e.entry_id, e.name, e.x, e.y, e.w, e.h),
            });
        Some(Frame {
            name: self.name,
            owner: self.owner,
            focus_id,
            width: self.width,
            height: self.height,
            entries: entries.collect(),
            selected_entry_id: self.selected_entry_id,
            ..Frame::new(self.surface_id)
        })
    }
}

fn layout_frames(frames: Lisp<Vec<FrameSpec>>) -> Vec<Frame> {
    frames.0.into_iter().filter_map(FrameSpec::frame).collect()
}

/// Declare the contents of every Emacs frame, a vector of `FrameSpec` plists.
#[defun]
fn layout_module(frames: Lisp<Vec<FrameSpec>>) -> Result<()> {
    push_command(ModuleCommand::Layout {
        frames: layout_frames(frames),
    });
    Ok(())
}

/// Move Emacs frame ID by DELTA slots along its strip.
#[defun]
fn move_frame_module(id: Lisp<u64>, delta: Lisp<i32>) -> Result<()> {
    push_command(ModuleCommand::MoveFrame {
        id: id.0,
        delta: delta.0,
    });
    Ok(())
}

/// Move floating frame ID by DX/DY logical pixels.
#[defun]
fn move_floating_frame_module(id: i64, dx: Lisp<f64>, dy: Lisp<f64>) -> Result<()> {
    if id <= 0 {
        tracing::warn!("move_floating_frame_module: ignored invalid id {id}");
        return Ok(());
    }
    push_command(ModuleCommand::MoveFloatingFrame {
        id: id as u64,
        dx: dx.0,
        dy: dy.0,
    });
    Ok(())
}

/// Resize floating frame ID by DW/DH logical pixels.
#[defun]
fn resize_floating_frame_module(id: i64, dw: Lisp<f64>, dh: Lisp<f64>) -> Result<()> {
    if id <= 0 {
        tracing::warn!("resize_floating_frame_module: ignored invalid id {id}");
        return Ok(());
    }
    push_command(ModuleCommand::ResizeFloatingFrame {
        id: id as u64,
        dw: dw.0,
        dh: dh.0,
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
    if let Some(entry_id) = NonZeroU64::new(entry_id.max(0) as u64) {
        push_command(ModuleCommand::ToggleFullscreen { entry_id });
    }
    Ok(())
}

/// Set compositor-owned fullscreen for ENTRY-ID to ON.
#[defun]
fn set_fullscreen_module(entry_id: i64, on: Value<'_>) -> Result<()> {
    if let Some(entry_id) = NonZeroU64::new(entry_id.max(0) as u64) {
        push_command(ModuleCommand::SetFullscreen {
            entry_id,
            on: on.is_not_nil(),
        });
    }
    Ok(())
}

/// Focus the active frame on the output adjacent to the current one.
/// DX/DY are -1/+1 along one axis. The other axis is 0.
#[defun]
fn focus_output_direction_module(dx: i64, dy: i64) -> Result<()> {
    push_command(ModuleCommand::Action {
        action: crate::Action::FocusOutput {
            dx: dx as i32,
            dy: dy as i32,
        },
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

/// Sole focus channel from Lisp. FOCUS-ID names a frame's own surface or a layout entry.
#[defun]
fn focus_target_module(focus_id: i64) -> Result<()> {
    let Some(focus_id) = NonZeroU64::new(focus_id as u64) else {
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
fn warp_pointer_module(x: Lisp<f64>, y: Lisp<f64>) -> Result<()> {
    push_command(ModuleCommand::WarpPointer { x: x.0, y: y.0 });
    Ok(())
}

/// Prepare the next frame as Emacs frame SEQ on OUTPUT, right of strip frame AFTER
/// when given (synchronous to avoid a race with surface creation).
#[defun]
fn prepare_frame_module(output: String, seq: Lisp<u64>, after: Lisp<Option<u64>>) -> Result<()> {
    tracing::info!("Prepared frame for output {}", output);
    prepare_frame(PendingFrame {
        output,
        placement: Placement::Tiled { after: after.0 },
        seq: seq.0,
    });
    Ok(())
}

/// Prepare the next frame as Emacs frame SEQ floating on OUTPUT, optionally at top-left X/Y.
#[defun]
fn prepare_floating_frame_module(
    output: String,
    seq: Lisp<u64>,
    x: Lisp<Option<f64>>,
    y: Lisp<Option<f64>>,
) -> Result<()> {
    let pos = x.0.zip(y.0).map(smithay::utils::Point::from);
    tracing::info!("Prepared floating frame for output {}", output);
    prepare_frame(PendingFrame {
        output,
        placement: Placement::Floating { pos },
        seq: seq.0,
    });
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

/// An output's requested configuration as Lisp spells it. Absent keys leave things as they are.
///
/// `custom` generates a CVT mode for `width`/`height`/`refresh` instead of matching an
/// advertised one. `modeline` takes precedence over the mode fields. `transform` counts
/// 0=Normal, 1=90, 2=180, 3=270, 4=Flipped, 5=Flipped90, 6=Flipped180, 7=Flipped270.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct OutputConfig {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub refresh: Option<f64>,
    pub custom: bool,
    pub modeline: Option<crate::output_mode::Modeline>,
    pub scale: Option<f64>,
    pub transform: Option<i32>,
    /// `nil` disables the output, absence leaves it.
    #[serde(deserialize_with = "elisp::serde::present")]
    pub enabled: Option<bool>,
}

/// Configure output NAME from CONFIG, an `OutputConfig` plist.
#[defun]
fn configure_output_module(name: String, config: Lisp<OutputConfig>) -> Result<()> {
    push_command(ModuleCommand::ConfigureOutput {
        name,
        config: config.0,
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

/// `swipe-FINGERS-DIRECTION` as a trigger.
fn swipe_trigger(name: &str) -> Option<Trigger> {
    let (fingers, direction) = name.strip_prefix("swipe-")?.split_once('-')?;
    let fingers = fingers.parse().ok()?;
    let direction = match direction {
        "up" => SwipeDirection::Up,
        "down" => SwipeDirection::Down,
        "left" => SwipeDirection::Left,
        "right" => SwipeDirection::Right,
        _ => return None,
    };
    Some(Trigger::Swipe { fingers, direction })
}

/// A key as Emacs names it: a character code or a key name like `left`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum KeyName {
    Code(u32),
    Name(String),
}

impl KeyName {
    /// The keysym, `None` when libxkbcommon knows no such name.
    pub fn keysym(&self) -> Option<xkb::Keysym> {
        let keysym = match self {
            KeyName::Code(code) => xkb::utf32_to_keysym(*code),
            KeyName::Name(name) => resolve_keysym_from_name(name),
        };
        (keysym != keysyms::KEY_NoSymbol.into()).then_some(keysym)
    }

    /// The trigger this names, a swipe before a keysym.
    fn trigger(&self) -> Option<Trigger> {
        if let KeyName::Name(name) = self
            && let Some(swipe) = swipe_trigger(name)
        {
            return Some(swipe);
        }
        self.keysym().map(|keysym| Trigger::Keysym(keysym.raw()))
    }

    fn description(&self) -> String {
        match self {
            KeyName::Code(code) => char::from_u32(*code)
                .map(String::from)
                .unwrap_or_else(|| format!("U+{code:X}")),
            KeyName::Name(name) => format!("<{name}>"),
        }
    }
}

/// One intercepted key as Lisp spells it.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) struct KeySpec {
    key: KeyName,
    description: Option<String>,
    #[serde(default)]
    ctrl: bool,
    #[serde(default)]
    alt: bool,
    #[serde(default)]
    shift: bool,
    #[serde(rename = "super", default)]
    logo: bool,
    #[serde(default)]
    fullscreen: bool,
    dispatch: Option<Dispatch>,
    #[serde(default)]
    repeat: bool,
    surface: Option<u64>,
    action: Option<crate::Action>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Dispatch {
    Keyboard,
    Command,
}

/// A key with the modifiers a client should see it under, as Lisp names it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ChordSpec {
    pub key: KeyName,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(rename = "super", default)]
    pub logo: bool,
}

impl KeySpec {
    /// Resolve the trigger. `None` skips a key neither a swipe nor libxkbcommon can name.
    fn resolve(self) -> Option<InterceptedKey> {
        let Some(trigger) = self.key.trigger() else {
            let key = self.key.description();
            tracing::warn!("Could not resolve key {key}, skipping");
            return None;
        };
        let dispatch = match (self.action, self.dispatch) {
            (Some(action), _) => InterceptDispatch::Action(action),
            (None, Some(Dispatch::Command)) => InterceptDispatch::Command,
            (None, _) => InterceptDispatch::Keyboard,
        };
        Some(InterceptedKey {
            trigger,
            key: self.description.unwrap_or_else(|| self.key.description()),
            ctrl: self.ctrl,
            alt: self.alt,
            shift: self.shift,
            logo: self.logo,
            allow_fullscreen: self.fullscreen,
            dispatch,
            repeat: self.repeat,
            surface: self.surface,
        })
    }
}

pub(crate) fn resolve_keys(specs: Vec<KeySpec>) -> Vec<InterceptedKey> {
    specs.into_iter().filter_map(KeySpec::resolve).collect()
}

/// Parse one printed Lisp form as `T`.
pub(crate) fn from_lisp<T: serde::de::DeserializeOwned>(text: &str) -> T {
    let payload = elisp::read(text)
        .next()
        .expect("a payload")
        .expect("a readable payload");
    elisp::serde::from_value(&payload).expect("the expected shape")
}

pub(crate) fn intercepted_keys_from_lisp(text: &str) -> Vec<InterceptedKey> {
    resolve_keys(from_lisp(text))
}

/// Set intercepted keys from KEYS, a vector of `KeySpec` plists.
#[defun]
fn intercept_keys_module(keys: Lisp<Vec<KeySpec>>) -> Result<()> {
    let keys = resolve_keys(keys.0);
    tracing::debug!("Intercepted keys set ({} keys)", keys.len());
    *intercepted_keys().write().unwrap() = keys;
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

/// Show TEXT as the composing text of the field on SURFACE-ID.
#[defun]
fn im_preedit_module(text: String, surface_id: i64) -> Result<()> {
    push_command(ModuleCommand::ImPreedit {
        text,
        surface_id: surface_id as u64,
    });
    Ok(())
}

/// (SURFACE-ID TEXT CURSOR ANCHOR) as handed to Lisp.
type SurroundingList = (u64, String, u32, u32);

/// Surrounding text of the active field as (SURFACE-ID TEXT CURSOR ANCHOR), or nil.
#[defun]
fn text_input_surrounding_module() -> Result<Lisp<Option<SurroundingList>>> {
    let snapshot = text_input_surrounding().lock().unwrap().clone();
    Ok(Lisp(
        snapshot.map(|s| (s.surface_id, s.text, s.cursor, s.anchor)),
    ))
}

/// Replace the whole content of the active field on SURFACE-ID with TEXT.
#[defun]
fn text_input_replace_module(text: String, surface_id: i64) -> Result<()> {
    push_command(ModuleCommand::TextInputReplace {
        text,
        surface_id: surface_id as u64,
    });
    Ok(())
}

/// Forward a key that Emacs declined to translate back to SURFACE-ID.
#[defun]
fn text_input_forward_key_module(
    surface_id: i64,
    keycode: i64,
    ctrl: Value<'_>,
    alt: Value<'_>,
    shift: Value<'_>,
    logo: Value<'_>,
) -> Result<()> {
    if surface_id <= 0 || keycode <= 0 {
        return Ok(());
    }
    push_command(ModuleCommand::TextInputForwardKey {
        surface_id: surface_id as u64,
        keycode: keycode as u32,
        ctrl: ctrl.is_not_nil(),
        alt: alt.is_not_nil(),
        shift: shift.is_not_nil(),
        logo: logo.is_not_nil(),
    });
    Ok(())
}

/// Tap KEYS on SURFACE-ID, or on the focused surface when nil, while it has focus.
#[defun]
fn send_key_module(surface_id: Lisp<Option<u64>>, keys: Lisp<Vec<ChordSpec>>) -> Result<()> {
    push_command(ModuleCommand::SendKey {
        surface_id: surface_id.0,
        keys: keys.0,
    });
    Ok(())
}

/// Enable/disable text input interception (module mode).
#[defun]
fn text_input_intercept_module(enabled: Value<'_>) -> Result<()> {
    push_command(ModuleCommand::TextInputIntercept {
        enabled: enabled.is_not_nil(),
    });
    Ok(())
}

/// Set who serves client text input: `emacs' or `external'.
#[defun]
fn configure_text_input_module(provider: Lisp<TextInputProvider>) -> Result<()> {
    push_command(ModuleCommand::ConfigureTextInput {
        provider: provider.0,
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

/// Whether Emacs holds the keyboard.
#[defun]
fn keyboard_capture_active_module() -> Result<bool> {
    Ok(get_keyboard_capture())
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
/// Blocks until the compositor replies. Returns the state as Lisp data, or nil on timeout.
#[defun]
fn get_debug_state_module() -> Result<Option<Lisp<Data>>> {
    Ok(request_reply(
        ModuleCommand::GetDebugState,
        Duration::from_millis(200),
        "compositor state dump",
    )
    .map(Lisp))
}

/// Tiled entry id under a floating frame's center, or nil (module mode).
#[defun]
fn entry_under_floating_center_module(floating_frame_id: i64) -> Result<Option<i64>> {
    let id = floating_frame_id.max(0) as u64;
    let reply = request_reply(
        move |reply| ModuleCommand::EntryUnderFloatingCenter {
            floating_frame_id: id,
            reply,
        },
        Duration::from_millis(200),
        "entry under floating center",
    );
    Ok(reply.flatten().map(|entry| entry.get() as i64))
}

/// Set clipboard selection from Emacs (module mode).
#[defun]
fn set_selection_module(text: String) -> Result<()> {
    push_command(ModuleCommand::SetSelection { text });
    Ok(())
}

/// Apply CONFIG, every option as one plist (module mode). Returns the sections that did not
/// parse as (KEY MESSAGE) lists, each left at its default.
#[defun]
fn configure_module(config: Lisp<Data>) -> Result<Lisp<Vec<(String, String)>>> {
    let (config, rejected) = Config::from_sections(&config.0);
    push_command(ModuleCommand::Configure { config });
    Ok(Lisp(rejected))
}

/// Tell the compositor whether Emacs is tracking a drag source (module mode).
/// ACTIVE is non-nil while `track-mouse' is `drag-source'.
#[defun]
fn set_drag_source(active: Value<'_>) -> Result<()> {
    push_command(ModuleCommand::SetDragSource {
        active: active.is_not_nil(),
    });
    Ok(())
}

/// Drive the overview (module mode). ACTION is "toggle", "open" or "close".
#[defun]
fn overview_module(action: String) -> Result<()> {
    let action = match action.as_str() {
        "toggle" => OverviewAction::Toggle,
        "open" => OverviewAction::Open,
        "close" => OverviewAction::Close,
        other => return Err(anyhow!("unknown overview action: {other}")),
    };
    push_command(ModuleCommand::Action {
        action: crate::Action::Overview(action),
    });
    Ok(())
}

/// Slide every frame off its output, or bring them back (module mode).
#[defun]
fn hide_toggle_module() -> Result<()> {
    push_command(ModuleCommand::Action {
        action: crate::Action::HideToggle,
    });
    Ok(())
}

/// Run ACTION in the compositor, as a key bound to it would.
#[defun]
fn action_module(action: Lisp<crate::Action>) -> Result<()> {
    push_command(ModuleCommand::Action { action: action.0 });
    Ok(())
}

/// Set manual idle inhibition (module mode).
/// When INHIBITED is non-nil, idle timeout and idle notifications are
/// suppressed regardless of other inhibition sources. When nil, manual
/// inhibition is lifted (other sources like D-Bus or Wayland protocol
/// may still inhibit idle).
#[defun]
fn set_idle_inhibited(inhibited: Value<'_>) -> Result<()> {
    push_command(ModuleCommand::SetIdleInhibited {
        inhibited: inhibited.is_not_nil(),
    });
    Ok(())
}

/// Power off all monitors (module mode).
#[defun]
fn power_off_monitors_module() -> Result<()> {
    push_command(ModuleCommand::PowerOffMonitors);
    Ok(())
}

/// Power all monitors back on (module mode).
#[defun]
fn power_on_monitors_module() -> Result<()> {
    push_command(ModuleCommand::PowerOnMonitors);
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
/// Returns an alist of (name . commandline) strings for apps that have a command, sorted by name.
/// Runs synchronously in the Emacs thread (GIO just reads .desktop files).
#[defun]
fn list_xdg_apps() -> Result<Lisp<Data>> {
    use gio::prelude::*;

    let apps: BTreeMap<String, String> = gio::AppInfo::all()
        .into_iter()
        .filter(|app| app.should_show())
        .filter_map(|app| {
            let commandline = app.commandline()?.to_string_lossy().into_owned();
            Some((app.name().to_string(), commandline))
        })
        .collect();
    // Built by hand: serialized as a map, the names would intern as symbols.
    let pair = |(name, commandline)| Data::cons(Data::Str(name), Data::Str(commandline));
    Ok(Lisp(Data::List(apps.into_iter().map(pair).collect())))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::{EventChannel, KeyboardCaptureState};
    use crate::event::Event;

    fn focus(n: u64) -> Event {
        Event::Focus {
            focus_id: NonZeroU64::new(n).unwrap(),
            x: None,
            y: None,
            pointer: false,
        }
    }

    fn printed(events: impl IntoIterator<Item = Event>) -> Vec<String> {
        events
            .into_iter()
            .map(|event| elisp::serde::to_value(&event).unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_newer_state_event_replaces_the_unread_one_in_place() {
        let mut channel = EventChannel::new(Box::new(Vec::new()));
        channel.push(focus(1));
        channel.push(Event::Close { id: 9 });
        channel.flush();
        channel.push(Event::Overview { open: true });
        channel.push(focus(2));
        channel.push(Event::Overview { open: false });
        let unread = channel.queue.iter().chain(channel.pending.iter()).cloned();
        assert_eq!(
            printed(unread),
            printed([
                Event::Close { id: 9 },
                focus(2),
                Event::Overview { open: false }
            ])
        );
    }

    #[test]
    fn request_events_are_never_dropped() {
        let mut channel = EventChannel::new(Box::new(Vec::new()));
        channel.push(Event::FrameHidden { id: 1 });
        channel.push(Event::Close { id: 1 });
        channel.push(Event::FrameShown { id: 1 });
        channel.push(Event::FrameHidden { id: 1 });
        assert_eq!(channel.pending.len(), 4);
    }

    #[test]
    fn coalescing_does_not_send_repeated_wakeups_to_a_stalled_reader() {
        use std::io::Read;
        let (mut reader, writer) = std::io::pipe().unwrap();
        crate::utils::set_nonblocking(&reader).unwrap();
        crate::utils::set_nonblocking(&writer).unwrap();
        let mut channel = EventChannel::new(Box::new(writer));
        for n in 1..100_000 {
            channel.push(focus(n));
            channel.flush();
        }
        assert_eq!(channel.queue.len(), 1);
        let mut bytes = [0; 16];
        assert_eq!(reader.read(&mut bytes).unwrap(), 1);
        assert_eq!(
            reader.read(&mut bytes).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(
            matches!(channel.take()[0], Event::Focus { focus_id, .. } if focus_id.get() == 99_999)
        );
        channel.push(focus(100_000));
        channel.flush();
        assert_eq!(reader.read(&mut bytes).unwrap(), 1);
    }

    #[test]
    fn a_full_wakeup_pipe_does_not_block_publication() {
        use std::io::{Read, Write};
        let (mut reader, mut writer) = std::io::pipe().unwrap();
        crate::utils::set_nonblocking(&reader).unwrap();
        crate::utils::set_nonblocking(&writer).unwrap();
        while writer.write(&[0; 4096]).is_ok() {}
        let mut channel = EventChannel::new(Box::new(writer));
        channel.push(focus(1));
        channel.flush();
        assert_eq!(channel.queue.len(), 1);
        let mut bytes = [0; 4096];
        while reader.read(&mut bytes).is_ok() {}
        assert_eq!(channel.take().len(), 1);
        channel.push(focus(2));
        channel.flush();
        assert_eq!(reader.read(&mut bytes).unwrap(), 1);
    }

    #[test]
    fn a_focus_is_unread_until_the_queue_is_drained() {
        let mut channel = EventChannel::new(Box::new(Vec::new()));
        assert!(!channel.focus_unread());
        channel.push(focus(1));
        assert!(channel.focus_unread());
        channel.flush();
        assert!(channel.focus_unread());
        channel.take();
        assert!(!channel.focus_unread());
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
