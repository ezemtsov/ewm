//! EWM Compositor - Phase 3: Compositor with Emacs IPC
//!
//! Run with: cargo run
//! Then in Emacs: (ewm-connect)
//! Then: WAYLAND_DISPLAY=wayland-ewm foot

use smithay::{
    backend::{
        allocator::Fourcc,
        input::{
            AbsolutePositionEvent, Axis, ButtonState, Event, InputEvent,
            KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
        },
        renderer::{
            damage::OutputDamageTracker,
            element::surface::WaylandSurfaceRenderElement,
            gles::GlesRenderer,
            ExportMem,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
    desktop::{space::render_output, Space, Window},
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
        Seat, SeatHandler, SeatState,
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            generic::Generic, EventLoop, Interest, LoopHandle, Mode as CalloopMode, PostAction,
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
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use tracing::{error, info, warn};

const IPC_SOCKET: &str = "/tmp/ewm.sock";

/// Events sent to Emacs
#[derive(Serialize)]
#[serde(tag = "event")]
enum IpcEvent {
    #[serde(rename = "new")]
    New { id: u32, app: String },
    #[serde(rename = "close")]
    Close { id: u32 },
}

/// Commands received from Emacs
#[derive(Deserialize)]
#[serde(tag = "cmd")]
enum Command {
    #[serde(rename = "layout")]
    Layout { id: u32, x: i32, y: i32, w: u32, h: u32 },
    #[serde(rename = "hide")]
    Hide { id: u32 },
    #[serde(rename = "close")]
    Close { id: u32 },
    #[serde(rename = "focus")]
    Focus { id: u32 },
    #[serde(rename = "screenshot")]
    Screenshot { path: Option<String> },
}

struct Ewm {
    running: bool,
    space: Space<Window>,
    display_handle: DisplayHandle,

    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    xdg_decoration_state: XdgDecorationState,
    shm_state: ShmState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    seat: Seat<Self>,

    // IPC
    next_surface_id: u32,
    window_ids: HashMap<Window, u32>,
    id_windows: HashMap<u32, Window>,
    pending_events: Vec<IpcEvent>,

    // Output
    output_size: (i32, i32),

    // Input
    pointer_location: (f64, f64),
}

impl Ewm {
    fn new(display_handle: DisplayHandle) -> Self {
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
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
            seat_state,
            data_device_state,
            seat,
            next_surface_id: 1,
            window_ids: HashMap::new(),
            id_windows: HashMap::new(),
            pending_events: Vec::new(),
            output_size: (800, 600), // Default, updated when output is created
            pointer_location: (0.0, 0.0),
        }
    }

    fn init_wayland_listener(
        display: &mut Display<Ewm>,
        event_loop: &LoopHandle<LoopData>,
    ) -> Result<std::ffi::OsString, Box<dyn std::error::Error>> {
        let socket = ListeningSocketSource::with_name("wayland-ewm")?;
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

        for window in self.space.elements() {
            window.on_commit();
        }
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
impl DataDeviceHandler for Ewm {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}
impl ClientDndGrabHandler for Ewm {}
impl ServerDndGrabHandler for Ewm {}
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
        let app = smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().unwrap().app_id.clone())
        })
        .unwrap_or_else(|| "unknown".to_string());

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

struct LoopData {
    state: Ewm,
    display: Display<Ewm>,
    emacs: Option<UnixStream>,
}

impl LoopData {
    fn send_event(&mut self, event: &IpcEvent) {
        if let Some(ref mut stream) = self.emacs {
            if let Ok(json) = serde_json::to_string(event) {
                if writeln!(stream, "{}", json).is_err() {
                    warn!("Failed to send event to Emacs, disconnecting");
                    self.emacs = None;
                }
            }
        }
    }
}

fn main() {
    if let Err(e) = run() {
        error!("Fatal error: {}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let mut event_loop: EventLoop<LoopData> = EventLoop::try_new()?;
    let mut display: Display<Ewm> = Display::new()?;
    let display_handle = display.handle();

    let socket_name = Ewm::init_wayland_listener(&mut display, &event_loop.handle())?;
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

    event_loop
        .handle()
        .insert_source(
            Generic::new(ipc_listener, Interest::READ, CalloopMode::Level),
            |_, listener, data| {
                if let Ok((stream, _)) = listener.accept() {
                    info!("Emacs connected");
                    stream.set_nonblocking(true).ok();
                    data.emacs = Some(stream);
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

    info!(
        "EWM compositor started. Run: WAYLAND_DISPLAY={:?} foot",
        socket_name
    );

    // Main loop
    // Set initial keyboard focus to first surface (Emacs)
    let mut keyboard_focus: Option<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface> = None;
    let mut screenshot_path: Option<String> = None;

    while data.state.running {
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
            WinitEvent::Input(event) => {
                input_events.push(event);
            }
            _ => {}
        });

        // Process input events
        for event in input_events {
            match event {
                InputEvent::Keyboard { event } => {
                    // Set focus to first surface if not set
                    if keyboard_focus.is_none() {
                        if let Some(window) = data.state.space.elements().next() {
                            if let Some(surface) = window.wl_surface() {
                                keyboard_focus = Some(surface.into_owned());
                            }
                        }
                    }

                    let serial = SERIAL_COUNTER.next_serial();
                    let time = Event::time_msec(&event);
                    let keyboard = data.state.seat.get_keyboard().unwrap();

                    // Set keyboard focus
                    if let Some(ref focus) = keyboard_focus {
                        keyboard.set_focus(&mut data.state, Some(focus.clone()), serial);
                    }

                    // Process key
                    keyboard.input::<(), _>(
                        &mut data.state,
                        event.key_code(),
                        event.state(),
                        serial,
                        time,
                        |_, _, _| FilterResult::Forward,
                    );
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

                    let button_state = match event.state() {
                        ButtonState::Pressed => {
                            // Focus window under pointer on click
                            if let Some((window, _)) = data.state.space.element_under(data.state.pointer_location) {
                                if let Some(surface) = window.wl_surface() {
                                    let owned_surface = surface.into_owned();
                                    keyboard_focus = Some(owned_surface.clone());
                                    let keyboard = data.state.seat.get_keyboard().unwrap();
                                    keyboard.set_focus(&mut data.state, Some(owned_surface), serial);
                                }
                            }
                            smithay::backend::input::ButtonState::Pressed
                        }
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
            let (renderer, mut framebuffer) = backend.bind()?;

            let result = render_output::<_, WaylandSurfaceRenderElement<GlesRenderer>, _, _>(
                &output,
                renderer,
                &mut framebuffer,
                1.0,
                0, // age - disabling damage tracking for now
                [&data.state.space],
                &[],
                &mut damage_tracker,
                [0.1, 0.1, 0.1, 1.0],
            );

            if let Err(e) = result {
                error!("Render error: {:?}", e);
            }

            // Screenshot capture - this invalidates the framebuffer, so we skip submit after
            if let Some(ref path) = screenshot_path {
                let size = data.state.output_size;
                let rect = smithay::utils::Rectangle::from_size((size.0, size.1).into());

                match renderer.copy_framebuffer(&framebuffer, rect, Fourcc::Xrgb8888) {
                    Ok(mapping) => {
                        match renderer.map_texture(&mapping) {
                            Ok(pixel_data) => {
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

                                match image::RgbImage::from_raw(width as u32, height as u32, rgb_data) {
                                    Some(img) => {
                                        if let Err(e) = img.save(path) {
                                            error!("Failed to save screenshot: {}", e);
                                        } else {
                                            info!("Screenshot saved to {}", path);
                                        }
                                    }
                                    None => error!("Failed to create image buffer"),
                                }
                            }
                            Err(e) => error!("Failed to map texture: {:?}", e),
                        }
                    }
                    Err(e) => error!("Failed to copy framebuffer: {:?}", e),
                }
                screenshot_path = None;
            }
        }

        // TODO: copy_framebuffer corrupts the EGL surface, so we skip submit after screenshots.
        // This drops one frame. A proper fix would render to an offscreen texture first
        // (like niri does), then copy from that without affecting the display framebuffer.
        // See: niri/src/render_helpers/mod.rs render_to_texture() and render_and_download()
        if !taking_screenshot {
            backend.submit(None)?;
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

        // Flush pending events to Emacs
        let events: Vec<_> = data.state.pending_events.drain(..).collect();
        for event in events {
            data.send_event(&event);
        }

        // Read commands from Emacs (non-blocking)
        if let Some(ref mut stream) = data.emacs {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if let Ok(cmd) = serde_json::from_str::<Command>(&line) {
                    match cmd {
                        Command::Layout { id, x, y, w, h } => {
                            if let Some(window) = data.state.id_windows.get(&id) {
                                // Use coordinates directly from Emacs.
                                // Transform::Flipped180 only affects content rendering (OpenGL fix),
                                // not the positioning coordinate system.
                                data.state.space.map_element(window.clone(), (x, y), true);
                                data.state.space.raise_element(window, true);
                                window.toplevel().map(|t| {
                                    t.with_pending_state(|state| {
                                        state.size = Some((w as i32, h as i32).into());
                                    });
                                    t.send_configure();
                                });
                                info!("Layout surface {} at ({}, {}) {}x{}", id, x, y, w, h);
                            }
                        }
                        Command::Hide { id } => {
                            if let Some(window) = data.state.id_windows.get(&id) {
                                // Move offscreen to hide (like EXWM's approach)
                                data.state.space.map_element(window.clone(), (-10000, -10000), false);
                                info!("Hide surface {}", id);
                            }
                        }
                        Command::Close { id } => {
                            if let Some(window) = data.state.id_windows.get(&id) {
                                // Send xdg_toplevel.close to request graceful close
                                if let Some(toplevel) = window.toplevel() {
                                    toplevel.send_close();
                                    info!("Close surface {} (sent close request)", id);
                                }
                            }
                        }
                        Command::Focus { id } => {
                            info!("Focus surface {} (not implemented)", id);
                        }
                        Command::Screenshot { path } => {
                            let target = path.unwrap_or_else(|| "/tmp/ewm-screenshot.png".to_string());
                            screenshot_path = Some(target.clone());
                            info!("Screenshot requested: {}", target);
                        }
                    }
                }
                line.clear();
            }
        }

        event_loop.dispatch(Some(std::time::Duration::from_millis(16)), &mut data)?;
    }

    Ok(())
}
