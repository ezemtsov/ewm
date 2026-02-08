//! org.gnome.Mutter.ServiceChannel D-Bus interface
//!
//! This interface allows xdg-desktop-portal-gnome to connect as a Wayland client
//! to the compositor. Following niri's implementation.

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use smithay::reexports::wayland_server::DisplayHandle;
use tracing::info;
use zbus::{blocking::Connection, fdo, interface, zvariant};

use super::Start;
use crate::ClientState;

/// ServiceChannel D-Bus interface
pub struct ServiceChannel {
    display: DisplayHandle,
}

impl ServiceChannel {
    pub fn new(display: DisplayHandle) -> Self {
        Self { display }
    }
}

#[interface(name = "org.gnome.Mutter.ServiceChannel")]
impl ServiceChannel {
    fn open_wayland_service_connection(
        &mut self,
        service_client_type: u32,
    ) -> fdo::Result<zvariant::OwnedFd> {
        // Type 1 is the portal service client
        if service_client_type != 1 {
            return Err(fdo::Error::InvalidArgs(
                "Invalid service client type".to_owned(),
            ));
        }

        info!("ServiceChannel: open_wayland_service_connection called");

        // Create a Unix socket pair
        let (sock1, sock2) = UnixStream::pair().map_err(|e| {
            fdo::Error::Failed(format!("Failed to create socket pair: {e}"))
        })?;

        // Insert the portal as a Wayland client using the same ClientState type
        // that the compositor uses for all clients (required for proper global binding)
        self.display
            .insert_client(sock2, Arc::new(ClientState::default()))
            .map_err(|e| fdo::Error::Failed(format!("Failed to insert client: {e}")))?;

        info!("Portal connected via ServiceChannel");

        // Return the other end to the portal
        Ok(zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(sock1)))
    }
}

impl Start for ServiceChannel {
    fn start(self) -> anyhow::Result<Connection> {
        info!("ServiceChannel::start() - requesting D-Bus name org.gnome.Mutter.ServiceChannel");
        let conn = zbus::blocking::connection::Builder::session()?
            .name("org.gnome.Mutter.ServiceChannel")?
            .serve_at("/org/gnome/Mutter/ServiceChannel", self)?
            .build()?;
        info!("ServiceChannel::start() - D-Bus connection established, unique name: {:?}", conn.unique_name());
        Ok(conn)
    }
}
