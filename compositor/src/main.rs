//! EWM - Emacs Wayland Manager
//!
//! A Wayland compositor that spawns a client application inside it.
//!
//! Usage:
//!   ewm <PROGRAM> [ARGS...]
//!
//! Examples:
//!   ewm emacs                   # Start Emacs
//!   ewm emacs -Q -l init.el     # Start Emacs with arguments
//!   ewm foot                    # Start foot terminal
//!   ewm weston-simple-shm       # Test with minimal Wayland client

mod backend;
mod cursor;
#[cfg(feature = "screencast")]
mod dbus;
mod input;
mod ipc;
#[cfg(feature = "screencast")]
mod pipewire;
mod protocols;
mod render;

pub use backend::DrmBackendState;

use smithay::{
    backend::renderer::utils::with_renderer_surface_state,
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output,
    delegate_primary_selection, delegate_seat, delegate_shm, delegate_xdg_shell,
    desktop::{Space, Window},
    input::{
        keyboard::xkb::keysyms,
        keyboard::ModifiersState,
        Seat, SeatHandler, SeatState,
    },
    reexports::{
        calloop::{
            generic::Generic, Interest, LoopHandle, Mode as CalloopMode, PostAction,
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState,
        wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle, Resource,
        },
    },
    utils::SERIAL_COUNTER,
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        seat::WaylandFocus,
        selection::{
            data_device::{
                set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
                ServerDndGrabHandler,
            },
            primary_selection::{
                set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
            },
            SelectionHandler,
        },
        shell::xdg::{
            decoration::{XdgDecorationHandler, XdgDecorationState},
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
        output::OutputManagerState,
        socket::ListeningSocketSource,
    },
};
use crate::protocols::screencopy::{Screencopy, ScreencopyHandler, ScreencopyManagerState};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::rc::Rc;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Kill combo: Super+Ctrl+Backspace
/// Returns true if this key event is the kill combo
/// Note: keycode 22 (X11/xkb) or 14 (evdev) for Backspace
pub fn is_kill_combo(keycode: u32, ctrl: bool, logo: bool) -> bool {
    (keycode == 14 || keycode == 22) && ctrl && logo
}

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

/// Events sent to Emacs
#[derive(Serialize)]
#[serde(tag = "event")]
enum IpcEvent {
    #[serde(rename = "new")]
    New {
        id: u32,
        app: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    #[serde(rename = "close")]
    Close { id: u32 },
    #[serde(rename = "title")]
    Title { id: u32, app: String, title: String },
    #[serde(rename = "focus")]
    Focus { id: u32 },
    #[serde(rename = "output_detected")]
    OutputDetected(OutputInfo),
    #[serde(rename = "output_disconnected")]
    OutputDisconnected { name: String },
    #[serde(rename = "outputs_complete")]
    OutputsComplete,
}

/// Cached surface info for change detection
#[derive(Clone, Default)]
struct SurfaceInfo {
    app_id: String,
    title: String,
}

/// A single view of a surface (position in an Emacs window)
#[derive(Deserialize, Clone, Debug)]
pub struct SurfaceView {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub active: bool, // True for the view in the selected Emacs window
}

/// Commands received from Emacs
#[derive(Deserialize)]
#[serde(tag = "cmd")]
enum Command {
    #[serde(rename = "layout")]
    Layout { id: u32, x: i32, y: i32, w: u32, h: u32 },
    #[serde(rename = "views")]
    Views { id: u32, views: Vec<SurfaceView> },
    #[serde(rename = "hide")]
    Hide { id: u32 },
    #[serde(rename = "close")]
    Close { id: u32 },
    #[serde(rename = "focus")]
    Focus { id: u32 },
    #[serde(rename = "warp-pointer")]
    WarpPointer { x: f64, y: f64 },
    #[serde(rename = "screenshot")]
    Screenshot { path: Option<String> },
    #[serde(rename = "intercept-keys")]
    InterceptKeys { keys: Vec<InterceptedKey> },
    #[serde(rename = "configure-output")]
    ConfigureOutput {
        name: String,
        x: Option<i32>,
        y: Option<i32>,
        #[allow(dead_code)]
        width: Option<i32>,
        #[allow(dead_code)]
        height: Option<i32>,
        #[allow(dead_code)]
        refresh: Option<i32>,
        #[allow(dead_code)]
        scale: Option<f64>,
        enabled: Option<bool>,
    },
    #[serde(rename = "assign-output")]
    AssignOutput { id: u32, output: String },
    #[serde(rename = "prepare-frame")]
    PrepareFrame { output: String },
}

/// Key identifier: either a keysym integer or a named key string
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum KeyId {
    Keysym(u32),
    Named(String),
}

impl KeyId {
    /// Convert to keysym, mapping named keys to XKB keysyms
    fn to_keysym(&self) -> Option<u32> {
        match self {
            KeyId::Keysym(k) => Some(*k),
            KeyId::Named(name) => match name.as_str() {
                // Arrow keys
                "left" => Some(keysyms::KEY_Left),
                "right" => Some(keysyms::KEY_Right),
                "up" => Some(keysyms::KEY_Up),
                "down" => Some(keysyms::KEY_Down),
                // Navigation
                "home" => Some(keysyms::KEY_Home),
                "end" => Some(keysyms::KEY_End),
                "prior" => Some(keysyms::KEY_Prior),
                "next" => Some(keysyms::KEY_Next),
                "insert" => Some(keysyms::KEY_Insert),
                "delete" => Some(keysyms::KEY_Delete),
                // Function keys
                "f1" => Some(keysyms::KEY_F1),
                "f2" => Some(keysyms::KEY_F2),
                "f3" => Some(keysyms::KEY_F3),
                "f4" => Some(keysyms::KEY_F4),
                "f5" => Some(keysyms::KEY_F5),
                "f6" => Some(keysyms::KEY_F6),
                "f7" => Some(keysyms::KEY_F7),
                "f8" => Some(keysyms::KEY_F8),
                "f9" => Some(keysyms::KEY_F9),
                "f10" => Some(keysyms::KEY_F10),
                "f11" => Some(keysyms::KEY_F11),
                "f12" => Some(keysyms::KEY_F12),
                // Special keys
                "return" => Some(keysyms::KEY_Return),
                "tab" => Some(keysyms::KEY_Tab),
                "escape" => Some(keysyms::KEY_Escape),
                "backspace" => Some(keysyms::KEY_BackSpace),
                _ => {
                    warn!("Unknown key name: {}", name);
                    None
                }
            },
        }
    }
}

/// Intercepted key: key + required modifiers (sent pre-parsed from Emacs)
#[derive(Debug, Clone, Deserialize)]
pub struct InterceptedKey {
    key: KeyId,
    #[serde(default)]
    ctrl: bool,
    #[serde(default)]
    alt: bool,
    #[serde(default)]
    shift: bool,
    #[serde(rename = "super", default)]
    logo: bool,
}

impl InterceptedKey {
    /// Check if this key matches the given keysym and modifiers
    pub fn matches(&self, keysym: u32, mods: &ModifiersState) -> bool {
        let target_keysym = match self.key.to_keysym() {
            Some(k) => k,
            None => return false,
        };

        // Handle case-insensitive letter matching (A-Z vs a-z)
        let keysym_match = target_keysym == keysym
            || (keysym >= keysyms::KEY_A
                && keysym <= keysyms::KEY_Z
                && target_keysym == keysym - keysyms::KEY_A + keysyms::KEY_a);

        keysym_match
            && self.ctrl == mods.ctrl
            && self.alt == mods.alt
            && (self.shift == mods.shift || (keysym >= keysyms::KEY_A && keysym <= keysyms::KEY_Z))
            && self.logo == mods.logo
    }
}

pub struct Ewm {
    pub running: bool,
    pub space: Space<Window>,
    pub display_handle: DisplayHandle,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub seat: Seat<Self>,

    // IPC
    next_surface_id: u32,
    pub window_ids: HashMap<Window, u32>,
    pub id_windows: HashMap<u32, Window>,
    surface_info: HashMap<u32, SurfaceInfo>,
    pub surface_views: HashMap<u32, Vec<SurfaceView>>,
    pending_events: Vec<IpcEvent>,

    // Output
    pub output_size: (i32, i32),
    pub outputs: Vec<OutputInfo>,

    // Input
    pub pointer_location: (f64, f64),
    pub focused_surface_id: u32,
    pub keyboard_focus: Option<WlSurface>,
    pub intercepted_keys: Vec<InterceptedKey>,

    // Emacs client tracking - used to identify which surfaces belong to Emacs
    // vs external applications (for key interception)
    pub emacs_pid: Option<u32>,
    pub emacs_surfaces: std::collections::HashSet<u32>,

    // Screenshot request
    pub pending_screenshot: Option<String>,

    // DRM backend (for early_import)
    pub drm_backend: Option<Rc<RefCell<DrmBackendState>>>,

    // Pending frame-to-output assignments (from prepare-frame command)
    pending_frame_outputs: Vec<String>,

    // Screencopy protocol state
    pub screencopy_state: ScreencopyManagerState,

    // Output manager state (provides xdg-output protocol)
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,

    // PipeWire for screen sharing (initialized lazily)
    #[cfg(feature = "screencast")]
    pub pipewire: Option<pipewire::PipeWire>,

    // Shared output info for D-Bus ScreenCast (thread-safe)
    #[cfg(feature = "screencast")]
    pub dbus_outputs: std::sync::Arc<std::sync::Mutex<Vec<dbus::OutputInfo>>>,

    // Active screen cast sessions (keyed by session_id)
    #[cfg(feature = "screencast")]
    pub screen_casts: std::collections::HashMap<usize, pipewire::stream::Cast>,

    // D-Bus servers (must be kept alive for interfaces to work)
    #[cfg(feature = "screencast")]
    pub dbus_servers: Option<dbus::DBusServers>,
}

impl Ewm {
    pub fn new(display_handle: DisplayHandle) -> Self {
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let dmabuf_state = DmabufState::new();
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&display_handle);

        let mut seat = seat_state.new_wl_seat(&display_handle, "seat0");
        seat.add_keyboard(Default::default(), 200, 25).unwrap();
        seat.add_pointer();

        // Initialize screencopy state before moving display_handle
        let screencopy_state = ScreencopyManagerState::new::<Self, _>(&display_handle, |_| true);

        // Initialize output manager with xdg-output protocol support
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);

        Self {
            running: true,
            space: Space::default(),
            display_handle,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            dmabuf_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            seat,
            next_surface_id: 1,
            window_ids: HashMap::new(),
            id_windows: HashMap::new(),
            surface_info: HashMap::new(),
            surface_views: HashMap::new(),
            pending_events: Vec::new(),
            output_size: (800, 600),
            outputs: Vec::new(),
            pointer_location: (0.0, 0.0),
            focused_surface_id: 1,
            keyboard_focus: None,
            intercepted_keys: Vec::new(),
            emacs_pid: None,
            emacs_surfaces: std::collections::HashSet::new(),
            pending_screenshot: None,
            drm_backend: None,
            pending_frame_outputs: Vec::new(),
            screencopy_state,
            output_manager_state,
            #[cfg(feature = "screencast")]
            pipewire: None,
            #[cfg(feature = "screencast")]
            dbus_outputs: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(feature = "screencast")]
            screen_casts: std::collections::HashMap::new(),
            #[cfg(feature = "screencast")]
            dbus_servers: None,
        }
    }

    /// Set the DRM backend reference for early_import support
    pub fn set_drm_backend(&mut self, backend: Rc<RefCell<DrmBackendState>>) {
        self.drm_backend = Some(backend);
    }

    /// Set the Emacs process PID for client identification
    pub fn set_emacs_pid(&mut self, pid: u32) {
        info!("Tracking Emacs PID: {}", pid);
        self.emacs_pid = Some(pid);
    }

    /// Check if a surface belongs to the Emacs client
    fn is_emacs_client(&self, surface: &WlSurface) -> bool {
        if let Some(emacs_pid) = self.emacs_pid {
            if let Ok(client) = self.display_handle.get_client(surface.id()) {
                if let Ok(creds) = client.get_credentials(&self.display_handle) {
                    return creds.pid == emacs_pid as i32;
                }
            }
        }
        false
    }

    /// Check if focus is on an Emacs surface (for key interception decisions)
    pub fn is_focus_on_emacs(&self) -> bool {
        self.emacs_surfaces.contains(&self.focused_surface_id)
    }

    /// Set focus to a surface and notify Emacs
    pub fn set_focus(&mut self, id: u32) {
        if id != self.focused_surface_id && id != 0 {
            self.focused_surface_id = id;
            // Notify Emacs about focus change (skip Emacs frames, they handle their own focus)
            if !self.emacs_surfaces.contains(&id) {
                self.pending_events.push(IpcEvent::Focus { id });
            }
        }
    }

    /// Get surface ID from a WlSurface
    pub fn surface_id(&self, surface: &WlSurface) -> Option<u32> {
        self.window_ids
            .iter()
            .find(|(w, _)| w.wl_surface().map(|s| &*s == surface).unwrap_or(false))
            .map(|(_, &id)| id)
    }

    /// Get the output where a surface is located
    fn get_surface_output(&self, surface_id: u32) -> Option<String> {
        let window = self.id_windows.get(&surface_id)?;
        let window_loc = self.space.element_location(window)?;

        // Find which output contains this window's location
        for output in self.space.outputs() {
            if let Some(geo) = self.space.output_geometry(output) {
                if window_loc.x >= geo.loc.x
                    && window_loc.x < geo.loc.x + geo.size.w
                    && window_loc.y >= geo.loc.y
                    && window_loc.y < geo.loc.y + geo.size.h
                {
                    return Some(output.name());
                }
            }
        }
        // Fallback to first output
        self.space.outputs().next().map(|o| o.name())
    }

    /// Get the output where the focused surface is located
    fn get_focused_output(&self) -> Option<String> {
        self.get_surface_output(self.focused_surface_id)
    }

    /// Find the Emacs surface on the same output as the focused surface
    pub fn get_emacs_surface_for_focused_output(&self) -> u32 {
        let focused_output = self.get_focused_output();

        // Find an Emacs surface on the same output
        for &emacs_id in &self.emacs_surfaces {
            if self.get_surface_output(emacs_id) == focused_output {
                return emacs_id;
            }
        }

        // Fallback to surface 1 (primary Emacs)
        1
    }

    /// Recalculate total output size from current space geometry
    pub fn recalculate_output_size(&mut self) {
        let (total_width, total_height) =
            self.space
                .outputs()
                .fold((0i32, 0i32), |(w, h), output| {
                    if let Some(geo) = self.space.output_geometry(output) {
                        (w.max(geo.loc.x + geo.size.w), h.max(geo.loc.y + geo.size.h))
                    } else {
                        (w, h)
                    }
                });
        self.output_size = (total_width, total_height);
    }

    /// Send output detected event to Emacs
    pub fn send_output_detected(&mut self, output: OutputInfo) {
        self.pending_events.push(IpcEvent::OutputDetected(output));
    }

    /// Send output disconnected event to Emacs
    pub fn send_output_disconnected(&mut self, name: &str) {
        self.pending_events.push(IpcEvent::OutputDisconnected {
            name: name.to_string(),
        });
    }

    /// Check if there are pending screencopy requests for any output
    pub fn has_pending_screencopies(&self) -> bool {
        // This is a workaround since we can't easily check the internal state
        // without mutable access. We'll always return false here and let
        // the render loop handle it with the mutable state.
        false
    }

    pub fn init_wayland_listener(
        display: &mut Display<Ewm>,
        event_loop: &LoopHandle<LoopData>,
    ) -> Result<std::ffi::OsString, Box<dyn std::error::Error>> {
        let socket_name_env =
            std::env::var("EWM_SOCKET").unwrap_or_else(|_| "wayland-ewm".to_string());
        info!("Creating Wayland socket with name: {}", socket_name_env);
        let socket = ListeningSocketSource::with_name(&socket_name_env)?;
        let socket_name = socket.socket_name().to_os_string();

        event_loop
            .insert_source(socket, |client, _, data| {
                if let Err(e) = data
                    .display
                    .handle()
                    .insert_client(client, Arc::new(ClientState::default()))
                {
                    warn!("Failed to insert client: {}", e);
                }
            })
            .expect("Failed to init wayland socket source");

        event_loop
            .insert_source(
                Generic::new(
                    display.backend().poll_fd().as_fd().try_clone_to_owned().unwrap(),
                    Interest::READ,
                    CalloopMode::Level,
                ),
                |_, _, data| {
                    data.display.dispatch_clients(&mut data.state).unwrap();
                    Ok(PostAction::Continue)
                },
            )
            .expect("Failed to init wayland source");

        Ok(socket_name)
    }

    fn get_client_process_name(&self, surface: &WlSurface) -> Option<String> {
        let client = self.display_handle.get_client(surface.id()).ok()?;
        let creds = client.get_credentials(&self.display_handle).ok()?;
        let comm_path = format!("/proc/{}/comm", creds.pid);
        std::fs::read_to_string(&comm_path)
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn check_surface_info_changes(&mut self, surface: &WlSurface) {
        let window = self
            .space
            .elements()
            .find(|w| w.wl_surface().map(|s| s.as_ref() == surface).unwrap_or(false))
            .cloned();

        if let Some(window) = window {
            if let Some(&id) = self.window_ids.get(&window) {
                // Skip title change events for Emacs surfaces
                if self.emacs_surfaces.contains(&id) {
                    return;
                }

                let (app_id, title) = smithay::wayland::compositor::with_states(surface, |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .map(|d| {
                            let data = d.lock().unwrap();
                            (
                                data.app_id.clone().unwrap_or_default(),
                                data.title.clone().unwrap_or_default(),
                            )
                        })
                        .unwrap_or_default()
                });

                let cached = self.surface_info.get(&id);
                let changed = match cached {
                    Some(info) => info.app_id != app_id || info.title != title,
                    None => true,
                };

                if changed && (!app_id.is_empty() || !title.is_empty()) {
                    info!(
                        "Surface {} info changed: app='{}' title='{}'",
                        id, app_id, title
                    );
                    self.surface_info.insert(
                        id,
                        SurfaceInfo {
                            app_id: app_id.clone(),
                            title: title.clone(),
                        },
                    );
                    self.pending_events.push(IpcEvent::Title {
                        id,
                        app: app_id,
                        title,
                    });
                }
            }
        }
    }
}

// Client tracking
#[derive(Default)]
pub struct ClientState {
    pub compositor: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        info!("Client connected: {:?}", client_id);
    }
    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        info!("Client disconnected: {:?}, reason: {:?}", client_id, reason);
    }
}

// Buffer handling
impl BufferHandler for Ewm {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

// Compositor protocol
impl CompositorHandler for Ewm {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // Early import for DRM backend
        if let Some(ref backend) = self.drm_backend {
            backend.borrow_mut().early_import(surface);
        }

        let has_buffer =
            with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false);

        if has_buffer {
            // Find which window this surface belongs to
            let window_id = self.space.elements().find_map(|window| {
                window.wl_surface().and_then(|ws| {
                    if ws.id() == surface.id() {
                        self.window_ids.get(window).copied()
                    } else {
                        None
                    }
                })
            });
            debug!(
                "Surface {:?} committed with buffer (window_id: {:?})",
                surface.id(),
                window_id
            );
            if let Some(ref backend) = self.drm_backend {
                backend.borrow_mut().queue_redraw();
            }
        }

        for window in self.space.elements() {
            window.on_commit();
        }

        self.check_surface_info_changes(surface);
    }
}
delegate_compositor!(Ewm);

// Shared memory
impl ShmHandler for Ewm {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
delegate_shm!(Ewm);

// DMA-BUF
impl DmabufHandler for Ewm {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        let _ = notifier.successful::<Ewm>();
    }
}
delegate_dmabuf!(Ewm);

// Seat / input
impl SeatHandler for Ewm {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|s| self.display_handle.get_client(s.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}
delegate_seat!(Ewm);

// Data device / selection
impl SelectionHandler for Ewm {
    type SelectionUserData = ();
}
impl ClientDndGrabHandler for Ewm {}
impl ServerDndGrabHandler for Ewm {}
impl DataDeviceHandler for Ewm {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
delegate_data_device!(Ewm);

impl PrimarySelectionHandler for Ewm {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}
delegate_primary_selection!(Ewm);

// Output
impl smithay::wayland::output::OutputHandler for Ewm {}
delegate_output!(Ewm);

// XDG Shell
impl XdgShellHandler for Ewm {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_surface_id;
        self.next_surface_id += 1;

        // Check if this surface belongs to the Emacs client
        let is_emacs = self.is_emacs_client(surface.wl_surface());
        if is_emacs {
            info!("Surface {} is an Emacs surface", id);
            self.emacs_surfaces.insert(id);
        }

        let app = smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().unwrap().app_id.clone())
        })
        .unwrap_or_else(|| {
            self.get_client_process_name(surface.wl_surface())
                .unwrap_or_else(|| "unknown".to_string())
        });

        surface.with_pending_state(|state| {
            state.size = Some(self.output_size.into());
            state.states.set(XdgToplevelState::Maximized);
            state.states.set(XdgToplevelState::Activated);
        });
        surface.send_configure();

        let window = Window::new_wayland_window(surface);
        self.window_ids.insert(window.clone(), id);
        self.id_windows.insert(id, window.clone());

        // Determine target output:
        // 1. Emacs frames: from prepare-frame command (position at output, size to fill)
        // 2. Other surfaces: from currently focused surface's output (offscreen, layout handles)
        let frame_output = if !self.pending_frame_outputs.is_empty() {
            Some(self.pending_frame_outputs.remove(0))
        } else {
            None
        };
        let target_output = frame_output.clone().or_else(|| self.get_focused_output());

        // Position based on surface type
        let position = if id == 1 {
            (0, 0)
        } else if let Some(ref output_name) = frame_output {
            // Emacs frame: position at output origin
            self.space
                .outputs()
                .find(|o| o.name() == *output_name)
                .and_then(|o| self.space.output_geometry(o))
                .map(|geo| (geo.loc.x, geo.loc.y))
                .unwrap_or((-10000, -10000))
        } else {
            // External app: offscreen, Emacs layout will position
            (-10000, -10000)
        };
        self.space.map_element(window.clone(), position, false);

        // Resize Emacs frames to fill their output
        if let Some(ref output_name) = frame_output {
            if let Some(geo) = self
                .space
                .outputs()
                .find(|o| o.name() == *output_name)
                .and_then(|o| self.space.output_geometry(o))
            {
                window.toplevel().map(|t| {
                    t.with_pending_state(|state| {
                        state.size = Some((geo.size.w, geo.size.h).into());
                    });
                    t.send_configure();
                });
            }
        }

        // Send event to Emacs with target output
        if id != 1 {
            self.pending_events.push(IpcEvent::New {
                id,
                app: app.clone(),
                output: target_output.clone(),
            });
        }
        info!(
            "New toplevel {} ({}) -> {:?}",
            id,
            app,
            target_output.as_deref().unwrap_or("unknown")
        );
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let window = self
            .space
            .elements()
            .find(|w| w.toplevel().map(|t| t == &surface).unwrap_or(false))
            .cloned();

        if let Some(window) = window {
            if let Some(id) = self.window_ids.remove(&window) {
                self.id_windows.remove(&id);
                self.surface_info.remove(&id);
                self.surface_views.remove(&id);
                self.emacs_surfaces.remove(&id);
                self.pending_events.push(IpcEvent::Close { id });
                info!("Toplevel {} destroyed", id);
            }
            self.space.unmap_elem(&window);
        }
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
    }

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}
delegate_xdg_shell!(Ewm);

// XDG Decoration
impl XdgDecorationHandler for Ewm {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(
        &mut self,
        toplevel: ToplevelSurface,
        _mode: smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    ) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, _toplevel: ToplevelSurface) {}
}
smithay::delegate_xdg_decoration!(Ewm);

// Screencopy protocol
impl ScreencopyHandler for Ewm {
    fn frame(&mut self, manager: &ZwlrScreencopyManagerV1, screencopy: Screencopy) {
        // Queue all screencopy requests for processing during render
        // (both with_damage and immediate requests are handled in the render loop)
        if let Some(queue) = self.screencopy_state.get_queue_mut(manager) {
            queue.push(screencopy);
        }
    }

    fn screencopy_state(&mut self) -> &mut ScreencopyManagerState {
        &mut self.screencopy_state
    }
}
delegate_screencopy!(Ewm);

/// Shared loop data for both winit and DRM backends
pub struct LoopData {
    pub state: Ewm,
    pub display: Display<Ewm>,
    pub emacs: Option<UnixStream>,
}

impl LoopData {
    /// Send an IPC event to Emacs
    fn send_event(&mut self, event: &IpcEvent) {
        if let Some(ref mut stream) = self.emacs {
            if let Ok(json) = serde_json::to_string(event) {
                if writeln!(stream, "{}", json).is_err() {
                    warn!("Failed to send event to Emacs, disconnecting");
                    self.emacs = None;
                } else {
                    let _ = stream.flush();
                }
            }
        }
    }

    /// Flush all pending events to Emacs
    pub fn flush_events(&mut self) {
        let events: Vec<_> = self.state.pending_events.drain(..).collect();
        for event in events {
            self.send_event(&event);
        }
    }

    /// Send output detected events for all known outputs
    pub fn send_output_events(&mut self) {
        let outputs: Vec<_> = self.state.outputs.clone();
        for output in outputs {
            self.send_event(&IpcEvent::OutputDetected(output));
        }
        // Signal that all outputs have been sent
        self.send_event(&IpcEvent::OutputsComplete);
    }

    /// Process IPC commands from a stream
    pub fn process_commands_from_stream(&mut self, stream: &mut UnixStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if let Ok(cmd) = serde_json::from_str::<Command>(&line) {
                self.handle_command(cmd);
            }
            line.clear();
        }
    }

    /// Handle a single IPC command
    fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::Layout { id, x, y, w, h } => {
                if let Some(window) = self.state.id_windows.get(&id) {
                    self.state.space.map_element(window.clone(), (x, y), true);
                    self.state.space.raise_element(window, true);
                    window.toplevel().map(|t| {
                        t.with_pending_state(|state| {
                            state.size = Some((w as i32, h as i32).into());
                        });
                        t.send_configure();
                    });
                    if let Some(ref backend) = self.state.drm_backend {
                        backend.borrow_mut().queue_redraw();
                    }
                    info!("Layout surface {} at ({}, {}) {}x{}", id, x, y, w, h);
                }
            }
            Command::Views { id, views } => {
                if let Some(window) = self.state.id_windows.get(&id) {
                    let primary_view = views.iter().find(|v| v.active).or_else(|| views.first());

                    if let Some(view) = primary_view {
                        self.state
                            .space
                            .map_element(window.clone(), (view.x, view.y), true);
                        self.state.space.raise_element(window, true);
                        window.toplevel().map(|t| {
                            t.with_pending_state(|state| {
                                state.size = Some((view.w as i32, view.h as i32).into());
                            });
                            t.send_configure();
                        });
                    }
                    self.state.surface_views.insert(id, views.clone());
                    if let Some(ref backend) = self.state.drm_backend {
                        backend.borrow_mut().queue_redraw();
                    }
                }
            }
            Command::Hide { id } => {
                if let Some(window) = self.state.id_windows.get(&id) {
                    self.state
                        .space
                        .map_element(window.clone(), (-10000, -10000), false);
                    self.state.surface_views.remove(&id);
                    if let Some(ref backend) = self.state.drm_backend {
                        backend.borrow_mut().queue_redraw();
                    }
                    info!("Hide surface {}", id);
                }
            }
            Command::Close { id } => {
                if let Some(window) = self.state.id_windows.get(&id) {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.send_close();
                        info!("Close surface {} (sent close request)", id);
                    }
                }
            }
            Command::Focus { id } => {
                self.state.focused_surface_id = id;
                if let Some(window) = self.state.id_windows.get(&id) {
                    if let Some(surface) = window.wl_surface() {
                        let serial = SERIAL_COUNTER.next_serial();
                        let keyboard = self.state.seat.get_keyboard().unwrap();
                        let focus_surface = surface.into_owned();
                        self.state.keyboard_focus = Some(focus_surface.clone());
                        keyboard.set_focus(&mut self.state, Some(focus_surface), serial);
                        info!("Focus surface {}", id);
                    }
                } else {
                    info!("Focus surface {} (surface not found)", id);
                }
            }
            Command::WarpPointer { x, y } => {
                self.state.pointer_location = (x, y);
                let pointer = self.state.seat.get_pointer().unwrap();
                let serial = SERIAL_COUNTER.next_serial();

                // Find surface under new pointer location
                let under = self
                    .state
                    .space
                    .element_under((x, y))
                    .and_then(|(window, loc)| {
                        window.wl_surface().map(|s| {
                            (
                                s.into_owned(),
                                smithay::utils::Point::from((loc.x as f64, loc.y as f64)),
                            )
                        })
                    });

                pointer.motion(
                    &mut self.state,
                    under,
                    &smithay::input::pointer::MotionEvent {
                        location: (x, y).into(),
                        serial,
                        time: 0,
                    },
                );
                pointer.frame(&mut self.state);

                if let Some(ref backend) = self.state.drm_backend {
                    backend.borrow_mut().queue_redraw();
                }
            }
            Command::Screenshot { path } => {
                let target = path.unwrap_or_else(|| "/tmp/ewm-screenshot.png".to_string());
                info!("Screenshot requested: {}", target);
                self.state.pending_screenshot = Some(target);
            }
            Command::InterceptKeys { keys } => {
                self.state.intercepted_keys = keys;
                info!("Intercepted keys set: {:?}", self.state.intercepted_keys);
            }
            Command::ConfigureOutput {
                name,
                x,
                y,
                width,
                height,
                refresh,
                scale: _,
                enabled,
            } => {
                // Find output by name
                let output = self
                    .state
                    .space
                    .outputs()
                    .find(|o| o.name() == name)
                    .cloned();

                if let Some(output) = output {
                    // Handle enable/disable
                    if let Some(false) = enabled {
                        // Disable: unmap from space
                        self.state.space.unmap_output(&output);
                        info!("Disabled output {}", name);
                    } else {
                        // Handle mode change if width/height specified
                        if let (Some(w), Some(h)) = (width, height) {
                            if let Some(ref backend) = self.state.drm_backend {
                                backend.borrow_mut().set_mode(&name, w, h, refresh);
                            }
                        }

                        // Reposition if coordinates provided
                        let new_x = x.unwrap_or(0);
                        let new_y = y.unwrap_or(0);
                        let new_pos = (new_x, new_y);

                        self.state.space.map_output(&output, new_pos);

                        // Update output's internal position state
                        output.change_current_state(
                            None,
                            None,
                            None,
                            Some(new_pos.into()),
                        );

                        // Update our cached output info
                        for out_info in &mut self.state.outputs {
                            if out_info.name == name {
                                out_info.x = new_x;
                                out_info.y = new_y;
                            }
                        }

                        // Recalculate total output size
                        self.state.recalculate_output_size();

                        info!("Configured output {} at ({}, {})", name, new_x, new_y);
                    }

                    // Queue redraw
                    if let Some(ref backend) = self.state.drm_backend {
                        backend.borrow_mut().queue_redraw();
                    }
                } else {
                    warn!("Output not found: {}", name);
                }
            }
            Command::AssignOutput { id, output } => {
                // Find the output by name and get its geometry
                let output_geo = self
                    .state
                    .space
                    .outputs()
                    .find(|o| o.name() == output)
                    .and_then(|o| self.state.space.output_geometry(o));

                if let Some(geo) = output_geo {
                    if let Some(window) = self.state.id_windows.get(&id) {
                        // Position surface to fill the output
                        self.state
                            .space
                            .map_element(window.clone(), (geo.loc.x, geo.loc.y), true);
                        self.state.space.raise_element(window, true);

                        // Resize to fill output
                        window.toplevel().map(|t| {
                            t.with_pending_state(|state| {
                                state.size = Some((geo.size.w, geo.size.h).into());
                            });
                            t.send_configure();
                        });

                        if let Some(ref backend) = self.state.drm_backend {
                            backend.borrow_mut().queue_redraw();
                        }
                        info!(
                            "Assigned surface {} to output {} at ({}, {}) {}x{}",
                            id, output, geo.loc.x, geo.loc.y, geo.size.w, geo.size.h
                        );
                    } else {
                        warn!("Surface not found: {}", id);
                    }
                } else {
                    warn!("Output not found: {}", output);
                }
            }
            Command::PrepareFrame { output } => {
                // Queue output for next frame creation
                self.state.pending_frame_outputs.push(output.clone());
                info!("Prepared frame for output {}", output);
            }
        }
    }
}

fn main() {
    tracing_subscriber::fmt::init();

    // Parse CLI: first arg is the program to spawn, rest are its arguments
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        eprintln!("Usage: ewm <PROGRAM> [ARGS...]");
        eprintln!("Examples:");
        eprintln!("  ewm emacs              # Start Emacs");
        eprintln!("  ewm foot               # Start foot terminal");
        eprintln!("  ewm weston-simple-shm  # Test with minimal client");
        std::process::exit(1);
    }

    let program = args[0].clone();
    let program_args: Vec<String> = args[1..].to_vec();

    info!("Will spawn: {} {:?}", program, program_args);

    // Choose backend based on environment
    let result = if backend::is_nested() {
        info!("Running nested (WAYLAND_DISPLAY or DISPLAY set), using winit backend");
        backend::winit::run_winit(program, program_args)
    } else {
        info!("Running standalone (no display server), using DRM backend");
        backend::drm::run_drm(program, program_args)
    };

    if let Err(e) = result {
        error!("Fatal error: {}", e);
        std::process::exit(1);
    }
}

/// Spawn a client application with the given Wayland display
pub fn spawn_client(program: &str, args: &[String], wayland_display: &str) -> std::io::Result<Child> {
    let mut final_args = args.to_vec();

    // If spawning Emacs and EWM_INIT is set, inject -l <path> -f ewm-connect before user args
    if program == "emacs" || program.ends_with("/emacs") {
        if let Ok(ewm_init) = std::env::var("EWM_INIT") {
            info!("Auto-loading EWM from: {}", ewm_init);
            let mut emacs_args = vec![
                "-l".to_string(),
                ewm_init,
                "-f".to_string(),
                "ewm-connect".to_string(),
            ];
            emacs_args.extend(final_args);
            final_args = emacs_args;
        }
    }

    info!("Spawning: {} {:?}", program, final_args);
    info!("  WAYLAND_DISPLAY={}", wayland_display);

    std::process::Command::new(program)
        .args(&final_args)
        .env("WAYLAND_DISPLAY", wayland_display)
        .spawn()
}
