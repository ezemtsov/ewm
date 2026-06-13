//! D-Bus interfaces for xdg-desktop-portal integration
//!
//! Ported from niri's D-Bus integration architecture.
//!
//! Implements:
//! - org.gnome.Mutter.ScreenCast for screen sharing
//! - org.gnome.Mutter.DisplayConfig for monitor enumeration
//! - org.gnome.Mutter.ServiceChannel for portal client connections
//! - org.gnome.Shell.Introspect for the portal window picker
//!
//! Each interface gets its own blocking connection to avoid deadlocks.
//! Names are registered with `AllowReplacement | ReplaceExisting` so the
//! active session always takes over from previous instances.

pub mod display_config;
pub mod introspect;
pub mod screen_cast;
pub mod service_channel;

use std::sync::Arc;

use smithay::reexports::calloop::channel::{self, Channel};
use smithay::reexports::wayland_server::DisplayHandle;
use tracing::{info, warn};
use zbus::blocking::Connection;
use zbus::object_server::{Interface, SignalEmitter};

/// Output information shared with D-Bus interfaces.
pub type OutputInfo = crate::OutputInfo;

pub fn current_output_mode(output: &OutputInfo) -> Option<&crate::OutputMode> {
    output
        .modes
        .iter()
        .find(|mode| mode.current)
        .or_else(|| output.modes.first())
}

pub use display_config::DisplayConfig;
pub use introspect::{
    CompositorToIntrospect, Introspect, IntrospectToCompositor, WindowProperties,
};
pub use screen_cast::{CastTarget, CursorMode, ScreenCast, ScreenCastToCompositor};
pub use service_channel::ServiceChannel;

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
    pub conn_introspect: Option<Connection>,
}

impl DBusServers {
    /// Start all D-Bus servers (called from main thread)
    pub fn start(
        outputs: Arc<std::sync::Mutex<Vec<OutputInfo>>>,
        display_handle: DisplayHandle,
    ) -> (
        Self,
        Channel<ScreenCastToCompositor>,
        Channel<IntrospectToCompositor>,
        async_channel::Sender<CompositorToIntrospect>,
    ) {
        let mut dbus = Self::default();

        // Start ServiceChannel first (needed for portal compatibility)
        let service_channel = ServiceChannel::new(display_handle);
        dbus.conn_service_channel = try_start(service_channel);

        // Start DisplayConfig
        let display_config = DisplayConfig::new(outputs.clone());
        dbus.conn_display_config = try_start(display_config);

        // Start ScreenCast with channel for compositor communication
        let (sc_sender, sc_receiver) = channel::channel::<ScreenCastToCompositor>();
        let screen_cast = ScreenCast::new(outputs, sc_sender);
        dbus.conn_screen_cast = try_start(screen_cast);

        // Start Introspect with bidirectional channels
        let (introspect_sender, introspect_receiver) = channel::channel::<IntrospectToCompositor>();
        let (reply_sender, reply_receiver) = async_channel::bounded::<CompositorToIntrospect>(1);
        let introspect_iface = Introspect::new(introspect_sender, reply_receiver);
        dbus.conn_introspect = try_start(introspect_iface);

        info!("D-Bus servers started");

        (dbus, sc_receiver, introspect_receiver, reply_sender)
    }

    /// Tell Introspect clients the window list changed.
    pub fn emit_windows_changed(&self) {
        let Some(conn) = &self.conn_introspect else {
            return;
        };
        let emitter = SignalEmitter::from_parts(conn.inner().clone(), introspect::PATH);
        if let Err(err) = async_io::block_on(Introspect::windows_changed(&emitter)) {
            warn!("failed to emit Introspect WindowsChanged: {err:?}");
        }
    }
}

fn try_start<I: Start>(iface: I) -> Option<Connection> {
    let name = I::name();
    info!("Starting D-Bus interface: {}", name);
    match iface.start() {
        Ok(conn) => {
            info!(
                "Started D-Bus interface: {} (unique_name: {:?})",
                name,
                conn.unique_name()
            );
            Some(conn)
        }
        Err(err) => {
            warn!("FAILED to start D-Bus interface {}: {err:?}", name);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_with_modes(modes: Vec<crate::OutputMode>) -> OutputInfo {
        OutputInfo {
            name: "HDMI-A-1".to_owned(),
            make: "Acme".to_owned(),
            model: "Panel".to_owned(),
            serial: String::new(),
            width_mm: 600,
            height_mm: 340,
            x: 0,
            y: 0,
            scale: 1.0,
            transform: 0,
            modes,
        }
    }

    #[test]
    fn current_output_mode_prefers_current_mode() {
        let output = output_with_modes(vec![
            crate::OutputMode {
                width: 1280,
                height: 720,
                refresh: 60_000,
                preferred: false,
                current: false,
            },
            crate::OutputMode {
                width: 1920,
                height: 1080,
                refresh: 144_000,
                preferred: true,
                current: true,
            },
        ]);

        let mode = current_output_mode(&output).unwrap();

        assert_eq!(
            (mode.width, mode.height, mode.refresh),
            (1920, 1080, 144_000)
        );
    }

    #[test]
    fn current_output_mode_falls_back_to_first_mode() {
        let output = output_with_modes(vec![crate::OutputMode {
            width: 1280,
            height: 720,
            refresh: 60_000,
            preferred: true,
            current: false,
        }]);

        let mode = current_output_mode(&output).unwrap();

        assert_eq!((mode.width, mode.height, mode.refresh), (1280, 720, 60_000));
    }
}
