//! Input method relay - connects to ourselves to activate input_method protocol
//!
//! This is a hack to make Smithay's text_input work without an external IM client.
//! We connect to our own compositor as a client and bind to input_method_v2.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::ZwpInputMethodV2,
};

/// Handle to the relay thread (keeps connection alive)
pub struct ImRelay {
    _handle: thread::JoinHandle<()>,
}

impl ImRelay {
    /// Spawn a thread that connects to our compositor as an input method
    pub fn connect(socket_path: &std::path::Path) -> Option<Self> {
        let path = socket_path.to_path_buf();
        let handle = thread::spawn(move || {
            // Small delay to let the event loop start
            thread::sleep(std::time::Duration::from_millis(100));
            if let Err(e) = run_relay(path) {
                tracing::warn!("IM relay error: {}", e);
            }
        });
        Some(ImRelay { _handle: handle })
    }
}

fn run_relay(socket_path: PathBuf) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stream = UnixStream::connect(&socket_path)?;
    let conn = Connection::from_socket(stream)?;
    let (globals, mut queue) = registry_queue_init::<RelayState>(&conn)?;
    let qh = queue.handle();

    let mut state = RelayState { _input_method: None };

    // Bind to input_method_manager
    let im_manager: ZwpInputMethodManagerV2 = globals.bind(&qh, 1..=1, ())?;

    // Bind to seat and get input_method
    let seat: wayland_client::protocol::wl_seat::WlSeat = globals.bind(&qh, 1..=9, ())?;
    let input_method = im_manager.get_input_method(&seat, &qh, ());
    state._input_method = Some(input_method);

    // Flush to ensure server sees our requests
    conn.flush()?;

    tracing::info!("Input method relay connected");

    // Keep the connection alive - just process events forever
    loop {
        queue.blocking_dispatch(&mut state)?;
    }
}

struct RelayState {
    _input_method: Option<ZwpInputMethodV2>,
}

// Minimal dispatch implementations
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for RelayState {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event,
             _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wayland_client::protocol::wl_seat::WlSeat, ()> for RelayState {
    fn event(_: &mut Self, _: &wayland_client::protocol::wl_seat::WlSeat,
             _: wayland_client::protocol::wl_seat::Event, _: &(),
             _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpInputMethodManagerV2, ()> for RelayState {
    fn event(_: &mut Self, _: &ZwpInputMethodManagerV2,
             _: wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2::Event,
             _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpInputMethodV2, ()> for RelayState {
    fn event(_: &mut Self, _: &ZwpInputMethodV2,
             event: wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2::Event,
             _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2::Event;
        match event {
            Event::Activate => tracing::info!("IM relay: ACTIVATE - text field focused"),
            Event::Deactivate => tracing::info!("IM relay: DEACTIVATE - text field unfocused"),
            Event::Done => tracing::debug!("IM relay: done"),
            Event::SurroundingText { text, cursor, anchor } => {
                tracing::debug!("IM relay: surrounding_text cursor={} anchor={} text={:?}", cursor, anchor, text);
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
