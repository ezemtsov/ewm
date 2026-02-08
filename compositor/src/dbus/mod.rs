//! D-Bus interfaces for xdg-desktop-portal integration
//!
//! Implements:
//! - org.gnome.Mutter.ScreenCast for screen sharing
//! - org.gnome.Mutter.DisplayConfig for monitor enumeration

pub mod display_config;
pub mod screen_cast;

use std::sync::Arc;

use anyhow::Context as _;
use smithay::reexports::calloop::channel::{self, Channel, Sender};
use tracing::{info, warn};

pub use display_config::DisplayConfig;
pub use screen_cast::{ScreenCast, ScreenCastToCompositor};

/// Start the D-Bus server for screen casting and display config
/// Returns a channel receiver that should be registered with the event loop
pub fn start_dbus_server(
    outputs: Arc<std::sync::Mutex<Vec<OutputInfo>>>,
) -> anyhow::Result<Channel<ScreenCastToCompositor>> {
    let (sender, receiver) = channel::channel::<ScreenCastToCompositor>();

    // Spawn async D-Bus server
    let outputs_clone = outputs.clone();
    std::thread::spawn(move || {
        if let Err(err) = run_dbus_server(outputs_clone, sender) {
            warn!("D-Bus server error: {err:?}");
        }
    });

    Ok(receiver)
}

/// Output information for D-Bus
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub refresh: u32,
}

fn run_dbus_server(
    outputs: Arc<std::sync::Mutex<Vec<OutputInfo>>>,
    sender: Sender<ScreenCastToCompositor>,
) -> anyhow::Result<()> {
    async_io::block_on(async {
        let screen_cast = ScreenCast::new(outputs.clone(), sender);
        let display_config = DisplayConfig::new(outputs);

        let _connection = zbus::connection::Builder::session()?
            .name("org.gnome.Mutter.ScreenCast")?
            .name("org.gnome.Mutter.DisplayConfig")?
            .serve_at("/org/gnome/Mutter/ScreenCast", screen_cast)?
            .serve_at("/org/gnome/Mutter/DisplayConfig", display_config)?
            .build()
            .await
            .context("Failed to build D-Bus connection")?;

        info!("D-Bus interfaces registered (ScreenCast, DisplayConfig)");

        // Keep the connection alive
        loop {
            std::future::pending::<()>().await;
        }
    })
}
