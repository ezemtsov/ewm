//! Input method relay - connects to ourselves to activate input_method protocol
//!
//! This is a hack to make Smithay's text_input work without an external IM client.
//! We connect to our own compositor as a client and bind to input_method_v2.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::ZwpInputMethodV2,
};

/// Events sent from IM relay to main thread
#[derive(Debug, Clone)]
pub enum ImEvent {
    /// Text field activated (client wants input)
    Activated,
    /// Text field deactivated
    Deactivated,
}

/// Commands sent from main thread to IM relay
#[derive(Debug, Clone)]
pub enum ImCommand {
    /// Commit text to the focused text field
    CommitString(String),
}

/// Handle to the relay thread
pub struct ImRelay {
    _handle: thread::JoinHandle<()>,
    /// Receive events from the relay thread
    pub event_rx: Receiver<ImEvent>,
    /// Send commands to the relay thread
    command_tx: Sender<ImCommand>,
}

impl ImRelay {
    /// Spawn a thread that connects to our compositor as an input method
    pub fn connect(socket_path: &std::path::Path) -> Option<Self> {
        let path = socket_path.to_path_buf();

        // Channel: relay thread → main thread (events)
        let (event_tx, event_rx) = mpsc::channel();
        // Channel: main thread → relay thread (commands)
        let (command_tx, command_rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            // Small delay to let the event loop start
            thread::sleep(Duration::from_millis(100));
            if let Err(e) = run_relay(path, event_tx, command_rx) {
                tracing::warn!("IM relay error: {}", e);
            }
        });

        Some(ImRelay {
            _handle: handle,
            event_rx,
            command_tx,
        })
    }

    /// Send a command to commit text
    pub fn commit_string(&self, text: String) {
        if let Err(e) = self.command_tx.send(ImCommand::CommitString(text)) {
            tracing::warn!("Failed to send commit command: {}", e);
        }
    }
}

fn run_relay(
    socket_path: PathBuf,
    event_tx: Sender<ImEvent>,
    command_rx: Receiver<ImCommand>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stream = UnixStream::connect(&socket_path)?;
    // Set non-blocking so we can interleave with command processing
    stream.set_nonblocking(true)?;

    let conn = Connection::from_socket(stream)?;
    let (globals, mut queue) = registry_queue_init::<RelayState>(&conn)?;
    let qh = queue.handle();

    let mut state = RelayState {
        input_method: None,
        event_tx,
        serial: 0,
        active: false,
    };

    // Bind to input_method_manager
    let im_manager: ZwpInputMethodManagerV2 = globals.bind(&qh, 1..=1, ())?;

    // Bind to seat and get input_method
    let seat: wayland_client::protocol::wl_seat::WlSeat = globals.bind(&qh, 1..=9, ())?;
    let input_method = im_manager.get_input_method(&seat, &qh, ());
    state.input_method = Some(input_method);

    // Flush to ensure server sees our requests
    conn.flush()?;

    tracing::info!("Input method relay connected");

    // Event loop: process Wayland events and commands
    loop {
        // Process any pending Wayland events (non-blocking)
        match queue.dispatch_pending(&mut state) {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("Dispatch error: {}", e);
                break;
            }
        }

        // Prepare to read from Wayland socket
        if let Some(guard) = conn.prepare_read() {
            // Try to read (non-blocking due to set_nonblocking above)
            let _ = guard.read();
        }

        // Flush any pending requests
        if let Err(e) = conn.flush() {
            tracing::warn!("Flush error: {}", e);
            break;
        }

        // Check for commands from main thread (non-blocking)
        while let Ok(cmd) = command_rx.try_recv() {
            match cmd {
                ImCommand::CommitString(text) => {
                    if let Some(ref im) = state.input_method {
                        if state.active {
                            tracing::info!("IM relay: committing text: {:?}", text);
                            im.commit_string(text);
                            im.commit(state.serial);
                            if let Err(e) = conn.flush() {
                                tracing::warn!("Flush error after commit: {}", e);
                            }
                        } else {
                            tracing::warn!("IM relay: commit requested but not active");
                        }
                    }
                }
            }
        }

        // Small sleep to avoid busy-waiting
        thread::sleep(Duration::from_millis(1));
    }

    Ok(())
}

struct RelayState {
    input_method: Option<ZwpInputMethodV2>,
    event_tx: Sender<ImEvent>,
    serial: u32,
    active: bool,
}

// Minimal dispatch implementations
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for RelayState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wayland_client::protocol::wl_seat::WlSeat, ()> for RelayState {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_seat::WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpInputMethodManagerV2, ()> for RelayState {
    fn event(
        _: &mut Self,
        _: &ZwpInputMethodManagerV2,
        _: wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpInputMethodV2, ()> for RelayState {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodV2,
        event: wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2::Event;
        match event {
            Event::Activate => {
                tracing::info!("IM relay: ACTIVATE - text field focused");
                state.active = true;
                let _ = state.event_tx.send(ImEvent::Activated);
            }
            Event::Deactivate => {
                tracing::info!("IM relay: DEACTIVATE - text field unfocused");
                state.active = false;
                let _ = state.event_tx.send(ImEvent::Deactivated);
            }
            Event::Done => {
                state.serial = state.serial.wrapping_add(1);
                tracing::debug!("IM relay: done, serial={}", state.serial);
            }
            Event::SurroundingText { text, cursor, anchor } => {
                tracing::debug!(
                    "IM relay: surrounding_text cursor={} anchor={} text={:?}",
                    cursor,
                    anchor,
                    text
                );
            }
            Event::ContentType { hint, purpose } => {
                tracing::debug!("IM relay: content_type hint={:?} purpose={:?}", hint, purpose);
            }
            Event::TextChangeCause { cause } => {
                tracing::debug!("IM relay: text_change_cause={:?}", cause);
            }
            Event::Unavailable => tracing::warn!("IM relay: unavailable"),
            _ => tracing::debug!("IM relay: other event"),
        }
    }
}
