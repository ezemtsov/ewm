//! Testing infrastructure for EWM compositor
//!
//! This module provides test fixtures and utilities for integration testing
//! the compositor without requiring real hardware.
//!
//! # Architecture
//!
//! The testing infrastructure is built on three key components:
//!
//! 1. **HeadlessBackend**: Provides virtual outputs and software rendering
//!    for running the compositor without DRM/GPU access.
//!
//! 2. **Fixture**: The main test harness that wraps the event loop, compositor
//!    state, and provides methods for simulating client interactions.
//!
//! 3. **TestClient**: A mock Wayland client for protocol-level testing,
//!    allowing tests to verify surface lifecycle, focus changes, etc.
//!
//! # Example
//!
//! ```ignore
//! use ewm_core::testing::{Fixture, ClientId};
//!
//! #[test]
//! fn test_surface_creation() {
//!     let mut fixture = Fixture::new().unwrap();
//!     fixture.add_output("Virtual-1", 1920, 1080);
//!
//!     // Create a test client
//!     let client_id = fixture.add_client();
//!
//!     // Dispatch the event loop
//!     fixture.dispatch();
//!
//!     assert_eq!(fixture.output_count(), 1);
//! }
//! ```

mod client;
mod fixture;

pub use client::{ClientId, ClientManager, ConfigureEvent, TestClient, TestSurface};
pub use fixture::Fixture;
