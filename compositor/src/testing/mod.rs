//! Testing infrastructure for EWM compositor
//!
//! Provides a headless test fixture for integration testing
//! the compositor without requiring real hardware.

mod client;
mod fixture;

pub use client::{ClientEvent, ClientState as TestClientState, Popup, TestClient, Toplevel};
pub use fixture::Fixture;

/// Record that `id` was the active frame when a close began.
///
/// Integration tests use this to simulate the pgtk race where focus moves to
/// another frame before `xdg_toplevel.destroy` reaches the compositor.
pub fn mark_active_frame_close(id: u64) {
    crate::module::mark_pending_active_frame_close(id);
}
