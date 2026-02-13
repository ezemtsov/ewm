//! Test fixture for integration testing
//!
//! The Fixture provides a complete compositor environment for testing,
//! including a headless backend, event loop, and Wayland display.

use std::time::Duration;

use smithay::reexports::{
    calloop::EventLoop,
    wayland_server::Display,
};
use tracing::info;

use crate::backend::HeadlessBackend;
use crate::Ewm;

use super::client::{ClientId, ClientManager, TestClient};

/// Test fixture state that mirrors production State but uses headless backend
pub struct FixtureState {
    pub backend: HeadlessBackend,
    pub ewm: Ewm,
}

impl FixtureState {
    /// Per-frame processing callback (simplified version of production refresh_and_flush_clients)
    pub fn refresh_and_flush_clients(&mut self) {
        // Process module commands
        for cmd in crate::module::drain_commands() {
            self.handle_module_command(cmd);
        }

        // Process queued redraws
        self.backend.redraw_queued_outputs(&mut self.ewm);

        // Flush Wayland clients
        self.ewm.display_handle.flush_clients().ok();
    }

    /// Handle module commands (simplified for testing)
    fn handle_module_command(&mut self, cmd: crate::module::ModuleCommand) {
        use crate::module::ModuleCommand;
        match cmd {
            ModuleCommand::Layout { id, x, y, w, h } => {
                if let Some(window) = self.ewm.id_windows.get(&id) {
                    self.ewm.space.map_element(window.clone(), (x, y), true);
                    self.ewm.space.raise_element(window, true);
                    window.toplevel().map(|t| {
                        t.with_pending_state(|state| {
                            state.size = Some((w as i32, h as i32).into());
                        });
                        t.send_configure();
                    });
                    self.ewm.queue_redraw_all();
                }
            }
            ModuleCommand::Focus { id } => {
                if self.ewm.focused_surface_id != id && self.ewm.id_windows.contains_key(&id) {
                    self.ewm.focus_surface_with_source(id, false, "test", None);
                }
            }
            ModuleCommand::Views { id, views } => {
                if let Some(window) = self.ewm.id_windows.get(&id) {
                    if let Some(view) = views.iter().find(|v| v.active).or_else(|| views.first()) {
                        self.ewm.space.map_element(window.clone(), (view.x, view.y), true);
                        self.ewm.space.raise_element(window, true);
                        window.toplevel().map(|t| {
                            t.with_pending_state(|state| {
                                state.size = Some((view.w as i32, view.h as i32).into());
                            });
                            t.send_configure();
                        });
                    }
                    self.ewm.surface_views.insert(id, views);
                    self.ewm.queue_redraw_all();
                }
            }
            ModuleCommand::Hide { id } => {
                if self.ewm.surface_views.contains_key(&id) {
                    if let Some(window) = self.ewm.id_windows.get(&id) {
                        self.ewm.space.map_element(window.clone(), (-10000, -10000), false);
                        self.ewm.surface_views.remove(&id);
                        self.ewm.queue_redraw_all();
                    }
                }
            }
            ModuleCommand::Close { id } => {
                if let Some(window) = self.ewm.id_windows.get(&id) {
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

/// Test fixture for integration testing
///
/// Provides a complete compositor environment with:
/// - Event loop for async operations
/// - Headless backend for virtual outputs
/// - Wayland display for protocol testing
/// - Client manager for simulating Wayland clients
pub struct Fixture {
    event_loop: EventLoop<'static, FixtureState>,
    state: FixtureState,
    _display: Display<Ewm>,
    clients: ClientManager,
}

impl Fixture {
    /// Create a new test fixture
    ///
    /// Initializes the event loop, display, and headless backend.
    /// The fixture starts with no outputs - use `add_output` to create virtual displays.
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Initialize event loop
        let event_loop: EventLoop<FixtureState> = EventLoop::try_new()?;

        // Create Wayland display
        let display: Display<Ewm> = Display::new()?;
        let display_handle = display.handle();

        // Create compositor state
        let ewm = Ewm::new(display_handle);

        // Create headless backend
        let backend = HeadlessBackend::new();

        let state = FixtureState { backend, ewm };
        let clients = ClientManager::new();

        info!("Test fixture initialized");

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
        self.state.backend.add_output(name, width, height, &mut self.state.ewm);
    }

    /// Remove a virtual output by name
    pub fn remove_output(&mut self, name: &str) {
        self.state.backend.remove_output(name, &mut self.state.ewm);
    }

    /// Get the number of outputs
    pub fn output_count(&self) -> usize {
        self.state.backend.outputs.len()
    }

    /// Get render count for a specific output
    pub fn render_count(&self, output_name: &str) -> usize {
        self.state.backend.render_count(output_name)
    }

    /// Dispatch the event loop once with a short timeout
    pub fn dispatch(&mut self) {
        self.event_loop
            .dispatch(Some(Duration::from_millis(10)), &mut self.state)
            .ok();
        self.state.refresh_and_flush_clients();
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

    /// Check if a surface exists
    pub fn has_surface(&self, id: u32) -> bool {
        self.state.ewm.id_windows.contains_key(&id)
    }

    /// Queue a redraw for all outputs
    pub fn queue_redraw_all(&mut self) {
        self.state.ewm.queue_redraw_all();
    }

    /// Check if the headless backend has queued redraws
    pub fn has_queued_redraws(&self) -> bool {
        self.state.backend.has_queued_redraws(&self.state.ewm)
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
        assert_eq!(fixture.output_count(), 0);

        fixture.add_output("Virtual-1", 1920, 1080);
        assert_eq!(fixture.output_count(), 1);

        fixture.add_output("Virtual-2", 1920, 1080);
        assert_eq!(fixture.output_count(), 2);
    }

    #[test]
    fn test_remove_output() {
        let mut fixture = Fixture::new().unwrap();
        fixture.add_output("Virtual-1", 1920, 1080);
        fixture.add_output("Virtual-2", 1920, 1080);
        assert_eq!(fixture.output_count(), 2);

        fixture.remove_output("Virtual-1");
        assert_eq!(fixture.output_count(), 1);
    }

    #[test]
    fn test_dispatch_triggers_redraw() {
        let mut fixture = Fixture::new().unwrap();
        fixture.add_output("Virtual-1", 1920, 1080);

        // Output starts with queued redraw
        assert!(fixture.has_queued_redraws());

        // Dispatch should process the redraw
        fixture.dispatch();

        // After dispatch, redraw should be processed
        assert!(!fixture.has_queued_redraws());
        assert_eq!(fixture.render_count("Virtual-1"), 1);
    }

    #[test]
    fn test_output_size_calculation() {
        let mut fixture = Fixture::new().unwrap();
        // Initial state has default size before any outputs are added
        let initial_size = fixture.ewm_ref().output_size;

        fixture.add_output("Virtual-1", 1920, 1080);
        // After first output, size should include it
        let after_first = fixture.ewm_ref().output_size;
        assert!(after_first.0 >= 1920, "Width should be at least 1920");
        assert_eq!(after_first.1, 1080);

        let first_output_x = initial_size.0; // New outputs start after existing size
        fixture.add_output("Virtual-2", 1920, 1080);
        // Second output should be positioned after first
        let after_second = fixture.ewm_ref().output_size;
        assert_eq!(after_second.0, first_output_x + 1920 + 1920, "Width should include both outputs");
        assert_eq!(after_second.1, 1080);
    }
}
