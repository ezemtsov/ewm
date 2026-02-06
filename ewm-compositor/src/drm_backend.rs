//! DRM/libinput backend for running EWM as a standalone Wayland session
//!
//! This module provides the backend for running directly on hardware without
//! another compositor (like running from a TTY).

use smithay::{
    backend::{
        allocator::gbm::GbmDevice,
        drm::{DrmDevice, DrmDeviceFd},
        egl::{EGLContext, EGLDisplay},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::gles::GlesRenderer,
        session::{libseat::LibSeatSession, Session},
        udev::UdevBackend,
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{generic::Generic, EventLoop, Interest, Mode as CalloopMode, PostAction},
        drm::control::{connector, Device as ControlDevice, ModeTypeFlags},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::Display,
    },
    utils::{DeviceFd, Transform},
};
use std::path::Path;
use tracing::{error, info, warn};

use crate::{Ewm, LoopData, IPC_SOCKET};

/// Run EWM with DRM/libinput backend (standalone session)
pub fn run_drm(emacs_args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting EWM with DRM backend");

    // 1. Initialize libseat session (must be first - handles logind/VT switching)
    let (mut session, notifier) = LibSeatSession::new()
        .map_err(|e| format!("Failed to create libseat session: {}. Are you running from a TTY?", e))?;
    let seat_name = session.seat();
    info!("libseat session opened, seat: {}", seat_name);

    // 2. Create event loop
    let mut event_loop: EventLoop<LoopData> = EventLoop::try_new()?;
    let mut display: Display<Ewm> = Display::new()?;
    let display_handle = display.handle();

    // 3. Initialize Wayland socket
    let socket_name = Ewm::init_wayland_listener(&mut display, &event_loop.handle())?;
    let socket_name_str = socket_name.to_string_lossy().to_string();
    info!("Wayland socket: {:?}", socket_name);

    let state = Ewm::new(display_handle.clone());
    let mut data = LoopData { state, display, emacs: None };

    // 4. Initialize udev for device enumeration
    let udev_backend = UdevBackend::new(&seat_name)?;

    // 5. Find primary GPU - device_list returns (DrmNode, PathBuf)
    let (primary_node, gpu_path) = udev_backend
        .device_list()
        .find(|(_, path)| {
            // Prefer card nodes over render nodes for KMS
            path.to_string_lossy().contains("/card")
        })
        .or_else(|| udev_backend.device_list().next())
        .ok_or("No GPU found")?;

    info!("Primary GPU: {:?} at {:?}", primary_node, gpu_path);

    // 6. Open the DRM device via libseat (session manages access rights)
    let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let fd = session.open(&gpu_path, open_flags)?;
    let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    // 7. Create DRM device (KMS API handle)
    let (drm_device, drm_notifier) = DrmDevice::new(device_fd.clone(), true)?;

    // 8. Create GBM device for buffer allocation
    let gbm_device = GbmDevice::new(device_fd.clone())?;

    // 9. Create EGL display and renderer
    let egl_display = unsafe { EGLDisplay::new(gbm_device.clone())? };
    let egl_context = EGLContext::new(&egl_display)?;
    let _renderer = unsafe { GlesRenderer::new(egl_context)? };

    info!("DRM/GBM/EGL initialized");

    // 10. Set up outputs (monitors)
    let resources = drm_device.resource_handles()?;

    for connector_handle in resources.connectors() {
        let connector_info = drm_device.get_connector(*connector_handle, false)?;

        if connector_info.state() != connector::State::Connected {
            continue;
        }

        // Find a suitable mode (prefer native/preferred mode)
        let mode = connector_info
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| connector_info.modes().first())
            .copied()
            .ok_or("No mode available for connector")?;

        info!(
            "Found connected display: {:?} {}x{}@{}Hz",
            connector_info.interface(),
            mode.size().0,
            mode.size().1,
            mode.vrefresh()
        );

        // Create Smithay output
        let output = Output::new(
            format!("{:?}-{}", connector_info.interface(), connector_info.interface_id()),
            PhysicalProperties {
                size: (connector_info.size().unwrap_or((0, 0)).0 as i32,
                       connector_info.size().unwrap_or((0, 0)).1 as i32).into(),
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
        output.create_global::<Ewm>(&display_handle);

        data.state.space.map_output(&output, (0, 0));
        data.state.output_size = (mode.size().0 as i32, mode.size().1 as i32);

        // For now, only use first connected display
        break;
    }

    // 11. Initialize libinput for keyboard/mouse
    let mut libinput_context = Libinput::new_with_udev(
        LibinputSessionInterface::from(session.clone())
    );
    libinput_context
        .udev_assign_seat(&seat_name)
        .map_err(|()| "Failed to assign seat to libinput")?;

    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    // 12. Register libinput with event loop
    event_loop
        .handle()
        .insert_source(libinput_backend, |event, _, _data| {
            // Process input events - basic handling for now
            match event {
                InputEvent::Keyboard { event: _ } => {
                    // TODO: Forward to keyboard handling
                }
                InputEvent::PointerMotion { event: _ } => {
                    // TODO: Forward to pointer handling
                }
                InputEvent::PointerButton { event: _ } => {
                    // TODO: Forward to pointer handling
                }
                _ => {}
            }
        })?;

    // 13. Register session notifier for VT switching
    event_loop
        .handle()
        .insert_source(notifier, |event, _, _data| {
            info!("Session event: {:?}", event);
            // TODO: Handle pause/resume for VT switching
        })?;

    // 14. Register DRM event notifier for VBlank
    event_loop
        .handle()
        .insert_source(drm_notifier, |event, _meta, _data| {
            match event {
                smithay::backend::drm::DrmEvent::VBlank(_crtc) => {
                    // TODO: Handle VBlank for frame scheduling
                }
                smithay::backend::drm::DrmEvent::Error(err) => {
                    warn!("DRM error: {:?}", err);
                }
            }
        })?;

    // 15. Set up IPC socket for Emacs
    let ipc_path = Path::new(IPC_SOCKET);
    if ipc_path.exists() {
        std::fs::remove_file(ipc_path)?;
    }
    let ipc_listener = std::os::unix::net::UnixListener::bind(ipc_path)?;
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
        )?;

    // 16. Spawn Emacs
    let mut emacs_process = crate::spawn_emacs(&socket_name_str, &emacs_args)?;
    info!("Emacs spawned with PID {}", emacs_process.id());

    info!("EWM DRM backend started");

    // Main loop - simplified for now
    // TODO: Full implementation with proper rendering using DRM compositor
    while data.state.running {
        // Check if Emacs has exited
        match emacs_process.try_wait() {
            Ok(Some(status)) => {
                info!("Emacs exited with status: {}", status);
                data.state.running = false;
                break;
            }
            Ok(None) => {}
            Err(e) => {
                error!("Error checking Emacs process: {}", e);
            }
        }

        // Process events
        event_loop.dispatch(Some(std::time::Duration::from_millis(16)), &mut data)?;

        // Flush Wayland clients
        data.display.flush_clients().unwrap();
    }

    Ok(())
}

/// Check if we're running inside another compositor/display server
pub fn is_nested() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_ok()
}
