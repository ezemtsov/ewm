//! Emacs dynamic module interface for EWM

use emacs::{defun, Env, IntoLisp, Result, Value};
use std::fs::File;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use smithay::reexports::calloop::LoopSignal;

use crate::event::Event;

/// Flag to request compositor shutdown from Emacs thread
pub static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Event loop signal for waking the compositor from Emacs thread
pub static LOOP_SIGNAL: OnceLock<LoopSignal> = OnceLock::new();

// ============================================================================
// Event Queue
// ============================================================================

/// Event queue shared between compositor thread and Emacs
static EVENT_QUEUE: OnceLock<Mutex<Vec<Event>>> = OnceLock::new();

/// Notification pipe: (read_fd, write_fd)
/// Emacs monitors read_fd; compositor writes to write_fd when events arrive
static NOTIFY_PIPE: OnceLock<(RawFd, RawFd)> = OnceLock::new();

fn event_queue() -> &'static Mutex<Vec<Event>> {
    EVENT_QUEUE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Initialize the notification pipe
fn init_notify_pipe() -> std::io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error());
    }

    // Set non-blocking on both ends
    for fd in &fds {
        let flags = unsafe { libc::fcntl(*fd, libc::F_GETFL) };
        unsafe { libc::fcntl(*fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }

    Ok((fds[0], fds[1]))
}

/// Push an event to the queue and notify Emacs
pub fn push_event(event: Event) {
    let mut queue = event_queue().lock().unwrap();
    queue.push(event);

    // Write a byte to wake Emacs (if pipe exists)
    if let Some((_, write_fd)) = NOTIFY_PIPE.get() {
        let buf = [1u8];
        unsafe { libc::write(*write_fd, buf.as_ptr() as *const libc::c_void, 1) };
    }
}

/// Get the notification pipe read fd (for Emacs to monitor)
#[defun]
fn event_fd(_: &Env) -> Result<Option<i64>> {
    let pipe = NOTIFY_PIPE.get_or_init(|| {
        init_notify_pipe().expect("Failed to create notification pipe")
    });
    Ok(Some(pipe.0 as i64))
}

/// Drain the notification pipe (call after processing events)
#[defun]
fn drain_events(_: &Env) -> Result<()> {
    if let Some((read_fd, _)) = NOTIFY_PIPE.get() {
        let mut buf = [0u8; 256];
        loop {
            let n = unsafe {
                libc::read(*read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n <= 0 {
                break;
            }
        }
    }
    Ok(())
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

    // Initialize notification pipe if not already done
    let _ = NOTIFY_PIPE.get_or_init(|| {
        init_notify_pipe().expect("Failed to create notification pipe")
    });

    // Spawn compositor thread - frames are created via output_detected events
    // (Emacs receives events and creates frames with ewm--create-frame-for-output)
    let handle = thread::spawn(move || {
        tracing::info!("Compositor thread starting");

        // Catch panics so they don't crash Emacs
        // No client spawn - frames created by Emacs via output_detected events
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_drm(None)));

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
