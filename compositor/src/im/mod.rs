//! Input method support.
//!
//! This module keeps the input-method-specific state machines out of the
//! compositor core. `lib.rs` still performs Wayland side effects that require
//! `State`, but relay lifetime, text-input commit gating, and intercepted-key
//! repeat ownership live here.

pub mod relay;
pub mod repeat;
pub mod text_input;
