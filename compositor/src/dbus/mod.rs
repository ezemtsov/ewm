//! D-Bus interface for screen casting
//!
//! Implements org.gnome.Mutter.ScreenCast interface for xdg-desktop-portal.

pub mod screen_cast;

use std::sync::Arc;

use anyhow::Context as _;
use smithay::reexports::calloop::channel::{self, Sender};
use smithay::reexports::calloop::LoopHandle;
use tracing::{info, warn};

pub use screen_cast::{ScreenCast, ScreenCastToCompositor};

/// Start the D-Bus server for screen casting
pub fn start_dbus_server<D: 'static>(
    event_loop: &LoopHandle<'static, D>,
    outputs: Arc<std::sync::Mutex<Vec<OutputInfo>>>,
) -> anyhow::Result<Sender<ScreenCastToCompositor>> {
    let (sender, receiver) = channel::channel::<ScreenCastToCompositor>();

    // Spawn async D-Bus server
    let sender_clone = sender.clone();
    std::thread::spawn(move || {
        if let Err(err) = run_dbus_server(outputs, sender_clone) {
            warn!("D-Bus server error: {err:?}");
        }
    });

    // Register receiver with calloop
    event_loop
        .insert_source(receiver, |event, _, _| {
            if let channel::Event::Msg(msg) = event {
                info!("Received D-Bus message: {:?}", msg);
                // Handle screen cast messages here
            }
        })
        .map_err(|e| anyhow::anyhow!("Failed to register D-Bus receiver: {}", e))?;

    Ok(sender)
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
        let screen_cast = ScreenCast::new(outputs, sender);

        let connection = zbus::connection::Builder::session()?
            .name("org.gnome.Mutter.ScreenCast")?
            .serve_at("/org/gnome/Mutter/ScreenCast", screen_cast)?
            .build()
            .await
            .context("Failed to build D-Bus connection")?;

        info!("D-Bus ScreenCast interface registered");

        // Keep the connection alive
        loop {
            std::future::pending::<()>().await;
        }
    })
}
