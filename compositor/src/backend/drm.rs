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
use std::path::PathBuf;
use std::time::Duration;

use smithay::{
    backend::{
        allocator::{
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Modifier,
        },
        drm::{
            compositor::{DrmCompositor, FrameFlags},
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode,
        },
        egl::{EGLDevice, EGLDisplay},
        input::{
            AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputEvent,
            KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
        },
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            gles::GlesRenderer,
            multigpu::{gbm::GbmGlesBackend, GpuManager},
            ImportDma, ImportEgl,
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{primary_gpu, UdevBackend, UdevEvent},
    },
    input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    output::{Mode, Output, OutputModeSource, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            channel::{channel, Sender},
            timer::{TimeoutAction, Timer},
            EventLoop, LoopHandle, RegistrationToken,
        },
        drm::control::{connector, crtc, Device as ControlDevice, ModeTypeFlags},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::{protocol::wl_surface::WlSurface, Display, DisplayHandle, Resource},
    },
    utils::{DeviceFd, Point, Scale, Transform, SERIAL_COUNTER},
    wayland::{dmabuf::DmabufFeedbackBuilder, seat::WaylandFocus},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, info, warn};

use crate::{
    cursor::CursorBuffer,
    input::{handle_keyboard_event, KeyboardAction},
    ipc::setup_ipc_listener,
    render::{collect_render_elements_with_cursor, process_screencopies_for_output},
    spawn_client, Ewm, LoopData, OutputInfo, OutputMode,
};

const SUPPORTED_COLOR_FORMATS: [smithay::backend::allocator::Fourcc; 4] = [
    smithay::backend::allocator::Fourcc::Xrgb8888,
    smithay::backend::allocator::Fourcc::Xbgr8888,
    smithay::backend::allocator::Fourcc::Argb8888,
    smithay::backend::allocator::Fourcc::Abgr8888,
];

/// Type alias for our DRM compositor
type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmDevice<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;

/// Redraw state machine for proper VBlank synchronization
/// Based on niri's approach to avoid the timing bug where the redraw flag
/// is cleared too early (after queue_frame instead of after VBlank).
#[derive(Debug, Default)]
pub enum RedrawState {
    /// No redraw pending, output is idle
    #[default]
    Idle,
    /// A redraw has been requested but not yet started
    Queued,
    /// Frame has been queued to DRM, waiting for VBlank
    /// redraw_needed tracks if another redraw was requested while waiting
    WaitingForVBlank { redraw_needed: bool },
    /// No damage, using estimated VBlank timer instead of real one
    WaitingForEstimatedVBlank(RegistrationToken),
    /// Estimated VBlank timer active AND a new redraw was queued
    WaitingForEstimatedVBlankAndQueued(RegistrationToken),
}

impl RedrawState {
    /// Transition to request a redraw
    fn queue_redraw(self) -> Self {
        match self {
            RedrawState::Idle => RedrawState::Queued,
            RedrawState::WaitingForVBlank { .. } => RedrawState::WaitingForVBlank { redraw_needed: true },
            RedrawState::WaitingForEstimatedVBlank(token) => {
                RedrawState::WaitingForEstimatedVBlankAndQueued(token)
            }
            other => other, // Already queued, no-op
        }
    }
}

/// Per-output surface state
struct OutputSurface {
    output: Output,
    compositor: GbmDrmCompositor,
    redraw_state: RedrawState,
    /// Refresh interval in microseconds (for estimated VBlank timer)
    refresh_interval_us: u64,
    /// Connector handle for mode lookups
    connector: connector::Handle,
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
    drm_scanner: DrmScanner,
    gbm: GbmDevice<DrmDeviceFd>,
    gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    render_node: DrmNode,
    surfaces: HashMap<crtc::Handle, OutputSurface>,
}

/// Marker type for DRM backend (used in Backend enum)
#[allow(dead_code)]
pub struct DrmBackend;

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
    /// Event loop handle for scheduling timers
    loop_handle: Option<LoopHandle<'static, LoopData>>,
    /// Cursor buffer for rendering the mouse cursor
    cursor_buffer: CursorBuffer,
    /// Display handle for creating output globals on hotplug
    display_handle: Option<DisplayHandle>,
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

    /// Mark that a redraw is needed for all outputs (called from client commit handler)
    /// This transitions each output's RedrawState appropriately
    pub fn queue_redraw(&mut self) {
        let Some(device) = &mut self.device else {
            return;
        };
        for surface in device.surfaces.values_mut() {
            let old_state = std::mem::take(&mut surface.redraw_state);
            surface.redraw_state = old_state.queue_redraw();
        }
    }

    /// Check if any output has a redraw queued
    pub fn needs_redraw(&self) -> bool {
        let Some(device) = &self.device else {
            return false;
        };
        device
            .surfaces
            .values()
            .any(|s| matches!(s.redraw_state, RedrawState::Queued))
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
            Err(err) => info!(
                "Early buffer import skipped/failed for surface {:?}: {:?}",
                surface.id(),
                err
            ),
        }
    }

    /// Handle session pause (VT switch away)
    fn pause(&mut self) {
        debug!("Pausing DRM session");
        self.libinput.suspend();
        if let Some(device) = &mut self.device {
            device.drm.pause();
            // Cancel any pending estimated VBlank timers and reset states to Idle
            for surface in device.surfaces.values_mut() {
                if let RedrawState::WaitingForEstimatedVBlank(token)
                | RedrawState::WaitingForEstimatedVBlankAndQueued(token) = surface.redraw_state
                {
                    if let Some(ref handle) = self.loop_handle {
                        handle.remove(token);
                    }
                }
                surface.redraw_state = RedrawState::Idle;
            }
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
            // Queue redraws for all outputs to resume rendering
            for surface in device.surfaces.values_mut() {
                surface.redraw_state = RedrawState::Queued;
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

    /// Set mode for an output by name
    /// Returns true on success, false if output not found or mode change failed
    pub fn set_mode(&mut self, output_name: &str, width: i32, height: i32, refresh: Option<i32>) -> bool {
        let Some(device) = &mut self.device else {
            warn!("DRM not initialized, cannot set mode");
            return false;
        };

        // Find the surface by output name
        let surface = device.surfaces.values_mut().find(|s| s.output.name() == output_name);
        let Some(surface) = surface else {
            warn!("Output not found: {}", output_name);
            return false;
        };

        // Get connector info to find available modes
        let Ok(connector_info) = device.drm.get_connector(surface.connector, false) else {
            warn!("Failed to get connector info for {}", output_name);
            return false;
        };

        // Find matching mode
        let mode = connector_info
            .modes()
            .iter()
            .filter(|m| {
                m.size().0 as i32 == width && m.size().1 as i32 == height
            })
            .max_by_key(|m| {
                // Prefer matching refresh rate, otherwise highest refresh
                if let Some(target_refresh) = refresh {
                    if (m.vrefresh() as i32 - target_refresh).abs() < 2 {
                        return 1000 + m.vrefresh() as i32;
                    }
                }
                m.vrefresh() as i32
            });

        let Some(mode) = mode.copied() else {
            warn!("No matching mode found for {}x{} on {}", width, height, output_name);
            return false;
        };

        info!(
            "Setting mode for {}: {}x{}@{}Hz",
            output_name,
            mode.size().0,
            mode.size().1,
            mode.vrefresh()
        );

        // Apply the mode
        if let Err(err) = surface.compositor.use_mode(mode) {
            warn!("Failed to set mode: {:?}", err);
            return false;
        }

        // Update Smithay output state
        let smithay_mode = Mode {
            size: (mode.size().0 as i32, mode.size().1 as i32).into(),
            refresh: (mode.vrefresh() * 1000) as i32,
        };
        surface.output.change_current_state(Some(smithay_mode), None, None, None);
        surface.output.set_preferred(smithay_mode);

        // Update refresh interval
        surface.refresh_interval_us = if mode.vrefresh() > 0 {
            1_000_000 / mode.vrefresh() as u64
        } else {
            16_667
        };

        // Queue redraw
        surface.redraw_state = RedrawState::Queued;

        info!(
            "Mode changed successfully for {}: {}x{}@{}Hz",
            output_name,
            mode.size().0,
            mode.size().1,
            mode.vrefresh()
        );
        true
    }

    /// Render a frame to the given output
    fn render_output(&mut self, crtc: crtc::Handle, ewm: &mut Ewm) {
        // First pass: check if we should render and extract needed data
        let (should_render, refresh_interval_us, output, render_node) = {
            let Some(device) = &self.device else {
                return;
            };

            let Some(surface) = device.surfaces.get(&crtc) else {
                return;
            };

            // Only render if we're in a queued state
            let should_render = matches!(
                surface.redraw_state,
                RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
            );
            if !should_render {
                debug!("Skipping render: state={:?}", surface.redraw_state);
                return;
            }

            if self.paused || !device.drm.is_active() {
                debug!(
                    "Skipping render: paused={} drm_active={}",
                    self.paused,
                    device.drm.is_active()
                );
                return;
            }

            (
                should_render,
                surface.refresh_interval_us,
                surface.output.clone(),
                device.render_node,
            )
        };

        if !should_render {
            return;
        }

        let output_scale = Scale::from(output.current_scale().fractional_scale());

        // Get output position in global space (like niri does)
        let output_pos = ewm
            .space
            .output_geometry(&output)
            .map(|geo| geo.loc)
            .unwrap_or_default();

        // Get a renderer from the GPU manager
        let Some(device) = &mut self.device else {
            return;
        };

        let Ok(mut renderer) = device.gpu_manager.single_renderer(&render_node) else {
            warn!("Failed to get renderer from GPU manager");
            return;
        };

        // Collect render elements with cursor using shared function
        let elements = collect_render_elements_with_cursor(
            ewm,
            renderer.as_mut(),
            output_scale,
            &self.cursor_buffer,
            output_pos,
        );

        // Use the same frame flags as niri for proper plane scanout
        let flags =
            FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY | FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT;

        // Render the frame - need to get surface mutably
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };

        let render_result = surface.compositor.render_frame::<_, _>(
            renderer.as_mut(),
            &elements,
            [0.1, 0.1, 0.1, 1.0], // Dark gray background
            flags,
        );

        // Track if we need to process screencopy after releasing the surface borrow
        let mut should_process_screencopy = false;

        match render_result {
            Ok(result) => {
                debug!("Render result: is_empty={}", result.is_empty);

                if !result.is_empty {
                    // There's damage to display - queue frame and wait for VBlank
                    match surface.compositor.queue_frame(()) {
                        Ok(()) => {
                            // Transition to WaitingForVBlank
                            surface.redraw_state = RedrawState::WaitingForVBlank { redraw_needed: false };
                        }
                        Err(err) => {
                            warn!("Error queueing frame: {:?}", err);
                            surface.redraw_state = RedrawState::Idle;
                        }
                    }
                } else {
                    // No damage - mark that we need to queue estimated vblank timer
                    // We'll do it after releasing the borrow
                    surface.redraw_state = RedrawState::Idle; // Temporarily, will be updated
                }

                should_process_screencopy = true;
            }
            Err(err) => {
                warn!("Error rendering frame: {:?}", err);
                surface.redraw_state = RedrawState::Idle;
            }
        }

        // Check if we need to queue estimated vblank timer (no-damage case)
        let need_estimated_vblank = {
            let Some(device) = &self.device else {
                return;
            };
            let Some(surface) = device.surfaces.get(&crtc) else {
                return;
            };
            matches!(surface.redraw_state, RedrawState::Idle) && should_process_screencopy
        };

        if need_estimated_vblank {
            self.queue_estimated_vblank_timer(crtc, refresh_interval_us);
        }

        // Send frame callbacks to clients so they can commit new buffers
        for window in ewm.space.elements() {
            window.send_frame(&output, Duration::ZERO, None, |_, _| Some(output.clone()));
        }

        // Process pending screencopy requests for this output
        if should_process_screencopy {
            if let Some(ref event_loop) = self.loop_handle {
                // Get renderer again for screencopy
                let Some(device) = &mut self.device else {
                    return;
                };
                let Ok(mut renderer) = device.gpu_manager.single_renderer(&render_node) else {
                    return;
                };
                process_screencopies_for_output(
                    ewm,
                    renderer.as_mut(),
                    &output,
                    &self.cursor_buffer,
                    event_loop,
                );
            }
        }
    }

    /// Queue an estimated VBlank timer when there's no damage
    fn queue_estimated_vblank_timer(&mut self, crtc: crtc::Handle, refresh_interval_us: u64) {
        let Some(handle) = self.loop_handle.clone() else {
            warn!("No loop handle available for estimated VBlank timer");
            return;
        };

        let Some(device) = &mut self.device else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };

        let duration = Duration::from_micros(refresh_interval_us.max(1000));

        match handle.insert_source(Timer::from_duration(duration), move |_, _, data| {
            // Clone the Rc to avoid borrow issues
            if let Some(drm_backend) = data.state.drm_backend.clone() {
                drm_backend
                    .borrow_mut()
                    .on_estimated_vblank_timer(crtc, &mut data.state);
            }
            TimeoutAction::Drop
        }) {
            Ok(token) => {
                surface.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
            }
            Err(err) => {
                warn!("Failed to insert estimated VBlank timer: {:?}", err);
                surface.redraw_state = RedrawState::Idle;
            }
        }
    }

    /// Handle estimated VBlank timer firing
    fn on_estimated_vblank_timer(&mut self, crtc: crtc::Handle, ewm: &mut Ewm) {
        let action = {
            let Some(device) = &mut self.device else {
                return;
            };
            let Some(surface) = device.surfaces.get_mut(&crtc) else {
                return;
            };

            match &surface.redraw_state {
                RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
                    surface.redraw_state = RedrawState::Queued;
                    Some(true)
                }
                RedrawState::WaitingForEstimatedVBlank(_) => {
                    let output = surface.output.clone();
                    for window in ewm.space.elements() {
                        window.send_frame(&output, Duration::ZERO, None, |_, _| Some(output.clone()));
                    }
                    surface.redraw_state = RedrawState::Idle;
                    Some(false)
                }
                other => {
                    debug!("Unexpected state in on_estimated_vblank_timer: {:?}", other);
                    None
                }
            }
        };

        if action == Some(true) {
            self.render_output(crtc, ewm);
        }
    }

    /// Process all outputs that have queued redraws
    pub(crate) fn redraw_queued_outputs(&mut self, ewm: &mut Ewm) {
        let Some(device) = &self.device else {
            return;
        };
        let queued_crtcs: Vec<crtc::Handle> = device
            .surfaces
            .iter()
            .filter(|(_, s)| matches!(s.redraw_state, RedrawState::Queued))
            .map(|(crtc, _)| *crtc)
            .collect();

        for crtc in queued_crtcs {
            self.render_output(crtc, ewm);
        }
    }

    /// Handle udev device change event (monitor hotplug)
    pub fn on_device_changed(&mut self, ewm: &mut Ewm) {
        if self.paused {
            return;
        }

        let Some(device) = &mut self.device else {
            return;
        };

        // Scan for connector changes
        let scan_result = match device.drm_scanner.scan_connectors(&device.drm) {
            Ok(x) => x,
            Err(err) => {
                warn!("Error scanning connectors: {:?}", err);
                return;
            }
        };

        let mut added = Vec::new();
        let mut removed = Vec::new();

        for event in scan_result {
            match event {
                DrmScanEvent::Connected { connector, crtc: Some(crtc) } => {
                    info!("Connector connected: {:?}-{}", connector.interface(), connector.interface_id());
                    added.push((connector, crtc));
                }
                DrmScanEvent::Connected { connector, crtc: None } => {
                    warn!("Connector {:?}-{} has no available CRTC", connector.interface(), connector.interface_id());
                }
                DrmScanEvent::Disconnected { connector, crtc: Some(crtc) } => {
                    info!("Connector disconnected: {:?}-{}", connector.interface(), connector.interface_id());
                    removed.push(crtc);
                }
                DrmScanEvent::Disconnected { connector, crtc: None } => {
                    debug!("Connector {:?}-{} disconnected (had no CRTC)", connector.interface(), connector.interface_id());
                }
            }
        }

        // Process disconnections first
        for crtc in removed {
            self.disconnect_output(crtc, ewm);
        }

        // Process new connections
        for (connector, crtc) in added {
            if let Err(err) = self.connect_output(connector, crtc, ewm) {
                warn!("Failed to connect output: {:?}", err);
            }
        }
    }

    /// Connect a new output
    fn connect_output(
        &mut self,
        connector: connector::Info,
        crtc: crtc::Handle,
        ewm: &mut Ewm,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(device) = &mut self.device else {
            return Err("DRM device not initialized".into());
        };

        let Some(display_handle) = &self.display_handle else {
            return Err("Display handle not available".into());
        };

        // Find preferred mode
        let mode = connector
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| connector.modes().first())
            .copied()
            .ok_or("No mode available")?;

        info!(
            "Connecting display: {:?}-{} {}x{}@{}Hz",
            connector.interface(),
            connector.interface_id(),
            mode.size().0,
            mode.size().1,
            mode.vrefresh()
        );

        // Create DRM surface
        let drm_surface = device.drm.create_surface(crtc, mode, &[connector.handle()])?;

        // Create allocator
        let gbm_flags = GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT;
        let allocator = GbmAllocator::new(device.gbm.clone(), gbm_flags);

        // Get render formats from GPU manager
        let renderer = device.gpu_manager.single_renderer(&device.render_node)?;
        let raw_render_formats = renderer.as_ref().egl_context().dmabuf_render_formats();

        // Filter out problematic modifiers
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
        let connector_name = format!(
            "{:?}-{}",
            connector.interface(),
            connector.interface_id()
        );
        let output = Output::new(
            connector_name.clone(),
            PhysicalProperties {
                size: connector
                    .size()
                    .map(|(w, h)| (w as i32, h as i32).into())
                    .unwrap_or_default(),
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

        // Create DrmCompositor
        let cursor_size = device.drm.cursor_size();
        let compositor = match DrmCompositor::new(
            OutputModeSource::Auto(output.clone()),
            drm_surface,
            None,
            allocator.clone(),
            device.gbm.clone(),
            SUPPORTED_COLOR_FORMATS,
            render_formats.clone(),
            cursor_size,
            Some(device.gbm.clone()),
        ) {
            Ok(c) => c,
            Err(err) => {
                warn!(
                    "Error creating DRM compositor, trying with Invalid modifier: {:?}",
                    err
                );

                let fallback_formats: FormatSet = render_formats
                    .iter()
                    .copied()
                    .filter(|format| format.modifier == Modifier::Invalid)
                    .collect();

                let drm_surface = device.drm.create_surface(crtc, mode, &[connector.handle()])?;

                DrmCompositor::new(
                    OutputModeSource::Auto(output.clone()),
                    drm_surface,
                    None,
                    allocator,
                    device.gbm.clone(),
                    SUPPORTED_COLOR_FORMATS,
                    fallback_formats,
                    cursor_size,
                    Some(device.gbm.clone()),
                )?
            }
        };

        info!("DrmCompositor created for {}", connector_name);

        let refresh_interval_us = if mode.vrefresh() > 0 {
            1_000_000 / mode.vrefresh() as u64
        } else {
            16_667
        };

        // Calculate position: place after existing outputs
        let x_offset = ewm.output_size.0;

        device.surfaces.insert(
            crtc,
            OutputSurface {
                output: output.clone(),
                compositor,
                redraw_state: RedrawState::Queued,
                refresh_interval_us,
                connector: connector.handle(),
            },
        );

        // Position this output horizontally after the previous ones
        ewm.space.map_output(&output, (x_offset, 0));
        info!(
            "Mapped output {} at position ({}, 0), size {}x{}",
            connector_name, x_offset, mode.size().0, mode.size().1
        );

        // Collect output info for IPC
        let physical_size = connector.size().unwrap_or((0, 0));
        let output_modes: Vec<OutputMode> = connector
            .modes()
            .iter()
            .map(|m| OutputMode {
                width: m.size().0 as i32,
                height: m.size().1 as i32,
                refresh: (m.vrefresh() * 1000) as i32,
                preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
            })
            .collect();

        let output_info = OutputInfo {
            name: connector_name.clone(),
            make: "Unknown".to_string(),
            model: "Unknown".to_string(),
            width_mm: physical_size.0 as i32,
            height_mm: physical_size.1 as i32,
            x: x_offset,
            y: 0,
            modes: output_modes,
        };

        ewm.outputs.push(output_info.clone());

        // Recalculate output_size
        self.recalculate_output_size(ewm);

        // Send IPC event
        ewm.send_output_detected(output_info);

        info!("Output connected: {}", connector_name);

        Ok(())
    }

    /// Disconnect an output
    fn disconnect_output(&mut self, crtc: crtc::Handle, ewm: &mut Ewm) {
        let Some(device) = &mut self.device else {
            return;
        };

        let Some(surface) = device.surfaces.remove(&crtc) else {
            return;
        };

        // Cancel pending timers
        if let RedrawState::WaitingForEstimatedVBlank(token)
            | RedrawState::WaitingForEstimatedVBlankAndQueued(token) = surface.redraw_state
        {
            if let Some(ref handle) = self.loop_handle {
                handle.remove(token);
            }
        }

        let output_name = surface.output.name();

        // Unmap from space
        ewm.space.unmap_output(&surface.output);

        // Remove from outputs list
        ewm.outputs.retain(|o| o.name != output_name);

        // Recalculate output_size
        self.recalculate_output_size(ewm);

        // Send IPC event
        ewm.send_output_disconnected(&output_name);

        info!("Output disconnected: {}", output_name);
    }

    /// Recalculate total output size from current surfaces
    fn recalculate_output_size(&self, ewm: &mut Ewm) {
        let (total_width, max_height) = ewm.space.outputs().fold((0i32, 0i32), |(w, h), output| {
            if let Some(geo) = ewm.space.output_geometry(output) {
                (w.max(geo.loc.x + geo.size.w), h.max(geo.size.h))
            } else {
                (w, h)
            }
        });
        ewm.output_size = (total_width, max_height);
        info!("Total output area: {}x{}", total_width, max_height);
    }
}

/// Initialize DRM device and set up outputs
fn initialize_drm(
    backend_state: &std::rc::Rc<std::cell::RefCell<DrmBackendState>>,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
    ewm_state: &mut Ewm,
    event_loop_handle: &LoopHandle<'static, LoopData>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut backend = backend_state.borrow_mut();

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

    if let Err(err) = drm.activate(true) {
        warn!("Failed to activate DRM device (acquire master): {:?}", err);
    } else {
        info!("DRM device activated, is_active: {}", drm.is_active());
    }

    // Create EGL display to get render node
    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_device = EGLDevice::device_for_display(&egl_display)?;
    let render_node = egl_device
        .try_get_render_node()?
        .ok_or("No render node found")?;
    info!("Render node: {:?}", render_node);

    // Create GPU manager
    let api: GbmGlesBackend<GlesRenderer, DrmDeviceFd> =
        GbmGlesBackend::with_context_priority(smithay::backend::egl::context::ContextPriority::High);
    let mut gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>> = GpuManager::new(api)?;
    gpu_manager.as_mut().add_node(render_node, gbm.clone())?;

    // Bind renderer to Wayland display
    {
        let mut renderer = gpu_manager.single_renderer(&render_node)?;
        if let Err(err) = renderer.bind_wl_display(display_handle) {
            warn!("Error binding wl-display in EGL: {:?}", err);
        } else {
            info!("Renderer bound to Wayland display");
        }

        // Create dmabuf global
        let dmabuf_formats = renderer.dmabuf_formats().clone();
        if let Ok(default_feedback) =
            DmabufFeedbackBuilder::new(render_node.dev_id(), dmabuf_formats).build()
        {
            let _global = ewm_state
                .dmabuf_state
                .create_global_with_default_feedback::<Ewm>(display_handle, &default_feedback);
            info!("Dmabuf global created");
        }
    }

    // Store display handle for hotplug
    backend.display_handle = Some(display_handle.clone());

    let mut surfaces = HashMap::new();
    let mut x_offset = 0i32;  // Track horizontal position for output placement

    // Create DrmScanner for connector management (initial scan and hotplug)
    let mut drm_scanner = DrmScanner::new();

    // Initial connector scan
    let scan_result = drm_scanner.scan_connectors(&drm)?;
    for event in scan_result {
        let (connector, crtc) = match event {
            DrmScanEvent::Connected { connector, crtc: Some(crtc) } => (connector, crtc),
            DrmScanEvent::Connected { connector, crtc: None } => {
                warn!(
                    "No available CRTC for connector {:?}-{}",
                    connector.interface(),
                    connector.interface_id()
                );
                continue;
            }
            DrmScanEvent::Disconnected { .. } => continue, // Skip disconnects on initial scan
        };

        let connector_info = &connector;

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
        let drm_surface = drm.create_surface(crtc, mode, &[connector.handle()])?;

        // Create allocator
        let gbm_flags = GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT;
        let allocator = GbmAllocator::new(gbm.clone(), gbm_flags);

        // Get render formats from GPU manager
        let renderer = gpu_manager.single_renderer(&render_node)?;
        let raw_render_formats = renderer.as_ref().egl_context().dmabuf_render_formats();

        // Filter out problematic modifiers
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
        let connector_name = format!(
            "{:?}-{}",
            connector_info.interface(),
            connector_info.interface_id()
        );
        let output = Output::new(
            connector_name.clone(),
            PhysicalProperties {
                size: connector_info
                    .size()
                    .map(|(w, h)| (w as i32, h as i32).into())
                    .unwrap_or_default(),
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

        // Create DrmCompositor
        let cursor_size = drm.cursor_size();
        let compositor = match DrmCompositor::new(
            OutputModeSource::Auto(output.clone()),
            drm_surface,
            None,
            allocator.clone(),
            gbm.clone(),
            SUPPORTED_COLOR_FORMATS,
            render_formats.clone(),
            cursor_size,
            Some(gbm.clone()),
        ) {
            Ok(c) => c,
            Err(err) => {
                warn!(
                    "Error creating DRM compositor, trying with Invalid modifier: {:?}",
                    err
                );

                let fallback_formats: FormatSet = render_formats
                    .iter()
                    .copied()
                    .filter(|format| format.modifier == Modifier::Invalid)
                    .collect();

                let drm_surface = drm.create_surface(crtc, mode, &[connector.handle()])?;

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

        let refresh_interval_us = if mode.vrefresh() > 0 {
            1_000_000 / mode.vrefresh() as u64
        } else {
            16_667
        };

        surfaces.insert(
            crtc,
            OutputSurface {
                output: output.clone(),
                compositor,
                redraw_state: RedrawState::Queued,
                refresh_interval_us,
                connector: connector.handle(),
            },
        );

        // Position this output horizontally after the previous ones
        ewm_state.space.map_output(&output, (x_offset, 0));
        info!(
            "Mapped output {} at position ({}, 0), size {}x{}",
            connector_name, x_offset, mode.size().0, mode.size().1
        );

        // Collect output info for IPC
        let physical_size = connector_info.size().unwrap_or((0, 0));
        let output_modes: Vec<OutputMode> = connector_info
            .modes()
            .iter()
            .map(|m| OutputMode {
                width: m.size().0 as i32,
                height: m.size().1 as i32,
                refresh: (m.vrefresh() * 1000) as i32,
                preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
            })
            .collect();

        ewm_state.outputs.push(OutputInfo {
            name: connector_name.clone(),
            make: "Unknown".to_string(),  // EDID parsing would be needed for real values
            model: "Unknown".to_string(),
            width_mm: physical_size.0 as i32,
            height_mm: physical_size.1 as i32,
            x: x_offset,
            y: 0,
            modes: output_modes,
        });

        // Update x_offset for next output
        x_offset += mode.size().0 as i32;
    }

    // Update output_size to total bounding box (all outputs combined horizontally)
    // For now, use the rightmost edge as width, and max height
    let (total_width, max_height) = surfaces.values().fold((0i32, 0i32), |(w, h), surface| {
        let output_geo = ewm_state.space.output_geometry(&surface.output);
        if let Some(geo) = output_geo {
            (w.max(geo.loc.x + geo.size.w), h.max(geo.size.h))
        } else {
            (w, h)
        }
    });
    ewm_state.output_size = (total_width, max_height);
    info!(
        "Total output area: {}x{} ({} outputs)",
        total_width,
        max_height,
        surfaces.len()
    );

    // Store device state
    backend.device = Some(DrmDeviceState {
        drm,
        drm_scanner,
        gbm,
        gpu_manager,
        render_node,
        surfaces,
    });

    drop(backend);

    // Register DRM event notifier for VBlank
    let backend_for_vblank = backend_state.clone();
    event_loop_handle.insert_source(drm_notifier, move |event, _, data| {
        if let DrmEvent::VBlank(crtc) = event {
            let mut backend = backend_for_vblank.borrow_mut();

            let mut should_render = false;
            if let Some(device) = &mut backend.device {
                if let Some(surface) = device.surfaces.get_mut(&crtc) {
                    match surface.compositor.frame_submitted() {
                        Ok(_) => {}
                        Err(err) => {
                            warn!("Error marking frame as submitted: {:?}", err);
                        }
                    }

                    match surface.redraw_state {
                        RedrawState::WaitingForVBlank { redraw_needed } => {
                            if redraw_needed {
                                surface.redraw_state = RedrawState::Queued;
                                should_render = true;
                            } else {
                                surface.redraw_state = RedrawState::Idle;
                            }
                        }
                        _ => {
                            debug!("VBlank received in unexpected state: {:?}", surface.redraw_state);
                        }
                    }
                }
            }

            if should_render {
                drop(backend);
                backend_for_vblank.borrow_mut().render_output(crtc, &mut data.state);
            }
        }
    })?;

    info!("DRM initialization complete");

    // Trigger initial render
    {
        let backend = backend_state.borrow_mut();
        if let Some(device) = &backend.device {
            let crtcs: Vec<_> = device.surfaces.keys().copied().collect();
            drop(backend);
            for crtc in crtcs {
                backend_state.borrow_mut().render_output(crtc, ewm_state);
            }
        }
    }

    Ok(())
}

/// Run EWM with DRM/libinput backend (standalone session)
pub fn run_drm(program: String, program_args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting EWM with DRM backend");

    // Initialize libseat session
    let (session, notifier) = LibSeatSession::new().map_err(|e| {
        format!(
            "Failed to create libseat session: {}. Are you running from a TTY?",
            e
        )
    })?;
    let seat_name = session.seat();
    info!("libseat session opened, seat: {}", seat_name);

    let session_active = session.is_active();
    info!("Session active at startup: {}", session_active);

    // Create event loop and Wayland display
    let mut event_loop: EventLoop<LoopData> = EventLoop::try_new()?;
    let mut display: Display<Ewm> = Display::new()?;
    let display_handle = display.handle();

    // Initialize Wayland socket
    let socket_name = Ewm::init_wayland_listener(&mut display, &event_loop.handle())?;
    let socket_name_str = socket_name.to_string_lossy().to_string();
    info!("Wayland socket: {:?}", socket_name);

    let state = Ewm::new(display_handle.clone());
    let mut data = LoopData {
        state,
        display,
        emacs: None,
    };

    // Find primary GPU
    let gpu_path = primary_gpu(&seat_name)?.ok_or("No GPU found")?;
    info!("Primary GPU: {:?}", gpu_path);

    // Initialize libinput
    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|()| "Failed to assign seat to libinput")?;

    // Create channel for deferred DRM initialization
    let (init_sender, init_receiver) = channel::<DrmMessage>();

    // Create backend state
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
        loop_handle: None,
        cursor_buffer: CursorBuffer::new(),
        display_handle: None, // Set during initialize_drm
    }));

    backend_state.borrow_mut().loop_handle = Some(event_loop.handle());
    data.state.set_drm_backend(backend_state.clone());

    // Initialize PipeWire for screen sharing
    #[cfg(feature = "screencast")]
    {
        use crate::pipewire::PipeWire;
        match PipeWire::new(&event_loop.handle(), || {
            tracing::warn!("PipeWire fatal error");
        }) {
            Ok(pw) => {
                tracing::info!("PipeWire initialized successfully");
                data.state.pipewire = Some(pw);
            }
            Err(err) => {
                tracing::warn!("PipeWire initialization failed: {err:?}");
            }
        }
    }

    // Register session notifier
    let backend_for_session = backend_state.clone();
    event_loop
        .handle()
        .insert_source(notifier, move |event, _, _| match event {
            SessionEvent::PauseSession => {
                info!("Session paused (VT switch away)");
                backend_for_session.borrow_mut().pause();
            }
            SessionEvent::ActivateSession => {
                info!("Session activated");
                let backend = backend_for_session.borrow_mut();
                if backend.device.is_none() {
                    info!("First session activation - triggering DRM init");
                    backend.trigger_init();
                } else {
                    drop(backend);
                    backend_for_session.borrow_mut().resume();
                }
            }
        })?;

    // Register UdevBackend for hotplug detection
    let udev_backend = UdevBackend::new(&seat_name)?;
    let backend_for_udev = backend_state.clone();
    event_loop.handle().insert_source(udev_backend, move |event, _, data| {
        match event {
            UdevEvent::Changed { device_id: _ } => {
                // Scan for connector changes
                backend_for_udev.borrow_mut().on_device_changed(&mut data.state);
                // Queue redraws after hotplug
                if let Some(ref backend) = data.state.drm_backend {
                    backend.borrow_mut().queue_redraw();
                }
            }
            UdevEvent::Added { device_id, path } => {
                debug!("UDev device added: {:?} at {:?}", device_id, path);
            }
            UdevEvent::Removed { device_id } => {
                debug!("UDev device removed: {:?}", device_id);
            }
        }
    })?;

    // Register channel receiver for deferred DRM initialization
    let backend_for_init = backend_state.clone();
    let display_handle_for_init = display_handle.clone();
    let event_loop_handle = event_loop.handle();
    event_loop
        .handle()
        .insert_source(init_receiver, move |event, _, data| {
            if let smithay::reexports::calloop::channel::Event::Msg(DrmMessage::InitializeDrm) = event
            {
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

    // Register libinput with event loop
    let libinput_backend = LibinputInputBackend::new(libinput);
    event_loop
        .handle()
        .insert_source(libinput_backend, move |event, _, data| {
            match event {
                InputEvent::Keyboard { event: kb_event } => {
                    let keyboard = data.state.seat.get_keyboard().unwrap();
                    let action = handle_keyboard_event(
                        &mut data.state,
                        &keyboard,
                        kb_event.key_code().into(),
                        kb_event.state(),
                        Event::time_msec(&kb_event),
                    );

                    if action == KeyboardAction::Shutdown {
                        info!("Kill combo pressed, shutting down");
                        data.state.running = false;
                    }
                }
                InputEvent::PointerMotion { event } => {
                    // Relative pointer motion (from mice)
                    let (current_x, current_y) = data.state.pointer_location;
                    let delta = event.delta();
                    let (output_w, output_h) = data.state.output_size;

                    // Calculate new position, clamped to output bounds
                    let new_x = (current_x + delta.x).clamp(0.0, output_w as f64);
                    let new_y = (current_y + delta.y).clamp(0.0, output_h as f64);
                    data.state.pointer_location = (new_x, new_y);

                    let pointer = data.state.seat.get_pointer().unwrap();
                    let serial = SERIAL_COUNTER.next_serial();

                    // Find surface under pointer
                    let under = data
                        .state
                        .space
                        .element_under((new_x, new_y))
                        .and_then(|(window, loc)| {
                            window
                                .wl_surface()
                                .map(|s| (s.into_owned(), Point::from((loc.x as f64, loc.y as f64))))
                        });

                    pointer.motion(
                        &mut data.state,
                        under.clone(),
                        &MotionEvent {
                            location: (new_x, new_y).into(),
                            serial,
                            time: event.time_msec(),
                        },
                    );

                    // Send relative motion event (needed by some games/apps)
                    pointer.relative_motion(
                        &mut data.state,
                        under,
                        &RelativeMotionEvent {
                            delta: event.delta(),
                            delta_unaccel: event.delta_unaccel(),
                            utime: event.time(),
                        },
                    );

                    pointer.frame(&mut data.state);

                    // Queue redraw to update cursor position
                    if let Some(ref backend) = data.state.drm_backend {
                        backend.borrow_mut().queue_redraw();
                    }
                }
                InputEvent::PointerMotionAbsolute { event } => {
                    // Absolute pointer motion (from touchpads in absolute mode, tablets)
                    let (output_w, output_h) = data.state.output_size;
                    let pos = event.position_transformed((output_w, output_h).into());
                    data.state.pointer_location = (pos.x, pos.y);

                    let pointer = data.state.seat.get_pointer().unwrap();
                    let serial = SERIAL_COUNTER.next_serial();

                    // Find surface under pointer
                    let under = data
                        .state
                        .space
                        .element_under((pos.x, pos.y))
                        .and_then(|(window, loc)| {
                            window
                                .wl_surface()
                                .map(|s| (s.into_owned(), Point::from((loc.x as f64, loc.y as f64))))
                        });

                    pointer.motion(
                        &mut data.state,
                        under,
                        &MotionEvent {
                            location: pos,
                            serial,
                            time: event.time_msec(),
                        },
                    );
                    pointer.frame(&mut data.state);

                    // Queue redraw to update cursor position
                    if let Some(ref backend) = data.state.drm_backend {
                        backend.borrow_mut().queue_redraw();
                    }
                }
                InputEvent::PointerButton { event } => {
                    let pointer = data.state.seat.get_pointer().unwrap();
                    let keyboard = data.state.seat.get_keyboard().unwrap();
                    let serial = SERIAL_COUNTER.next_serial();

                    let button_state = match event.state() {
                        ButtonState::Pressed => ButtonState::Pressed,
                        ButtonState::Released => ButtonState::Released,
                    };

                    // Click-to-focus: on button press, focus the surface under pointer
                    if button_state == ButtonState::Pressed {
                        let (px, py) = data.state.pointer_location;
                        // Get surface info before mutating state
                        let focus_info = data
                            .state
                            .space
                            .element_under((px, py))
                            .and_then(|(window, _)| {
                                let id = data.state.window_ids.get(&window).copied()?;
                                let surface = window.wl_surface()?.into_owned();
                                Some((id, surface))
                            });

                        if let Some((id, surface)) = focus_info {
                            data.state.set_focus(id);
                            data.state.keyboard_focus = Some(surface.clone());
                            keyboard.set_focus(&mut data.state, Some(surface), serial);
                        }
                    }

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

                    // Get scroll amounts - try continuous first, then discrete v120
                    // Negate for natural scrolling (content follows finger direction)
                    let horizontal_amount = event.amount(Axis::Horizontal);
                    let vertical_amount = event.amount(Axis::Vertical);

                    let horizontal = -horizontal_amount
                        .or_else(|| event.amount_v120(Axis::Horizontal).map(|v| v / 120.0 * 15.0))
                        .unwrap_or(0.0);
                    let vertical = -vertical_amount
                        .or_else(|| event.amount_v120(Axis::Vertical).map(|v| v / 120.0 * 15.0))
                        .unwrap_or(0.0);

                    let mut frame = AxisFrame::new(event.time_msec()).source(source);
                    if horizontal != 0.0 {
                        frame = frame.value(Axis::Horizontal, horizontal);
                    }
                    if vertical != 0.0 {
                        frame = frame.value(Axis::Vertical, vertical);
                    }

                    // For finger scroll (touchpad), send stop events when scrolling ends
                    // (libinput sends a final event with amount == Some(0.0))
                    if source == AxisSource::Finger {
                        if horizontal_amount == Some(0.0) {
                            frame = frame.stop(Axis::Horizontal);
                        }
                        if vertical_amount == Some(0.0) {
                            frame = frame.stop(Axis::Vertical);
                        }
                    }

                    pointer.axis(&mut data.state, frame);
                    pointer.frame(&mut data.state);
                }
                _ => {}
            }
        })?;

    // Set up IPC listener (shared code)
    setup_ipc_listener(&event_loop.handle())?;

    info!("EWM DRM backend started (waiting for session activation)");
    info!("VT switching: Ctrl+Alt+F1-F7");
    info!("Kill combo: Super+Ctrl+Backspace");

    // If session is already active, initialize DRM immediately
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

    // Spawn client
    let client_process: std::rc::Rc<std::cell::RefCell<Option<std::process::Child>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    info!("Spawning client...");
    match spawn_client(&program, &program_args, &socket_name_str) {
        Ok(child) => {
            let pid = child.id();
            info!("Client spawned with PID {}", pid);
            data.state.set_emacs_pid(pid);
            *client_process.borrow_mut() = Some(child);
        }
        Err(e) => {
            warn!("Failed to spawn client: {:?}", e);
        }
    }

    // Main loop
    while data.state.running {
        if let Some(ref mut child) = *client_process.borrow_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                info!("Client exited with status: {}", status);
                break;
            }
        }

        event_loop.dispatch(None, &mut data)?;
        data.display.flush_clients().unwrap();

        // Process any queued redraws after event dispatch
        backend_state.borrow_mut().redraw_queued_outputs(&mut data.state);

        // Flush pending events to Emacs
        data.flush_events();
    }

    info!("EWM DRM backend shutting down");
    Ok(())
}
