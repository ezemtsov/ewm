//! org.gnome.Mutter.ScreenCast D-Bus interface implementation
//!
//! Based on niri's `dbus/mutter_screen_cast.rs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use smithay::reexports::calloop::channel::Sender;
use tracing::{debug, info, warn};
use zbus::blocking::Connection;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::{DeserializeDict, OwnedObjectPath, Type, Value};
use zbus::{fdo, interface, ObjectServer};

use super::{OutputInfo, Start};

/// Target for a screen cast stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastTarget {
    /// Cast an entire output/monitor.
    Output { name: String },
    /// Cast an individual window by its compositor ID.
    Window { id: u64 },
}

/// Properties for the RecordWindow D-Bus method.
#[derive(Debug, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
struct RecordWindowProperties {
    #[zvariant(rename = "window-id")]
    window_id: u64,
}

/// Messages sent from D-Bus to compositor
pub enum ScreenCastToCompositor {
    StartCast {
        session_id: usize,
        target: CastTarget,
        signal_ctx: SignalEmitter<'static>,
        cursor_mode: u32,
    },
    StopCast {
        session_id: usize,
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
    sessions: Arc<Mutex<Vec<usize>>>,
}

impl ScreenCast {
    pub fn new(
        outputs: Arc<Mutex<Vec<OutputInfo>>>,
        to_compositor: Sender<ScreenCastToCompositor>,
    ) -> Self {
        Self {
            outputs,
            to_compositor,
            sessions: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

static SESSION_ID: AtomicUsize = AtomicUsize::new(0);

#[interface(name = "org.gnome.Mutter.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        properties: HashMap<&str, Value<'_>>,
    ) -> fdo::Result<OwnedObjectPath> {
        let session_id = SESSION_ID.fetch_add(1, Ordering::SeqCst);
        let path = format!("/org/gnome/Mutter/ScreenCast/Session/u{}", session_id);
        let path =
            OwnedObjectPath::try_from(path).expect("D-Bus path from format!() is always valid");

        // Parse cursor-mode: 0=Hidden, 1=Embedded (default), 2=Metadata
        let cursor_mode = properties
            .get("cursor-mode")
            .and_then(|v| match v {
                Value::U32(m) => Some(*m),
                _ => None,
            })
            .unwrap_or(1); // Default to Embedded
        debug!("Session {} cursor_mode={}", session_id, cursor_mode);

        let session = Session::new(
            session_id,
            self.outputs.clone(),
            self.to_compositor.clone(),
            cursor_mode,
        );

        match server.at(&path, session).await {
            Ok(true) => {
                self.sessions.lock().unwrap().push(session_id);
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
    id: usize,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    to_compositor: Sender<ScreenCastToCompositor>,
    streams: Arc<Mutex<Vec<StreamInfo>>>,
    stopped: Arc<AtomicBool>,
    cursor_mode: u32,
}

impl Session {
    fn new(
        id: usize,
        outputs: Arc<Mutex<Vec<OutputInfo>>>,
        to_compositor: Sender<ScreenCastToCompositor>,
        cursor_mode: u32,
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

static STREAM_ID: AtomicUsize = AtomicUsize::new(0);

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
                self.cursor_mode,
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
                session_id = self.id,
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
        _properties: HashMap<&str, Value<'_>>,
    ) -> fdo::Result<OwnedObjectPath> {
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

        let stream_id = STREAM_ID.fetch_add(1, Ordering::SeqCst);
        let path = format!("/org/gnome/Mutter/ScreenCast/Stream/u{}", stream_id);
        let path =
            OwnedObjectPath::try_from(path).expect("D-Bus path from format!() is always valid");

        let stream = Stream::new_output(stream_id, self.id, output, self.to_compositor.clone());

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
        info!("RecordWindow: window_id={}", window_id);

        let target = CastTarget::Window { id: window_id };

        let stream_id = STREAM_ID.fetch_add(1, Ordering::SeqCst);
        let path = format!("/org/gnome/Mutter/ScreenCast/Stream/u{}", stream_id);
        let path =
            OwnedObjectPath::try_from(path).expect("D-Bus path from format!() is always valid");

        let stream = Stream::new_window(stream_id, self.id, window_id, self.to_compositor.clone());

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
#[allow(dead_code)]
enum StreamTarget {
    Output(OutputInfo),
    Window { id: u64 },
}

/// Stream interface
#[derive(Clone)]
pub struct Stream {
    #[allow(dead_code)]
    id: usize,
    #[allow(dead_code)]
    session_id: usize,
    target: StreamTarget,
    to_compositor: Sender<ScreenCastToCompositor>,
    was_started: Arc<AtomicBool>,
}

impl Stream {
    fn new_output(
        id: usize,
        session_id: usize,
        output: OutputInfo,
        to_compositor: Sender<ScreenCastToCompositor>,
    ) -> Self {
        Self {
            id,
            session_id,
            target: StreamTarget::Output(output),
            to_compositor,
            was_started: Arc::new(AtomicBool::new(false)),
        }
    }

    fn new_window(
        id: usize,
        session_id: usize,
        window_id: u64,
        to_compositor: Sender<ScreenCastToCompositor>,
    ) -> Self {
        Self {
            id,
            session_id,
            target: StreamTarget::Window { id: window_id },
            to_compositor,
            was_started: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start(
        &self,
        session_id: usize,
        cast_target: CastTarget,
        signal_ctx: SignalEmitter<'static>,
        cursor_mode: u32,
    ) {
        if self.was_started.swap(true, Ordering::SeqCst) {
            return;
        }

        info!(
            "Stream {} starting for {:?} (cursor_mode={})",
            self.id, cast_target, cursor_mode
        );

        if let Err(err) = self.to_compositor.send(ScreenCastToCompositor::StartCast {
            session_id,
            target: cast_target,
            signal_ctx,
            cursor_mode,
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
            StreamTarget::Window { .. } => {
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
