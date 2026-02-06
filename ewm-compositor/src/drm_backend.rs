//! DRM/libinput backend for running EWM as a standalone Wayland session
//!
//! This module provides the backend for running directly on hardware without
//! another compositor (like running from a TTY).
//!
//! Based on niri's TTY backend approach.
//!
//! Key insight: DRM master can only be acquired when the session is active.
//! Session activation happens asynchronously via libseat, so we must defer
//! all DRM operations until we receive an ActivateSession event.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use smithay::{
    backend::{
        allocator::{
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc, Modifier,
        },
        drm::{
            compositor::{DrmCompositor, FrameFlags},
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode,
        },
        egl::{EGLDevice, EGLDisplay},
        input::{Event, InputEvent, KeyboardKeyEvent},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            element::{
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                Kind,
            },
            gles::GlesRenderer,
            multigpu::{gbm::GbmGlesBackend, GpuManager},
            ImportDma, ImportEgl,
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::primary_gpu,
    },
    desktop::Window,
    input::keyboard::FilterResult,
    output::{Mode, Output, OutputModeSource, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            generic::Generic, timer::{TimeoutAction, Timer}, EventLoop, Interest,
            Mode as CalloopMode, PostAction,
            channel::{Sender, channel},
        },
        drm::control::{connector, crtc, Device as ControlDevice, ModeTypeFlags},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::{protocol::wl_surface::WlSurface, Display, Resource},
    },
    utils::{DeviceFd, Scale, Transform, SERIAL_COUNTER},
    wayland::{dmabuf::DmabufFeedbackBuilder, seat::WaylandFocus},
};
use tracing::{debug, info, warn};

use crate::{Ewm, LoopData, IPC_SOCKET};

const SUPPORTED_COLOR_FORMATS: [Fourcc; 4] = [
    Fourcc::Xrgb8888,
    Fourcc::Xbgr8888,
    Fourcc::Argb8888,
    Fourcc::Abgr8888,
];

/// Type alias for our DRM compositor
type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmDevice<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;

/// Per-output surface state
struct OutputSurface {
    output: Output,
    compositor: GbmDrmCompositor,
}

/// Message to trigger deferred DRM initialization
pub enum DrmMessage {
    InitializeDrm,
}

/// State needed to initialize DRM (kept until session becomes active)
#[allow(dead_code)]
struct DrmPendingInit {
    gpu_path: PathBuf,
    seat_name: String,
}

/// DRM device state (only present after session activation)
#[allow(dead_code)]
struct DrmDeviceState {
    drm: DrmDevice,
    gbm: GbmDevice<DrmDeviceFd>,
    gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    render_node: DrmNode,
    surfaces: HashMap<crtc::Handle, OutputSurface>,
}

/// Shared DRM backend state
#[allow(dead_code)]
pub struct DrmBackendState {
    session: LibSeatSession,
    libinput: Libinput,
    /// DRM device state - None until session is active and DRM is initialized
    device: Option<DrmDeviceState>,
    /// Pending initialization data - Some until DRM is initialized
    pending: Option<DrmPendingInit>,
    paused: bool,
    /// Channel to trigger deferred initialization
    init_sender: Option<Sender<DrmMessage>>,
    /// Flag indicating a redraw is needed (set by client commits)
    /// This is how client buffer commits trigger renders
    needs_redraw: bool,
}

impl DrmBackendState {
    /// Check if DRM is initialized and ready
    pub fn is_initialized(&self) -> bool {
        self.device.is_some()
    }

    /// Get the render node (if DRM is initialized)
    pub fn render_node(&self) -> Option<DrmNode> {
        self.device.as_ref().map(|d| d.render_node)
    }

    /// Mark that a redraw is needed (called from client commit handler)
    /// This is how client buffer commits trigger the render loop
    pub fn queue_redraw(&mut self) {
        self.needs_redraw = true;
    }

    /// Check if a redraw is needed
    pub fn needs_redraw(&self) -> bool {
        self.needs_redraw
    }

    /// Perform early buffer import for a surface
    /// This is crucial for proper dmabuf/EGL buffer import on DRM backends
    pub fn early_import(&mut self, surface: &WlSurface) {
        let Some(device) = &mut self.device else {
            debug!("DRM not initialized yet, skipping early_import");
            return;
        };
        match device.gpu_manager.early_import(device.render_node, surface) {
            Ok(_) => info!("Early import succeeded for surface {:?}", surface.id()),
            Err(err) => info!("Early buffer import skipped/failed for surface {:?}: {:?}", surface.id(), err),
        }
    }

    /// Handle session pause (VT switch away)
    fn pause(&mut self) {
        debug!("Pausing DRM session");
        self.libinput.suspend();
        if let Some(device) = &mut self.device {
            device.drm.pause();
        }
        self.paused = true;
    }

    /// Handle session resume (VT switch back)
    fn resume(&mut self) {
        debug!("Resuming DRM session");
        self.paused = false;

        if self.libinput.resume().is_err() {
            warn!("Error resuming libinput");
        }

        if let Some(device) = &mut self.device {
            if let Err(err) = device.drm.activate(true) {
                warn!("Error activating DRM device: {:?}", err);
            } else {
                info!("DRM device activated successfully (DRM master acquired)");
            }
        }
    }

    /// Trigger deferred DRM initialization (called when session becomes active)
    fn trigger_init(&self) {
        if let Some(sender) = &self.init_sender {
            if let Err(e) = sender.send(DrmMessage::InitializeDrm) {
                warn!("Failed to send DRM init message: {:?}", e);
            }
        }
    }

    /// Render a frame to the given output
    fn render_output(&mut self, crtc: crtc::Handle, space: &smithay::desktop::Space<Window>) {
        let Some(device) = &mut self.device else {
            return;
        };

        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };

        if self.paused || !device.drm.is_active() {
            debug!("Skipping render: paused={} drm_active={}", self.paused, device.drm.is_active());
            return;
        }

        let output = &surface.output;
        let output_scale = Scale::from(output.current_scale().fractional_scale());

        // Get a renderer from the GPU manager
        let Ok(mut renderer) = device.gpu_manager.single_renderer(&device.render_node) else {
            warn!("Failed to get renderer from GPU manager");
            return;
        };

        // Create render elements from window surface trees
        let mut elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
        let space_element_count = space.elements().count();
        for window in space.elements() {
            if let Some(wl_surface) = window.wl_surface() {
                let loc = space.element_location(window).unwrap_or_default();
                let window_geo = window.geometry();
                let window_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = render_elements_from_surface_tree(
                    renderer.as_mut(),
                    &wl_surface,
                    loc.to_physical_precise_round(output_scale),
                    output_scale,
                    1.0,
                    Kind::Unspecified,
                );
                debug!("Window at {:?}, geometry {:?}: {} render elements", loc, window_geo, window_elements.len());
                elements.extend(window_elements);
            }
        }

        debug!("Rendering {} elements from {} windows", elements.len(), space_element_count);

        // Use the same frame flags as niri for proper plane scanout
        let flags = FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY
            | FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT;

        // Render the frame
        let compositor = &mut surface.compositor;
        match compositor.render_frame::<_, _>(
            renderer.as_mut(),
            &elements,
            [0.1, 0.1, 0.1, 1.0], // Dark gray background
            flags,
        ) {
            Ok(result) => {
                debug!("Render result: is_empty={}", result.is_empty);

                // Only queue frame if there's actual damage to display
                // This matches niri's behavior - queueing empty frames can cause issues
                if !result.is_empty {
                    match compositor.queue_frame(()) {
                        Ok(()) => {
                            // Successfully queued frame - clear the redraw flag
                            // VBlank will fire and continue the render cycle
                            self.needs_redraw = false;
                        }
                        Err(err) => {
                            warn!("Error queueing frame: {:?}", err);
                        }
                    }
                }

                // Send frame callbacks to clients so they can commit new buffers
                for window in space.elements() {
                    window.send_frame(output, Duration::ZERO, None, |_, _| Some(output.clone()));
                }
            }
            Err(err) => {
                warn!("Error rendering frame: {:?}", err);
            }
        }
    }
}

/// Initialize DRM device and set up outputs
/// This is called when the session becomes active
fn initialize_drm(
    backend_state: &std::rc::Rc<std::cell::RefCell<DrmBackendState>>,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
    ewm_state: &mut Ewm,
    event_loop_handle: &smithay::reexports::calloop::LoopHandle<'static, LoopData>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut backend = backend_state.borrow_mut();

    // Get pending init data
    let pending = backend.pending.take().ok_or("DRM already initialized")?;

    info!("Initializing DRM device (session is now active)");

    // Open DRM device via libseat
    let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let fd = backend.session.open(&pending.gpu_path, open_flags)?;
    let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    // Create DRM and GBM devices
    let (mut drm, drm_notifier) = DrmDevice::new(device_fd.clone(), true)?;
    let gbm = GbmDevice::new(device_fd.clone())?;

    info!("DRM device created, is_active: {}", drm.is_active());

    // Explicitly acquire DRM master - this is crucial!
    // DrmDevice::new() may fail to get master initially, so we request it now
    // that the session is confirmed active
    if let Err(err) = drm.activate(true) {
        warn!("Failed to activate DRM device (acquire master): {:?}", err);
    } else {
        info!("DRM device activated, is_active: {}", drm.is_active());
    }

    // Create EGL display to get render node (using niri's approach)
    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_device = EGLDevice::device_for_display(&egl_display)?;
    let render_node = egl_device
        .try_get_render_node()?
        .ok_or("No render node found")?;
    info!("Render node: {:?}", render_node);

    // Create GPU manager (handles renderer creation and EGL context)
    let api: GbmGlesBackend<GlesRenderer, DrmDeviceFd> = GbmGlesBackend::with_context_priority(
        smithay::backend::egl::context::ContextPriority::High,
    );
    let mut gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>> = GpuManager::new(api)?;
    gpu_manager.as_mut().add_node(render_node, gbm.clone())?;

    // Get a renderer and bind to Wayland display
    {
        let mut renderer = gpu_manager.single_renderer(&render_node)?;
        if let Err(err) = renderer.bind_wl_display(display_handle) {
            warn!("Error binding wl-display in EGL: {:?}", err);
        } else {
            info!("Renderer bound to Wayland display");
        }

        // Create dmabuf global
        let dmabuf_formats = renderer.dmabuf_formats().clone();
        if let Ok(default_feedback) = DmabufFeedbackBuilder::new(render_node.dev_id(), dmabuf_formats).build() {
            let _global = ewm_state.dmabuf_state.create_global_with_default_feedback::<Ewm>(
                display_handle,
                &default_feedback,
            );
            info!("Dmabuf global created");
        }
    }

    let mut surfaces = HashMap::new();

    // Set up outputs (monitors)
    let resources = drm.resource_handles()?;

    for connector_handle in resources.connectors() {
        let connector_info = drm.get_connector(*connector_handle, false)?;
        if connector_info.state() != connector::State::Connected {
            continue;
        }

        // Find a suitable CRTC
        let crtc = connector_info
            .encoders()
            .iter()
            .filter_map(|enc| drm.get_encoder(*enc).ok())
            .find_map(|enc| {
                resources.filter_crtcs(enc.possible_crtcs()).into_iter().next()
            })
            .ok_or("No suitable CRTC found")?;

        // Find preferred mode
        let mode = connector_info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| connector_info.modes().first())
            .copied()
            .ok_or("No mode available")?;

        info!(
            "Setting up display: {:?} {}x{}@{}Hz",
            connector_info.interface(),
            mode.size().0,
            mode.size().1,
            mode.vrefresh()
        );

        // Create DRM surface
        let drm_surface = drm.create_surface(crtc, mode, &[*connector_handle])?;

        // Create allocator
        let gbm_flags = GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT;
        let allocator = GbmAllocator::new(gbm.clone(), gbm_flags);

        // Get render formats from GPU manager
        let renderer = gpu_manager.single_renderer(&render_node)?;
        let raw_render_formats = renderer.as_ref().egl_context().dmabuf_render_formats();

        // Filter out problematic modifiers (CCS compression modifiers on Intel)
        let render_formats: FormatSet = raw_render_formats
            .iter()
            .copied()
            .filter(|format| {
                !matches!(
                    format.modifier,
                    Modifier::I915_y_tiled_ccs
                    | Modifier::I915_y_tiled_gen12_rc_ccs
                    | Modifier::I915_y_tiled_gen12_mc_ccs
                )
            })
            .collect();

        // Create Smithay output
        let connector_name = format!("{:?}-{}", connector_info.interface(), connector_info.interface_id());
        let output = Output::new(
            connector_name.clone(),
            PhysicalProperties {
                size: connector_info.size().map(|(w, h)| (w as i32, h as i32).into()).unwrap_or_default(),
                subpixel: Subpixel::Unknown,
                make: "EWM".into(),
                model: "DRM".into(),
            },
        );

        let smithay_mode = Mode {
            size: (mode.size().0 as i32, mode.size().1 as i32).into(),
            refresh: (mode.vrefresh() * 1000) as i32,
        };
        output.change_current_state(Some(smithay_mode), Some(Transform::Normal), None, None);
        output.set_preferred(smithay_mode);
        output.create_global::<Ewm>(display_handle);

        // Create DrmCompositor - try with filtered formats first, fallback to Invalid modifier only
        let cursor_size = drm.cursor_size();
        let compositor = match DrmCompositor::new(
            OutputModeSource::Auto(output.clone()),
            drm_surface,
            None, // planes
            allocator.clone(),
            gbm.clone(),
            SUPPORTED_COLOR_FORMATS,
            render_formats.clone(),
            cursor_size,
            Some(gbm.clone()),
        ) {
            Ok(c) => c,
            Err(err) => {
                warn!("Error creating DRM compositor, trying with Invalid modifier: {:?}", err);

                // Fallback: only use formats with Invalid modifier (linear)
                let fallback_formats: FormatSet = render_formats
                    .iter()
                    .copied()
                    .filter(|format| format.modifier == Modifier::Invalid)
                    .collect();

                // Recreate the surface since DrmCompositor::new consumed it
                let drm_surface = drm.create_surface(crtc, mode, &[*connector_handle])?;

                DrmCompositor::new(
                    OutputModeSource::Auto(output.clone()),
                    drm_surface,
                    None,
                    allocator,
                    gbm.clone(),
                    SUPPORTED_COLOR_FORMATS,
                    fallback_formats,
                    cursor_size,
                    Some(gbm.clone()),
                )?
            }
        };

        info!("DrmCompositor created for {}", connector_name);

        surfaces.insert(crtc, OutputSurface { output: output.clone(), compositor });
        ewm_state.space.map_output(&output, (0, 0));
        ewm_state.output_size = (mode.size().0 as i32, mode.size().1 as i32);

        break; // Only use first display
    }

    // Store device state
    backend.device = Some(DrmDeviceState {
        drm,
        gbm,
        gpu_manager,
        render_node,
        surfaces,
    });

    // Need to drop the borrow before registering the DRM notifier
    drop(backend);

    // Register DRM event notifier for VBlank
    let backend_for_vblank = backend_state.clone();
    event_loop_handle.insert_source(drm_notifier, move |event, _, data| {
        if let DrmEvent::VBlank(crtc) = event {
            let mut backend = backend_for_vblank.borrow_mut();

            // Mark the frame as submitted - this is crucial!
            // Without this, the compositor thinks there's still a pending frame
            if let Some(device) = &mut backend.device {
                if let Some(surface) = device.surfaces.get_mut(&crtc) {
                    match surface.compositor.frame_submitted() {
                        Ok(_) => {}
                        Err(err) => {
                            warn!("Error marking frame as submitted: {:?}", err);
                        }
                    }
                }
            }

            // Now render the next frame
            drop(backend);
            backend_for_vblank.borrow_mut().render_output(crtc, &data.state.space);
        }
    })?;

    info!("DRM initialization complete");

    // Trigger initial render to start the VBlank chain
    {
        let backend = backend_state.borrow_mut();
        if let Some(device) = &backend.device {
            let crtcs: Vec<_> = device.surfaces.keys().copied().collect();
            drop(backend);
            for crtc in crtcs {
                backend_state.borrow_mut().render_output(crtc, &ewm_state.space);
            }
        }
    }

    Ok(())
}

/// Run EWM with DRM/libinput backend (standalone session)
pub fn run_drm(program: String, program_args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting EWM with DRM backend");

    // 1. Initialize libseat session
    let (session, notifier) = LibSeatSession::new().map_err(|e| {
        format!("Failed to create libseat session: {}. Are you running from a TTY?", e)
    })?;
    let seat_name = session.seat();
    info!("libseat session opened, seat: {}", seat_name);

    // Check if session is already active
    let session_active = session.is_active();
    info!("Session active at startup: {}", session_active);

    // 2. Create event loop and Wayland display
    let mut event_loop: EventLoop<LoopData> = EventLoop::try_new()?;
    let mut display: Display<Ewm> = Display::new()?;
    let display_handle = display.handle();

    // 3. Initialize Wayland socket
    let socket_name = Ewm::init_wayland_listener(&mut display, &event_loop.handle())?;
    let socket_name_str = socket_name.to_string_lossy().to_string();
    info!("Wayland socket: {:?}", socket_name);

    let state = Ewm::new(display_handle.clone());
    let mut data = LoopData {
        state,
        display,
        emacs: None,
    };

    // 4. Find primary GPU (we can do this without DRM master)
    let gpu_path = primary_gpu(&seat_name)?
        .ok_or("No GPU found")?;
    info!("Primary GPU: {:?}", gpu_path);

    // 5. Initialize libinput (doesn't require DRM master)
    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput.udev_assign_seat(&seat_name).map_err(|()| "Failed to assign seat to libinput")?;

    // 6. Create channel for deferred DRM initialization
    let (init_sender, init_receiver) = channel::<DrmMessage>();

    // 7. Create backend state (DRM device will be initialized later)
    let backend_state = std::rc::Rc::new(std::cell::RefCell::new(DrmBackendState {
        session,
        libinput: libinput.clone(),
        device: None,
        pending: Some(DrmPendingInit {
            gpu_path: gpu_path.clone(),
            seat_name: seat_name.clone(),
        }),
        paused: false,
        init_sender: Some(init_sender),
        needs_redraw: true, // Start with redraw needed to render initial frame
    }));

    // Set the DRM backend on Ewm for early_import support
    data.state.set_drm_backend(backend_state.clone());

    // 8. Register session notifier FIRST (before any DRM operations)
    // This is critical - we need to receive ActivateSession before doing DRM setup
    let backend_for_session = backend_state.clone();
    event_loop.handle().insert_source(notifier, move |event, _, _| {
        match event {
            SessionEvent::PauseSession => {
                info!("Session paused (VT switch away)");
                backend_for_session.borrow_mut().pause();
            }
            SessionEvent::ActivateSession => {
                info!("Session activated");
                let backend = backend_for_session.borrow_mut();
                if backend.device.is_none() {
                    // First activation - trigger DRM initialization
                    info!("First session activation - triggering DRM init");
                    backend.trigger_init();
                } else {
                    // VT switch back - resume
                    drop(backend);  // Release borrow before calling resume
                    backend_for_session.borrow_mut().resume();
                }
            }
        }
    })?;

    // 9. Register channel receiver for deferred DRM initialization
    let backend_for_init = backend_state.clone();
    let display_handle_for_init = display_handle.clone();
    let event_loop_handle = event_loop.handle();
    event_loop.handle().insert_source(init_receiver, move |event, _, data| {
        if let smithay::reexports::calloop::channel::Event::Msg(DrmMessage::InitializeDrm) = event {
            info!("Received DRM init message");
            if let Err(e) = initialize_drm(
                &backend_for_init,
                &display_handle_for_init,
                &mut data.state,
                &event_loop_handle,
            ) {
                warn!("Failed to initialize DRM: {:?}", e);
            }
        }
    })?;

    // 10. Register libinput with event loop
    let libinput_backend = LibinputInputBackend::new(libinput);
    event_loop.handle().insert_source(libinput_backend, move |event, _, data| {
        match event {
            InputEvent::Keyboard { event: kb_event } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&kb_event);
                let keycode = kb_event.key_code();
                let key_state = kb_event.state();
                let is_press = key_state == smithay::backend::input::KeyState::Pressed;

                let prefix_keys = data.state.prefix_keys.clone();
                let current_focus_id = data.state.focused_surface_id;
                let keyboard = data.state.seat.get_keyboard().unwrap();

                let filter_result = keyboard.input::<u8, _>(
                    &mut data.state,
                    keycode,
                    key_state,
                    serial,
                    time,
                    |_, mods, handle| {
                        if !is_press {
                            return FilterResult::Forward;
                        }

                        if crate::is_kill_combo(keycode.raw(), mods.ctrl, mods.logo) {
                            return FilterResult::Intercept(2);
                        }

                        let keysym = handle.modified_sym();
                        let is_prefix = prefix_keys.iter().any(|pk| pk.matches(keysym.raw(), mods));

                        if is_prefix && current_focus_id != 1 {
                            FilterResult::Intercept(1)
                        } else {
                            FilterResult::Forward
                        }
                    },
                );

                if filter_result == Some(2) {
                    info!("Kill combo pressed, shutting down");
                    data.state.running = false;
                    return;
                }

                if filter_result == Some(1) {
                    data.state.focused_surface_id = 1;
                    if let Some(window) = data.state.id_windows.get(&1) {
                        if let Some(surface) = window.wl_surface() {
                            let emacs_surface: WlSurface = surface.into_owned();
                            keyboard.set_focus(&mut data.state, Some(emacs_surface.clone()), serial);
                            keyboard.input::<(), _>(
                                &mut data.state,
                                keycode,
                                key_state,
                                serial,
                                time,
                                |_, _, _| FilterResult::Forward,
                            );
                        }
                    }
                } else {
                    let target_id = data.state.focused_surface_id;
                    if let Some(window) = data.state.id_windows.get(&target_id) {
                        if let Some(surface) = window.wl_surface() {
                            let focus_surface: WlSurface = surface.into_owned();
                            keyboard.set_focus(&mut data.state, Some(focus_surface), serial);
                        }
                    }
                }
            }
            _ => {}
        }
    })?;

    // 11. Fallback render timer
    // VBlanks drive normal rendering, but if they stop (e.g., no damage queued),
    // this timer restarts the render loop when new content arrives.
    // Key insight from niri: client commits set needs_redraw, timer checks it.
    let backend_for_timer = backend_state.clone();
    event_loop.handle().insert_source(
        Timer::from_duration(Duration::from_millis(16)),
        move |_, _, data| {
            let backend = backend_for_timer.borrow();
            let should_render = backend.is_initialized() && !backend.paused && backend.needs_redraw;
            let crtcs: Vec<_> = backend.device.as_ref()
                .map(|d| d.surfaces.keys().copied().collect())
                .unwrap_or_default();
            drop(backend);

            if should_render {
                for crtc in crtcs {
                    backend_for_timer.borrow_mut().render_output(crtc, &data.state.space);
                }
            }
            TimeoutAction::ToDuration(Duration::from_millis(16))
        },
    )?;

    // 12. Set up IPC socket
    let ipc_path = Path::new(IPC_SOCKET);
    if ipc_path.exists() {
        std::fs::remove_file(ipc_path)?;
    }
    let ipc_listener = std::os::unix::net::UnixListener::bind(ipc_path)?;
    ipc_listener.set_nonblocking(true)?;
    info!("IPC socket: {}", IPC_SOCKET);

    event_loop.handle().insert_source(
        Generic::new(ipc_listener, Interest::READ, CalloopMode::Level),
        |_, listener, data| {
            if let Ok((stream, _)) = listener.accept() {
                info!("Emacs connected");
                stream.set_nonblocking(true).ok();
                data.emacs = Some(stream);
            }
            Ok(PostAction::Continue)
        },
    )?;

    info!("EWM DRM backend started (waiting for session activation)");
    info!("VT switching: Ctrl+Alt+F1-F7");
    info!("Kill combo: Super+Ctrl+Backspace");

    // 13. If session is already active, initialize DRM immediately
    // (This handles the case where we're the foreground session from the start)
    if session_active {
        info!("Session already active, initializing DRM now");
        if let Err(e) = initialize_drm(
            &backend_state,
            &display_handle,
            &mut data.state,
            &event_loop.handle(),
        ) {
            return Err(format!("Failed to initialize DRM: {:?}", e).into());
        }
    }

    // 14. Spawn client after a delay (using a one-shot timer so event loop runs first)
    let client_process: std::rc::Rc<std::cell::RefCell<Option<std::process::Child>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let client_process_for_timer = client_process.clone();
    event_loop.handle().insert_source(
        Timer::from_duration(Duration::from_secs(2)),
        move |_, _, _data| {
            info!("Spawning client after delay...");
            match crate::spawn_client(&program, &program_args, &socket_name_str) {
                Ok(child) => {
                    info!("Client spawned with PID {}", child.id());
                    *client_process_for_timer.borrow_mut() = Some(child);
                }
                Err(e) => {
                    warn!("Failed to spawn client: {:?}", e);
                }
            }
            TimeoutAction::Drop // One-shot timer
        },
    )?;

    // Main loop
    while data.state.running {
        if let Some(ref mut child) = *client_process.borrow_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                info!("Client exited with status: {}", status);
                break;
            }
        }

        event_loop.dispatch(Some(Duration::from_millis(16)), &mut data)?;
        data.display.flush_clients().unwrap();
    }

    info!("EWM DRM backend shutting down");
    Ok(())
}

/// Check if we're running inside another compositor/display server
pub fn is_nested() -> bool {
    std::env::var("WAYLAND_DISPLAY").ok().filter(|s| !s.is_empty()).is_some()
        || std::env::var("DISPLAY").ok().filter(|s| !s.is_empty()).is_some()
}
