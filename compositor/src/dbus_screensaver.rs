//! org.freedesktop.ScreenSaver D-Bus interface for idle inhibition.
//!
//! Implements the freedesktop ScreenSaver D-Bus interface, which allows
//! applications (Firefox, Chrome, media players) to inhibit the screensaver
//! via D-Bus. This is the primary mechanism used by non-Wayland-native apps
//! to prevent idle/screen blanking.
//!
//! Ported from niri's `dbus/freedesktop_screensaver.rs`.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Context;
use tracing::{info, trace, warn};
use zbus::fdo::{self, RequestNameFlags};
use zbus::message::Header;
use zbus::names::OwnedUniqueName;
use zbus::zvariant::NoneValue;
use zbus::{Task, interface};

/// org.freedesktop.ScreenSaver D-Bus interface.
///
/// Tracks inhibition requests from D-Bus clients using cookies.
/// The `is_inhibited` Arc<AtomicBool> is shared with the compositor's
/// `Ewm::is_fdo_idle_inhibited` field.
#[derive(Clone)]
pub struct ScreenSaver {
    is_inhibited: Arc<AtomicBool>,
    inhibitors: Arc<Mutex<HashMap<u32, OwnedUniqueName>>>,
    counter: Arc<AtomicU32>,
    monitor_task: Arc<OnceLock<Task<()>>>,
}

#[interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver {
    async fn inhibit(
        &mut self,
        #[zbus(header)] hdr: Header<'_>,
        application_name: &str,
        reason_for_inhibit: &str,
    ) -> fdo::Result<u32> {
        trace!(
            "fdo inhibit, app: `{application_name}`, reason: `{reason_for_inhibit}`, owner: {:?}",
            hdr.sender()
        );

        let Some(name) = hdr.sender() else {
            return Err(fdo::Error::Failed(String::from("no sender")));
        };
        let name = OwnedUniqueName::from(name.to_owned());

        let mut inhibitors = self.inhibitors.lock().unwrap();

        let mut cookie = None;
        for _ in 0..3 {
            let mut inhibitor_key = self.counter.fetch_add(1, Ordering::SeqCst);
            if inhibitor_key == 0 {
                // Some clients don't like 0, add one more.
                inhibitor_key = self.counter.fetch_add(1, Ordering::SeqCst);
            }

            if let Entry::Vacant(entry) = inhibitors.entry(inhibitor_key) {
                entry.insert(name);
                self.is_inhibited.store(true, Ordering::SeqCst);
                info!(
                    "Idle inhibited via D-Bus: app=`{application_name}`, reason=`{reason_for_inhibit}`, cookie={inhibitor_key}"
                );
                let _ = cookie.insert(inhibitor_key);
                break;
            }
        }

        cookie.ok_or_else(|| fdo::Error::Failed(String::from("no available cookie")))
    }

    async fn un_inhibit(&mut self, cookie: u32) -> fdo::Result<()> {
        trace!("fdo uninhibit, cookie: {cookie}");

        let mut inhibitors = self.inhibitors.lock().unwrap();

        if inhibitors.remove(&cookie).is_some() {
            if inhibitors.is_empty() {
                self.is_inhibited.store(false, Ordering::SeqCst);
            }
            info!("Idle uninhibited via D-Bus: cookie={cookie}");

            Ok(())
        } else {
            Err(fdo::Error::Failed(String::from("invalid cookie")))
        }
    }
}

impl ScreenSaver {
    pub fn new(is_inhibited: Arc<AtomicBool>) -> Self {
        Self {
            is_inhibited,
            inhibitors: Arc::new(Mutex::new(HashMap::new())),
            // Start from 1 because some clients don't like 0.
            counter: Arc::new(AtomicU32::new(1)),
            monitor_task: Arc::new(OnceLock::new()),
        }
    }

    /// Start the D-Bus interface on a blocking connection.
    ///
    /// Returns the connection (must be kept alive for the interface to work).
    pub fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let is_inhibited = self.is_inhibited.clone();
        let inhibitors = self.inhibitors.clone();
        let monitor_task = self.monitor_task.clone();

        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        let org_fd_ss_registered = conn
            .object_server()
            .at("/org/freedesktop/ScreenSaver", self.clone())?;
        let ss_registered = conn.object_server().at("/ScreenSaver", self)?;

        if !org_fd_ss_registered && !ss_registered {
            anyhow::bail!("failed to register any org.freedesktop.ScreenSaver interface")
        }

        conn.request_name_with_flags("org.freedesktop.ScreenSaver", flags)?;

        let async_conn = conn.inner();
        let future = {
            let conn = async_conn.clone();
            async move {
                if let Err(err) =
                    monitor_disappeared_clients(&conn, is_inhibited.clone(), inhibitors.clone())
                        .await
                {
                    warn!("error monitoring org.freedesktop.ScreenSaver clients: {err:?}");
                    is_inhibited.store(false, Ordering::SeqCst);
                    inhibitors.lock().unwrap().clear();
                }
            }
        };
        let task = async_conn
            .executor()
            .spawn(future, "monitor disappearing clients");
        monitor_task.set(task).unwrap();

        info!("org.freedesktop.ScreenSaver D-Bus interface started");

        Ok(conn)
    }
}

/// Monitor D-Bus clients for disconnection.
/// If a client that had an inhibitor disconnects without calling UnInhibit,
/// clean up its inhibitors automatically.
async fn monitor_disappeared_clients(
    conn: &zbus::Connection,
    is_inhibited: Arc<AtomicBool>,
    inhibitors: Arc<Mutex<HashMap<u32, OwnedUniqueName>>>,
) -> anyhow::Result<()> {
    use futures_util::StreamExt;
    use zbus::names::UniqueName;

    let proxy = fdo::DBusProxy::new(conn)
        .await
        .context("error creating a DBusProxy")?;

    let mut stream = proxy
        .receive_name_owner_changed_with_args(&[(2, UniqueName::null_value())])
        .await
        .context("error creating a NameOwnerChanged stream")?;

    while let Some(signal) = stream.next().await {
        let args = signal
            .args()
            .context("error retrieving NameOwnerChanged args")?;

        let Some(name) = &**args.old_owner() else {
            continue;
        };

        if args.new_owner().is_none() {
            trace!("fdo ScreenSaver client disappeared: {name}");

            let mut inhibitors = inhibitors.lock().unwrap();
            inhibitors.retain(|_, owner| owner != name);
            is_inhibited.store(!inhibitors.is_empty(), Ordering::SeqCst);
        }
    }

    Ok(())
}
