//! org.gnome.Mutter.ScreenCast D-Bus interface implementation

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use smithay::reexports::calloop::channel::Sender;
use tracing::{debug, info, warn};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, Value};
use zbus::{fdo, interface, ObjectServer};

use super::OutputInfo;

/// Messages sent from D-Bus to compositor
#[derive(Debug)]
pub enum ScreenCastToCompositor {
    StartCast {
        session_id: usize,
        output_name: String,
    },
    StopCast {
        session_id: usize,
    },
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
        debug!("CreateSession called with properties: {:?}", properties);

        if properties.contains_key("remote-desktop-session-id") {
            return Err(fdo::Error::Failed(
                "remote desktop sessions not supported".to_owned(),
            ));
        }

        let session_id = SESSION_ID.fetch_add(1, Ordering::SeqCst);
        let path = format!("/org/gnome/Mutter/ScreenCast/Session/u{}", session_id);
        let path = OwnedObjectPath::try_from(path).unwrap();

        let session = Session::new(
            session_id,
            self.outputs.clone(),
            self.to_compositor.clone(),
        );

        match server.at(&path, session).await {
            Ok(true) => {
                self.sessions.lock().unwrap().push(session_id);
                info!("Created ScreenCast session: {}", path);
            }
            Ok(false) => {
                return Err(fdo::Error::Failed("session path already exists".to_owned()))
            }
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating session object: {err:?}"
                )))
            }
        }

        Ok(path)
    }

    #[zbus(property)]
    async fn version(&self) -> i32 {
        4
    }
}

/// Session interface
#[derive(Clone)]
pub struct Session {
    id: usize,
    outputs: Arc<Mutex<Vec<OutputInfo>>>,
    to_compositor: Sender<ScreenCastToCompositor>,
    streams: Arc<Mutex<Vec<usize>>>,
}

impl Session {
    fn new(
        id: usize,
        outputs: Arc<Mutex<Vec<OutputInfo>>>,
        to_compositor: Sender<ScreenCastToCompositor>,
    ) -> Self {
        Self {
            id,
            outputs,
            to_compositor,
            streams: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

static STREAM_ID: AtomicUsize = AtomicUsize::new(0);

#[interface(name = "org.gnome.Mutter.ScreenCast.Session")]
impl Session {
    async fn start(&self) {
        debug!("Session {} start", self.id);
        // Streams are started when RecordMonitor is called
    }

    async fn stop(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_context)] ctxt: SignalEmitter<'_>,
    ) {
        debug!("Session {} stop", self.id);

        if let Err(err) = self.to_compositor.send(ScreenCastToCompositor::StopCast {
            session_id: self.id,
        }) {
            warn!("Failed to send StopCast: {err:?}");
        }

        // Signal that session is closed
        let _ = Session::closed(&ctxt).await;

        // Remove session from server
        let _ = server.remove::<Session, _>(ctxt.path()).await;
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
            info!("RecordMonitor: looking for '{}', available: {:?}", connector, available);
            outputs.iter().find(|o| o.name == connector).cloned()
        };

        let Some(output) = output else {
            return Err(fdo::Error::Failed(format!(
                "output '{}' not found",
                connector
            )));
        };

        let stream_id = STREAM_ID.fetch_add(1, Ordering::SeqCst);
        let path = format!(
            "/org/gnome/Mutter/ScreenCast/Stream/u{}",
            stream_id
        );
        let path = OwnedObjectPath::try_from(path).unwrap();

        let stream = Stream::new(
            stream_id,
            self.id,
            output,
            self.to_compositor.clone(),
        );

        match server.at(&path, stream).await {
            Ok(true) => {
                self.streams.lock().unwrap().push(stream_id);
                info!("Created ScreenCast stream: {}", path);
            }
            Ok(false) => {
                return Err(fdo::Error::Failed("stream path already exists".to_owned()))
            }
            Err(err) => {
                return Err(fdo::Error::Failed(format!(
                    "error creating stream object: {err:?}"
                )))
            }
        }

        // Notify compositor to start casting
        if let Err(err) = self.to_compositor.send(ScreenCastToCompositor::StartCast {
            session_id: self.id,
            output_name: connector.to_string(),
        }) {
            warn!("Failed to send StartCast: {err:?}");
        }

        Ok(path)
    }

    #[zbus(signal)]
    async fn closed(signal_ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Stream interface
#[derive(Clone)]
pub struct Stream {
    id: usize,
    session_id: usize,
    output: OutputInfo,
    to_compositor: Sender<ScreenCastToCompositor>,
    pipewire_node_id: Arc<Mutex<Option<u32>>>,
}

impl Stream {
    fn new(
        id: usize,
        session_id: usize,
        output: OutputInfo,
        to_compositor: Sender<ScreenCastToCompositor>,
    ) -> Self {
        Self {
            id,
            session_id,
            output,
            to_compositor,
            pipewire_node_id: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_pipewire_node_id(&self, node_id: u32) {
        *self.pipewire_node_id.lock().unwrap() = Some(node_id);
    }
}

#[interface(name = "org.gnome.Mutter.ScreenCast.Stream")]
impl Stream {
    #[zbus(property)]
    async fn parameters(&self) -> HashMap<String, Value<'static>> {
        let mut params = HashMap::new();
        params.insert(
            "position".to_string(),
            Value::new((0i32, 0i32)),
        );
        params.insert(
            "size".to_string(),
            Value::new((self.output.width, self.output.height)),
        );
        params
    }

    #[zbus(signal)]
    async fn pipe_wire_stream_added(
        signal_ctxt: &SignalEmitter<'_>,
        node_id: u32,
    ) -> zbus::Result<()>;
}
