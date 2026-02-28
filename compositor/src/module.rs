//! Emacs dynamic module interface for EWM
//!
//! # Design Invariants
//!
//! 1. **Thread safety**: The compositor runs in a separate thread from Emacs.
//!    Communication uses queues and shared state:
//!    - `COMMAND_QUEUE`: Emacs → Compositor commands (layouts, focus, etc.)
//!    - `EVENT_QUEUE`: Compositor → Emacs events (new surfaces, focus changes)
//!    - `SHARED_STATE`: Compositor → Emacs snapshot (focus, pointer, outputs)
//!    - `ACTIVATION_TOKEN_POOL`: Compositor → Emacs pre-generated XDG activation tokens
//!    - `IN_PREFIX_SEQUENCE`: Bidirectional atomic (hot input path)
//!
//! 2. **No blocking**: Module functions called from Emacs must never block.
//!    They push to queues and return immediately. The compositor processes
//!    queues on its event loop. Exception: `create_activation_token` may
//!    busy-wait up to 100ms if the token pool is empty.
//!
//! 3. **Synchronous state for races**: Some state must be synchronous to avoid
//!    race conditions:
//!    - `PENDING_FRAME_OUTPUTS`: Must be read atomically with surface creation
//!    - `INTERCEPTED_KEYS`: Must be available before first key event
//!
//! 4. **Focus debugging**: `FOCUS_HISTORY` records the last 20 focus changes
//!    with source tracking. Essential for diagnosing focus races where the
//!    compositor and Emacs disagree about which surface has focus.
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

use emacs::{defun, use_symbols, Env, IntoLisp, Result, Value};

// Cached Lisp symbols used in hot-path defuns (interning on every call is wasteful).
use_symbols! {
    // Boolean sentinel: Emacs uses :false to pass an explicit false through the module interface,
    // since nil is indistinguishable from "argument absent".
    kw_false => ":false"
    // output_layout_module / intercept_keys_module plist keys
    kw_id          => ":id"
    kw_x           => ":x"
    kw_y           => ":y"
    kw_w           => ":w"
    kw_h           => ":h"
    kw_focused     => ":focused"
    kw_fullscreen  => ":fullscreen"
    kw_key         => ":key"
    kw_ctrl        => ":ctrl"
    kw_alt         => ":alt"
    kw_shift       => ":shift"
    kw_super       => ":super"
    kw_is_fullscreen_toggle => ":is-fullscreen-toggle"
    kw_name   => ":name"
    kw_active => ":active"
    // configure_output_module: marker for "not provided" (distinct from nil)
    kw_unset  => ":unset"
}
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::thread::{self, JoinHandle};

use smithay::reexports::calloop::LoopSignal;

use crate::event::Event;
use smithay::input::keyboard::{keysyms, xkb};

use crate::{InterceptedKey, LayoutEntry};

// ============================================================================
// Shared State (read by Emacs, written by compositor)
// ============================================================================

/// Compositor state snapshot, updated once per tick in `update_shared_state()`.
/// Only contains fields that Emacs reads synchronously via per-field defuns.
#[derive(Default)]
pub struct SharedState {
    pub focused_surface_id: u32,
    pub pointer_location: (f64, f64),
    pub output_offsets: HashMap<String, (i32, i32)>,
}

static SHARED_STATE: OnceLock<Mutex<SharedState>> = OnceLock::new();

pub fn shared_state() -> &'static Mutex<SharedState> {
    SHARED_STATE.get_or_init(|| Mutex::new(SharedState::default()))
}

/// Pending frame-to-output assignments (synchronous to avoid race with surface creation)
static PENDING_FRAME_OUTPUTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn pending_frame_outputs() -> &'static Mutex<Vec<String>> {
    PENDING_FRAME_OUTPUTS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Take the next pending frame output (called by compositor when creating surfaces).
pub fn take_pending_frame_output() -> Option<String> {
    let mut outputs = pending_frame_outputs().lock().unwrap();
    if outputs.is_empty() {
        None
    } else {
        Some(outputs.remove(0))
    }
}

/// Get pending frame outputs for state dump.
pub fn peek_pending_frame_outputs() -> Vec<String> {
    pending_frame_outputs().lock().unwrap().clone()
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

/// Flag indicating we're in an incomplete prefix key sequence.
/// Set when prefix key intercepted, cleared when Emacs completes a command.
static IN_PREFIX_SEQUENCE: AtomicBool = AtomicBool::new(false);

/// Set the prefix sequence flag (called from input handling)
pub fn set_in_prefix_sequence(value: bool) {
    IN_PREFIX_SEQUENCE.store(value, Ordering::Relaxed);
}

/// Get the prefix sequence flag (for state dump)
pub fn get_in_prefix_sequence() -> bool {
    IN_PREFIX_SEQUENCE.load(Ordering::Relaxed)
}

// Per-field accessors: thin defuns for hot-path reads.
// Each locks SharedState briefly and returns a single value — no alist construction.

/// Query focused surface ID (called by Emacs on every post-command-hook).
#[defun]
fn get_focused_id() -> Result<i64> {
    Ok(shared_state().lock().unwrap().focused_surface_id as i64)
}

/// Query output offset (called by Emacs for frame positioning).
/// Returns (x . y) cons cell, or nil if output not found.
#[defun]
fn get_output_offset<'a>(env: &'a Env, name: String) -> Result<Value<'a>> {
    let state = shared_state().lock().unwrap();
    match state.output_offsets.get(&name) {
        Some((x, y)) => env.call("cons", (*x as i64, *y as i64)),
        None => ().into_lisp(env),
    }
}

/// Query pointer location (called by Emacs for mouse-follows-focus).
/// Returns (x . y) cons cell in compositor coordinates.
#[defun]
fn get_pointer_location<'a>(env: &'a Env) -> Result<Value<'a>> {
    let (x, y) = shared_state().lock().unwrap().pointer_location;
    env.call("cons", (x, y))
}

// ============================================================================
// Module Commands (Emacs -> Compositor)
// ============================================================================

/// Commands sent from Emacs to the compositor via the module interface.
#[derive(Debug, Clone)]
pub enum ModuleCommand {
    Close {
        id: u32,
    },
    Focus {
        id: u32,
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
        surface_id: u32,
    },
    TextInputIntercept {
        enabled: bool,
    },
    SwitchLayout {
        layout: String,
    },
    GetLayouts,
    /// Request verbose state dump for debugging (ewm-show-state)
    GetDebugState,
    /// Request activation token creation (compositor will push to ACTIVATION_TOKEN_POOL)
    CreateActivationToken,
    /// Set clipboard selection from Emacs
    SetSelection {
        text: String,
    },
    /// Declarative per-output layout (includes workspace tab state)
    OutputLayout {
        output: String,
        surfaces: Vec<LayoutEntry>,
        tabs: Vec<TabInfo>,
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
}

/// Tab info sent from Emacs for workspace protocol
#[derive(Debug, Clone, serde::Serialize)]
pub struct TabInfo {
    pub name: String,
    pub active: bool,
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

/// Get the pending focus target, if any.
/// Called before keyboard event handling to ensure focus is synced.
pub fn take_pending_focus() -> Option<u32> {
    let mut queue = command_queue().lock().unwrap();
    let mut focus_id = None;
    // Find the last Focus command (most recent wins)
    queue.retain(|cmd| {
        if let ModuleCommand::Focus { id } = cmd {
            focus_id = Some(*id);
            false // Remove from queue
        } else {
            true // Keep other commands
        }
    });
    focus_id
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

/// Flag to request compositor shutdown from Emacs thread
pub static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Debug mode flag - when enabled, more verbose logging is output
pub static DEBUG_MODE: AtomicBool = AtomicBool::new(false);

/// Event loop signal for waking the compositor from Emacs thread
pub static LOOP_SIGNAL: OnceLock<LoopSignal> = OnceLock::new();

// ============================================================================
// Activation Token Pool (for XDG activation)
// ============================================================================

/// Pool of pre-generated activation tokens.
/// Compositor pushes tokens here, Emacs pops them for spawning processes.
static ACTIVATION_TOKEN_POOL: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

fn activation_token_pool() -> &'static Mutex<VecDeque<String>> {
    ACTIVATION_TOKEN_POOL.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Push an activation token to the pool (called by compositor)
pub fn push_activation_token(token: String) {
    activation_token_pool().lock().unwrap().push_back(token);
}

/// Get an activation token for spawning a process.
/// Returns a token string that can be set as XDG_ACTIVATION_TOKEN env var.
/// If the pool is empty, requests one from the compositor and waits briefly.
#[defun]
fn create_activation_token() -> Result<Option<String>> {
    // Try to get from pool first
    {
        let mut pool = activation_token_pool().lock().unwrap();
        if let Some(token) = pool.pop_front() {
            return Ok(Some(token));
        }
    }

    // Pool empty - request token from compositor
    push_command(ModuleCommand::CreateActivationToken);

    // Wait briefly for compositor to create token (up to 100ms)
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut pool = activation_token_pool().lock().unwrap();
        if let Some(token) = pool.pop_front() {
            return Ok(Some(token));
        }
    }

    // Timeout - compositor might be busy
    tracing::warn!("Timeout waiting for activation token");
    Ok(None)
}

// ============================================================================
// Focus History (for debugging focus issues)
// ============================================================================

/// Maximum focus history entries
const FOCUS_HISTORY_SIZE: usize = 20;

/// A single focus event for the history
#[derive(Clone, Debug, serde::Serialize)]
pub struct FocusEvent {
    /// Timestamp (monotonic counter)
    pub seq: usize,
    /// Surface ID that received focus
    pub surface_id: u32,
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
pub fn record_focus(surface_id: u32, source: &str, context: Option<&str>) {
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

// ============================================================================
// Event Queue
// ============================================================================

/// Event queue shared between compositor thread and Emacs
static EVENT_QUEUE: OnceLock<Mutex<VecDeque<Event>>> = OnceLock::new();

fn event_queue() -> &'static Mutex<VecDeque<Event>> {
    EVENT_QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Push an event to the queue and notify Emacs via SIGUSR1
pub fn push_event(event: Event) {
    let mut queue = event_queue().lock().unwrap();
    queue.push_back(event);
    drop(queue); // Release lock before signaling

    // Send SIGUSR1 to wake Emacs event loop (only when running as module).
    // In test/standalone mode LOOP_SIGNAL is never set, so skip the signal
    // to avoid killing the process.
    if LOOP_SIGNAL.get().is_some() {
        unsafe {
            libc::raise(libc::SIGUSR1);
        }
    }
}

/// Pop the next event from the queue, returning it as a Lisp alist
/// Returns nil if the queue is empty
#[defun]
fn pop_event(env: &Env) -> Result<Value<'_>> {
    let mut queue = event_queue().lock().unwrap();
    let event = queue.pop_front();
    drop(queue);

    match event {
        None => ().into_lisp(env),
        Some(e) => event_to_lisp(env, e),
    }
}

/// Convert a Event to a Lisp alist
fn event_to_lisp<'a>(env: &'a Env, event: Event) -> Result<Value<'a>> {
    let cons = |k: &str, v: Value<'a>| -> Result<Value<'a>> { env.call("cons", (k, v)) };
    let list = |items: Vec<Value<'a>>| -> Result<Value<'a>> { env.call("list", items.as_slice()) };

    match event {
        Event::Ready => list(vec![cons("event", "ready".into_lisp(env)?)?]),
        Event::New { id, app, output, pid } => {
            let mut items = vec![
                cons("event", "new".into_lisp(env)?)?,
                cons("id", (id as i64).into_lisp(env)?)?,
                cons("app", app.into_lisp(env)?)?,
                cons("pid", (pid as i64).into_lisp(env)?)?,
            ];
            if let Some(out) = output {
                items.push(cons("output", out.into_lisp(env)?)?);
            }
            list(items)
        }
        Event::Close { id } => list(vec![
            cons("event", "close".into_lisp(env)?)?,
            cons("id", (id as i64).into_lisp(env)?)?,
        ]),
        Event::Minimize { id } => list(vec![
            cons("event", "minimize".into_lisp(env)?)?,
            cons("id", (id as i64).into_lisp(env)?)?,
        ]),
        Event::Title { id, app, title } => list(vec![
            cons("event", "title".into_lisp(env)?)?,
            cons("id", (id as i64).into_lisp(env)?)?,
            cons("app", app.into_lisp(env)?)?,
            cons("title", title.into_lisp(env)?)?,
        ]),
        Event::Focus { id } => list(vec![
            cons("event", "focus".into_lisp(env)?)?,
            cons("id", (id as i64).into_lisp(env)?)?,
        ]),
        Event::OutputDetected(info) => {
            // Convert modes to list of alists
            let modes: Result<Vec<Value<'a>>> = info
                .modes
                .iter()
                .map(|m| {
                    list(vec![
                        cons("width", (m.width as i64).into_lisp(env)?)?,
                        cons("height", (m.height as i64).into_lisp(env)?)?,
                        cons("refresh", (m.refresh as i64).into_lisp(env)?)?,
                        cons("preferred", m.preferred.into_lisp(env)?)?,
                    ])
                })
                .collect();
            let modes_list = env.call("list", modes?.as_slice())?;

            list(vec![
                cons("event", "output_detected".into_lisp(env)?)?,
                cons("name", info.name.into_lisp(env)?)?,
                cons("make", info.make.into_lisp(env)?)?,
                cons("model", info.model.into_lisp(env)?)?,
                cons("width-mm", (info.width_mm as i64).into_lisp(env)?)?,
                cons("height-mm", (info.height_mm as i64).into_lisp(env)?)?,
                cons("x", (info.x as i64).into_lisp(env)?)?,
                cons("y", (info.y as i64).into_lisp(env)?)?,
                cons("scale", info.scale.into_lisp(env)?)?,
                cons("transform", (info.transform as i64).into_lisp(env)?)?,
                cons("modes", modes_list)?,
            ])
        }
        Event::OutputDisconnected { name } => list(vec![
            cons("event", "output_disconnected".into_lisp(env)?)?,
            cons("name", name.into_lisp(env)?)?,
        ]),
        Event::OutputsComplete => list(vec![cons("event", "outputs_complete".into_lisp(env)?)?]),
        Event::Layouts { layouts, current } => {
            let layouts_list: Result<Vec<Value<'a>>> =
                layouts.into_iter().map(|s| s.into_lisp(env)).collect();
            let layouts_val = env.call("list", layouts_list?.as_slice())?;
            list(vec![
                cons("event", "layouts".into_lisp(env)?)?,
                cons("layouts", layouts_val)?,
                cons("current", (current as i64).into_lisp(env)?)?,
            ])
        }
        Event::LayoutSwitched { layout, index } => list(vec![
            cons("event", "layout-switched".into_lisp(env)?)?,
            cons("layout", layout.into_lisp(env)?)?,
            cons("index", (index as i64).into_lisp(env)?)?,
        ]),
        Event::TextInputActivated => {
            list(vec![cons("event", "text-input-activated".into_lisp(env)?)?])
        }
        Event::TextInputDeactivated => list(vec![cons(
            "event",
            "text-input-deactivated".into_lisp(env)?,
        )?]),
        Event::Key { keysym, utf8 } => {
            let mut items = vec![
                cons("event", "key".into_lisp(env)?)?,
                cons("keysym", (keysym as i64).into_lisp(env)?)?,
            ];
            if let Some(s) = utf8 {
                items.push(cons("utf8", s.into_lisp(env)?)?);
            }
            list(items)
        }
        Event::DebugState { json } => list(vec![
            cons("event", "debug_state".into_lisp(env)?)?,
            cons("json", json.into_lisp(env)?)?,
        ]),
        Event::WorkingArea {
            output,
            x,
            y,
            width,
            height,
        } => list(vec![
            cons("event", "working_area".into_lisp(env)?)?,
            cons("output", output.into_lisp(env)?)?,
            cons("x", (x as i64).into_lisp(env)?)?,
            cons("y", (y as i64).into_lisp(env)?)?,
            cons("width", (width as i64).into_lisp(env)?)?,
            cons("height", (height as i64).into_lisp(env)?)?,
        ]),
        Event::OutputConfigChanged {
            name,
            width,
            height,
            refresh,
            x,
            y,
            scale,
            transform,
        } => list(vec![
            cons("event", "output_config_changed".into_lisp(env)?)?,
            cons("name", name.into_lisp(env)?)?,
            cons("width", (width as i64).into_lisp(env)?)?,
            cons("height", (height as i64).into_lisp(env)?)?,
            cons("refresh", (refresh as i64).into_lisp(env)?)?,
            cons("x", (x as i64).into_lisp(env)?)?,
            cons("y", (y as i64).into_lisp(env)?)?,
            cons("scale", scale.into_lisp(env)?)?,
            cons("transform", (transform as i64).into_lisp(env)?)?,
        ]),
        Event::SelectionChanged { text } => list(vec![
            cons("event", "selection-changed".into_lisp(env)?)?,
            cons("text", text.into_lisp(env)?)?,
        ]),
        Event::ActivateWorkspace { output, tab_index } => list(vec![
            cons("event", "activate_workspace".into_lisp(env)?)?,
            cons("output", output.into_lisp(env)?)?,
            cons("tab_index", (tab_index as i64).into_lisp(env)?)?,
        ]),
        Event::FullscreenRequest { id, output } => {
            let mut items = vec![
                cons("event", "fullscreen_request".into_lisp(env)?)?,
                cons("id", (id as i64).into_lisp(env)?)?,
            ];
            if let Some(out) = output {
                items.push(cons("output", out.into_lisp(env)?)?);
            }
            list(items)
        }
        Event::UnfullscreenRequest { id } => list(vec![
            cons("event", "unfullscreen_request".into_lisp(env)?)?,
            cons("id", (id as i64).into_lisp(env)?)?,
        ]),
        Event::MaximizeRequest { id } => list(vec![
            cons("event", "maximize_request".into_lisp(env)?)?,
            cons("id", (id as i64).into_lisp(env)?)?,
        ]),
        Event::UnmaximizeRequest { id } => list(vec![
            cons("event", "unmaximize_request".into_lisp(env)?)?,
            cons("id", (id as i64).into_lisp(env)?)?,
        ]),
        Event::IdleStateChanged { idle } => list(vec![
            cons("event", "idle_state_changed".into_lisp(env)?)?,
            cons("idle", idle.into_lisp(env)?)?,
        ]),
        Event::Environment { vars } => {
            let pairs: Result<Vec<Value<'a>>> = vars
                .into_iter()
                .map(|(k, v)| env.call("cons", (k, v)))
                .collect();
            let vars_list = env.call("list", pairs?.as_slice())?;
            list(vec![
                cons("event", "environment".into_lisp(env)?)?,
                cons("vars", vars_list)?,
            ])
        }
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

/// Start the compositor in a background thread.
/// Must be called from a TTY (not inside another compositor).
/// Returns t if started successfully, nil if already running.
#[defun]
fn start() -> Result<bool> {
    use crate::backend::drm::run_drm;

    init_logging();

    let mut state = compositor_state().lock().unwrap();

    // Check if already running
    if state.thread.as_ref().is_some_and(|t| !t.is_finished()) {
        tracing::warn!("Compositor already running");
        return Ok(false);
    }

    // Reset stop flag and event queue
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    event_queue().lock().unwrap().clear();

    // Spawn compositor thread - frames are created via output_detected events
    // (Emacs receives events and creates frames with ewm--create-frame-for-output)
    let handle = thread::spawn(move || {
        tracing::info!("Compositor thread starting");

        // Catch panics so they don't crash Emacs
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run_drm));

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

    if !state.thread.as_ref().is_some_and(|t| !t.is_finished()) {
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
    let state = compositor_state().lock().unwrap();
    Ok(state.thread.as_ref().is_some_and(|t| !t.is_finished()))
}

/// Get the Wayland display socket name (if compositor is running).
#[defun]
fn socket() -> Result<Option<String>> {
    Ok(std::env::var("EWM_WAYLAND_DISPLAY").ok())
}

// ============================================================================
// Module Command Functions (direct Emacs → Compositor)
// ============================================================================

/// Set declarative layout for an output (module mode).
/// OUTPUT is the output name. SURFACES is a vector of plists with :id :x :y :w :h :primary keys.
/// TABS is a vector of plists (each a vector [:name NAME :active ACTIVE]).
/// Coordinates are relative to the output's working area (frame-relative).
#[defun]
fn output_layout_module(
    env: &Env,
    output: String,
    surfaces: Value<'_>,
    tabs: Value<'_>,
) -> Result<()> {
    let mut entries = Vec::new();

    let len_val: Value = env.call("length", (surfaces,))?;
    let len: i64 = len_val.into_rust()?;
    for i in 0..len {
        let entry: Value = env.call("aref", (surfaces, i))?;

        let id_val: Value = env.call("plist-get", (entry, kw_id.bind(env)))?;
        let x_val: Value = env.call("plist-get", (entry, kw_x.bind(env)))?;
        let y_val: Value = env.call("plist-get", (entry, kw_y.bind(env)))?;
        let w_val: Value = env.call("plist-get", (entry, kw_w.bind(env)))?;
        let h_val: Value = env.call("plist-get", (entry, kw_h.bind(env)))?;
        let id: i64 = id_val.into_rust()?;
        let x: i64 = x_val.into_rust()?;
        let y: i64 = y_val.into_rust()?;
        let w: i64 = w_val.into_rust()?;
        let h: i64 = h_val.into_rust()?;

        let false_sym = kw_false.bind(env);
        let focused_val: Value = env.call("plist-get", (entry, kw_focused.bind(env)))?;
        let focused = focused_val.is_not_nil() && !env.call("eq", (focused_val, false_sym))?.is_not_nil();

        let fullscreen_val: Value = env.call("plist-get", (entry, kw_fullscreen.bind(env)))?;
        let fullscreen = fullscreen_val.is_not_nil() && !env.call("eq", (fullscreen_val, false_sym))?.is_not_nil();

        entries.push(LayoutEntry {
            id: id as u32,
            x: x as i32,
            y: y as i32,
            w: w as u32,
            h: h as u32,
            focused,
            primary: false, // computed by apply_output_layout
            fullscreen,
        });
    }

    let mut parsed_tabs = Vec::new();
    let tabs_len_val: Value = env.call("length", (tabs,))?;
    let tabs_len: i64 = tabs_len_val.into_rust()?;
    let false_sym = kw_false.bind(env);

    for i in 0..tabs_len {
        let tab: Value = env.call("aref", (tabs, i))?;

        let name_val: Value = env.call("plist-get", (tab, kw_name.bind(env)))?;
        let name: String = name_val.into_rust().unwrap_or_default();

        let active_val: Value = env.call("plist-get", (tab, kw_active.bind(env)))?;
        let active = active_val.is_not_nil() && !env.call("eq", (active_val, false_sym))?.is_not_nil();

        parsed_tabs.push(TabInfo { name, active });
    }

    push_command(ModuleCommand::OutputLayout {
        output,
        surfaces: entries,
        tabs: parsed_tabs,
    });
    Ok(())
}

/// Request surface to close (module mode).
#[defun]
fn close_module(id: i64) -> Result<()> {
    push_command(ModuleCommand::Close { id: id as u32 });
    Ok(())
}

/// Focus a surface (module mode).
#[defun]
fn focus_module(id: i64) -> Result<()> {
    push_command(ModuleCommand::Focus { id: id as u32 });
    Ok(())
}

/// Warp pointer to absolute position (module mode).
#[defun]
fn warp_pointer_module(x: f64, y: f64) -> Result<()> {
    push_command(ModuleCommand::WarpPointer { x, y });
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
    pending_frame_outputs().lock().unwrap().push(output.clone());
    tracing::info!("Prepared frame for output {}", output);
    Ok(())
}

/// Configure output (module mode).
/// ENABLED should be t, nil, or omitted.
/// SCALE is a float (e.g. 1.5). TRANSFORM is an integer (0=Normal, 1=90, 2=180, 3=270, 4=Flipped, 5=Flipped90, 6=Flipped180, 7=Flipped270).
#[defun]
fn configure_output_module(
    env: &Env,
    name: String,
    x: Option<i64>,
    y: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
    refresh: Option<i64>,
    scale: Option<f64>,
    transform: Option<i64>,
    enabled: Value<'_>,
) -> Result<()> {
    // Convert enabled value: t -> Some(true), nil -> Some(false), unspecified -> None
    // We use a special marker to detect "not provided" vs nil
    let enabled_opt = if enabled.is_not_nil() {
        // Check if it's :unset (our marker for "not provided")
        let eq_result: Value = env.call("eq", (enabled, kw_unset.bind(env)))?;
        if eq_result.is_not_nil() {
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
/// words (e.g., XKB "ISO_Left_Tab" → Emacs "iso-lefttab"), losing word boundaries.
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
/// Handles: case differences (Emacs "left" → XKB "Left"),
/// hyphens vs underscores (Emacs "kp-add" → XKB "KP_Add"),
/// XF86 prefix (Emacs "AudioMute" → XKB "XF86AudioMute"),
/// and compound-word lossy names via fallback table.
fn resolve_keysym_from_name(name: &str) -> xkb::Keysym {
    let no_symbol: xkb::Keysym = keysyms::KEY_NoSymbol.into();

    // Try the name as-is (case-insensitive)
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
    if sym != no_symbol {
        return sym;
    }

    // Try with hyphens replaced by underscores (Emacs convention → XKB convention)
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

/// Set intercepted keys (module mode).
/// KEYS is a vector of plists with :key :ctrl :alt :shift :super keys.
#[defun]
fn intercept_keys_module(env: &Env, keys: Value<'_>) -> Result<()> {
    let mut parsed_keys = Vec::new();

    let len_val: Value = env.call("length", (keys,))?;
    let len: i64 = len_val.into_rust()?;
    let false_sym = kw_false.bind(env);

    for i in 0..len {
        let key_spec: Value = env.call("aref", (keys, i))?;

        // Resolve keysym from Emacs key description using libxkbcommon.
        // Emacs passes either an integer (Unicode codepoint) or a string (key name).
        let key_val = env.call("plist-get", (key_spec, kw_key.bind(env)))?;
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
        let is_true = |v: Value| -> bool {
            if !v.is_not_nil() {
                return false;
            }
            let eq_result: Value = env.call("eq", (v, false_sym)).unwrap();
            !eq_result.is_not_nil()
        };

        // Extract modifier flags
        let ctrl_val: Value = env.call("plist-get", (key_spec, kw_ctrl.bind(env)))?;
        let alt_val: Value = env.call("plist-get", (key_spec, kw_alt.bind(env)))?;
        let shift_val: Value = env.call("plist-get", (key_spec, kw_shift.bind(env)))?;
        let super_val: Value = env.call("plist-get", (key_spec, kw_super.bind(env)))?;
        let is_fs_toggle_val: Value =
            env.call("plist-get", (key_spec, kw_is_fullscreen_toggle.bind(env)))?;

        parsed_keys.push(InterceptedKey {
            keysym: keysym.raw(),
            ctrl: is_true(ctrl_val),
            alt: is_true(alt_val),
            shift: is_true(shift_val),
            logo: is_true(super_val),
            is_fullscreen_toggle: is_true(is_fs_toggle_val),
        });
    }

    *intercepted_keys().write().unwrap() = parsed_keys;
    tracing::info!(
        "Intercepted keys set ({} keys)",
        intercepted_keys().read().unwrap().len()
    );
    Ok(())
}

/// Commit text to a client text field.
/// SURFACE-ID identifies the target surface, used for queuing commits
/// that arrive while the client is in a disable→enable gap.
#[defun]
fn im_commit_module(text: String, surface_id: i64) -> Result<()> {
    push_command(ModuleCommand::ImCommit {
        text,
        surface_id: surface_id as u32,
    });
    Ok(())
}

/// Enable/disable text input interception (module mode).
/// ENABLED should be t or nil.
#[defun]
fn text_input_intercept_module(enabled: Value<'_>) -> Result<()> {
    push_command(ModuleCommand::TextInputIntercept {
        enabled: enabled.is_not_nil(),
    });
    Ok(())
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
#[defun]
fn get_debug_state_module() -> Result<()> {
    push_command(ModuleCommand::GetDebugState);
    Ok(())
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

/// Configure input devices (module mode).
/// CONFIGS is a vector of plists, each with :device :type and setting properties.
#[defun]
fn configure_input_module(env: &Env, configs: Value<'_>) -> Result<()> {
    use crate::input;

    let len: i64 = env.call("length", (configs,))?.into_rust()?;
    let mut entries = Vec::new();

    for i in 0..len {
        let plist: Value = env.call("aref", (configs, i))?;
        let mut entry = input::InputConfigEntry::default();

        // Walk plist key/value pairs. The match arms ARE the schema:
        // unknown keys hit the _ arm → error, wrong types → error.
        let mut rest = plist;
        while rest.is_not_nil() {
            let key: String =
                env.call("symbol-name", (env.call("car", (rest,))?,))?.into_rust()?;
            let val: Value = env.call("cadr", (rest,))?;

            match key.as_str() {
                ":device" => {
                    if val.is_not_nil() {
                        entry.device = Some(val.into_rust()?);
                    }
                }
                ":type" => {
                    if val.is_not_nil() {
                        let s: String = val.into_rust()?;
                        match input::DeviceType::parse(&s) {
                            Some(dt) => entry.device_type = Some(dt),
                            None => env.signal(
                                "error",
                                (format!("Invalid :type \"{s}\", expected: touchpad, mouse, trackball, trackpoint, keyboard"),),
                            )?,
                        }
                    }
                }
                ":natural-scroll" => entry.natural_scroll = Some(val.is_not_nil()),
                ":tap" => entry.tap = Some(val.is_not_nil()),
                ":dwt" => entry.dwt = Some(val.is_not_nil()),
                ":left-handed" => entry.left_handed = Some(val.is_not_nil()),
                ":middle-emulation" => entry.middle_emulation = Some(val.is_not_nil()),
                ":accel-speed" => {
                    if val.is_not_nil() {
                        entry.accel_speed = Some(val.into_rust()?);
                    }
                }
                ":accel-profile" => {
                    if val.is_not_nil() {
                        let s: String = val.into_rust()?;
                        match input::parse_accel_profile(&s) {
                            Some(v) => entry.accel_profile = Some(v),
                            None => env.signal(
                                "error",
                                (format!("Invalid :accel-profile \"{s}\", expected: flat, adaptive"),),
                            )?,
                        }
                    }
                }
                ":click-method" => {
                    if val.is_not_nil() {
                        let s: String = val.into_rust()?;
                        match input::parse_click_method(&s) {
                            Some(v) => entry.click_method = Some(v),
                            None => env.signal(
                                "error",
                                (format!("Invalid :click-method \"{s}\", expected: button-areas, clickfinger"),),
                            )?,
                        }
                    }
                }
                ":scroll-method" => {
                    if val.is_not_nil() {
                        let s: String = val.into_rust()?;
                        match input::parse_scroll_method(&s) {
                            Some(v) => entry.scroll_method = Some(v),
                            None => env.signal(
                                "error",
                                (format!("Invalid :scroll-method \"{s}\", expected: no-scroll, two-finger, edge, on-button-down"),),
                            )?,
                        }
                    }
                }
                ":tap-button-map" => {
                    if val.is_not_nil() {
                        let s: String = val.into_rust()?;
                        match input::parse_tap_button_map(&s) {
                            Some(v) => entry.tap_button_map = Some(v),
                            None => env.signal(
                                "error",
                                (format!("Invalid :tap-button-map \"{s}\", expected: left-right-middle, left-middle-right"),),
                            )?,
                        }
                    }
                }
                ":repeat-delay" => {
                    if val.is_not_nil() {
                        entry.repeat_delay = Some(val.into_rust::<i64>()? as i32);
                    }
                }
                ":repeat-rate" => {
                    if val.is_not_nil() {
                        entry.repeat_rate = Some(val.into_rust::<i64>()? as i32);
                    }
                }
                ":xkb-layouts" => {
                    if val.is_not_nil() {
                        entry.xkb_layouts = Some(val.into_rust()?);
                    }
                }
                ":xkb-options" => {
                    if val.is_not_nil() {
                        entry.xkb_options = Some(val.into_rust()?);
                    }
                }
                _ => env.signal(
                    "error",
                    (format!("Unknown input config key {key}"),),
                )?,
            }

            rest = env.call("cddr", (rest,))?;
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

/// Query prefix sequence state (called by Emacs before focus sync).
#[defun]
fn in_prefix_sequence_p() -> Result<bool> {
    Ok(IN_PREFIX_SEQUENCE.load(Ordering::Relaxed))
}

/// Clear prefix sequence flag (called by Emacs when command completes).
#[defun]
fn clear_prefix_sequence() -> Result<()> {
    IN_PREFIX_SEQUENCE.store(false, Ordering::Relaxed);
    Ok(())
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

        let pair = env.call("cons", (name.as_str(), commandline.as_str()))?;
        result = env.call("cons", (pair, result))?;
    }

    result = env.call("nreverse", (result,))?;
    Ok(result)
}
