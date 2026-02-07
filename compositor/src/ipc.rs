//! IPC socket setup and event handling
//!
//! This module provides shared IPC functionality for communication
//! with Emacs, used by both Winit and DRM backends.

use std::cell::RefCell;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::rc::Rc;

use smithay::reexports::calloop::{
    generic::Generic, Interest, LoopHandle, Mode as CalloopMode, PostAction, RegistrationToken,
};
use tracing::info;

use crate::LoopData;

/// Socket filename
const IPC_SOCKET_NAME: &str = "ewm.sock";

/// Get the IPC socket path, using XDG_RUNTIME_DIR if available
pub fn ipc_socket_path() -> String {
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) => format!("{}/{}", dir, IPC_SOCKET_NAME),
        Err(_) => format!("/tmp/{}", IPC_SOCKET_NAME),
    }
}

/// Set up the IPC listener for Emacs communication
///
/// This creates a Unix socket at $XDG_RUNTIME_DIR/ewm.sock and registers it with
/// the event loop to accept connections and process commands.
///
/// Returns Ok(()) on success, or an error if socket creation fails.
pub fn setup_ipc_listener(
    event_loop: &LoopHandle<'static, LoopData>,
) -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = ipc_socket_path();
    let ipc_path = Path::new(&socket_path);

    // Remove existing socket if present
    if ipc_path.exists() {
        std::fs::remove_file(ipc_path)?;
    }

    // Create and configure the listener
    let ipc_listener = UnixListener::bind(ipc_path)?;
    ipc_listener.set_nonblocking(true)?;
    info!("IPC socket: {}", socket_path);

    // Track IPC stream registration token for cleanup on reconnect
    let ipc_stream_token: Rc<RefCell<Option<RegistrationToken>>> = Rc::new(RefCell::new(None));
    let ipc_stream_token_clone = ipc_stream_token.clone();
    let loop_handle = event_loop.clone();

    // Register the listener to accept connections
    event_loop.insert_source(
        Generic::new(ipc_listener, Interest::READ, CalloopMode::Level),
        move |_, listener, data| {
            if let Ok((stream, _)) = listener.accept() {
                info!("Emacs connected");
                stream.set_nonblocking(true).ok();

                // Remove previous stream source if any
                if let Some(token) = ipc_stream_token_clone.borrow_mut().take() {
                    loop_handle.remove(token);
                }

                // Clone stream for writing (stored in data.emacs)
                let write_stream = stream.try_clone().unwrap();
                data.emacs = Some(write_stream);

                // Register stream for reading as event source
                let token = loop_handle
                    .insert_source(
                        Generic::new(stream, Interest::READ, CalloopMode::Level),
                        |_, source, data: &mut LoopData| {
                            // SAFETY: We're inside the event loop callback where the source is valid
                            let stream = unsafe { source.get_mut() };
                            data.process_commands_from_stream(stream);
                            Ok(PostAction::Continue)
                        },
                    )
                    .expect("Failed to register IPC stream");

                *ipc_stream_token_clone.borrow_mut() = Some(token);
            }
            Ok(PostAction::Continue)
        },
    )?;

    Ok(())
}
