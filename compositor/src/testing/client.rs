//! Test client for protocol-level testing
//!
//! The TestClient simulates a Wayland client by directly interacting with
//! the compositor's protocol handlers. This allows testing surface lifecycle,
//! configure sequences, and focus behavior without a real Wayland connection.
//!
//! # Design
//!
//! Instead of using real Wayland sockets, TestClient directly creates protocol
//! objects and triggers the compositor's handlers. This provides:
//!
//! 1. **Deterministic testing**: No async IPC, all operations are synchronous
//! 2. **Direct state inspection**: Can check compositor state after each operation
//! 3. **Snapshot compatibility**: Output is reproducible for insta snapshots

use std::collections::VecDeque;

use crate::Ewm;

/// A unique identifier for a test client
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(pub usize);

/// A test surface created by a TestClient
#[derive(Debug)]
pub struct TestSurface {
    /// The compositor's surface ID
    pub id: u32,
    /// Configure events received (width, height, states)
    pub configures: VecDeque<ConfigureEvent>,
    /// Whether the surface has been mapped
    pub mapped: bool,
}

/// A configure event for a toplevel surface
#[derive(Debug, Clone)]
pub struct ConfigureEvent {
    pub width: i32,
    pub height: i32,
    pub states: Vec<String>,
    pub serial: u32,
}

impl TestSurface {
    /// Create a new test surface
    pub fn new(id: u32) -> Self {
        Self {
            id,
            configures: VecDeque::new(),
            mapped: false,
        }
    }

    /// Get the last configure event
    pub fn last_configure(&self) -> Option<&ConfigureEvent> {
        self.configures.back()
    }

    /// Format configures for snapshot testing
    pub fn format_configures(&self) -> String {
        use std::fmt::Write;
        let mut output = String::new();
        for (i, cfg) in self.configures.iter().enumerate() {
            writeln!(
                &mut output,
                "configure[{}]: {}x{} states={:?}",
                i, cfg.width, cfg.height, cfg.states
            )
            .unwrap();
        }
        output
    }
}

/// A simulated Wayland client for testing
///
/// TestClient allows tests to simulate client behavior and inspect
/// the compositor's response without real Wayland protocol communication.
///
/// # Example
///
/// ```ignore
/// let mut fixture = Fixture::new().unwrap();
/// let mut client = TestClient::new(ClientId(1));
///
/// // Create a toplevel surface
/// let surface_id = client.create_toplevel(&mut fixture);
///
/// // Dispatch to process the surface
/// fixture.dispatch();
///
/// // Check the configure sequence
/// let surface = client.get_surface(surface_id).unwrap();
/// assert!(!surface.configures.is_empty());
/// ```
pub struct TestClient {
    /// Client identifier
    pub id: ClientId,
    /// Surfaces owned by this client
    surfaces: Vec<TestSurface>,
    /// Next surface ID to assign (local to this client)
    next_surface_id: usize,
}

impl TestClient {
    /// Create a new test client
    pub fn new(id: ClientId) -> Self {
        Self {
            id,
            surfaces: Vec::new(),
            next_surface_id: 0,
        }
    }

    /// Create a new toplevel surface
    ///
    /// This simulates a client creating an xdg_toplevel. The compositor
    /// will assign a surface ID and send initial configure events.
    ///
    /// Returns the compositor's surface ID for the new surface.
    pub fn create_toplevel(&mut self, ewm: &mut Ewm) -> u32 {
        // The compositor assigns IDs starting from 1
        // We can predict the next ID from next_surface_id in Ewm
        let expected_id = ewm.id_windows.len() as u32 + 1;

        // For now, we track that we want to create a surface
        // The actual surface creation happens through the XdgShellHandler
        // In a real implementation, we'd need to trigger protocol events

        let surface = TestSurface::new(expected_id);
        self.surfaces.push(surface);
        self.next_surface_id += 1;

        expected_id
    }

    /// Get a surface by compositor ID
    pub fn get_surface(&self, id: u32) -> Option<&TestSurface> {
        self.surfaces.iter().find(|s| s.id == id)
    }

    /// Get a mutable surface by compositor ID
    pub fn get_surface_mut(&mut self, id: u32) -> Option<&mut TestSurface> {
        self.surfaces.iter_mut().find(|s| s.id == id)
    }

    /// Record a configure event for a surface
    pub fn record_configure(&mut self, surface_id: u32, event: ConfigureEvent) {
        if let Some(surface) = self.get_surface_mut(surface_id) {
            surface.configures.push_back(event);
        }
    }

    /// Get all surfaces owned by this client
    pub fn surfaces(&self) -> &[TestSurface] {
        &self.surfaces
    }

    /// Get the number of surfaces
    pub fn surface_count(&self) -> usize {
        self.surfaces.len()
    }
}

/// Manager for test clients in a fixture
pub struct ClientManager {
    clients: Vec<TestClient>,
    next_client_id: usize,
}

impl Default for ClientManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientManager {
    /// Create a new client manager
    pub fn new() -> Self {
        Self {
            clients: Vec::new(),
            next_client_id: 0,
        }
    }

    /// Create a new test client
    pub fn add_client(&mut self) -> ClientId {
        let id = ClientId(self.next_client_id);
        self.next_client_id += 1;
        self.clients.push(TestClient::new(id));
        id
    }

    /// Get a client by ID
    pub fn get_client(&self, id: ClientId) -> Option<&TestClient> {
        self.clients.iter().find(|c| c.id == id)
    }

    /// Get a mutable client by ID
    pub fn get_client_mut(&mut self, id: ClientId) -> Option<&mut TestClient> {
        self.clients.iter_mut().find(|c| c.id == id)
    }

    /// Get total number of surfaces across all clients
    pub fn total_surfaces(&self) -> usize {
        self.clients.iter().map(|c| c.surface_count()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let mut manager = ClientManager::new();
        let id1 = manager.add_client();
        let id2 = manager.add_client();

        assert_eq!(id1, ClientId(0));
        assert_eq!(id2, ClientId(1));
        assert!(manager.get_client(id1).is_some());
        assert!(manager.get_client(id2).is_some());
    }

    #[test]
    fn test_surface_configure_tracking() {
        let mut surface = TestSurface::new(1);

        surface.configures.push_back(ConfigureEvent {
            width: 800,
            height: 600,
            states: vec!["maximized".to_string()],
            serial: 1,
        });

        assert_eq!(surface.configures.len(), 1);
        let cfg = surface.last_configure().unwrap();
        assert_eq!(cfg.width, 800);
        assert_eq!(cfg.height, 600);
    }

    #[test]
    fn test_format_configures() {
        let mut surface = TestSurface::new(1);

        surface.configures.push_back(ConfigureEvent {
            width: 1920,
            height: 1080,
            states: vec!["maximized".to_string(), "activated".to_string()],
            serial: 1,
        });

        let formatted = surface.format_configures();
        assert!(formatted.contains("1920x1080"));
        assert!(formatted.contains("maximized"));
    }
}
