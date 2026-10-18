//! The logind session: the lid-switch inhibitor and suspend.

use std::os::fd::OwnedFd;

use tracing::{info, warn};
use zbus::blocking::Connection;

/// Logind connection holding the lid-switch inhibitor, released when dropped.
pub struct Login1 {
    conn: Connection,
    _lid_inhibitor: OwnedFd,
}

impl Login1 {
    /// Connect and take the `handle-lid-switch` inhibitor, none without logind.
    pub fn new() -> Option<Self> {
        let conn = Connection::system()
            .inspect_err(|e| warn!("No system D-Bus, lid switch managed by system default: {e}"))
            .ok()?;
        let lid_inhibitor = inhibit(&conn, "handle-lid-switch", "Compositor manages lid switch")
            .inspect_err(|e| warn!("Failed to acquire lid switch inhibitor: {e}"))
            .ok()?;
        info!("Acquired logind handle-lid-switch inhibitor");
        Some(Self {
            conn,
            _lid_inhibitor: lid_inhibitor,
        })
    }

    /// Ask logind to suspend the system.
    pub fn suspend(&self) {
        if let Err(e) = self.conn.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Suspend",
            &(false,),
        ) {
            warn!("Failed to suspend via logind: {e}");
        }
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
