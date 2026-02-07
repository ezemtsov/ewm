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

mod drm_backend;

pub use drm_backend::DrmBackendState;

use smithay::{
    backend::{
        allocator::Fourcc,
        input::{
            AbsolutePositionEvent, Axis, ButtonState, Event, InputEvent,
            KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
        },
        renderer::{
            damage::OutputDamageTracker,
            element::{surface::WaylandSurfaceRenderElement, AsRenderElements},
            gles::GlesRenderer,
            utils::with_renderer_surface_state,
            ExportMem,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
    desktop::{Space, Window},
    input::{
        keyboard::{FilterResult, xkb::keysyms, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
        Seat, SeatHandler, SeatState,
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            generic::Generic, EventLoop, Interest, LoopHandle, Mode as CalloopMode, PostAction,
            RegistrationToken,
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle, Resource,
        },
    },
    utils::{Transform, SERIAL_COUNTER},
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
            SelectionHandler,
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
            decoration::{XdgDecorationHandler, XdgDecorationState},
        },
        shm::{ShmHandler, ShmState},
        socket::ListeningSocketSource,
    },
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::Child;
use std::rc::Rc;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

const IPC_SOCKET: &str = "/tmp/ewm.sock";

/// Kill combo: Super+Ctrl+Backspace
/// Returns true if this key event is the kill combo
/// Note: keycode 22 (X11/xkb) or 14 (evdev) for Backspace
pub fn is_kill_combo(keycode: u32, ctrl: bool, logo: bool) -> bool {
    (keycode == 14 || keycode == 22) && ctrl && logo
}

/// Events sent to Emacs
#[derive(Serialize)]
#[serde(tag = "event")]
enum IpcEvent {
    #[serde(rename = "new")]
    New { id: u32, app: String },
    #[serde(rename = "close")]
    Close { id: u32 },
    #[serde(rename = "title")]
    Title { id: u32, app: String, title: String },
}

/// Cached surface info for change detection
#[derive(Clone, Default)]
struct SurfaceInfo {
    app_id: String,
    title: String,
}

/// A single view of a surface (position in an Emacs window)
#[derive(Deserialize, Clone, Debug)]
struct SurfaceView {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    active: bool,  // True for the view in the selected Emacs window
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
    #[serde(rename = "screenshot")]
    Screenshot { path: Option<String> },
    #[serde(rename = "prefix-keys")]
    PrefixKeys { keys: Vec<String> },
}

/// Parsed prefix key: keysym + required modifiers
#[derive(Debug, Clone)]
struct PrefixKey {
    keysym: u32,
    ctrl: bool,
    alt: bool,
    shift: bool,
    logo: bool,  // Super/Windows key
}

impl PrefixKey {
    /// Parse an Emacs-style key description like "C-x", "M-x", "C-M-c"
    fn parse(key_desc: &str) -> Option<Self> {
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut logo = false;
        let mut remaining = key_desc;

        // Parse modifiers (C- for Ctrl, M- for Meta/Alt, S- for Shift, s- for Super)
        loop {
            if remaining.starts_with("C-") {
                ctrl = true;
                remaining = &remaining[2..];
            } else if remaining.starts_with("M-") {
                alt = true;
                remaining = &remaining[2..];
            } else if remaining.starts_with("S-") {
                shift = true;
                remaining = &remaining[2..];
            } else if remaining.starts_with("s-") {
                logo = true;
                remaining = &remaining[2..];
            } else {
                break;
            }
        }

        // Parse the base key
        let keysym = match remaining {
            "SPC" | "space" => keysyms::KEY_space,
            "RET" | "return" => keysyms::KEY_Return,
            "TAB" | "tab" => keysyms::KEY_Tab,
            "ESC" | "escape" => keysyms::KEY_Escape,
            "DEL" | "delete" => keysyms::KEY_Delete,
            "backspace" => keysyms::KEY_BackSpace,
            // Special characters
            "`" => keysyms::KEY_grave,
            ":" => keysyms::KEY_colon,
            ";" => keysyms::KEY_semicolon,
            "&" => keysyms::KEY_ampersand,
            "!" => keysyms::KEY_exclam,
            "@" => keysyms::KEY_at,
            "#" => keysyms::KEY_numbersign,
            "$" => keysyms::KEY_dollar,
            "%" => keysyms::KEY_percent,
            "^" => keysyms::KEY_asciicircum,
            "*" => keysyms::KEY_asterisk,
            "(" => keysyms::KEY_parenleft,
            ")" => keysyms::KEY_parenright,
            "-" => keysyms::KEY_minus,
            "_" => keysyms::KEY_underscore,
            "=" => keysyms::KEY_equal,
            "+" => keysyms::KEY_plus,
            "[" => keysyms::KEY_bracketleft,
            "]" => keysyms::KEY_bracketright,
            "{" => keysyms::KEY_braceleft,
            "}" => keysyms::KEY_braceright,
            "\\" => keysyms::KEY_backslash,
            "|" => keysyms::KEY_bar,
            "'" => keysyms::KEY_apostrophe,
            "\"" => keysyms::KEY_quotedbl,
            "," => keysyms::KEY_comma,
            "." => keysyms::KEY_period,
            "/" => keysyms::KEY_slash,
            "<" => keysyms::KEY_less,
            ">" => keysyms::KEY_greater,
            "?" => keysyms::KEY_question,
            "~" => keysyms::KEY_asciitilde,
            s if s.len() == 1 => {
                let c = s.chars().next().unwrap();
                if c.is_ascii_lowercase() {
                    // a-z
                    keysyms::KEY_a + (c as u32 - 'a' as u32)
                } else if c.is_ascii_uppercase() {
                    // A-Z (shifted)
                    shift = true;
                    keysyms::KEY_a + (c.to_ascii_lowercase() as u32 - 'a' as u32)
                } else if c.is_ascii_digit() {
                    // 0-9
                    keysyms::KEY_0 + (c as u32 - '0' as u32)
                } else {
                    return None;
                }
            }
            _ => return None,
        };

        Some(PrefixKey { keysym, ctrl, alt, shift, logo })
    }

    /// Check if this prefix key matches the given keysym and modifiers
    fn matches(&self, keysym: u32, mods: &ModifiersState) -> bool {
        // For prefix keys, we check exact modifier match
        // Note: keysym should be the unshifted version for letters
        let keysym_match = self.keysym == keysym ||
            // Also match shifted keysyms (A-Z map to a-z with shift)
            (keysym >= keysyms::KEY_A && keysym <= keysyms::KEY_Z &&
             self.keysym == keysym - keysyms::KEY_A + keysyms::KEY_a);

        keysym_match &&
            self.ctrl == mods.ctrl &&
            self.alt == mods.alt &&
            // For shift, we're lenient - shifted letters are handled above
            (self.shift == mods.shift || (keysym >= keysyms::KEY_A && keysym <= keysyms::KEY_Z)) &&
            self.logo == mods.logo
    }
}

struct Ewm {
    running: bool,
    space: Space<Window>,
    display_handle: DisplayHandle,

    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    #[allow(dead_code)] // Kept for protocol state
    xdg_decoration_state: XdgDecorationState,
    shm_state: ShmState,
    dmabuf_state: DmabufState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    seat: Seat<Self>,

    // IPC
    next_surface_id: u32,
    window_ids: HashMap<Window, u32>,
    id_windows: HashMap<u32, Window>,
    surface_info: HashMap<u32, SurfaceInfo>,  // Cached app_id/title for change detection
    surface_views: HashMap<u32, Vec<SurfaceView>>,  // Multi-view positions per surface
    pending_events: Vec<IpcEvent>,

    // Output
    output_size: (i32, i32),

    // Input
    pointer_location: (f64, f64),
    focused_surface_id: u32,  // Which surface has keyboard focus (1 = Emacs)
    keyboard_focus: Option<WlSurface>,  // Actual keyboard focus surface
    prefix_keys: Vec<PrefixKey>, // Parsed prefix keys that redirect to Emacs

    // Screenshot request (set by IPC, consumed by render loop)
    pending_screenshot: Option<String>,

    // DRM backend (for early_import)
    drm_backend: Option<Rc<RefCell<DrmBackendState>>>,
}

impl Ewm {
    fn new(display_handle: DisplayHandle) -> Self {
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let dmabuf_state = DmabufState::new();
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);

        let mut seat = seat_state.new_wl_seat(&display_handle, "seat0");
        seat.add_keyboard(Default::default(), 200, 25).unwrap();
        seat.add_pointer();

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
            seat,
            next_surface_id: 1,
            window_ids: HashMap::new(),
            id_windows: HashMap::new(),
            surface_info: HashMap::new(),
            surface_views: HashMap::new(),
            pending_events: Vec::new(),
            output_size: (800, 600), // Default, updated when output is created
            pointer_location: (0.0, 0.0),
            focused_surface_id: 1, // Default: Emacs has focus
            keyboard_focus: None,
            prefix_keys: Vec::new(),
            pending_screenshot: None,
            drm_backend: None,
        }
    }

    /// Set the DRM backend reference for early_import support
    pub fn set_drm_backend(&mut self, backend: Rc<RefCell<DrmBackendState>>) {
        self.drm_backend = Some(backend);
    }

    fn init_wayland_listener(
        display: &mut Display<Ewm>,
        event_loop: &LoopHandle<LoopData>,
    ) -> Result<std::ffi::OsString, Box<dyn std::error::Error>> {
        let socket_name_env = std::env::var("EWM_SOCKET").unwrap_or_else(|_| "wayland-ewm".to_string());
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
                Generic::new(display.backend().poll_fd().as_fd().try_clone_to_owned().unwrap(), Interest::READ, CalloopMode::Level),
                |_, _, data| {
                    data.display.dispatch_clients(&mut data.state).unwrap();
                    Ok(PostAction::Continue)
                },
            )
            .expect("Failed to init wayland source");

        Ok(socket_name)
    }

    fn get_client_process_name(&self, surface: &WlSurface) -> Option<String> {
        // Get the client for this surface
        let client = self.display_handle.get_client(surface.id()).ok()?;

        // Get client credentials (PID, UID, GID)
        let creds = client.get_credentials(&self.display_handle).ok()?;

        // Read process name from /proc/PID/comm
        let comm_path = format!("/proc/{}/comm", creds.pid);
        std::fs::read_to_string(&comm_path)
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn check_surface_info_changes(&mut self, surface: &WlSurface) {
        // Find the window for this surface
        let window = self.space.elements().find(|w| {
            w.wl_surface().map(|s| s.as_ref() == surface).unwrap_or(false)
        }).cloned();

        if let Some(window) = window {
            if let Some(&id) = self.window_ids.get(&window) {
                // Skip Emacs surface (id=1)
                if id == 1 {
                    return;
                }

                // Get current app_id and title from the surface
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

                // Check if changed from cached values
                let cached = self.surface_info.get(&id);
                let changed = match cached {
                    Some(info) => info.app_id != app_id || info.title != title,
                    None => true, // First time seeing this surface's info
                };

                if changed && (!app_id.is_empty() || !title.is_empty()) {
                    info!("Surface {} info changed: app='{}' title='{}'", id, app_id, title);
                    self.surface_info.insert(id, SurfaceInfo {
                        app_id: app_id.clone(),
                        title: title.clone(),
                    });
                    self.pending_events.push(IpcEvent::Title { id, app: app_id, title });
                }
            }
        }
    }
}

// Client tracking
#[derive(Default)]
struct ClientState {
    compositor: CompositorClientState,
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
        // Must be called first to populate renderer surface state for bbox calculation
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // Early import for DRM backend (crucial for proper buffer import)
        if let Some(ref backend) = self.drm_backend {
            backend.borrow_mut().early_import(surface);
        }

        // Check if surface now has a buffer
        let has_buffer = with_renderer_surface_state(surface, |state| {
            state.buffer().is_some()
        }).unwrap_or(false);

        if has_buffer {
            debug!("Surface {:?} committed with buffer", surface.id());
            // Trigger a redraw - this is the critical connection between
            // client buffer commits and the render loop (like niri's queue_redraw)
            if let Some(ref backend) = self.drm_backend {
                backend.borrow_mut().queue_redraw();
            }
        }

        for window in self.space.elements() {
            window.on_commit();
        }

        // Check for title/app_id changes on toplevel surfaces
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

// DMA-BUF for efficient GPU buffer sharing
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
        // Accept all dmabufs - the renderer will validate and import them lazily
        // In a more complete implementation, we'd validate the format here
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
        set_data_device_focus(&self.display_handle, seat, client);
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

        // Get app_id for the surface (may be empty initially)
        // Fall back to process name from /proc if app_id not set
        let app = smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().unwrap().app_id.clone())
        })
        .unwrap_or_else(|| {
            // Try to get process name from client credentials
            self.get_client_process_name(surface.wl_surface())
                .unwrap_or_else(|| "unknown".to_string())
        });

        // Send initial configure - maximized to fill the output
        // This is how Wayland compositors tell clients to be fullscreen
        surface.with_pending_state(|state| {
            state.size = Some(self.output_size.into());
            state.states.set(XdgToplevelState::Maximized);
            state.states.set(XdgToplevelState::Activated);
        });
        surface.send_configure();

        let window = Window::new_wayland_window(surface);
        self.window_ids.insert(window.clone(), id);
        self.id_windows.insert(id, window.clone());

        // First surface (id=1) is assumed to be Emacs - keep it fullscreen
        // Other surfaces are hidden until Emacs positions them via layout commands
        // This mimics EXWM's behavior of unmapping windows until displayed
        let position = if id == 1 { (0, 0) } else { (-10000, -10000) };
        self.space.map_element(window, position, false);

        // Don't notify Emacs about its own surface (id=1) to avoid feedback loop
        if id != 1 {
            self.pending_events.push(IpcEvent::New { id, app: app.clone() });
        }
        info!("New toplevel {} ({})", id, app);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Find and remove the window
        let window = self.space.elements().find(|w| {
            w.toplevel().map(|t| t == &surface).unwrap_or(false)
        }).cloned();

        if let Some(window) = window {
            if let Some(id) = self.window_ids.remove(&window) {
                self.id_windows.remove(&id);
                self.surface_info.remove(&id);
                self.surface_views.remove(&id);
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

// XDG Decoration - tell clients to not use CSD
impl XdgDecorationHandler for Ewm {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

        // Request server-side decorations (which we don't draw, so no decorations)
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode) {
        use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;

        // Always use server-side (no decorations)
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, _toplevel: ToplevelSurface) {}
}
smithay::delegate_xdg_decoration!(Ewm);

/// Shared loop data for both winit and DRM backends
pub(crate) struct LoopData {
    pub state: Ewm,
    pub display: Display<Ewm>,
    pub emacs: Option<UnixStream>,
}

impl LoopData {
    /// Send an IPC event to Emacs
    pub fn send_event(&mut self, event: &IpcEvent) {
        if let Some(ref mut stream) = self.emacs {
            if let Ok(json) = serde_json::to_string(event) {
                if writeln!(stream, "{}", json).is_err() {
                    warn!("Failed to send event to Emacs, disconnecting");
                    self.emacs = None;
                } else {
                    // Flush to ensure event is sent immediately
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

    /// Process IPC commands from a stream (called by event source)
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
                    // Trigger redraw for DRM backend
                    if let Some(ref backend) = self.state.drm_backend {
                        backend.borrow_mut().queue_redraw();
                    }
                    info!("Layout surface {} at ({}, {}) {}x{}", id, x, y, w, h);
                }
            }
            Command::Views { id, views } => {
                if let Some(window) = self.state.id_windows.get(&id) {
                    let primary_view = views.iter().find(|v| v.active)
                        .or_else(|| views.first());

                    if let Some(view) = primary_view {
                        self.state.space.map_element(window.clone(), (view.x, view.y), true);
                        self.state.space.raise_element(window, true);
                        window.toplevel().map(|t| {
                            t.with_pending_state(|state| {
                                state.size = Some((view.w as i32, view.h as i32).into());
                            });
                            t.send_configure();
                        });
                    }
                    self.state.surface_views.insert(id, views.clone());
                    // Trigger redraw for DRM backend
                    if let Some(ref backend) = self.state.drm_backend {
                        backend.borrow_mut().queue_redraw();
                    }
                }
            }
            Command::Hide { id } => {
                if let Some(window) = self.state.id_windows.get(&id) {
                    self.state.space.map_element(window.clone(), (-10000, -10000), false);
                    self.state.surface_views.remove(&id);
                    // Trigger redraw for DRM backend
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
            Command::Screenshot { path } => {
                let target = path.unwrap_or_else(|| "/tmp/ewm-screenshot.png".to_string());
                info!("Screenshot requested: {}", target);
                self.state.pending_screenshot = Some(target);
            }
            Command::PrefixKeys { keys } => {
                self.state.prefix_keys = keys.iter()
                    .filter_map(|k| {
                        let parsed = PrefixKey::parse(k);
                        if parsed.is_none() {
                            warn!("Failed to parse prefix key: {}", k);
                        }
                        parsed
                    })
                    .collect();
                info!("Prefix keys set: {:?}", self.state.prefix_keys);
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
    let result = if drm_backend::is_nested() {
        info!("Running nested (WAYLAND_DISPLAY or DISPLAY set), using winit backend");
        run_winit(program, program_args)
    } else {
        info!("Running standalone (no display server), using DRM backend");
        drm_backend::run_drm(program, program_args)
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

fn run_winit(program: String, program_args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<LoopData> = EventLoop::try_new()?;
    let mut display: Display<Ewm> = Display::new()?;
    let display_handle = display.handle();

    let socket_name = Ewm::init_wayland_listener(&mut display, &event_loop.handle())?;
    let socket_name_str = socket_name.to_string_lossy().to_string();
    info!("Wayland socket: {:?}", socket_name);

    let state = Ewm::new(display_handle.clone());
    let mut data = LoopData { state, display, emacs: None };

    // IPC socket for Emacs
    let ipc_path = Path::new(IPC_SOCKET);
    if ipc_path.exists() {
        std::fs::remove_file(ipc_path)?;
    }
    let ipc_listener = UnixListener::bind(ipc_path)?;
    ipc_listener.set_nonblocking(true)?;
    info!("IPC socket: {}", IPC_SOCKET);

    // Track IPC stream registration token for cleanup on reconnect
    let ipc_stream_token: Rc<RefCell<Option<RegistrationToken>>> = Rc::new(RefCell::new(None));
    let ipc_stream_token_clone = ipc_stream_token.clone();
    let loop_handle = event_loop.handle();
    let loop_handle_for_listener = loop_handle.clone();

    event_loop
        .handle()
        .insert_source(
            Generic::new(ipc_listener, Interest::READ, CalloopMode::Level),
            move |_, listener, data| {
                if let Ok((stream, _)) = listener.accept() {
                    info!("Emacs connected");
                    stream.set_nonblocking(true).ok();

                    // Remove previous stream source if any
                    if let Some(token) = ipc_stream_token_clone.borrow_mut().take() {
                        loop_handle_for_listener.remove(token);
                    }

                    // Clone stream for writing (stored in data.emacs)
                    let write_stream = stream.try_clone().unwrap();
                    data.emacs = Some(write_stream);

                    // Register stream for reading as event source
                    let token = loop_handle_for_listener.insert_source(
                        Generic::new(stream, Interest::READ, CalloopMode::Level),
                        |_, source, data: &mut LoopData| {
                            // SAFETY: We're inside the event loop callback where the source is valid
                            let stream = unsafe { source.get_mut() };
                            data.process_commands_from_stream(stream);
                            Ok(PostAction::Continue)
                        },
                    ).expect("Failed to register IPC stream");

                    *ipc_stream_token_clone.borrow_mut() = Some(token);
                }
                Ok(PostAction::Continue)
            },
        )
        .expect("Failed to init IPC listener");

    // Winit backend
    let (mut backend, mut winit_evt): (WinitGraphicsBackend<GlesRenderer>, _) =
        winit::init().map_err(|e| format!("Failed to init winit: {:?}", e))?;

    // Output
    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "EWM".into(),
            model: "Winit".into(),
        },
    );
    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };
    // Transform::Flipped180 is required for winit backend because OpenGL has Y=0 at
    // bottom while window systems have Y=0 at top. This flip corrects the rendering.
    // Note: Surface positions are compensated in the layout command handler.
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, None);
    output.set_preferred(mode);
    output.create_global::<Ewm>(&display_handle);
    data.state.space.map_output(&output, (0, 0));
    data.state.output_size = (mode.size.w, mode.size.h);

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    // Spawn client inside the compositor
    let mut client_process = spawn_client(&program, &program_args, &socket_name_str)?;
    info!("Client spawned with PID {}", client_process.id());

    info!("EWM compositor started");

    // Main loop
    let mut screenshot_path: Option<String> = None;

    while data.state.running {
        // Check if client has exited
        match client_process.try_wait() {
            Ok(Some(status)) => {
                info!("Client exited with status: {}", status);
                data.state.running = false;
                break;
            }
            Ok(None) => {} // Still running
            Err(e) => {
                error!("Error checking client process: {}", e);
            }
        }

        // Collect input events
        let mut input_events = Vec::new();

        // Winit events
        let _ = winit_evt.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => {
                let mode = Mode {
                    size,
                    refresh: 60_000,
                };
                output.change_current_state(Some(mode), None, None, None);
                data.state.output_size = (size.w, size.h);

                // Notify all surfaces of new size so they can resize
                // (especially Emacs which should fill the compositor window)
                for window in data.state.space.elements() {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.with_pending_state(|state| {
                            state.size = Some((size.w, size.h).into());
                        });
                        toplevel.send_configure();
                    }
                }
                info!("Output resized to {}x{}, notified {} surfaces",
                      size.w, size.h, data.state.space.elements().count());
            }
            WinitEvent::CloseRequested => {
                data.state.running = false;
            }
            WinitEvent::Focus(focused) => {
                let keyboard = data.state.seat.get_keyboard().unwrap();

                if !focused {
                    // Window lost focus - release all currently pressed keys to prevent
                    // stuck modifiers (e.g., Super key held when invoking external launcher)
                    let pressed = keyboard.pressed_keys();
                    if !pressed.is_empty() {
                        info!("Window lost focus, releasing {} pressed keys", pressed.len());
                        let serial = SERIAL_COUNTER.next_serial();
                        let time = 0u32;
                        for keycode in pressed {
                            keyboard.input::<(), _>(
                                &mut data.state,
                                keycode,
                                smithay::backend::input::KeyState::Released,
                                serial,
                                time,
                                |_, _, _| FilterResult::Forward,
                            );
                        }
                    }

                    // Clear focus
                    let serial = SERIAL_COUNTER.next_serial();
                    keyboard.set_focus(&mut data.state, None, serial);
                    data.state.keyboard_focus = None;
                } else {
                    // Restore focus to the previously focused surface
                    let target_id = data.state.focused_surface_id;
                    if let Some(window) = data.state.id_windows.get(&target_id) {
                        if let Some(surface) = window.wl_surface() {
                            let serial = SERIAL_COUNTER.next_serial();
                            let focus_surface = surface.into_owned();
                            data.state.keyboard_focus = Some(focus_surface.clone());
                            keyboard.set_focus(&mut data.state, Some(focus_surface), serial);
                        }
                    }
                }
            }
            WinitEvent::Input(event) => {
                input_events.push(event);
            }
            _ => {}
        });

        // Process input events
        for event in input_events {
            match event {
                InputEvent::Keyboard { event } => {
                    let serial = SERIAL_COUNTER.next_serial();
                    let time = Event::time_msec(&event);
                    let keyboard = data.state.seat.get_keyboard().unwrap();

                    // Check if this is a prefix key (only on key press, not release)
                    let is_press = event.state() == smithay::backend::input::KeyState::Pressed;
                    let keycode = event.key_code();

                    // Clone prefix_keys to use in filter closure
                    let prefix_keys = data.state.prefix_keys.clone();
                    let current_focus_id = data.state.focused_surface_id;

                    // Process key with filter to detect prefix keys and kill combo
                    let filter_result = keyboard.input::<u8, _>(
                        &mut data.state,
                        keycode,
                        event.state(),
                        serial,
                        time,
                        |_, mods, handle| {
                            if !is_press {
                                return FilterResult::Forward;
                            }

                            // Check for kill combo first (Super+Ctrl+Backspace)
                            if is_kill_combo(keycode.raw(), mods.ctrl, mods.logo) {
                                return FilterResult::Intercept(2); // 2 = kill
                            }

                            // Get the keysym for this key and check if it matches any prefix key
                            let keysym = handle.modified_sym();
                            let is_prefix = prefix_keys.iter().any(|pk| pk.matches(keysym.raw(), mods));

                            if is_prefix && current_focus_id != 1 {
                                // This is a prefix key and focus is not on Emacs
                                // Signal that we need to redirect focus
                                FilterResult::Intercept(1) // 1 = redirect to emacs
                            } else {
                                FilterResult::Forward
                            }
                        },
                    );

                    // Handle filter results
                    if filter_result == Some(2) {
                        info!("Kill combo pressed (Super+Ctrl+Backspace), shutting down");
                        data.state.running = false;
                        continue;
                    }

                    let redirect_to_emacs = filter_result == Some(1);

                    // If we intercepted a prefix key, redirect focus to Emacs
                    if redirect_to_emacs {
                        // Switch focus to Emacs (surface 1)
                        // Update focused_surface_id so subsequent keys also go to Emacs
                        // (Emacs will send a focus command when it's done with the key sequence)
                        data.state.focused_surface_id = 1;
                        if let Some(window) = data.state.id_windows.get(&1) {
                            if let Some(surface) = window.wl_surface() {
                                let emacs_surface = surface.into_owned();
                                data.state.keyboard_focus = Some(emacs_surface.clone());
                                keyboard.set_focus(&mut data.state, Some(emacs_surface.clone()), serial);
                                info!("Prefix key detected, redirecting focus to Emacs (focused_surface_id=1)");

                                // Re-send the key to Emacs
                                keyboard.input::<(), _>(
                                    &mut data.state,
                                    event.key_code(),
                                    event.state(),
                                    serial,
                                    time,
                                    |_, _, _| FilterResult::Forward,
                                );
                            }
                        }
                    } else {
                        // Normal key handling - use current focused surface
                        let target_id = data.state.focused_surface_id;
                        if let Some(window) = data.state.id_windows.get(&target_id) {
                            if let Some(surface) = window.wl_surface() {
                                let new_focus = surface.into_owned();
                                if data.state.keyboard_focus.as_ref() != Some(&new_focus) {
                                    data.state.keyboard_focus = Some(new_focus.clone());
                                    keyboard.set_focus(&mut data.state, Some(new_focus), serial);
                                }
                            }
                        }
                    }
                }
                InputEvent::PointerMotionAbsolute { event } => {
                    let output_geo = data.state.space.output_geometry(&output).unwrap();
                    let pos = event.position_transformed(output_geo.size);
                    data.state.pointer_location = (pos.x, pos.y);

                    let pointer = data.state.seat.get_pointer().unwrap();
                    let serial = SERIAL_COUNTER.next_serial();

                    // Find surface under pointer
                    let under = data.state.space.element_under((pos.x, pos.y))
                        .and_then(|(window, loc)| {
                            window.wl_surface().map(|s| (s.into_owned(), (loc.x as f64, loc.y as f64).into()))
                        });

                    pointer.motion(
                        &mut data.state,
                        under,
                        &MotionEvent {
                            location: pos.into(),
                            serial,
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(&mut data.state);
                }
                InputEvent::PointerButton { event } => {
                    let pointer = data.state.seat.get_pointer().unwrap();
                    let serial = SERIAL_COUNTER.next_serial();

                    // Note: Unlike typical compositors, we do NOT change keyboard focus on click.
                    // Keyboard focus is controlled by Emacs via the focus command, implementing
                    // EXWM-style line-mode (keys to Emacs) vs char-mode (keys to surface).
                    // Clicks still go to the surface under pointer for mouse interactions.
                    let button_state = match event.state() {
                        ButtonState::Pressed => smithay::backend::input::ButtonState::Pressed,
                        ButtonState::Released => smithay::backend::input::ButtonState::Released,
                    };

                    pointer.button(
                        &mut data.state,
                        &ButtonEvent {
                            button: event.button_code(),
                            state: button_state,
                            serial,
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(&mut data.state);
                }
                InputEvent::PointerAxis { event } => {
                    let pointer = data.state.seat.get_pointer().unwrap();

                    let source = event.source();
                    let horizontal = event.amount(Axis::Horizontal)
                        .or_else(|| event.amount_v120(Axis::Horizontal).map(|v| v * 3.0 / 120.0))
                        .unwrap_or(0.0);
                    let vertical = event.amount(Axis::Vertical)
                        .or_else(|| event.amount_v120(Axis::Vertical).map(|v| v * 3.0 / 120.0))
                        .unwrap_or(0.0);

                    let mut frame = AxisFrame::new(event.time_msec()).source(source);
                    if horizontal != 0.0 {
                        frame = frame.value(Axis::Horizontal, horizontal);
                    }
                    if vertical != 0.0 {
                        frame = frame.value(Axis::Vertical, vertical);
                    }

                    pointer.axis(&mut data.state, frame);
                    pointer.frame(&mut data.state);
                }
                _ => {}
            }
        }

        // Render
        let taking_screenshot = screenshot_path.is_some();
        {
            backend.bind()?;
            let age = backend.buffer_age().unwrap_or(0);
            let renderer = backend.renderer();

            // Create render elements for secondary views (non-active views)
            let mut elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
            for (&id, views) in &data.state.surface_views {
                if let Some(window) = data.state.id_windows.get(&id) {
                    for view in views.iter().filter(|v| !v.active) {
                        let location = smithay::utils::Point::from((view.x, view.y));
                        let view_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                            window.render_elements(renderer, location, 1.0.into(), 1.0);
                        elements.extend(view_elements);
                    }
                }
            }

            // Add elements from space (primary surfaces)
            for window in data.state.space.elements() {
                let loc = data.state.space.element_location(window).unwrap_or_default();
                let loc_physical = loc.to_physical_precise_round(1.0);
                let window_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    window.render_elements(renderer, loc_physical, 1.0.into(), 1.0);
                elements.extend(window_elements);
            }

            let result = damage_tracker.render_output(
                renderer,
                age,
                &elements,
                [0.1, 0.1, 0.1, 1.0],
            );

            match result {
                Ok(render_output_result) => {
                    // Screenshot capture
                    if let Some(ref path) = screenshot_path {
                        let size = data.state.output_size;
                        let mapping = renderer.copy_framebuffer(
                            smithay::utils::Rectangle::from_size((size.0, size.1).into()),
                            Fourcc::Xrgb8888,
                        );

                        if let Ok(mapping) = mapping {
                            if let Ok(pixel_data) = renderer.map_texture(&mapping) {
                                let width = size.0 as usize;
                                let height = size.1 as usize;
                                let stride = width * 4;

                                // Convert and flip vertically
                                let mut rgb_data: Vec<u8> = Vec::with_capacity(width * height * 3);
                                for y in (0..height).rev() {
                                    let row_start = y * stride;
                                    for x in 0..width {
                                        let pixel_start = row_start + x * 4;
                                        if pixel_start + 4 <= pixel_data.len() {
                                            // BGRX layout in memory
                                            let b = pixel_data[pixel_start];
                                            let g = pixel_data[pixel_start + 1];
                                            let r = pixel_data[pixel_start + 2];
                                            rgb_data.extend_from_slice(&[r, g, b]);
                                        }
                                    }
                                }

                                if let Some(img) = image::RgbImage::from_raw(width as u32, height as u32, rgb_data) {
                                    if let Err(e) = img.save(path) {
                                        error!("Failed to save screenshot: {}", e);
                                    } else {
                                        info!("Screenshot saved to {}", path);
                                    }
                                } else {
                                    error!("Failed to create image buffer");
                                }
                            }
                        }
                        screenshot_path = None;
                    }

                    // Submit if there was damage (and not taking screenshot)
                    if !taking_screenshot {
                        if let Some(ref damage) = render_output_result.damage {
                            backend.submit(Some(damage.as_slice()))?;
                        }
                    }
                }
                Err(e) => {
                    error!("Render error: {:?}", e);
                }
            }
        }

        // Frame callbacks
        data.state.space.elements().for_each(|window| {
            window.send_frame(
                &output,
                std::time::Duration::ZERO,
                None,
                |_, _| Some(output.clone()),
            );
        });

        data.state.space.refresh();
        data.display.flush_clients().unwrap();

        // Flush pending events to Emacs (shared IPC handling)
        data.flush_events();

        // Check for pending screenshot request
        if let Some(path) = data.state.pending_screenshot.take() {
            screenshot_path = Some(path);
        }

        event_loop.dispatch(None, &mut data)?;
    }

    Ok(())
}
