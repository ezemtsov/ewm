//! Input method relay: keepalive self-connection for input_method_v2.
//!
//! Smithay's text_input_v3 requires an input_method_v2 instance (`has_instance()`).
//! This relay satisfies that by connecting to our own compositor as a Wayland client.
//! It forwards Activate/Deactivate events over a calloop channel so activation
//! is processed before the next key, not one dispatch late; text commits go
//! directly through TextInputHandle to avoid cross-thread serial races.

use std::os::unix::net::UnixStream;
use std::thread;

use smithay::reexports::calloop::channel::{Channel, Sender, channel};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2::ZwpInputMethodManagerV2;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2::ZwpInputMethodV2;

#[derive(Debug, PartialEq, Eq)]
pub enum ImEvent {
    Activated,
    Deactivated,
    /// `cursor`/`anchor` are byte offsets into `text`.
    SurroundingText {
        text: String,
        cursor: u32,
        anchor: u32,
    },
}

/// Spawn the relay thread on STREAM and return the calloop channel carrying its
/// activate/deactivate events. The detached thread owns the Wayland connection
/// backing the input_method_v2 instance for the process lifetime.
pub fn connect(stream: UnixStream) -> Channel<ImEvent> {
    let (event_tx, event_rx) = channel();

    thread::spawn(move || {
        if let Err(e) = run_relay(stream, event_tx) {
            tracing::warn!("IM relay error: {}", e);
        }
    });

    event_rx
}

fn run_relay(
    stream: UnixStream,
    event_tx: Sender<ImEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::from_socket(stream)?;
    let (globals, mut queue) = registry_queue_init::<RelayState>(&conn)?;
    let qh = queue.handle();

    let mut state = RelayState { event_tx };

    let im_manager: ZwpInputMethodManagerV2 = globals.bind(&qh, 1..=1, ())?;
    let seat: wayland_client::protocol::wl_seat::WlSeat = globals.bind(&qh, 1..=9, ())?;
    let _input_method = im_manager.get_input_method(&seat, &qh, ());

    conn.flush()?;
    tracing::info!("IM relay connected");

    loop {
        if let Err(e) = queue.blocking_dispatch(&mut state) {
            tracing::warn!("IM relay dispatch error: {}", e);
            break;
        }
    }

    Ok(())
}

struct RelayState {
    event_tx: Sender<ImEvent>,
}

// Required Dispatch impls. Only ZwpInputMethodV2 events are meaningful.

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
                let _ = state.event_tx.send(ImEvent::Activated);
            }
            Event::Deactivate => {
                let _ = state.event_tx.send(ImEvent::Deactivated);
            }
            Event::SurroundingText {
                text,
                cursor,
                anchor,
            } => {
                let _ = state.event_tx.send(ImEvent::SurroundingText {
                    text,
                    cursor,
                    anchor,
                });
            }
            Event::Unavailable => tracing::warn!("IM relay: unavailable"),
            _ => {}
        }
    }
}
