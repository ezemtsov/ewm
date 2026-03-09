//! Testing infrastructure for EWM compositor
//!
//! Provides a headless test fixture for integration testing
//! the compositor without requiring real hardware.

mod fixture;

pub use fixture::Fixture;
