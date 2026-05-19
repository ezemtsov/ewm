//! Test fixture for integration testing
//!
//! The Fixture provides a complete compositor environment for testing,
//! including a headless backend, event loop, and Wayland display.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode as CalloopMode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Logical, Rectangle, Size};
use tracing::info;

use crate::backend::{Backend, HeadlessBackend};
use crate::testing::TestClient;
use crate::{ClientState, Ewm, State};

/// Test fixture for integration testing
///
/// Provides a complete compositor environment with:
/// - Event loop for async operations
/// - Headless backend for virtual outputs
/// - Wayland display for protocol testing
///
/// Uses the same `State` struct as production, but with `Backend::Headless`.
pub struct Fixture {
    event_loop: EventLoop<'static, State>,
    state: State,
}

impl Fixture {
    /// Create a new test fixture
    ///
    /// Initializes the event loop, display, and headless backend.
    /// The fixture starts with no outputs - use `add_output` to create virtual displays.
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Initialize event loop
        let event_loop: EventLoop<State> = EventLoop::try_new()?;

        // Create Wayland display (typed for State, which has handlers)
        let display: Display<State> = Display::new()?;
        let display_handle = display.handle();

        // Create compositor state with headless backend
        let ewm = Ewm::new(display_handle, event_loop.handle(), false);
        let backend = Backend::Headless(HeadlessBackend::new());

        let state = State { backend, ewm };

        // Dispatch inserted clients on each event_loop tick.
        let display_source = Generic::new(display, Interest::READ, CalloopMode::Level);
        event_loop
            .handle()
            .insert_source(display_source, |_, display, state| {
                let display = unsafe { display.get_mut() };
                if let Err(e) = display.dispatch_clients(state) {
                    tracing::error!("test wayland dispatch error: {e}");
                }
                Ok(PostAction::Continue)
            })
            .expect("Failed to insert display source");

        info!("Test fixture initialized with headless backend");

        Ok(Self { event_loop, state })
    }

    /// Pair a `UnixStream` with the server `Display`, returning a `TestClient`
    /// on the other end. Roundtrip after to bind globals.
    pub fn new_client(&mut self) -> Result<TestClient, Box<dyn std::error::Error>> {
        let (server_end, client_end) = UnixStream::pair()?;
        self.state
            .ewm
            .display_handle
            .insert_client(server_end, Arc::new(ClientState::default()))?;
        TestClient::new(client_end)
    }

    /// Drive a full xdg toplevel lifecycle: request, configure/ack, attach
    /// an SHM buffer + commit. The buffer is what makes server-side
    /// `RendererSurfaceState::surface_view` (and the committed xdg state)
    /// flip to populated, so hit-tests find the surface.
    pub fn open_toplevel(
        &mut self,
        client: &mut TestClient,
        title: &str,
        size: Size<i32, Logical>,
    ) -> crate::testing::Toplevel {
        let toplevel = client.create_toplevel(title);
        self.roundtrip(client);
        let buffer = client.create_shm_buffer(size);
        toplevel.surface.attach(Some(&buffer), 0, 0);
        toplevel.surface.damage_buffer(0, 0, size.w, size.h);
        toplevel.xdg_surface.set_window_geometry(0, 0, size.w, size.h);
        toplevel.surface.commit();
        self.roundtrip(client);
        toplevel
    }

    /// Drive a full xdg popup lifecycle: request, configure/ack, attach
    /// an SHM buffer + commit.
    pub fn open_popup(
        &mut self,
        client: &mut TestClient,
        parent: &crate::testing::Toplevel,
        anchor: Rectangle<i32, Logical>,
        size: Size<i32, Logical>,
    ) -> crate::testing::Popup {
        let popup = client.create_popup(parent, anchor, size);
        self.roundtrip(client);
        let buffer = client.create_shm_buffer(size);
        popup.surface.attach(Some(&buffer), 0, 0);
        popup.surface.damage_buffer(0, 0, size.w, size.h);
        popup.xdg_surface.set_window_geometry(0, 0, size.w, size.h);
        popup.surface.commit();
        self.roundtrip(client);
        popup
    }

    /// Pump client and server until both sides go idle for two consecutive
    /// passes (one quiet pass can still race a delayed server flush).
    pub fn roundtrip(&mut self, client: &mut TestClient) {
        let mut idle_passes = 0;
        for _ in 0..32 {
            let _ = client.eq.flush();
            self.event_loop
                .dispatch(Some(Duration::from_millis(5)), &mut self.state)
                .ok();
            self.refresh_and_flush_clients();
            if let Some(guard) = client.conn.prepare_read() {
                let _ = guard.read();
            }
            let dispatched = client.eq.dispatch_pending(&mut client.state).unwrap_or(0);
            if dispatched == 0 {
                idle_passes += 1;
                if idle_passes >= 2 {
                    return;
                }
            } else {
                idle_passes = 0;
            }
        }
    }

    /// Find the server-side surface_id for a client `WlSurface`. Matches by
    /// the protocol wire id (unique within a client connection).
    pub fn find_surface_id(
        &self,
        client_surface: &wayland_client::protocol::wl_surface::WlSurface,
    ) -> Option<u64> {
        use smithay::reexports::wayland_server::Resource as _;
        use smithay::wayland::seat::WaylandFocus;
        use wayland_client::Proxy as _;
        let target = client_surface.id().protocol_id();
        for (&id, window) in &self.state.ewm.id_windows {
            let Some(server_surface) = window.wl_surface() else {
                continue;
            };
            if server_surface.id().protocol_id() == target {
                return Some(id);
            }
        }
        None
    }

    /// Add a virtual output with the given name and size
    pub fn add_output(&mut self, name: &str, width: i32, height: i32) {
        if let Some(headless) = self.state.backend.as_headless_mut() {
            headless.add_output(name, width, height, &mut self.state.ewm);
        }
    }

    /// Remove a virtual output by name
    pub fn remove_output(&mut self, name: &str) {
        if let Some(headless) = self.state.backend.as_headless_mut() {
            headless.remove_output(name, &mut self.state.ewm);
        }
    }

    /// Get the number of outputs
    pub fn output_count(&self) -> usize {
        if let Some(headless) = self.state.backend.as_headless() {
            headless.outputs.len()
        } else {
            0
        }
    }

    pub fn has_output(&self, name: &str) -> bool {
        self.state
            .backend
            .as_headless()
            .is_some_and(|h| h.outputs.contains_key(name))
    }

    /// Get render count for a specific output
    pub fn render_count(&self, output_name: &str) -> usize {
        if let Some(headless) = self.state.backend.as_headless() {
            headless.render_count(output_name)
        } else {
            0
        }
    }

    /// Dispatch the event loop once with a short timeout
    pub fn dispatch(&mut self) {
        self.event_loop
            .dispatch(Some(Duration::from_millis(10)), &mut self.state)
            .ok();
        self.refresh_and_flush_clients();
    }

    /// Dispatch the event loop multiple times to allow async operations to complete
    pub fn dispatch_roundtrip(&mut self, iterations: usize) {
        for _ in 0..iterations {
            self.dispatch();
        }
    }

    /// Get mutable access to the compositor state
    pub fn ewm(&mut self) -> &mut Ewm {
        &mut self.state.ewm
    }

    /// Get immutable access to the compositor state
    pub fn ewm_ref(&self) -> &Ewm {
        &self.state.ewm
    }

    /// Get the focused surface ID
    pub fn focused_surface_id(&self) -> u64 {
        self.state.ewm.focused_surface_id()
    }

    /// Get the number of tracked surfaces
    pub fn surface_count(&self) -> usize {
        self.state.ewm.id_windows.len()
    }

    /// Check if backend has any queued redraws
    pub fn has_queued_redraws(&self) -> bool {
        self.state.backend.has_queued_redraws(&self.state.ewm)
    }

    /// Queue redraws for all outputs
    pub fn queue_redraw_all(&mut self) {
        self.state.ewm.queue_redraw_all();
    }

    /// Apply stored output config for the named output (headless backend)
    pub fn apply_output_config(&mut self, output_name: &str) {
        self.state
            .backend
            .apply_output_config(&mut self.state.ewm, output_name);
    }

    /// Check if a surface with the given ID exists
    pub fn has_surface(&self, id: u64) -> bool {
        self.state.ewm.id_windows.contains_key(&id)
    }

    /// Warp the pointer to an absolute position.
    pub fn warp_pointer(&mut self, x: f64, y: f64) {
        self.state.warp_pointer(x, y);
    }

    /// Send a pointer button event at the seat's current pointer focus.
    /// Mirrors the production `button` path: pointer.button + pointer.frame.
    pub fn pointer_button(&mut self, button: u32, pressed: bool) {
        use smithay::backend::input::ButtonState;
        use smithay::input::pointer::ButtonEvent;
        use smithay::utils::SERIAL_COUNTER;
        let pointer = self.state.ewm.pointer.clone();
        let state = if pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        };
        pointer.button(
            &mut self.state,
            &ButtonEvent {
                button,
                state,
                serial: SERIAL_COUNTER.next_serial(),
                time: 0,
            },
        );
        pointer.frame(&mut self.state);
    }

    /// Start mirroring `queue_event` into a per-Ewm buffer.
    pub fn enable_event_capture(&mut self) {
        self.state.ewm.captured_events = Some(Vec::new());
    }

    /// Take and clear captured events.  Empty if capture is disabled.
    pub fn drain_events(&mut self) -> Vec<crate::event::Event> {
        self.state
            .ewm
            .captured_events
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// Per-frame processing callback (simplified version of production refresh_and_flush_clients)
    fn refresh_and_flush_clients(&mut self) {
        // Process module commands
        for cmd in crate::module::drain_commands() {
            self.handle_module_command(cmd);
        }

        // Sync keyboard focus after module commands (matches production behavior)
        self.state.sync_keyboard_focus();

        // Refresh workspace protocol state (pull model)
        crate::protocols::workspace::refresh::<crate::State>(
            &mut self.state.ewm.workspace_state,
            &self.state.ewm.output_strips,
            self.state.ewm.space.outputs(),
        );

        // Process queued redraws
        self.state
            .ewm
            .redraw_queued_outputs(&mut self.state.backend);

        // Clean up dead elements (output enter/leave managed explicitly)
        self.state.ewm.cleanup_dead_windows();

        // Flush Wayland clients
        self.state.ewm.display_handle.flush_clients().ok();
    }

    /// Handle module commands (simplified for testing)
    fn handle_module_command(&mut self, cmd: crate::module::ModuleCommand) {
        use crate::module::ModuleCommand;
        match cmd {
            ModuleCommand::Close { id } => {
                if let Some(window) = self.state.ewm.id_windows.get(&id) {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.send_close();
                    }
                }
            }
            ModuleCommand::OutputLayout { output, frames } => {
                self.state.ewm.apply_output_layout(&output, frames);
            }
            // Other commands can be added as needed for testing
            _ => {}
        }
    }
}
