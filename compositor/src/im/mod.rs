//! Input method support.
//!
//! This module keeps the input-method-specific state machines out of the
//! compositor core. `lib.rs` still performs Wayland side effects that require
//! `State`, but relay thread, text-input commit gating, and intercepted-key
//! repeat ownership live here.

use serde::Deserialize;

pub mod popup;
pub mod relay;
pub mod repeat;
pub mod text_input;

/// Who serves text input for clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextInputProvider {
    /// Emacs input methods, through the relay.
    #[default]
    Emacs,
    /// An input_method_v2 client such as fcitx5 or ibus.
    External,
}
