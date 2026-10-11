//! Event types for the compositor-to-Emacs interface.
//!
//! These types represent events sent from the compositor to Emacs
//! via the dynamic module interface, as Lisp data through the elisp crate.

use std::collections::HashMap;

use serde::Serialize;

/// Output mode information
#[derive(Serialize, Clone, Debug)]
pub struct OutputMode {
    pub width: i32,
    pub height: i32,
    pub refresh: i32, // mHz
    pub preferred: bool,
    pub current: bool,
}

/// Output information sent to Emacs
#[derive(Serialize, Clone, Debug)]
pub struct OutputInfo {
    pub name: String,
    pub make: String,
    pub model: String,
    /// Serial number from EDID, empty string if unavailable.
    pub serial: String,
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

/// A key intercepted for text input, re-sent to Emacs on every repeat tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextInputKey {
    pub keycode: u32,
    pub keysym: u32,
    pub utf8: Option<String>,
    pub surface_id: u64,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

/// Events sent from compositor to Emacs.
///
/// Reaches Lisp as `(variant . body)`: a struct variant is an alist, a unit variant the bare
/// symbol.
#[derive(Serialize, Clone, Debug)]
pub enum Event {
    /// Compositor is ready
    #[serde(rename = "ready")]
    Ready,
    /// New surface created. The buffer is created now and placed at `Mapped`.
    #[serde(rename = "new")]
    New {
        id: u64,
        app: String,
        /// Emacs frames only: the `ewm-frame-seq` the surface belongs to.
        seq: Option<u64>,
        pid: i32,
    },
    /// App surface mapped: placed once, floating (`open_floating`) or tiled in `frame_surface_id`.
    /// `activate` asks Emacs to select it: a token or its focused parent wants it and no fullscreen
    /// surface holds focus. `width`/`height` carry the client's natural size, nil when it fills the
    /// working area.
    #[serde(rename = "mapped")]
    Mapped {
        id: u64,
        open_floating: bool,
        activate: bool,
        output: Option<String>,
        frame_surface_id: Option<u64>,
        width: Option<f64>,
        height: Option<f64>,
    },
    /// Surface closed, or an Emacs frame the compositor dropped for Emacs to delete
    #[serde(rename = "close")]
    Close { id: u64 },
    /// Surface minimize requested
    #[serde(rename = "minimize")]
    Minimize { id: u64 },
    /// Activation asked for a surface Emacs no longer shows
    #[serde(rename = "activate_surface")]
    ActivateSurface { id: u64 },
    /// Surface title changed
    #[serde(rename = "title")]
    Title { id: u64, app: String, title: String },
    #[serde(rename = "focus")]
    Focus {
        focus_id: std::num::NonZeroU64,
        x: Option<f64>,
        y: Option<f64>,
        pointer: bool,
    },
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
    #[serde(rename = "text-input-replaced")]
    TextInputReplaced { surface_id: u64 },
    #[serde(rename = "text-input-replace-dropped")]
    TextInputReplaceDropped { surface_id: u64 },
    /// Intercepted key; `keycode`/`surface_id` let Lisp forward it back to the client.
    #[serde(rename = "key")]
    Key(TextInputKey),
    /// Intercepted Emacs command key. Lisp replays this through its keymaps.
    #[serde(rename = "intercepted-command")]
    InterceptedCommand { key: String },
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
    /// A client asked for the workspace of Emacs frame `id` (e.g. a bar click).
    #[serde(rename = "activate_workspace")]
    ActivateWorkspace { id: u64 },
    /// Surface requests maximized mode
    #[serde(rename = "maximize_request")]
    MaximizeRequest { id: u64 },
    /// Surface requests to leave maximized mode
    #[serde(rename = "unmaximize_request")]
    UnmaximizeRequest { id: u64 },
    /// Surface requests fullscreen.
    #[serde(rename = "fullscreen_request")]
    FullscreenRequest { id: u64 },
    /// Surface requests to leave fullscreen.
    #[serde(rename = "unfullscreen_request")]
    UnfullscreenRequest { id: u64 },
    /// Client rang the system bell.
    #[serde(rename = "bell")]
    Bell { id: Option<u64> },
    /// Idle state changed (native idle timeout)
    #[serde(rename = "idle_state_changed")]
    IdleStateChanged { idle: bool },
    /// Environment variables for Emacs to apply
    #[serde(rename = "environment")]
    Environment { vars: HashMap<String, String> },
    /// The overview opened or closed.
    #[serde(rename = "overview")]
    Overview { open: bool },
    /// Frames slid off their outputs or came back.
    #[serde(rename = "hide")]
    Hide { hidden: bool },
    /// Emacs frame became the active one in its strip.
    #[serde(rename = "frame_shown")]
    FrameShown { id: u64 },
    /// Emacs frame left the active strip slot.
    #[serde(rename = "frame_hidden")]
    FrameHidden { id: u64 },
}
