//! D-Bus interfaces for xdg-desktop-portal integration
//!
//! Implements:
//! - org.gnome.Mutter.ScreenCast for screen sharing
//! - org.gnome.Mutter.DisplayConfig for monitor enumeration
//! - org.gnome.Mutter.ServiceChannel for portal client connections
//!
//! Each interface gets its own blocking connection to avoid deadlocks.

pub mod display_config;
pub mod screen_cast;
pub mod service_channel;

use std::sync::Arc;

use smithay::reexports::calloop::channel::{self, Channel};
use smithay::reexports::wayland_server::DisplayHandle;
use tracing::{info, warn};
use zbus::blocking::Connection;
use zbus::object_server::Interface;

pub use display_config::DisplayConfig;
pub use screen_cast::{ScreenCast, ScreenCastToCompositor};
pub use service_channel::ServiceChannel;

/// Output information for D-Bus
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub refresh: u32,
}

/// Trait for starting D-Bus interfaces
trait Start: Interface {
    fn start(self) -> anyhow::Result<Connection>;
}

/// D-Bus server connections
#[derive(Default)]
pub struct DBusServers {
    pub conn_service_channel: Option<Connection>,
    pub conn_display_config: Option<Connection>,
    pub conn_screen_cast: Option<Connection>,
}

impl DBusServers {
    /// Start all D-Bus servers (called from main thread)
    pub fn start(
        outputs: Arc<std::sync::Mutex<Vec<OutputInfo>>>,
        display_handle: DisplayHandle,
    ) -> (Self, Channel<ScreenCastToCompositor>) {
        let mut dbus = Self::default();

        // Start ServiceChannel first (needed for portal compatibility)
        let service_channel = ServiceChannel::new(display_handle);
        dbus.conn_service_channel = try_start(service_channel);

        // Start DisplayConfig
        let display_config = DisplayConfig::new(outputs.clone());
        dbus.conn_display_config = try_start(display_config);

        // Start ScreenCast with channel for compositor communication
        let (sender, receiver) = channel::channel::<ScreenCastToCompositor>();
        let screen_cast = ScreenCast::new(outputs, sender);
        dbus.conn_screen_cast = try_start(screen_cast);

        info!("D-Bus servers started");

        (dbus, receiver)
    }
}

fn try_start<I: Start>(iface: I) -> Option<Connection> {
    info!("Attempting to start D-Bus interface: {}", I::name());
    match iface.start() {
        Ok(conn) => {
            info!("Successfully started D-Bus interface: {} (unique_name: {:?})", I::name(), conn.unique_name());
            Some(conn)
        }
        Err(err) => {
            warn!("FAILED to start D-Bus interface {}: {err:?}", I::name());
            None
        }
    }
}
