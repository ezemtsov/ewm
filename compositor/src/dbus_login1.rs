//! The logind session.

use std::env;
use std::os::fd::OwnedFd;

use futures_util::StreamExt;
use smithay::reexports::calloop::channel::Sender;
use tracing::{info, warn};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

pub enum Login1ToEwm {
    WakeFromSleep,
}

/// Logind connection holding the lid-switch inhibitor, released when dropped.
pub struct Login1 {
    conn: Connection,
    session: Proxy<'static>,
    _lid_inhibitor: OwnedFd,
    sleep_inhibitor: Option<OwnedFd>,
    _wake_watcher: zbus::Task<()>,
}

impl Login1 {
    /// Connect and take the `handle-lid-switch` inhibitor, none outside a logind session.
    pub fn new(to_ewm: Sender<Login1ToEwm>) -> Option<Self> {
        let conn = Connection::system()
            .inspect_err(|e| warn!("No system D-Bus, lid switch managed by system default: {e}"))
            .ok()?;
        let session = session_proxy(&conn)
            .inspect_err(|e| warn!("No logind session: {e}"))
            .ok()?;
        let lid_inhibitor = inhibit(&conn, "handle-lid-switch", "Compositor manages lid switch")
            .inspect_err(|e| warn!("Failed to acquire lid switch inhibitor: {e}"))
            .ok()?;
        info!("Acquired logind handle-lid-switch inhibitor");
        let wake_watcher = watch_wake(&conn, to_ewm);
        Some(Self {
            conn,
            session,
            _lid_inhibitor: lid_inhibitor,
            sleep_inhibitor: None,
            _wake_watcher: wake_watcher,
        })
    }

    /// Whether the previous instance left the session locked.
    pub fn locked_hint(&self) -> bool {
        self.session.get_property("LockedHint").unwrap_or(false)
    }

    /// Set the LockedHint the next instance starts under.
    pub fn set_locked_hint(&self, locked: bool) {
        if let Err(e) = self.session.call_method("SetLockedHint", &(locked,)) {
            warn!("Failed to set the logind locked hint: {e}");
        }
    }

    /// Refuse sleep requests from others, as while kept awake behind a closed lid.
    pub fn set_sleep_inhibited(&mut self, inhibited: bool) {
        if !inhibited {
            self.sleep_inhibitor = None;
        } else if self.sleep_inhibitor.is_none() {
            self.sleep_inhibitor =
                inhibit(&self.conn, "sleep", "Insomnia mode keeps the system awake")
                    .inspect_err(|e| warn!("Failed to acquire sleep inhibitor: {e}"))
                    .ok();
        }
    }

    /// Ask logind to suspend the system, false when it refuses.
    pub fn suspend(&self) -> bool {
        self.conn
            .call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                "Suspend",
                &(false,),
            )
            .map(drop)
            // A sleep someone else asked for is already on its way.
            .or_else(|e| match e {
                zbus::Error::MethodError(name, ..)
                    if name.as_str() == "org.freedesktop.login1.OperationInProgress" =>
                {
                    Ok(())
                }
                e => Err(e),
            })
            .inspect_err(|e| warn!("Failed to suspend via logind: {e}"))
            .is_ok()
    }
}

fn session_proxy(conn: &Connection) -> zbus::Result<Proxy<'static>> {
    let path = match env::var("XDG_SESSION_ID") {
        Ok(id) => conn
            .call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                "GetSession",
                &(id,),
            )?
            .body()
            .deserialize::<OwnedObjectPath>()?
            .into(),
        // session/auto is the session of the caller.
        Err(_) => ObjectPath::from_static_str_unchecked("/org/freedesktop/login1/session/auto"),
    };
    Proxy::new(
        conn,
        "org.freedesktop.login1",
        path,
        "org.freedesktop.login1.Session",
    )
}

/// Take a logind block inhibitor, released when the fd is dropped.
fn inhibit(conn: &Connection, what: &str, why: &str) -> zbus::Result<OwnedFd> {
    let reply = conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        "Inhibit",
        &(what, "ewm", why, "block"),
    )?;
    let fd: zbus::zvariant::OwnedFd = reply.body().deserialize()?;
    Ok(fd.into())
}

/// Forward logind's end-of-sleep signal to the event loop.
fn watch_wake(conn: &Connection, to_ewm: Sender<Login1ToEwm>) -> zbus::Task<()> {
    let conn = conn.inner().clone();
    conn.executor().clone().spawn(
        async move {
            if let Err(e) = forward_wake(conn, to_ewm).await {
                warn!("Failed to watch logind PrepareForSleep: {e}");
            }
        },
        "logind PrepareForSleep",
    )
}

async fn forward_wake(conn: zbus::Connection, to_ewm: Sender<Login1ToEwm>) -> zbus::Result<()> {
    let proxy = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    let mut signals = proxy.receive_signal("PrepareForSleep").await?;
    while let Some(msg) = signals.next().await {
        if matches!(msg.body().deserialize::<bool>(), Ok(false)) {
            let _ = to_ewm.send(Login1ToEwm::WakeFromSleep);
        }
    }
    Ok(())
}
