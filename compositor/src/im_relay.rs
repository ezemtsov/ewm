//! Input method relay: keepalive self-connection for input_method_v2.
//!
//! Smithay's text_input_v3 requires an input_method_v2 instance (`has_instance()`).
//! This relay satisfies that by connecting to our own compositor as a Wayland client.
//! It forwards Activate/Deactivate events to the compositor thread; text commits
//! go directly through TextInputHandle to avoid cross-thread serial races.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2::ZwpInputMethodManagerV2;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2::ZwpInputMethodV2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImEvent {
    Activated,
    Deactivated,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PendingCommit {
    pub surface_id: u64,
    pub text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommitResult {
    Deliver(String),
    Queued,
    DroppedStale,
}

/// Surface-aware commit gate for the input-method relay.
///
/// Commits may arrive while a client is cycling text-input disable/enable.
/// Queue only while no active text-input surface is known; once a surface is
/// active, text for any other surface is stale and must not be delivered later.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImRelayState {
    active: bool,
    pending_commits: Vec<PendingCommit>,
}

impl ImRelayState {
    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn pending_commits(&self) -> &[PendingCommit] {
        &self.pending_commits
    }

    pub fn activate(&mut self) -> bool {
        if self.active {
            false
        } else {
            self.active = true;
            true
        }
    }

    pub fn deactivate(&mut self) -> bool {
        if self.active {
            self.active = false;
            true
        } else {
            false
        }
    }

    pub fn commit(
        &mut self,
        active_surface_id: Option<u64>,
        target_surface_id: u64,
        text: String,
    ) -> CommitResult {
        match active_surface_id {
            Some(active) if active == target_surface_id => CommitResult::Deliver(text),
            Some(_) => CommitResult::DroppedStale,
            None => {
                self.pending_commits.push(PendingCommit {
                    surface_id: target_surface_id,
                    text,
                });
                CommitResult::Queued
            }
        }
    }

    /// Drain queued text for `active_surface_id`, dropping text for any other
    /// surface because it is stale once a different surface is active.
    pub fn drain_pending_for_active_surface(
        &mut self,
        active_surface_id: Option<u64>,
    ) -> Option<String> {
        let active_surface_id = active_surface_id?;
        let mut text = String::new();
        for commit in self.pending_commits.drain(..) {
            if commit.surface_id == active_surface_id {
                text.push_str(&commit.text);
            }
        }
        (!text.is_empty()).then_some(text)
    }
}

/// Must stay alive for the compositor lifetime. Dropping destroys the
/// input_method_v2 instance and disables text_input_v3.
pub struct ImRelay {
    _handle: thread::JoinHandle<()>,
    pub event_rx: Receiver<ImEvent>,
}

impl ImRelay {
    /// Spawn the relay thread. The actual Wayland connection happens
    /// asynchronously; failures are logged by the thread.
    pub fn connect(socket_path: &std::path::Path) -> Self {
        let path = socket_path.to_path_buf();
        let (event_tx, event_rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            if let Err(e) = run_relay(path, event_tx) {
                tracing::warn!("IM relay error: {}", e);
            }
        });

        ImRelay {
            _handle: handle,
            event_rx,
        }
    }
}

fn run_relay(
    socket_path: PathBuf,
    event_tx: Sender<ImEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stream = UnixStream::connect(&socket_path)?;
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
            Event::Unavailable => tracing::warn!("IM relay: unavailable"),
            _ => {}
        }
    }
}
