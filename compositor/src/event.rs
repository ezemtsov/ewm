//! Event types shared between IPC and module interfaces.
//!
//! These types represent events sent from the compositor to Emacs,
//! regardless of transport (socket IPC or dynamic module).

use serde::Serialize;

/// Output mode information
#[derive(Serialize, Clone, Debug)]
pub struct OutputMode {
    pub width: i32,
    pub height: i32,
    pub refresh: i32, // mHz
    pub preferred: bool,
}

/// Output information sent to Emacs
#[derive(Serialize, Clone, Debug)]
pub struct OutputInfo {
    pub name: String,
    pub make: String,
    pub model: String,
    pub width_mm: i32,
    pub height_mm: i32,
    pub x: i32,
    pub y: i32,
    pub modes: Vec<OutputMode>,
}

/// Events sent from compositor to Emacs.
///
/// Used by both IPC (JSON serialization) and module (direct Lisp conversion).
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "event")]
pub enum Event {
    /// Compositor is ready
    #[serde(rename = "ready")]
    Ready,
    /// New surface created
    #[serde(rename = "new")]
    New {
        id: u32,
        app: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// Surface closed
    #[serde(rename = "close")]
    Close { id: u32 },
    /// Surface title changed
    #[serde(rename = "title")]
    Title { id: u32, app: String, title: String },
    /// Focus changed to surface
    #[serde(rename = "focus")]
    Focus { id: u32 },
    /// Output connected
    #[serde(rename = "output_detected")]
    OutputDetected(OutputInfo),
    /// Output disconnected
    #[serde(rename = "output_disconnected")]
    OutputDisconnected { name: String },
    /// All outputs have been sent
    #[serde(rename = "outputs_complete")]
    OutputsComplete,
    /// Keyboard layouts available
    #[serde(rename = "layouts")]
    Layouts { layouts: Vec<String>, current: usize },
    /// Keyboard layout switched
    #[serde(rename = "layout-switched")]
    LayoutSwitched { layout: String, index: usize },
    /// Text input activated (for input method)
    #[serde(rename = "text-input-activated")]
    TextInputActivated,
    /// Text input deactivated
    #[serde(rename = "text-input-deactivated")]
    TextInputDeactivated,
    /// Key event (for intercepted keys)
    #[serde(rename = "key")]
    Key {
        keysym: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        utf8: Option<String>,
    },
    /// Compositor state dump (for debugging)
    #[serde(rename = "state")]
    State { json: String },
}
