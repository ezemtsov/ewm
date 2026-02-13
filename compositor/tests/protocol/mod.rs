//! Protocol-level tests for Wayland protocol compliance
//!
//! These tests verify that EWM correctly implements Wayland protocols
//! by using the test fixture to simulate client interactions.
//!
//! # Test Organization
//!
//! - `xdg_shell.rs` - Surface lifecycle, configure sequences, output management
//! - `focus.rs` - Keyboard focus, click-to-focus, focus state transitions
//! - `module_interface.rs` - Emacs ↔ compositor communication, command processing

mod focus;
mod layer_shell;
mod module_interface;
mod xdg_shell;
