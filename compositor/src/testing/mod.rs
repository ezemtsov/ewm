//! Testing infrastructure for EWM compositor
//!
//! Provides a headless test fixture for integration testing
//! the compositor without requiring real hardware.

mod client;
mod fixture;

pub use client::{ClientState as TestClientState, Popup, TestClient, Toplevel};
pub use fixture::Fixture;
