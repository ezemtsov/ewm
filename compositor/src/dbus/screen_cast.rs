//! org.gnome.Mutter.ScreenCast D-Bus interface implementation
//!
//! Based on niri's `dbus/mutter_screen_cast.rs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use smithay::reexports::calloop::channel::Sender;
use tracing::{debug, info, warn};
use zbus::blocking::Connection;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::{DeserializeDict, OwnedObjectPath, SerializeDict, Type, Value};
use zbus::{ObjectServer, fdo, interface};

use super::{OutputInfo, Start, current_output_mode};
use crate::LayoutEntryId;
use crate::screencasting::{CastSessionId, CastStreamId};

/// Target for a screen cast stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastTarget {
    /// Cast an entire output/monitor.
    Output { name: String },
    /// Cast an individual Emacs layout entry by its entry id.
    LayoutEntry { entry_id: LayoutEntryId },
}

/// Cursor mode requested through org.gnome.Mutter.ScreenCast.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Type)]
pub enum CursorMode {
    #[default]
    Hidden = 0,
    Embedded = 1,
    Metadata = 2,
}

impl CursorMode {
    pub fn includes_cursor(self) -> bool {
        self != Self::Hidden
    }
}

impl std::fmt::Display for CursorMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hidden => f.write_str("hidden"),
            Self::Embedded => f.write_str("embedded"),
            Self::Metadata => f.write_str("metadata"),
        }
    }
}

/// Properties for the RecordMonitor D-Bus method.
#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct RecordMonitorProperties {
    #[zvariant(rename = "cursor-mode")]
    cursor_mode: Option<CursorMode>,
    #[zvariant(rename = "is-recording")]
    _is_recording: Option<bool>,
}

/// Properties for the RecordWindow D-Bus method.
#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct RecordWindowProperties {
    #[zvariant(rename = "window-id")]
    window_id: u64,
    #[zvariant(rename = "cursor-mode")]
    cursor_mode: Option<CursorMode>,
    #[zvariant(rename = "is-recording")]
    _is_recording: Option<bool>,
}

#[derive(Debug, SerializeDict, Type, Value)]
#[zvariant(signature = "dict")]
struct StreamParameters {
    position: (i32, i32),
    size: (i32, i32),
}

fn validate_create_session_properties(properties: &HashMap<&str, Value<'_>>) -> fdo::Result<()> {
    if properties.contains_key("remote-desktop-session-id") {
        return Err(fdo::Error::Failed(
            "there are no remote desktop sessions".to_owned(),
        ));
    }

    Ok(())
}

/// Messages sent from D-Bus to compositor
pub enum ScreenCastToCompositor {
    StartCast {
        session_id: CastSessionId,
        target: CastTarget,
        signal_ctx: SignalEmitter<'static>,
        cursor_mode: CursorMode,
    },
    StopCast {
        session_id: CastSessionId,
    },
}

// Manual Debug impl since SignalEmitter doesn't implement Debug
impl std::fmt::Debug for ScreenCastToCompositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StartCast {
                session_id, target, ..
            } => f
                .debug_struct("StartCast")
                .field("session_id", session_id)
                .field("target", target)
                .finish(),
            Self::StopCast { session_id } => f
                .debug_struct("StopCast")
                .field("session_id", session_id)
                .finish(),
        }
    }
}

/// Main ScreenCast interface
#[derive(Clone)]
pub struct ScreenCast {
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    to_compositor: Sender<ScreenCastToCompositor>,
}

impl ScreenCast {
    pub fn new(
        outputs: Arc<Mutex<Vec<OutputInfo>>>,
        to_compositor: Sender<ScreenCastToCompositor>,
    ) -> Self {
        Self {
            outputs,
            to_compositor,
        }
    }
}

#[interface(name = "org.gnome.Mutter.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        properties: HashMap<&str, Value<'_>>,
    ) -> fdo::Result<OwnedObjectPath> {
        let session_id = CastSessionId::next();
        let path = format!("/org/gnome/Mutter/ScreenCast/Session/u{}", session_id);
        let path =
            OwnedObjectPath::try_from(path).expect("D-Bus path from format!() is always valid");

        validate_create_session_properties(&properties)?;
        debug!("Session {} created", session_id);

        let session = Session::new(session_id, self.outputs.clone(), self.to_compositor.clone());

        match server.at(&path, session).await {
            Ok(true) => {
                info!("Created ScreenCast session: {}", path);
                Ok(path)
            }
            Ok(false) => Err(fdo::Error::Failed("session path already exists".to_owned())),
            Err(err) => Err(fdo::Error::Failed(format!(
                "error creating session object: {err:?}"
            ))),
        }
    }

    #[zbus(property, name = "Version")]
    fn version(&self) -> i32 {
        4
    }
}

/// Streams of a session, each with the D-Bus interface it is exported under.
type SessionStreams = Vec<(Stream, InterfaceRef<Stream>)>;

/// Session interface
#[derive(Clone)]
pub struct Session {
    id: CastSessionId,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    to_compositor: Sender<ScreenCastToCompositor>,
    streams: Arc<Mutex<SessionStreams>>,
    stopped: Arc<AtomicBool>,
}

impl Session {
    fn new(
        id: CastSessionId,
        outputs: Arc<Mutex<Vec<OutputInfo>>>,
        to_compositor: Sender<ScreenCastToCompositor>,
    ) -> Self {
        Self {
            id,
            outputs,
            to_compositor,
            streams: Arc::new(Mutex::new(Vec::new())),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn add_stream(
        &self,
        server: &ObjectServer,
        path: &OwnedObjectPath,
        stream: Stream,
    ) -> fdo::Result<()> {
        match server.at(path, stream.clone()).await {
            Ok(true) => {
                let iface = server.interface::<_, Stream>(path).await.map_err(|err| {
                    fdo::Error::Failed(format!("error getting stream interface: {err:?}"))
                })?;
                self.streams.lock().unwrap().push((stream, iface));
                Ok(())
            }
            Ok(false) => Err(fdo::Error::Failed("stream path already exists".to_owned())),
            Err(err) => Err(fdo::Error::Failed(format!(
                "error creating stream object: {err:?}"
            ))),
        }
    }
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Session")]
impl Session {
    async fn start(&self) {
        debug!("Session {} start", self.id);

        // Start all streams - send StartCast with signal emitter for each
        let streams = self.streams.lock().unwrap();
        for (stream, iface) in streams.iter() {
            stream.start(iface.signal_emitter().clone());
        }
    }

    pub async fn stop(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_context)] ctxt: SignalEmitter<'_>,
    ) {
        debug!("Session {} stop", self.id);

        if self.stopped.swap(true, Ordering::SeqCst) {
            // Already stopped
            return;
        }

        // Signal that session is closed
        if let Err(err) = Session::closed(&ctxt).await {
            warn!(
                session_id = %self.id,
                "failed to emit Closed signal: {err:?}"
            );
        }

        if let Err(err) = self.to_compositor.send(ScreenCastToCompositor::StopCast {
            session_id: self.id,
        }) {
            warn!("Failed to send StopCast: {err:?}");
        }

        // Remove stream objects
        let streams = std::mem::take(&mut *self.streams.lock().unwrap());
        for (_, iface) in streams {
            let stream_path = iface.signal_emitter().path().to_owned();
            if let Err(err) = server.remove::<Stream, _>(&stream_path).await {
                warn!(path = %stream_path, "failed to remove Stream from D-Bus: {err:?}");
            }
        }

        // Remove session from server
        let session_path = ctxt.path().to_owned();
        if let Err(err) = server.remove::<Session, _>(&session_path).await {
            warn!(path = %session_path, "failed to remove Session from D-Bus: {err:?}");
        }
    }

    async fn record_monitor(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        connector: &str,
        properties: RecordMonitorProperties,
    ) -> fdo::Result<OwnedObjectPath> {
        let cursor_mode = properties.cursor_mode.unwrap_or_default();
        // Find the output
        let output = {
            let outputs = self.outputs.lock().unwrap();
            outputs.iter().find(|o| o.name == connector).cloned()
        };

        let Some(output) = output else {
            return Err(fdo::Error::Failed(format!(
                "output '{}' not found",
                connector
            )));
        };

        let stream_id = CastStreamId::next();
        let path = format!("/org/gnome/Mutter/ScreenCast/Stream/u{}", stream_id);
        let path =
            OwnedObjectPath::try_from(path).expect("D-Bus path from format!() is always valid");

        let target = StreamTarget::Output(output);
        let stream = Stream::new(
            stream_id,
            self.id,
            target,
            self.to_compositor.clone(),
            cursor_mode,
        );

        self.add_stream(server, &path, stream).await?;
        info!("Created ScreenCast stream: {}", path);
        Ok(path)
    }

    async fn record_window(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        properties: RecordWindowProperties,
    ) -> fdo::Result<OwnedObjectPath> {
        let window_id = properties.window_id;
        let Some(entry_id) = LayoutEntryId::new(window_id) else {
            return Err(fdo::Error::InvalidArgs(
                "window-id must be a non-zero layout entry id".to_owned(),
            ));
        };
        let cursor_mode = properties.cursor_mode.unwrap_or_default();
        info!("RecordWindow: window_id={}", window_id);

        let stream_id = CastStreamId::next();
        let path = format!("/org/gnome/Mutter/ScreenCast/Stream/u{}", stream_id);
        let path =
            OwnedObjectPath::try_from(path).expect("D-Bus path from format!() is always valid");

        let target = StreamTarget::LayoutEntry { entry_id };
        let stream = Stream::new(
            stream_id,
            self.id,
            target,
            self.to_compositor.clone(),
            cursor_mode,
        );

        self.add_stream(server, &path, stream).await?;
        info!("Created ScreenCast window stream: {}", path);
        Ok(path)
    }

    #[zbus(signal)]
    async fn closed(signal_ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Ensure session cleanup even if stop() is not called
impl Drop for Session {
    fn drop(&mut self) {
        // Send StopCast to ensure compositor cleans up even if stop() wasn't called
        let _ = self.to_compositor.send(ScreenCastToCompositor::StopCast {
            session_id: self.id,
        });
    }
}

/// The target of a stream (for D-Bus property reporting).
#[derive(Clone, Debug)]
enum StreamTarget {
    Output(OutputInfo),
    LayoutEntry { entry_id: LayoutEntryId },
}

/// Stream interface
#[derive(Clone)]
pub struct Stream {
    id: CastStreamId,
    session_id: CastSessionId,
    target: StreamTarget,
    to_compositor: Sender<ScreenCastToCompositor>,
    was_started: Arc<AtomicBool>,
    cursor_mode: CursorMode,
}

impl Stream {
    fn new(
        id: CastStreamId,
        session_id: CastSessionId,
        target: StreamTarget,
        to_compositor: Sender<ScreenCastToCompositor>,
        cursor_mode: CursorMode,
    ) -> Self {
        Self {
            id,
            session_id,
            target,
            to_compositor,
            was_started: Arc::new(AtomicBool::new(false)),
            cursor_mode,
        }
    }

    fn start(&self, signal_ctx: SignalEmitter<'static>) {
        if self.was_started.swap(true, Ordering::SeqCst) {
            return;
        }

        info!(
            "Stream {} starting for {:?} (cursor_mode={})",
            self.id, self.target, self.cursor_mode
        );

        if let Err(err) = self.to_compositor.send(ScreenCastToCompositor::StartCast {
            session_id: self.session_id,
            target: self.target.cast_target(),
            signal_ctx,
            cursor_mode: self.cursor_mode,
        }) {
            warn!("Failed to send StartCast: {err:?}");
        }
    }
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Stream")]
impl Stream {
    #[zbus(property)]
    async fn parameters(&self) -> StreamParameters {
        let size = match &self.target {
            StreamTarget::Output(output) => current_output_mode(output)
                .map(|mode| (mode.width, mode.height))
                .unwrap_or((1, 1)),
            StreamTarget::LayoutEntry { .. } => (1, 1),
        };
        StreamParameters {
            position: (0, 0),
            size,
        }
    }

    #[zbus(signal)]
    pub async fn pipe_wire_stream_added(
        signal_ctxt: &SignalEmitter<'_>,
        node_id: u32,
    ) -> zbus::Result<()>;
}

impl StreamTarget {
    fn cast_target(&self) -> CastTarget {
        match self {
            StreamTarget::Output(output) => CastTarget::Output {
                name: output.name.clone(),
            },
            StreamTarget::LayoutEntry { entry_id } => CastTarget::LayoutEntry {
                entry_id: *entry_id,
            },
        }
    }
}

impl Start for ScreenCast {
    fn start(self) -> anyhow::Result<Connection> {
        use zbus::fdo::RequestNameFlags;

        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server()
            .at("/org/gnome/Mutter/ScreenCast", self)?;
        conn.request_name_with_flags("org.gnome.Mutter.ScreenCast", flags)?;

        Ok(conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_session_rejects_remote_desktop_session_id() {
        let properties = HashMap::from([("remote-desktop-session-id", Value::U32(1))]);
        let err = validate_create_session_properties(&properties)
            .expect_err("remote desktop sessions are unsupported");

        assert!(matches!(err, fdo::Error::Failed(_)));
    }

    #[test]
    fn create_session_ignores_stream_cursor_mode() {
        let properties = HashMap::from([("cursor-mode", Value::U32(3))]);

        validate_create_session_properties(&properties).unwrap();
    }
}
