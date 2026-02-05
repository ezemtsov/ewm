//! EWM Compositor - Phase 3: Compositor with Emacs IPC
//!
//! Run with: cargo run
//! Then in Emacs: (ewm-connect)
//! Then: WAYLAND_DISPLAY=wayland-ewm foot

use smithay::{
    backend::{
        renderer::{
            damage::OutputDamageTracker,
            element::surface::WaylandSurfaceRenderElement,
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
    desktop::{space::render_output, Space, Window},
    input::{Seat, SeatHandler, SeatState},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            generic::Generic, EventLoop, Interest, LoopHandle, Mode as CalloopMode, PostAction,
        },
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle, Resource,
        },
    },
    utils::Transform,
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
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
enum Event {
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
    #[serde(rename = "focus")]
    Focus { id: u32 },
}

struct Ewm {
    running: bool,
    space: Space<Window>,
    display_handle: DisplayHandle,

    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,

    // IPC
    next_surface_id: u32,
    window_ids: HashMap<Window, u32>,
    id_windows: HashMap<u32, Window>,
    pending_events: Vec<Event>,
}

impl Ewm {
    fn new(display_handle: DisplayHandle) -> Self {
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
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
            shm_state,
            seat_state,
            data_device_state,
            next_surface_id: 1,
            window_ids: HashMap::new(),
            id_windows: HashMap::new(),
            pending_events: Vec::new(),
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

        // Get app_id for the surface
        let app = smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().unwrap().app_id.clone())
        })
        .unwrap_or_else(|| "unknown".to_string());

        // Send initial configure
        surface.with_pending_state(|state| {
            state.size = Some((800, 600).into());
        });
        surface.send_configure();

        let window = Window::new_wayland_window(surface);
        self.window_ids.insert(window.clone(), id);
        self.id_windows.insert(id, window.clone());
        self.space.map_element(window, (0, 0), false);

        self.pending_events.push(Event::New { id, app: app.clone() });
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
                self.pending_events.push(Event::Close { id });
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

struct LoopData {
    state: Ewm,
    display: Display<Ewm>,
    emacs: Option<UnixStream>,
}

impl LoopData {
    fn send_event(&mut self, event: &Event) {
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
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, None);
    output.set_preferred(mode);
    output.create_global::<Ewm>(&display_handle);
    data.state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    info!(
        "EWM compositor started. Run: WAYLAND_DISPLAY={:?} foot",
        socket_name
    );

    // Main loop
    while data.state.running {
        // Winit events
        let _ = winit_evt.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => {
                let mode = Mode {
                    size,
                    refresh: 60_000,
                };
                output.change_current_state(Some(mode), None, None, None);
            }
            WinitEvent::CloseRequested => {
                data.state.running = false;
            }
            WinitEvent::Input(_event) => {}
            _ => {}
        });

        // Render
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
        }
        backend.submit(None)?;

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
                                data.state.space.map_element(window.clone(), (x, y), false);
                                window.toplevel().map(|t| {
                                    t.with_pending_state(|state| {
                                        state.size = Some((w as i32, h as i32).into());
                                    });
                                    t.send_configure();
                                });
                                info!("Layout surface {} to ({}, {}) {}x{}", id, x, y, w, h);
                            }
                        }
                        Command::Focus { id } => {
                            info!("Focus surface {} (not implemented)", id);
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
