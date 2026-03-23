//! org.gnome.Shell.Introspect D-Bus interface implementation
//!
//! Provides the window list that xdg-desktop-portal-gnome uses to populate
//! the window picker dialog when OBS (or other apps) request window capture.
//!
//! Based on niri's `dbus/gnome_shell_introspect.rs`.

use std::collections::HashMap;

use tracing::warn;
use zbus::fdo::{self, RequestNameFlags};
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{SerializeDict, Type, Value};

use smithay::reexports::calloop;

use super::Start;

/// D-Bus object implementing org.gnome.Shell.Introspect.
pub struct Introspect {
    to_compositor: calloop::channel::Sender<IntrospectToCompositor>,
    from_compositor: async_channel::Receiver<CompositorToIntrospect>,
}

/// Messages from the Introspect D-Bus interface to the compositor.
pub enum IntrospectToCompositor {
    /// Request the current window list.
    GetWindows,
}

/// Messages from the compositor back to the Introspect D-Bus interface.
pub enum CompositorToIntrospect {
    /// The current window list.
    Windows(HashMap<u64, WindowProperties>),
}

/// Properties of a window exposed via D-Bus.
#[derive(Debug, SerializeDict, Type, Value)]
#[zvariant(signature = "dict")]
pub struct WindowProperties {
    /// Window title.
    pub title: String,
    /// Window app ID (desktop file name).
    #[zvariant(rename = "app-id")]
    pub app_id: String,
}

#[interface(name = "org.gnome.Shell.Introspect")]
impl Introspect {
    async fn get_windows(&self) -> fdo::Result<HashMap<u64, WindowProperties>> {
        if let Err(err) = self.to_compositor.send(IntrospectToCompositor::GetWindows) {
            warn!("error sending GetWindows to compositor: {err:?}");
            return Err(fdo::Error::Failed("internal error".to_owned()));
        }

        match self.from_compositor.recv().await {
            Ok(CompositorToIntrospect::Windows(windows)) => Ok(windows),
            Err(err) => {
                warn!("error receiving window list from compositor: {err:?}");
                Err(fdo::Error::Failed("internal error".to_owned()))
            }
        }
    }

    /// Signal emitted when the window list changes.
    #[zbus(signal)]
    pub async fn windows_changed(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;
}

impl Introspect {
    pub fn new(
        to_compositor: smithay::reexports::calloop::channel::Sender<IntrospectToCompositor>,
        from_compositor: async_channel::Receiver<CompositorToIntrospect>,
    ) -> Self {
        Self {
            to_compositor,
            from_compositor,
        }
    }
}

impl Start for Introspect {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server()
            .at("/org/gnome/Shell/Introspect", self)?;
        conn.request_name_with_flags("org.gnome.Shell.Introspect", flags)?;

        Ok(conn)
    }
}
