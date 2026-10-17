//! The logind session: the lid-switch and sleep inhibitors, suspend, and the end of sleep.

use std::os::fd::OwnedFd;

use futures_util::StreamExt;
use smithay::reexports::calloop::channel::Sender;
use tracing::{info, warn};
use zbus::blocking::Connection;

pub enum Login1ToEwm {
    WakeFromSleep,
}

/// Logind connection holding the lid-switch inhibitor, released when dropped.
pub struct Login1 {
    conn: Connection,
    _lid_inhibitor: OwnedFd,
    sleep_inhibitor: Option<OwnedFd>,
    _wake_watcher: zbus::Task<()>,
}

impl Login1 {
    /// Connect and take the `handle-lid-switch` inhibitor, none without logind.
    pub fn new(to_ewm: Sender<Login1ToEwm>) -> Option<Self> {
        let conn = Connection::system()
            .inspect_err(|e| warn!("No system D-Bus, lid switch managed by system default: {e}"))
            .ok()?;
        let lid_inhibitor = inhibit(&conn, "handle-lid-switch", "Compositor manages lid switch")
            .inspect_err(|e| warn!("Failed to acquire lid switch inhibitor: {e}"))
            .ok()?;
        info!("Acquired logind handle-lid-switch inhibitor");
        let wake_watcher = watch_wake(&conn, to_ewm);
        Some(Self {
            conn,
            _lid_inhibitor: lid_inhibitor,
            sleep_inhibitor: None,
            _wake_watcher: wake_watcher,
        })
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
