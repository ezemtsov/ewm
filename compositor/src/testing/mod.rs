//! Testing infrastructure for EWM compositor
//!
//! Provides a headless test fixture for integration testing
//! the compositor without requiring real hardware.

mod client;
mod fixture;

#[cfg(feature = "testing-layer-shell")]
pub use client::LayerSurface;
pub use client::{ClientEvent, ClientState as TestClientState, Popup, TestClient, Toplevel};
pub use fixture::Fixture;

/// Record that `id` was the active frame when a close began.
///
/// Integration tests use this to simulate the pgtk race where focus moves to
/// another frame before `xdg_toplevel.destroy` reaches the compositor.
pub fn mark_active_frame_close(id: u64) {
    crate::module::mark_pending_active_frame_close(id);
}

/// Queue a tiled Emacs frame for OUTPUT, as `ewm-prepare-frame` does.
pub fn prepare_frame(output: &str) {
    crate::module::prepare_frame(crate::module::PendingFrame {
        output: output.to_owned(),
        floating: false,
        pos: None,
    });
}

/// Drop every queued frame; the queue is a process-wide static.
pub fn clear_pending_frames() {
    while crate::module::take_pending_frame().is_some() {}
}
