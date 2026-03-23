//! Event types for the compositor-to-Emacs interface.
//!
//! These types represent events sent from the compositor to Emacs
//! via the dynamic module interface. The `Serialize` derive is used
//! for debug state serialization.

use serde::Serialize;
use std::collections::HashMap;

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
    pub scale: f64,
    pub transform: i32,
    pub modes: Vec<OutputMode>,
}

/// Working area information (area available after layer-shell exclusive zones)
#[derive(Serialize, Clone, Debug)]
pub struct WorkingAreaInfo {
    pub output: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Events sent from compositor to Emacs.
///
/// Serialized to JSON via serde and sent to Emacs over a pipe fd.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "event")]
pub enum Event {
    /// Compositor is ready
    #[serde(rename = "ready")]
    Ready,
    /// New surface created
    #[serde(rename = "new")]
    New {
        id: u64,
        app: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        pid: i32,
    },
    /// Surface closed
    #[serde(rename = "close")]
    Close { id: u64 },
    /// Surface minimize requested
    #[serde(rename = "minimize")]
    Minimize { id: u64 },
    /// Surface title changed
    #[serde(rename = "title")]
    Title { id: u64, app: String, title: String },
    /// Focus changed to surface
    #[serde(rename = "focus")]
    Focus { id: u64 },
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
    Layouts {
        layouts: Vec<String>,
        current: usize,
    },
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
    /// Verbose compositor state dump (for debugging via ewm-show-state)
    #[serde(rename = "debug_state")]
    DebugState { json: String },
    /// Working area changed (due to layer-shell exclusive zones)
    #[serde(rename = "working_area")]
    WorkingArea {
        output: String,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// Output configuration applied (after ConfigureOutput command)
    #[serde(rename = "output_config_changed")]
    OutputConfigChanged {
        name: String,
        width: i32,
        height: i32,
        refresh: i32,
        x: i32,
        y: i32,
        scale: f64,
        transform: i32,
    },
    /// Clipboard selection changed (Wayland client copied text)
    #[serde(rename = "selection-changed")]
    SelectionChanged { text: String },
    /// Workspace activation requested (e.g. waybar click)
    #[serde(rename = "activate_workspace")]
    ActivateWorkspace { output: String, tab_index: usize },
    /// Surface requests fullscreen mode
    #[serde(rename = "fullscreen_request")]
    FullscreenRequest {
        id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// Surface requests to leave fullscreen mode
    #[serde(rename = "unfullscreen_request")]
    UnfullscreenRequest { id: u64 },
    /// Surface requests maximized mode
    #[serde(rename = "maximize_request")]
    MaximizeRequest { id: u64 },
    /// Surface requests to leave maximized mode
    #[serde(rename = "unmaximize_request")]
    UnmaximizeRequest { id: u64 },
    /// Idle state changed (native idle timeout)
    #[serde(rename = "idle_state_changed")]
    IdleStateChanged { idle: bool },
    /// Environment variables for Emacs to apply
    #[serde(rename = "environment")]
    Environment { vars: HashMap<String, String> },
}
