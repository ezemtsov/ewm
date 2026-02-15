//! Test fixture for integration testing
//!
//! The Fixture provides a complete compositor environment for testing,
//! including a headless backend, event loop, and Wayland display.

use std::time::Duration;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use tracing::info;

use crate::backend::{Backend, HeadlessBackend};
use crate::{Ewm, State};

use super::client::{ClientId, ClientManager, TestClient};

/// Test fixture for integration testing
///
/// Provides a complete compositor environment with:
/// - Event loop for async operations
/// - Headless backend for virtual outputs
/// - Wayland display for protocol testing
/// - Client manager for simulating Wayland clients
///
/// Uses the same `State` struct as production, but with `Backend::Headless`.
pub struct Fixture {
    event_loop: EventLoop<'static, State>,
    state: State,
    _display: Display<State>,
    clients: ClientManager,
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
        let ewm = Ewm::new(display_handle, event_loop.handle());
        let backend = Backend::Headless(HeadlessBackend::new());

        let state = State { backend, ewm };
        let clients = ClientManager::new();

        info!("Test fixture initialized with headless backend");

        Ok(Self {
            event_loop,
            state,
            _display: display,
            clients,
        })
    }

    /// Add a new test client
    ///
    /// Returns a ClientId that can be used to interact with the client.
    pub fn add_client(&mut self) -> ClientId {
        self.clients.add_client()
    }

    /// Get a test client by ID
    pub fn get_client(&self, id: ClientId) -> Option<&TestClient> {
        self.clients.get_client(id)
    }

    /// Get a mutable test client by ID
    pub fn get_client_mut(&mut self, id: ClientId) -> Option<&mut TestClient> {
        self.clients.get_client_mut(id)
    }

    /// Get total number of test clients
    pub fn client_count(&self) -> usize {
        self.clients.total_surfaces()
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
    pub fn focused_surface_id(&self) -> u32 {
        self.state.ewm.focused_surface_id
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
    pub fn has_surface(&self, id: u32) -> bool {
        self.state.ewm.id_windows.contains_key(&id)
    }

    /// Per-frame processing callback (simplified version of production refresh_and_flush_clients)
    fn refresh_and_flush_clients(&mut self) {
        // Process module commands
        for cmd in crate::module::drain_commands() {
            self.handle_module_command(cmd);
        }

        // Sync keyboard focus after module commands (matches production behavior)
        self.state.sync_keyboard_focus();

        // Process queued redraws
        self.state
            .backend
            .redraw_queued_outputs(&mut self.state.ewm);

        // Flush Wayland clients
        self.state.ewm.display_handle.flush_clients().ok();
    }

    /// Handle module commands (simplified for testing)
    fn handle_module_command(&mut self, cmd: crate::module::ModuleCommand) {
        use crate::module::ModuleCommand;
        match cmd {
            ModuleCommand::Layout { id, x, y, w, h } => {
                if let Some(window) = self.state.ewm.id_windows.get(&id) {
                    self.state
                        .ewm
                        .space
                        .map_element(window.clone(), (x, y), true);
                    self.state.ewm.space.raise_element(window, true);
                    window.toplevel().map(|t| {
                        t.with_pending_state(|state| {
                            state.size = Some((w as i32, h as i32).into());
                        });
                        t.send_configure();
                    });
                    self.state.ewm.queue_redraw_all();
                }
            }
            ModuleCommand::Focus { id } => {
                if self.state.ewm.focused_surface_id != id
                    && self.state.ewm.id_windows.contains_key(&id)
                {
                    self.state
                        .ewm
                        .focus_surface_with_source(id, false, "test", None);
                }
            }
            ModuleCommand::Views { id, views } => {
                if let Some(window) = self.state.ewm.id_windows.get(&id) {
                    if let Some(view) = views.iter().find(|v| v.active).or_else(|| views.first()) {
                        self.state
                            .ewm
                            .space
                            .map_element(window.clone(), (view.x, view.y), true);
                        self.state.ewm.space.raise_element(window, true);
                        window.toplevel().map(|t| {
                            t.with_pending_state(|state| {
                                state.size = Some((view.w as i32, view.h as i32).into());
                            });
                            t.send_configure();
                        });
                    }
                    self.state.ewm.surface_views.insert(id, views);
                    self.state.ewm.queue_redraw_all();
                }
            }
            ModuleCommand::Hide { id } => {
                if self.state.ewm.surface_views.contains_key(&id) {
                    if let Some(window) = self.state.ewm.id_windows.get(&id) {
                        self.state
                            .ewm
                            .space
                            .map_element(window.clone(), (-10000, -10000), false);
                        self.state.ewm.surface_views.remove(&id);
                        self.state.ewm.queue_redraw_all();
                    }
                }
            }
            ModuleCommand::Close { id } => {
                if let Some(window) = self.state.ewm.id_windows.get(&id) {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.send_close();
                    }
                }
            }
            // Other commands can be added as needed for testing
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fixture_creation() {
        let fixture = Fixture::new();
        assert!(fixture.is_ok());
    }

    #[test]
    fn test_add_output() {
        let mut fixture = Fixture::new().unwrap();
        fixture.add_output("test", 1920, 1080);
        assert_eq!(fixture.output_count(), 1);
    }

    #[test]
    fn test_remove_output() {
        let mut fixture = Fixture::new().unwrap();
        fixture.add_output("test1", 1920, 1080);
        fixture.add_output("test2", 1280, 720);
        assert_eq!(fixture.output_count(), 2);
        fixture.remove_output("test1");
        assert_eq!(fixture.output_count(), 1);
    }

    #[test]
    fn test_dispatch_triggers_redraw() {
        let mut fixture = Fixture::new().unwrap();
        fixture.add_output("test", 1920, 1080);
        fixture.dispatch();
        // Just verify dispatch doesn't panic
    }

    #[test]
    fn test_output_size_calculation() {
        let mut fixture = Fixture::new().unwrap();
        fixture.add_output("test1", 1920, 1080);
        fixture.add_output("test2", 1280, 720);
        // After adding two outputs side by side, total width should be sum
        let (total_w, total_h) = fixture.ewm().output_size;
        assert_eq!(total_w, 1920 + 1280);
        assert_eq!(total_h, 1080); // Height is max of outputs
    }
}
