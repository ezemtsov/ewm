//! Emacs dynamic module interface for EWM

use emacs::{defun, Env, IntoLisp, Result, Value};
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use smithay::reexports::calloop::LoopSignal;

use crate::event::Event;
use crate::{InterceptedKey, KeyId, SurfaceView};

// ============================================================================
// Module Commands (Emacs -> Compositor)
// ============================================================================

/// Commands sent from Emacs to the compositor via the module interface.
#[derive(Debug, Clone)]
pub enum ModuleCommand {
    Layout { id: u32, x: i32, y: i32, w: u32, h: u32 },
    Views { id: u32, views: Vec<SurfaceView> },
    Hide { id: u32 },
    Close { id: u32 },
    Focus { id: u32 },
    WarpPointer { x: f64, y: f64 },
    Screenshot { path: Option<String> },
    AssignOutput { id: u32, output: String },
    PrepareFrame { output: String },
    ConfigureOutput {
        name: String,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<i32>,
        height: Option<i32>,
        refresh: Option<i32>,
        enabled: Option<bool>,
    },
    InterceptKeys { keys: Vec<InterceptedKey> },
    ImCommit { text: String },
    TextInputIntercept { enabled: bool },
    ConfigureXkb { layouts: String, options: Option<String> },
    SwitchLayout { layout: String },
    GetLayouts,
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

/// Event loop signal for waking the compositor from Emacs thread
pub static LOOP_SIGNAL: OnceLock<LoopSignal> = OnceLock::new();

// ============================================================================
// Event Queue
// ============================================================================

/// Event queue shared between compositor thread and Emacs
static EVENT_QUEUE: OnceLock<Mutex<Vec<Event>>> = OnceLock::new();

fn event_queue() -> &'static Mutex<Vec<Event>> {
    EVENT_QUEUE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Push an event to the queue and notify Emacs via SIGUSR1
pub fn push_event(event: Event) {
    let mut queue = event_queue().lock().unwrap();
    queue.push(event);
    drop(queue); // Release lock before signaling

    // Send SIGUSR1 to wake Emacs event loop
    // Signal coalescing is fine - Emacs will drain the whole queue
    unsafe {
        libc::raise(libc::SIGUSR1);
    }
}

/// Pop the next event from the queue, returning it as a Lisp alist
/// Returns nil if the queue is empty
#[defun]
fn pop_event(env: &Env) -> Result<Value<'_>> {
    let mut queue = event_queue().lock().unwrap();
    let event = queue.pop();
    drop(queue);

    match event {
        None => ().into_lisp(env),
        Some(e) => event_to_lisp(env, e),
    }
}

/// Convert a Event to a Lisp alist
fn event_to_lisp<'a>(env: &'a Env, event: Event) -> Result<Value<'a>> {
    let cons = |k: &str, v: Value<'a>| -> Result<Value<'a>> {
        env.call("cons", (k, v))
    };
    let list = |items: Vec<Value<'a>>| -> Result<Value<'a>> {
        env.call("list", items.as_slice())
    };

    match event {
        Event::Ready => {
            list(vec![cons("event", "ready".into_lisp(env)?)?])
        }
        Event::New { id, app, output } => {
            let mut items = vec![
                cons("event", "new".into_lisp(env)?)?,
                cons("id", (id as i64).into_lisp(env)?)?,
                cons("app", app.into_lisp(env)?)?,
            ];
            if let Some(out) = output {
                items.push(cons("output", out.into_lisp(env)?)?);
            }
            list(items)
        }
        Event::Close { id } => {
            list(vec![
                cons("event", "close".into_lisp(env)?)?,
                cons("id", (id as i64).into_lisp(env)?)?,
            ])
        }
        Event::Title { id, app, title } => {
            list(vec![
                cons("event", "title".into_lisp(env)?)?,
                cons("id", (id as i64).into_lisp(env)?)?,
                cons("app", app.into_lisp(env)?)?,
                cons("title", title.into_lisp(env)?)?,
            ])
        }
        Event::Focus { id } => {
            list(vec![
                cons("event", "focus".into_lisp(env)?)?,
                cons("id", (id as i64).into_lisp(env)?)?,
            ])
        }
        Event::OutputDetected(info) => {
            // Convert modes to list of alists
            let modes: Result<Vec<Value<'a>>> = info.modes.iter().map(|m| {
                list(vec![
                    cons("width", (m.width as i64).into_lisp(env)?)?,
                    cons("height", (m.height as i64).into_lisp(env)?)?,
                    cons("refresh", (m.refresh as i64).into_lisp(env)?)?,
                    cons("preferred", m.preferred.into_lisp(env)?)?,
                ])
            }).collect();
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
                cons("modes", modes_list)?,
            ])
        }
        Event::OutputDisconnected { name } => {
            list(vec![
                cons("event", "output_disconnected".into_lisp(env)?)?,
                cons("name", name.into_lisp(env)?)?,
            ])
        }
        Event::OutputsComplete => {
            list(vec![cons("event", "outputs_complete".into_lisp(env)?)?])
        }
        Event::Layouts { layouts, current } => {
            let layouts_list: Result<Vec<Value<'a>>> = layouts
                .into_iter()
                .map(|s| s.into_lisp(env))
                .collect();
            let layouts_val = env.call("list", layouts_list?.as_slice())?;
            list(vec![
                cons("event", "layouts".into_lisp(env)?)?,
                cons("layouts", layouts_val)?,
                cons("current", (current as i64).into_lisp(env)?)?,
            ])
        }
        Event::LayoutSwitched { layout, index } => {
            list(vec![
                cons("event", "layout-switched".into_lisp(env)?)?,
                cons("layout", layout.into_lisp(env)?)?,
                cons("index", (index as i64).into_lisp(env)?)?,
            ])
        }
        Event::TextInputActivated => {
            list(vec![cons("event", "text-input-activated".into_lisp(env)?)?])
        }
        Event::TextInputDeactivated => {
            list(vec![cons("event", "text-input-deactivated".into_lisp(env)?)?])
        }
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
    }
}

/// Test function - returns a greeting
#[defun]
fn hello(_: &Env) -> Result<String> {
    Ok("Hello from EWM compositor!".to_string())
}

/// Return the module version
#[defun]
fn version(_: &Env) -> Result<String> {
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

/// Initialize logging to file or stderr.
/// Uses EWM_LOG_FILE env var, falls back to $XDG_RUNTIME_DIR/ewm.log or stderr.
fn init_logging() {
    use std::sync::Once;
    static INIT_LOG: Once = Once::new();
    INIT_LOG.call_once(|| {
        let log_path = std::env::var("EWM_LOG_FILE").ok().or_else(|| {
            std::env::var("XDG_RUNTIME_DIR")
                .ok()
                .map(|dir| format!("{}/ewm.log", dir))
        });

        let initialized = log_path.and_then(|path| {
            File::create(&path).ok().map(|file| {
                tracing_subscriber::fmt()
                    .with_writer(Mutex::new(file))
                    .with_ansi(false)
                    .with_max_level(tracing::Level::INFO)
                    .init();
            })
        });

        if initialized.is_none() {
            tracing_subscriber::fmt::init();
        }
    });
}

/// Start the compositor in a background thread.
/// Must be called from a TTY (not inside another compositor).
/// Returns t if started successfully, nil if already running.
#[defun]
fn start(_: &Env) -> Result<bool> {
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
fn stop(_: &Env) -> Result<bool> {
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
fn running(_: &Env) -> Result<bool> {
    let state = compositor_state().lock().unwrap();
    Ok(state.thread.as_ref().is_some_and(|t| !t.is_finished()))
}

/// Get the Wayland display socket name (if compositor is running).
#[defun]
fn socket(_: &Env) -> Result<Option<String>> {
    Ok(std::env::var("EWM_WAYLAND_DISPLAY").ok())
}

// ============================================================================
// Module Command Functions (direct Emacs → Compositor)
// ============================================================================

/// Set surface position and size (module mode).
#[defun]
fn layout_module(_: &Env, id: i64, x: i64, y: i64, w: i64, h: i64) -> Result<()> {
    push_command(ModuleCommand::Layout {
        id: id as u32,
        x: x as i32,
        y: y as i32,
        w: w as u32,
        h: h as u32,
    });
    Ok(())
}

/// Set multiple views for a surface (module mode).
/// VIEWS is a vector of plists with :x :y :w :h :active keys.
#[defun]
fn views_module(env: &Env, id: i64, views: Value<'_>) -> Result<()> {
    let mut parsed_views = Vec::new();

    // Iterate through the vector of views
    let len_val: Value = env.call("length", (views,))?;
    let len: i64 = len_val.into_rust()?;
    for i in 0..len {
        let view: Value = env.call("aref", (views, i))?;

        // Extract fields from plist
        let x_val: Value = env.call("plist-get", (view, env.intern(":x")?))?;
        let y_val: Value = env.call("plist-get", (view, env.intern(":y")?))?;
        let w_val: Value = env.call("plist-get", (view, env.intern(":w")?))?;
        let h_val: Value = env.call("plist-get", (view, env.intern(":h")?))?;
        let x: i64 = x_val.into_rust()?;
        let y: i64 = y_val.into_rust()?;
        let w: i64 = w_val.into_rust()?;
        let h: i64 = h_val.into_rust()?;

        let active_val: Value = env.call("plist-get", (view, env.intern(":active")?))?;
        // Active is true unless it's nil or :false
        let false_sym = env.intern(":false")?;
        let eq_result: Value = env.call("eq", (active_val, false_sym))?;
        let is_false = eq_result.is_not_nil();
        let active = active_val.is_not_nil() && !is_false;

        parsed_views.push(SurfaceView {
            x: x as i32,
            y: y as i32,
            w: w as u32,
            h: h as u32,
            active,
        });
    }

    push_command(ModuleCommand::Views {
        id: id as u32,
        views: parsed_views,
    });
    Ok(())
}

/// Hide a surface (module mode).
#[defun]
fn hide_module(_: &Env, id: i64) -> Result<()> {
    push_command(ModuleCommand::Hide { id: id as u32 });
    Ok(())
}

/// Request surface to close (module mode).
#[defun]
fn close_module(_: &Env, id: i64) -> Result<()> {
    push_command(ModuleCommand::Close { id: id as u32 });
    Ok(())
}

/// Focus a surface (module mode).
#[defun]
fn focus_module(_: &Env, id: i64) -> Result<()> {
    push_command(ModuleCommand::Focus { id: id as u32 });
    Ok(())
}

/// Warp pointer to absolute position (module mode).
#[defun]
fn warp_pointer_module(_: &Env, x: f64, y: f64) -> Result<()> {
    push_command(ModuleCommand::WarpPointer { x, y });
    Ok(())
}

/// Take a screenshot (module mode).
#[defun]
fn screenshot_module(_: &Env, path: Option<String>) -> Result<()> {
    push_command(ModuleCommand::Screenshot { path });
    Ok(())
}

/// Assign surface to output (module mode).
#[defun]
fn assign_output_module(_: &Env, id: i64, output: String) -> Result<()> {
    push_command(ModuleCommand::AssignOutput {
        id: id as u32,
        output,
    });
    Ok(())
}

/// Prepare next frame for output (module mode).
#[defun]
fn prepare_frame_module(_: &Env, output: String) -> Result<()> {
    push_command(ModuleCommand::PrepareFrame { output });
    Ok(())
}

/// Configure output (module mode).
/// ENABLED should be t, nil, or omitted.
#[defun]
fn configure_output_module(
    env: &Env,
    name: String,
    x: Option<i64>,
    y: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
    refresh: Option<i64>,
    enabled: Value<'_>,
) -> Result<()> {
    // Convert enabled value: t -> Some(true), nil -> Some(false), unspecified -> None
    // We use a special marker to detect "not provided" vs nil
    let enabled_opt = if enabled.is_not_nil() {
        // Check if it's :unset (our marker for "not provided")
        let unset_sym = env.intern(":unset")?;
        let eq_result: Value = env.call("eq", (enabled, unset_sym))?;
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
        enabled: enabled_opt,
    });
    Ok(())
}

/// Set intercepted keys (module mode).
/// KEYS is a vector of plists with :key :ctrl :alt :shift :super keys.
#[defun]
fn intercept_keys_module(env: &Env, keys: Value<'_>) -> Result<()> {
    let mut parsed_keys = Vec::new();

    let len_val: Value = env.call("length", (keys,))?;
    let len: i64 = len_val.into_rust()?;
    let false_sym = env.intern(":false")?;

    for i in 0..len {
        let key_spec: Value = env.call("aref", (keys, i))?;

        // Extract :key (can be integer or string)
        let key_val: Value = env.call("plist-get", (key_spec, env.intern(":key")?))?;
        let key = if let Ok(k) = key_val.into_rust::<i64>() {
            KeyId::Keysym(k as u32)
        } else if let Ok(s) = key_val.into_rust::<String>() {
            KeyId::Named(s)
        } else {
            continue; // Skip invalid keys
        };

        // Helper to check if a value is truthy (not nil and not :false)
        let is_true = |v: Value| -> bool {
            if !v.is_not_nil() {
                return false;
            }
            let eq_result: Value = env.call("eq", (v, false_sym)).unwrap();
            !eq_result.is_not_nil()
        };

        // Extract modifier flags
        let ctrl_val: Value = env.call("plist-get", (key_spec, env.intern(":ctrl")?))?;
        let alt_val: Value = env.call("plist-get", (key_spec, env.intern(":alt")?))?;
        let shift_val: Value = env.call("plist-get", (key_spec, env.intern(":shift")?))?;
        let super_val: Value = env.call("plist-get", (key_spec, env.intern(":super")?))?;

        parsed_keys.push(InterceptedKey {
            key,
            ctrl: is_true(ctrl_val),
            alt: is_true(alt_val),
            shift: is_true(shift_val),
            logo: is_true(super_val),
        });
    }

    push_command(ModuleCommand::InterceptKeys { keys: parsed_keys });
    Ok(())
}

/// Commit text to focused input field (module mode).
#[defun]
fn im_commit_module(_: &Env, text: String) -> Result<()> {
    push_command(ModuleCommand::ImCommit { text });
    Ok(())
}

/// Enable/disable text input interception (module mode).
/// ENABLED should be t or nil.
#[defun]
fn text_input_intercept_module(_: &Env, enabled: Value<'_>) -> Result<()> {
    push_command(ModuleCommand::TextInputIntercept {
        enabled: enabled.is_not_nil(),
    });
    Ok(())
}

/// Configure XKB layouts (module mode).
#[defun]
fn configure_xkb_module(_: &Env, layouts: String, options: Option<String>) -> Result<()> {
    push_command(ModuleCommand::ConfigureXkb { layouts, options });
    Ok(())
}

/// Switch to named XKB layout (module mode).
#[defun]
fn switch_layout_module(_: &Env, layout: String) -> Result<()> {
    push_command(ModuleCommand::SwitchLayout { layout });
    Ok(())
}

/// Get current XKB layouts (module mode).
#[defun]
fn get_layouts_module(_: &Env) -> Result<()> {
    push_command(ModuleCommand::GetLayouts);
    Ok(())
}
