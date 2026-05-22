//! org.gnome.Mutter.ScreenCast D-Bus interface implementation
//!
//! Based on niri's `dbus/mutter_screen_cast.rs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use smithay::reexports::calloop::channel::Sender;
use tracing::{debug, info, warn};
use zbus::blocking::Connection;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::{DeserializeDict, OwnedObjectPath, Type, Value};
use zbus::{fdo, interface, ObjectServer};

use super::{OutputInfo, Start};
use crate::screencasting::{CastSessionId, CastStreamId};

/// Target for a screen cast stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastTarget {
    /// Cast an entire output/monitor.
    Output { name: String },
    /// Cast an individual window by its compositor ID.
    Window { id: u64 },
}

/// Cursor mode requested through org.gnome.Mutter.ScreenCast.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CursorMode {
    Hidden,
    #[default]
    Embedded,
    Metadata,
}

impl CursorMode {
    fn from_raw(mode: u32) -> Option<Self> {
        match mode {
            0 => Some(Self::Hidden),
            1 => Some(Self::Embedded),
            2 => Some(Self::Metadata),
            _ => None,
        }
    }

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

/// Properties for the RecordWindow D-Bus method.
#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct RecordWindowProperties {
    #[zvariant(rename = "window-id")]
    window_id: u64,
    #[zvariant(rename = "cursor-mode")]
    cursor_mode: Option<u32>,
}

fn parse_cursor_mode(
    properties: &HashMap<&str, Value<'_>>,
    default: CursorMode,
) -> fdo::Result<CursorMode> {
    match properties.get("cursor-mode") {
        None => Ok(default),
        Some(Value::U32(mode)) => parse_optional_cursor_mode(Some(*mode), default),
        Some(_) => Err(fdo::Error::InvalidArgs(
            "cursor-mode must be a u32".to_owned(),
        )),
    }
}

fn parse_optional_cursor_mode(mode: Option<u32>, default: CursorMode) -> fdo::Result<CursorMode> {
    let Some(mode) = mode else {
        return Ok(default);
    };
    CursorMode::from_raw(mode).ok_or_else(|| {
        fdo::Error::InvalidArgs(format!("invalid cursor-mode {mode}; expected 0, 1, or 2"))
    })
}

fn validate_create_session_properties(
    properties: &HashMap<&str, Value<'_>>,
) -> fdo::Result<CursorMode> {
    if properties.contains_key("remote-desktop-session-id") {
        return Err(fdo::Error::Failed(
            "there are no remote desktop sessions".to_owned(),
        ));
    }

    parse_cursor_mode(properties, CursorMode::default())
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

        let cursor_mode = validate_create_session_properties(&properties)?;
        debug!("Session {} cursor_mode={}", session_id, cursor_mode);

        let session = Session::new(
            session_id,
            self.outputs.clone(),
            self.to_compositor.clone(),
            cursor_mode,
        );

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

/// Stream info stored in session
struct StreamInfo {
    stream: Stream,
    iface: InterfaceRef<Stream>,
    target: CastTarget,
}

/// Session interface
#[derive(Clone)]
pub struct Session {
    id: CastSessionId,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    to_compositor: Sender<ScreenCastToCompositor>,
    streams: Arc<Mutex<Vec<StreamInfo>>>,
    stopped: Arc<AtomicBool>,
    cursor_mode: CursorMode,
}

impl Session {
    fn new(
        id: CastSessionId,
        outputs: Arc<Mutex<Vec<OutputInfo>>>,
        to_compositor: Sender<ScreenCastToCompositor>,
        cursor_mode: CursorMode,
    ) -> Self {
        Self {
            id,
            outputs,
            to_compositor,
            streams: Arc::new(Mutex::new(Vec::new())),
            stopped: Arc::new(AtomicBool::new(false)),
            cursor_mode,
        }
    }
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Session")]
impl Session {
    async fn start(&self) {
        debug!("Session {} start", self.id);

        // Start all streams - send StartCast with signal emitter for each
        let streams = self.streams.lock().unwrap();
        for stream_info in streams.iter() {
            stream_info.stream.start(
                self.id,
                stream_info.target.clone(),
                stream_info.iface.signal_emitter().clone(),
            );
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
        for stream_info in streams {
            let stream_path = stream_info.iface.signal_emitter().path().to_owned();
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
        properties: HashMap<&str, Value<'_>>,
    ) -> fdo::Result<OwnedObjectPath> {
        let cursor_mode = parse_cursor_mode(&properties, self.cursor_mode)?;
        // Find the output
        let output = {
            let outputs = self.outputs.lock().unwrap();
            let available: Vec<_> = outputs.iter().map(|o| o.name.clone()).collect();
            info!(
                "RecordMonitor: looking for '{}', available: {:?}",
                connector, available
            );
            outputs.iter().find(|o| o.name == connector).cloned()
        };

        let Some(output) = output else {
            return Err(fdo::Error::Failed(format!(
                "output '{}' not found",
                connector
            )));
        };

        let target = CastTarget::Output {
            name: connector.to_string(),
        };

        let stream_id = CastStreamId::next();
        let path = format!("/org/gnome/Mutter/ScreenCast/Stream/u{}", stream_id);
        let path =
            OwnedObjectPath::try_from(path).expect("D-Bus path from format!() is always valid");

        let stream = Stream::new_output(stream_id, output, self.to_compositor.clone(), cursor_mode);

        // Register stream with D-Bus and get InterfaceRef
        let iface = match server.at(&path, stream.clone()).await {
            Ok(true) => {
                // Get the InterfaceRef for the stream we just registered
                match server.interface::<_, Stream>(&path).await {
                    Ok(iface) => iface,
                    Err(err) => {
                        return Err(fdo::Error::Failed(format!(
                            "error getting stream interface: {err:?}"
                        )));
                    }
                }
            }
            Ok(false) => return Err(fdo::Error::Failed("stream path already exists".to_owned())),
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating stream object: {err:?}"
                )))
            }
        };

        // Store stream info for later use in start()
        self.streams.lock().unwrap().push(StreamInfo {
            stream,
            iface,
            target,
        });

        info!("Created ScreenCast stream: {}", path);
        Ok(path)
    }

    async fn record_window(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        properties: RecordWindowProperties,
    ) -> fdo::Result<OwnedObjectPath> {
        let window_id = properties.window_id;
        let cursor_mode = parse_optional_cursor_mode(properties.cursor_mode, self.cursor_mode)?;
        info!("RecordWindow: window_id={}", window_id);

        let target = CastTarget::Window { id: window_id };

        let stream_id = CastStreamId::next();
        let path = format!("/org/gnome/Mutter/ScreenCast/Stream/u{}", stream_id);
        let path =
            OwnedObjectPath::try_from(path).expect("D-Bus path from format!() is always valid");

        let stream = Stream::new_window(stream_id, self.to_compositor.clone(), cursor_mode);

        // Register stream with D-Bus and get InterfaceRef
        let iface = match server.at(&path, stream.clone()).await {
            Ok(true) => match server.interface::<_, Stream>(&path).await {
                Ok(iface) => iface,
                Err(err) => {
                    return Err(fdo::Error::Failed(format!(
                        "error getting stream interface: {err:?}"
                    )));
                }
            },
            Ok(false) => return Err(fdo::Error::Failed("stream path already exists".to_owned())),
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating stream object: {err:?}"
                )))
            }
        };

        self.streams.lock().unwrap().push(StreamInfo {
            stream,
            iface,
            target,
        });

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
    Window,
}

/// Stream interface
#[derive(Clone)]
pub struct Stream {
    id: CastStreamId,
    target: StreamTarget,
    to_compositor: Sender<ScreenCastToCompositor>,
    was_started: Arc<AtomicBool>,
    /// Per-stream cursor-mode (0=Hidden, 1=Embedded, 2=Metadata),
    /// taken from the per-call `cursor-mode` property in
    /// RecordMonitor/RecordWindow if present, else inherited from
    /// the session.
    cursor_mode: CursorMode,
}

impl Stream {
    fn new_output(
        id: CastStreamId,
        output: OutputInfo,
        to_compositor: Sender<ScreenCastToCompositor>,
        cursor_mode: CursorMode,
    ) -> Self {
        Self {
            id,
            target: StreamTarget::Output(output),
            to_compositor,
            was_started: Arc::new(AtomicBool::new(false)),
            cursor_mode,
        }
    }

    fn new_window(
        id: CastStreamId,
        to_compositor: Sender<ScreenCastToCompositor>,
        cursor_mode: CursorMode,
    ) -> Self {
        Self {
            id,
            target: StreamTarget::Window,
            to_compositor,
            was_started: Arc::new(AtomicBool::new(false)),
            cursor_mode,
        }
    }

    fn start(
        &self,
        session_id: CastSessionId,
        cast_target: CastTarget,
        signal_ctx: SignalEmitter<'static>,
    ) {
        if self.was_started.swap(true, Ordering::SeqCst) {
            return;
        }

        info!(
            "Stream {} starting for {:?} (cursor_mode={})",
            self.id, cast_target, self.cursor_mode
        );

        if let Err(err) = self.to_compositor.send(ScreenCastToCompositor::StartCast {
            session_id,
            target: cast_target,
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
    async fn parameters(&self) -> HashMap<String, Value<'static>> {
        let mut params = HashMap::new();
        match &self.target {
            StreamTarget::Output(output) => {
                params.insert("position".to_string(), Value::new((0i32, 0i32)));
                params.insert(
                    "size".to_string(),
                    Value::new((output.width, output.height)),
                );
            }
            StreamTarget::Window => {
                // Window size is dynamic; consumers don't rely on this
                params.insert("position".to_string(), Value::new((0i32, 0i32)));
                params.insert("size".to_string(), Value::new((1i32, 1i32)));
            }
        }
        params
    }

    #[zbus(signal)]
    pub async fn pipe_wire_stream_added(
        signal_ctxt: &SignalEmitter<'_>,
        node_id: u32,
    ) -> zbus::Result<()>;
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

    fn cursor_props(value: Value<'static>) -> HashMap<&'static str, Value<'static>> {
        HashMap::from([("cursor-mode", value)])
    }

    #[test]
    fn cursor_mode_defaults_when_absent() {
        assert_eq!(
            parse_cursor_mode(&HashMap::new(), CursorMode::Hidden).unwrap(),
            CursorMode::Hidden
        );
    }

    #[test]
    fn cursor_mode_accepts_protocol_values() {
        assert_eq!(
            parse_cursor_mode(&cursor_props(Value::U32(0)), CursorMode::Embedded).unwrap(),
            CursorMode::Hidden
        );
        assert_eq!(
            parse_cursor_mode(&cursor_props(Value::U32(1)), CursorMode::Hidden).unwrap(),
            CursorMode::Embedded
        );
        assert_eq!(
            parse_cursor_mode(&cursor_props(Value::U32(2)), CursorMode::Hidden).unwrap(),
            CursorMode::Metadata
        );
    }

    #[test]
    fn cursor_mode_rejects_out_of_range_values() {
        let err = parse_cursor_mode(&cursor_props(Value::U32(3)), CursorMode::Embedded)
            .expect_err("invalid cursor mode must fail");

        assert!(matches!(err, fdo::Error::InvalidArgs(_)));
    }

    #[test]
    fn cursor_mode_rejects_wrong_value_type() {
        let err = parse_cursor_mode(&cursor_props(Value::Bool(true)), CursorMode::Embedded)
            .expect_err("wrong cursor mode type must fail");

        assert!(matches!(err, fdo::Error::InvalidArgs(_)));
    }

    #[test]
    fn create_session_rejects_remote_desktop_session_id() {
        let properties = HashMap::from([("remote-desktop-session-id", Value::U32(1))]);
        let err = validate_create_session_properties(&properties)
            .expect_err("remote desktop sessions are unsupported");

        assert!(matches!(err, fdo::Error::Failed(_)));
    }
}
