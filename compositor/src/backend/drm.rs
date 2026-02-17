//! DRM/libinput backend for running EWM as a standalone Wayland session
//!
//! This module provides the backend for running directly on hardware without
//! another compositor (like running from a TTY).
//!
//! # Design Invariants
//!
//! 1. **Deferred DRM initialization**: DRM master can only be acquired when the
//!    session is active. Session activation happens asynchronously via libseat,
//!    so we defer all DRM operations until we receive an ActivateSession event.
//!
//! 2. **Field ordering for Drop**: The order of fields in DrmBackendState and
//!    DrmDeviceState is critical. Surfaces must be dropped before drm/gbm to
//!    avoid use-after-free. See https://github.com/Smithay/smithay/issues/1102
//!
//! 3. **Session notifier cleanup**: The session notifier must be removed from the
//!    event loop BEFORE the session is dropped. This is essential for embedded
//!    mode where process exit doesn't clean up resources automatically.
//!
//! 4. **Per-output rendering**: Each output has independent redraw state and
//!    VBlank synchronization. Outputs never share frame timing.

use std::collections::HashMap;

use crate::tracy_span;
use tracing::{debug, error, info, trace, warn};
use std::path::PathBuf;
use std::time::Duration;

#[cfg(feature = "screencast")]
use smithay::utils::Size;
use smithay::{
    backend::{
        allocator::{
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Modifier,
        },
        drm::{
            compositor::{DrmCompositor, FrameFlags, PrimaryPlaneElement},
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode,
        },
        egl::{EGLDevice, EGLDisplay},
        input::{Event, InputEvent, KeyboardKeyEvent},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            gles::GlesRenderer,
            multigpu::{gbm::GbmGlesBackend, GpuManager},
            ImportDma, ImportEgl,
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{primary_gpu, UdevBackend, UdevEvent},
    },
    output::{Mode, Output, OutputModeSource, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            channel::{channel, Sender},
            timer::{TimeoutAction, Timer},
            EventLoop, LoopHandle, RegistrationToken,
        },
        drm::control::{
            connector, crtc, Device as ControlDevice, Mode as DrmMode, ModeTypeFlags,
        },
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::{protocol::wl_surface::WlSurface, Display, DisplayHandle},
    },
    utils::{DeviceFd, Scale, Transform},
    wayland::dmabuf::DmabufFeedbackBuilder,
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::{
    backend::Backend,
    cursor::CursorBuffer,
    input::{handle_device_added, handle_keyboard_event, KeyboardAction},
    render::{collect_render_elements_for_output, process_screencopies_for_output},
    Ewm, OutputInfo, OutputMode, OutputState, RedrawState, State,
};


const SUPPORTED_COLOR_FORMATS: [smithay::backend::allocator::Fourcc; 4] = [
    smithay::backend::allocator::Fourcc::Xrgb8888,
    smithay::backend::allocator::Fourcc::Xbgr8888,
    smithay::backend::allocator::Fourcc::Argb8888,
    smithay::backend::allocator::Fourcc::Abgr8888,
];

/// Find a DRM mode matching the requested resolution and optional refresh rate.
///
/// `refresh_mhz`: target refresh rate in millihertz (e.g., 60000 for 60Hz).
/// When multiple modes match the resolution, prefers the one closest to the
/// target refresh rate (within 2Hz tolerance), otherwise picks the highest.
fn resolve_drm_mode(
    modes: &[DrmMode],
    width: i32,
    height: i32,
    refresh_mhz: Option<i32>,
) -> Option<DrmMode> {
    modes
        .iter()
        .filter(|m| m.size().0 as i32 == width && m.size().1 as i32 == height)
        .max_by_key(|m| {
            let mode_mhz = (m.vrefresh() * 1000) as i32;
            if let Some(target) = refresh_mhz {
                // Within 2Hz tolerance = 2000 mHz
                if (mode_mhz - target).abs() < 2000 {
                    return 1_000_000 + mode_mhz;
                }
            }
            mode_mhz
        })
        .copied()
}

/// Find the preferred DRM mode, falling back to the first available.
fn preferred_drm_mode(modes: &[DrmMode]) -> Option<DrmMode> {
    modes
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| modes.first())
        .copied()
}

/// Type alias for our DRM compositor
type GbmDrmCompositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmDevice<DrmDeviceFd>, (), DrmDeviceFd>;

/// Per-output surface state (DRM-specific, redraw state is in Ewm::output_state)
struct OutputSurface {
    output: Output,
    compositor: GbmDrmCompositor,
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
///
/// Field order is critical for safe Drop: surfaces must be dropped before drm/gbm.
/// See https://github.com/Smithay/smithay/issues/1102
#[allow(dead_code)]
struct DrmDeviceState {
    render_node: DrmNode,
    drm_scanner: DrmScanner,
    gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    surfaces: HashMap<crtc::Handle, OutputSurface>,
    // SAFETY: drm and gbm must be dropped after surfaces
    drm: DrmDevice,
    gbm: GbmDevice<DrmDeviceFd>,
}

/// Marker type for DRM backend (used in Backend enum)
#[allow(dead_code)]
pub struct DrmBackend;

/// Shared DRM backend state
///
/// Field order matters for Drop: device must drop before session.
/// See https://github.com/Smithay/smithay/issues/1102
///
/// IMPORTANT: We implement Drop to remove the session notifier from the event
/// loop BEFORE the session is dropped. The notifier holds references to session
/// internals that become invalid after session drop. This is critical for
/// embedded mode where process exit doesn't clean up resources.
#[allow(dead_code)]
pub struct DrmBackendState {
    /// Channel to trigger deferred initialization
    init_sender: Option<Sender<DrmMessage>>,
    /// Event loop handle for scheduling timers
    loop_handle: Option<LoopHandle<'static, State>>,
    /// Cursor buffer for rendering the mouse cursor
    cursor_buffer: CursorBuffer,
    /// Display handle for creating output globals on hotplug
    display_handle: Option<DisplayHandle>,
    /// Pending initialization data - Some until DRM is initialized
    pending: Option<DrmPendingInit>,
    paused: bool,
    /// Token for session notifier - must be removed before session drops
    session_notifier_token: Option<RegistrationToken>,
    // SAFETY: Fields below are dropped in declaration order.
    // device must drop before session (surfaces → drm → libseat).
    // See https://github.com/Smithay/smithay/issues/1102
    device: Option<DrmDeviceState>,
    libinput: Libinput,
    session: Option<LibSeatSession>,
}

impl Drop for DrmBackendState {
    fn drop(&mut self) {
        // CRITICAL: Remove session notifier from event loop BEFORE session is dropped.
        // The notifier holds references to session internals that become invalid after
        // session drop. This is essential for embedded mode where process exit doesn't
        // clean up resources automatically.
        if let (Some(handle), Some(token)) = (&self.loop_handle, self.session_notifier_token.take())
        {
            info!("Removing session notifier from event loop before session drop");
            handle.remove(token);
        }
        info!("DrmBackendState dropping - session will be released");
        // After this, fields drop in declaration order: device → libinput → session
    }
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

    /// Get the GBM device for screencasting (if DRM is initialized)
    #[cfg(feature = "screencast")]
    pub fn gbm_device(&self) -> Option<GbmDevice<DrmDeviceFd>> {
        self.device.as_ref().map(|d| d.gbm.clone())
    }

    /// Check if any output has a redraw queued (checks Ewm output_state)
    pub fn has_queued_redraws(&self, ewm: &Ewm) -> bool {
        ewm.output_state
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
        // Early import for DMA-BUF surfaces (errors are expected for SHM surfaces)
        let _ = device.gpu_manager.early_import(device.render_node, surface);
    }

    /// Handle session pause (VT switch away)
    pub(crate) fn pause(&mut self, ewm: &mut Ewm) {
        debug!("Pausing DRM session");
        self.libinput.suspend();
        if let Some(device) = &mut self.device {
            device.drm.pause();
            // Cancel any pending estimated VBlank timers and reset states to Idle
            for surface in device.surfaces.values() {
                if let Some(output_state) = ewm.output_state.get_mut(&surface.output) {
                    if let RedrawState::WaitingForEstimatedVBlank(token)
                    | RedrawState::WaitingForEstimatedVBlankAndQueued(token) =
                        output_state.redraw_state
                    {
                        if let Some(ref handle) = self.loop_handle {
                            handle.remove(token);
                        }
                    }
                    output_state.redraw_state = RedrawState::Idle;
                }
            }
        }
        self.paused = true;
    }

    /// Handle session resume (VT switch back)
    pub(crate) fn resume(&mut self, ewm: &mut Ewm) {
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
            ewm.queue_redraw_all();
        }
    }

    /// Trigger deferred DRM initialization (called when session becomes active)
    pub(crate) fn trigger_init(&self) {
        if let Some(sender) = &self.init_sender {
            if let Err(e) = sender.send(DrmMessage::InitializeDrm) {
                warn!("Failed to send DRM init message: {:?}", e);
            }
        }
    }

    /// Change to a different VT (virtual terminal)
    /// This is used for Ctrl+Alt+F1-F12 VT switching.
    pub fn change_vt(&mut self, vt: i32) {
        debug!(
            "change_vt called with vt={}, session={:?}",
            vt,
            self.session.is_some()
        );
        if let Some(ref mut session) = self.session {
            info!("Switching to VT {}", vt);
            if let Err(err) = session.change_vt(vt) {
                warn!("Error changing VT to {}: {}", vt, err);
            }
        } else {
            warn!("Cannot change VT: no session");
        }
    }
}

impl DrmBackendState {
    /// Apply output configuration for a live output.
    ///
    /// Resolves the final state for mode, scale, transform, and position,
    /// then applies everything in one pass. Updates all bookkeeping:
    /// OutputInfo, D-Bus outputs, refresh interval, working areas.
    pub fn apply_output_config(&mut self, ewm: &mut Ewm, output_name: &str) {
        let config = match ewm.output_config.get(output_name) {
            Some(c) => c.clone(),
            None => return,
        };

        let output = ewm
            .space
            .outputs()
            .find(|o| o.name() == output_name)
            .cloned();
        let Some(output) = output else {
            warn!("apply_output_config: output not found: {}", output_name);
            return;
        };

        // Handle disabled output
        if !config.enabled {
            ewm.space.unmap_output(&output);
            info!("Disabled output {}", output_name);
            ewm.queue_redraw_all();
            return;
        }

        let Some(device) = &mut self.device else {
            warn!("DRM not initialized, cannot apply config");
            return;
        };

        // Find the DRM surface for this output
        let surface = device
            .surfaces
            .values_mut()
            .find(|s| s.output.name() == output_name);
        let Some(surface) = surface else {
            warn!("No DRM surface for output: {}", output_name);
            return;
        };

        // --- Resolve and apply DRM mode ---
        let new_drm_mode = if let Some((w, h, refresh)) = config.mode {
            let connector_info = match device.drm.get_connector(surface.connector, false) {
                Ok(info) => info,
                Err(e) => {
                    warn!(
                        "Failed to get connector info for {}: {:?}",
                        output_name, e
                    );
                    return;
                }
            };
            let resolved = resolve_drm_mode(connector_info.modes(), w, h, refresh);
            if resolved.is_none() {
                warn!(
                    "Configured mode {}x{} not found for {}, keeping current",
                    w, h, output_name
                );
            }
            resolved
        } else {
            None
        };

        if let Some(drm_mode) = new_drm_mode {
            if let Err(err) = surface.compositor.use_mode(drm_mode) {
                warn!("Failed to set mode for {}: {:?}", output_name, err);
            } else {
                info!(
                    "Mode set for {}: {}x{}@{}Hz",
                    output_name,
                    drm_mode.size().0,
                    drm_mode.size().1,
                    drm_mode.vrefresh()
                );
            }
        }

        // --- Build final Smithay state and apply in one call ---
        let smithay_mode = new_drm_mode.map(|m| Mode {
            size: (m.size().0 as i32, m.size().1 as i32).into(),
            refresh: (m.vrefresh() * 1000) as i32,
        });
        let scale = config
            .scale
            .map(|s| smithay::output::Scale::Fractional(super::closest_representable_scale(s)));
        let transform = config.transform;
        let position = config.position.map(|(x, y)| (x, y).into());

        output.change_current_state(smithay_mode, transform, scale, position);
        if let Some(mode) = smithay_mode {
            output.set_preferred(mode);
        }

        // Map output at configured position
        if let Some((x, y)) = config.position {
            ewm.space.map_output(&output, (x, y));
        }

        // --- Update refresh interval ---
        if let Some(drm_mode) = new_drm_mode {
            let refresh_interval_us = if drm_mode.vrefresh() > 0 {
                1_000_000 / drm_mode.vrefresh() as u64
            } else {
                16_667
            };
            if let Some(output_state) = ewm.output_state.get_mut(&output) {
                output_state.refresh_interval_us = refresh_interval_us;
            }
        }

        // --- Update OutputInfo ---
        for out_info in &mut ewm.outputs {
            if out_info.name == output_name {
                if let Some(drm_mode) = new_drm_mode {
                    // Update mode list entries to reflect current mode dimensions
                    for mode_info in &mut out_info.modes {
                        mode_info.preferred = false;
                    }
                    // Mark the applied mode as preferred
                    if let Some(mode_info) = out_info.modes.iter_mut().find(|m| {
                        m.width == drm_mode.size().0 as i32
                            && m.height == drm_mode.size().1 as i32
                            && m.refresh == (drm_mode.vrefresh() * 1000) as i32
                    }) {
                        mode_info.preferred = true;
                    }
                }
                if let Some(scale) = config.scale {
                    out_info.scale = super::closest_representable_scale(scale);
                }
                if let Some(transform) = config.transform {
                    out_info.transform = super::transform_to_int(transform);
                }
                if let Some((x, y)) = config.position {
                    out_info.x = x;
                    out_info.y = y;
                }
            }
        }

        // --- Update D-Bus outputs ---
        #[cfg(feature = "screencast")]
        {
            let mut dbus_outputs = ewm.dbus_outputs.lock().unwrap();
            for dbus_out in dbus_outputs.iter_mut() {
                if dbus_out.name == output_name {
                    if let Some(drm_mode) = new_drm_mode {
                        dbus_out.width = drm_mode.size().0 as i32;
                        dbus_out.height = drm_mode.size().1 as i32;
                        dbus_out.refresh = drm_mode.vrefresh();
                    }
                    if let Some((x, y)) = config.position {
                        dbus_out.x = x;
                        dbus_out.y = y;
                    }
                }
            }
        }

        // --- Notify existing surfaces of scale/transform change ---
        ewm.send_scale_transform_to_output_surfaces(&output);

        // --- Resize lock buffer and reconfigure lock surface for new output size ---
        let output_size = crate::utils::output_size(&output);
        let is_locked = ewm.is_locked();
        if let Some(state) = ewm.output_state.get_mut(&output) {
            state.resize_lock_buffer((output_size.w as i32, output_size.h as i32));
            if is_locked {
                if let Some(lock_surface) = &state.lock_surface {
                    crate::configure_lock_surface(lock_surface, &output);
                }
            }
        }

        // --- Recalculate total output size, working areas, and queue redraw ---
        ewm.recalculate_output_size();
        ewm.check_working_area_change(&output);
        ewm.queue_redraw_all();

        // --- Notify Emacs of the applied config ---
        let current_mode = output.current_mode().unwrap_or(Mode {
            size: (0, 0).into(),
            refresh: 0,
        });
        let current_geo = ewm
            .space
            .output_geometry(&output)
            .unwrap_or_default();
        ewm.queue_event(crate::event::Event::OutputConfigChanged {
            name: output_name.to_string(),
            width: current_mode.size.w,
            height: current_mode.size.h,
            refresh: current_mode.refresh,
            x: current_geo.loc.x,
            y: current_geo.loc.y,
            scale: output.current_scale().fractional_scale(),
            transform: super::transform_to_int(output.current_transform()),
        });

        info!(
            "Applied config for {}: mode={:?}, scale={:?}, transform={:?}, pos={:?}",
            output_name, config.mode, config.scale, config.transform, config.position,
        );
    }

    /// Render a frame to the given output
    pub(crate) fn render_output(&mut self, crtc: crtc::Handle, ewm: &mut Ewm) {
        tracy_span!("render_output");

        // Refresh foreign toplevel state before rendering
        ewm.refresh_foreign_toplevel();

        // First pass: check if we should render and extract needed data
        let (should_render, refresh_interval_us, output, render_node) = {
            let Some(device) = &self.device else {
                return;
            };

            let Some(surface) = device.surfaces.get(&crtc) else {
                return;
            };

            // Get output state from Ewm
            let Some(output_state) = ewm.output_state.get(&surface.output) else {
                debug!("No output state for {:?}", surface.output.name());
                return;
            };

            // Only render if we're in a queued state
            let should_render = matches!(
                output_state.redraw_state,
                RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
            );
            if !should_render {
                debug!("Skipping render: state={:?}", output_state.redraw_state);
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
                output_state.refresh_interval_us,
                surface.output.clone(),
                device.render_node,
            )
        };

        if !should_render {
            return;
        }

        let output_scale = Scale::from(output.current_scale().fractional_scale());

        // Get output geometry in global space
        let output_geo = ewm.space.output_geometry(&output).unwrap_or_default();
        let output_pos = output_geo.loc;
        let output_size = output_geo.size;

        // Get a renderer from the GPU manager
        let Some(device) = &mut self.device else {
            return;
        };

        let Ok(mut renderer) = device.gpu_manager.single_renderer(&render_node) else {
            warn!("Failed to get renderer from GPU manager");
            return;
        };

        // Collect render elements for this specific output
        let elements = collect_render_elements_for_output(
            ewm,
            renderer.as_mut(),
            output_scale,
            &self.cursor_buffer,
            output_pos,
            output_size,
            true, // include_cursor
            &output,
        );

        // Frame flags for proper plane scanout
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
        let mut need_estimated_vblank = false;

        match render_result {
            Ok(result) => {
                // Wait for GPU completion if the kernel can't handle fencing.
                // Without this, we may submit a buffer before the GPU has finished
                // rendering, causing the display controller to show stale content.
                if result.needs_sync() {
                    if let PrimaryPlaneElement::Swapchain(element) = &result.primary_element {
                        if let Err(err) = element.sync.wait() {
                            warn!("error waiting for frame completion: {err:?}");
                        }
                    }
                }

                // Update primary scanout output tracking (needed for frame callback
                // throttling). Called regardless of damage, same as niri.
                ewm.update_primary_scanout_output(&output, &result.states);

                if !result.is_empty {
                    // There's damage to display - queue frame and wait for VBlank
                    match surface.compositor.queue_frame(()) {
                        Ok(()) => {
                            // Transition to WaitingForVBlank
                            let is_locked = ewm.is_locked();
                            if let Some(output_state) = ewm.output_state.get_mut(&output) {
                                // Increment sequence for frame callback throttling
                                output_state.frame_callback_sequence =
                                    output_state.frame_callback_sequence.wrapping_add(1);

                                trace!(
                                    "{}: {} -> WaitingForVBlank",
                                    output.name(),
                                    output_state.redraw_state
                                );
                                output_state.redraw_state = RedrawState::WaitingForVBlank {
                                    redraw_needed: false,
                                };
                                // Start Tracy frame tracking for VBlank interval
                                output_state.vblank_tracker.begin_frame();

                                // Mark this output as having rendered a locked frame
                                if is_locked && output_state.lock_surface.is_some() {
                                    output_state.lock_render_state = crate::LockRenderState::Locked;
                                }
                            }

                            // Check if all outputs have rendered locked frames
                            ewm.check_lock_complete();
                        }
                        Err(err) => {
                            warn!("{}: Error queueing frame: {:?}", output.name(), err);
                            if let Some(output_state) = ewm.output_state.get_mut(&output) {
                                trace!(
                                    "{}: {} -> Idle (queue_frame error)",
                                    output.name(),
                                    output_state.redraw_state
                                );
                                output_state.redraw_state = RedrawState::Idle;
                            }
                            // Abort lock if we failed to queue frame during Locking state
                            ewm.abort_lock_on_render_failure();
                        }
                    }
                } else {
                    // No damage - mark that we need to queue estimated vblank timer
                    if let Some(output_state) = ewm.output_state.get_mut(&output) {
                        trace!(
                            "{}: {} -> Idle (no damage)",
                            output.name(),
                            output_state.redraw_state
                        );
                        output_state.redraw_state = RedrawState::Idle;
                    }
                    need_estimated_vblank = true;
                }

                should_process_screencopy = true;
            }
            Err(err) => {
                warn!("{}: Error rendering frame: {:?}", output.name(), err);
                if let Some(output_state) = ewm.output_state.get_mut(&output) {
                    trace!(
                        "{}: {} -> Idle (render error)",
                        output.name(),
                        output_state.redraw_state
                    );
                    output_state.redraw_state = RedrawState::Idle;
                }
                // Abort lock if we failed to render during Locking state
                ewm.abort_lock_on_render_failure();
            }
        }

        if need_estimated_vblank {
            self.queue_estimated_vblank_timer(crtc, ewm, refresh_interval_us);
        }

        // Send frame callbacks to all surfaces so clients can commit new buffers
        ewm.send_frame_callbacks(&output);

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

        // Render to active screen casts for this output
        #[cfg(feature = "screencast")]
        if should_process_screencopy {
            use crate::protocols::screencopy::get_monotonic_time;

            // Physical size for PipeWire buffer
            let output_size_physical = output
                .current_mode()
                .map(|m| Size::from((m.size.w, m.size.h)))
                .unwrap_or_else(|| Size::from((1920, 1080)));

            // Get current time for frame rate limiting
            let target_frame_time = get_monotonic_time();

            // Use mem::take pattern to avoid borrow conflicts
            let mut screen_casts = std::mem::take(&mut ewm.screen_casts);
            let mut sc_elements = None;

            // Collect valid output names to detect orphaned casts
            let valid_outputs: std::collections::HashSet<String> =
                ewm.space.outputs().map(|o| o.name()).collect();

            for cast in screen_casts.values_mut() {
                // Skip casts for outputs that no longer exist (orphaned)
                if !valid_outputs.contains(&cast.output_name) {
                    trace!(output = %cast.output_name, "skipping orphaned cast");
                    continue;
                }

                if !cast.is_streaming() || cast.output_name != output.name() {
                    continue;
                }

                // Frame rate limiting - skip if too soon
                if cast.should_skip_frame(target_frame_time) {
                    trace!("PipeWire frame too soon, skipping");
                    continue;
                }

                // Lazily collect elements for this output on first active cast
                let elements = sc_elements.get_or_insert_with(|| {
                    let Some(device) = &mut self.device else {
                        return Vec::new();
                    };
                    let Ok(mut renderer) = device.gpu_manager.single_renderer(&render_node) else {
                        return Vec::new();
                    };
                    // Collect elements only for this output
                    collect_render_elements_for_output(
                        ewm,
                        renderer.as_mut(),
                        output_scale,
                        &self.cursor_buffer,
                        output_pos,
                        output_size,
                        true, // include_cursor
                        &output,
                    )
                });

                // Get renderer for rendering to screen cast
                let Some(device) = &mut self.device else {
                    break;
                };
                let Ok(mut renderer) = device.gpu_manager.single_renderer(&render_node) else {
                    break;
                };

                // Render frame to the screen cast (includes damage-based skipping)
                if cast.dequeue_buffer_and_render(
                    renderer.as_mut(),
                    elements,
                    output_size_physical,
                    output_scale,
                ) {
                    // Update last_frame_time on successful render
                    cast.last_frame_time = target_frame_time;
                }
            }

            ewm.screen_casts = screen_casts;
        }
    }

    /// Queue an estimated VBlank timer when there's no damage
    fn queue_estimated_vblank_timer(
        &mut self,
        crtc: crtc::Handle,
        ewm: &mut Ewm,
        refresh_interval_us: u64,
    ) {
        let Some(handle) = self.loop_handle.clone() else {
            warn!("No loop handle available for estimated VBlank timer");
            return;
        };

        let Some(device) = &self.device else {
            return;
        };
        let Some(surface) = device.surfaces.get(&crtc) else {
            return;
        };
        let output = surface.output.clone();

        let duration = Duration::from_micros(refresh_interval_us.max(1000));

        match handle.insert_source(Timer::from_duration(duration), move |_, _, state| {
            state
                .backend
                .on_estimated_vblank_timer(crtc, &mut state.ewm);
            TimeoutAction::Drop
        }) {
            Ok(token) => {
                if let Some(output_state) = ewm.output_state.get_mut(&output) {
                    trace!("{}: Idle -> WaitingForEstVBlank", output.name());
                    output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
                }
            }
            Err(err) => {
                warn!("Failed to insert estimated VBlank timer: {:?}", err);
                if let Some(output_state) = ewm.output_state.get_mut(&output) {
                    output_state.redraw_state = RedrawState::Idle;
                }
            }
        }
    }

    /// Handle estimated VBlank timer firing
    pub(crate) fn on_estimated_vblank_timer(&mut self, crtc: crtc::Handle, ewm: &mut Ewm) {
        let (action, output) = {
            let Some(device) = &self.device else {
                return;
            };
            let Some(surface) = device.surfaces.get(&crtc) else {
                return;
            };
            let output = surface.output.clone();

            let Some(output_state) = ewm.output_state.get_mut(&output) else {
                return;
            };

            // Increment sequence for frame callback throttling
            output_state.frame_callback_sequence =
                output_state.frame_callback_sequence.wrapping_add(1);

            let action = match &output_state.redraw_state {
                RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
                    trace!("{}: WaitingForEstVBlank+Queued -> Queued (est. VBlank)", output.name());
                    output_state.redraw_state = RedrawState::Queued;
                    Some(true)
                }
                RedrawState::WaitingForEstimatedVBlank(_) => {
                    trace!("{}: WaitingForEstVBlank -> Idle (est. VBlank)", output.name());
                    output_state.redraw_state = RedrawState::Idle;
                    Some(false)
                }
                other => {
                    debug!(
                        "{}: unexpected state in on_estimated_vblank_timer: {}",
                        output.name(),
                        other
                    );
                    None
                }
            };
            (action, output)
        };

        match action {
            Some(false) => ewm.send_frame_callbacks(&output),
            Some(true) => self.render_output(crtc, ewm),
            None => {}
        }
    }

    /// Process all outputs that have queued redraws
    pub(crate) fn redraw_queued_outputs(&mut self, ewm: &mut Ewm) {
        tracy_span!("redraw_queued_outputs");

        let Some(device) = &self.device else {
            return;
        };

        // Find crtcs for outputs that have queued redraws
        let queued_crtcs: Vec<crtc::Handle> = device
            .surfaces
            .iter()
            .filter(|(_, surface)| {
                ewm.output_state
                    .get(&surface.output)
                    .map(|s| matches!(s.redraw_state, RedrawState::Queued))
                    .unwrap_or(false)
            })
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
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    info!(
                        "Connector connected: {:?}-{}",
                        connector.interface(),
                        connector.interface_id()
                    );
                    added.push((connector, crtc));
                }
                DrmScanEvent::Connected {
                    connector,
                    crtc: None,
                } => {
                    warn!(
                        "Connector {:?}-{} has no available CRTC",
                        connector.interface(),
                        connector.interface_id()
                    );
                }
                DrmScanEvent::Disconnected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    info!(
                        "Connector disconnected: {:?}-{}",
                        connector.interface(),
                        connector.interface_id()
                    );
                    removed.push(crtc);
                }
                DrmScanEvent::Disconnected {
                    connector,
                    crtc: None,
                } => {
                    debug!(
                        "Connector {:?}-{} disconnected (had no CRTC)",
                        connector.interface(),
                        connector.interface_id()
                    );
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
    ///
    /// Creates the DRM surface, Smithay output, and DrmCompositor.
    /// Reads `ewm.output_config` for mode/scale/transform/position.
    /// Sends OutputDetected and WorkingArea events to Emacs.
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

        // Build connector name early so we can look up config
        let connector_name = format!("{:?}-{}", connector.interface(), connector.interface_id());
        let config = ewm.output_config.get(&connector_name).cloned();

        // Select mode: use configured mode if available, otherwise preferred
        let mode = if let Some((w, h, refresh)) = config.as_ref().and_then(|c| c.mode) {
            resolve_drm_mode(connector.modes(), w, h, refresh).unwrap_or_else(|| {
                warn!(
                    "Configured mode {}x{} not found for {}, using preferred",
                    w, h, connector_name
                );
                preferred_drm_mode(connector.modes()).unwrap()
            })
        } else {
            preferred_drm_mode(connector.modes()).ok_or("No mode available")?
        };

        info!(
            "Connecting display: {} {}x{}@{}Hz",
            connector_name,
            mode.size().0,
            mode.size().1,
            mode.vrefresh()
        );

        // Create DRM surface
        let drm_surface = device
            .drm
            .create_surface(crtc, mode, &[connector.handle()])?;

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
        let initial_transform = config
            .as_ref()
            .and_then(|c| c.transform)
            .unwrap_or(Transform::Normal);
        let initial_scale = config
            .as_ref()
            .and_then(|c| c.scale)
            .map(|s| smithay::output::Scale::Fractional(super::closest_representable_scale(s)));
        output.change_current_state(
            Some(smithay_mode),
            Some(initial_transform),
            initial_scale,
            None,
        );
        output.set_preferred(smithay_mode);
        output.create_global::<State>(display_handle);

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

                let drm_surface = device
                    .drm
                    .create_surface(crtc, mode, &[connector.handle()])?;

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

        // Calculate position: use config or auto horizontal layout
        let (x_offset, y_offset) = config
            .as_ref()
            .and_then(|c| c.position)
            .unwrap_or((ewm.output_size.w, 0));

        device.surfaces.insert(
            crtc,
            OutputSurface {
                output: output.clone(),
                compositor,
                connector: connector.handle(),
            },
        );

        // Initialize output state in Ewm (redraw state, refresh interval)
        let logical_size = crate::utils::output_size(&output);
        ewm.output_state.insert(
            output.clone(),
            OutputState::new(
                &connector_name,
                refresh_interval_us,
                (logical_size.w as i32, logical_size.h as i32),
            ),
        );

        // Check if output should be enabled (skip mapping if disabled)
        let is_enabled = config.as_ref().map(|c| c.enabled).unwrap_or(true);
        if is_enabled {
            ewm.space.map_output(&output, (x_offset, y_offset));
            info!(
                "Mapped output {} at position ({}, {}), size {}x{}",
                connector_name, x_offset, y_offset, mode.size().0, mode.size().1
            );
        } else {
            info!("Output {} connected but disabled by config", connector_name);
        }

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

        let applied_scale =
            super::closest_representable_scale(config.as_ref().and_then(|c| c.scale).unwrap_or(1.0));
        let applied_transform = config
            .as_ref()
            .and_then(|c| c.transform)
            .unwrap_or(Transform::Normal);

        let output_info = OutputInfo {
            name: connector_name.clone(),
            make: "Unknown".to_string(),
            model: "Unknown".to_string(),
            width_mm: physical_size.0 as i32,
            height_mm: physical_size.1 as i32,
            x: x_offset,
            y: y_offset,
            scale: applied_scale,
            transform: super::transform_to_int(applied_transform),
            modes: output_modes,
        };

        ewm.outputs.push(output_info.clone());

        // Update D-Bus outputs for screen casting
        #[cfg(feature = "screencast")]
        {
            let mut dbus_outputs = ewm.dbus_outputs.lock().unwrap();
            dbus_outputs.push(crate::dbus::OutputInfo {
                name: connector_name.clone(),
                x: x_offset,
                y: y_offset,
                width: mode.size().0 as i32,
                height: mode.size().1 as i32,
                refresh: mode.vrefresh(),
            });
            info!(
                "Added D-Bus output: {} at ({}, {}) (total: {})",
                connector_name,
                x_offset,
                y_offset,
                dbus_outputs.len()
            );
        }

        // Recalculate output_size
        ewm.recalculate_output_size();

        // Send IPC event
        ewm.send_output_detected(output_info);

        // Send initial working area (equals full output initially, before any panels)
        let working_area = ewm.get_working_area(&output);
        ewm.working_areas
            .insert(connector_name.clone(), working_area);
        ewm.queue_event(crate::event::Event::WorkingArea {
            output: connector_name.clone(),
            x: working_area.loc.x,
            y: working_area.loc.y,
            width: working_area.size.w,
            height: working_area.size.h,
        });

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

        let output_name = surface.output.name();

        // Cancel pending timers and remove output state from Ewm
        if let Some(output_state) = ewm.output_state.remove(&surface.output) {
            if let RedrawState::WaitingForEstimatedVBlank(token)
            | RedrawState::WaitingForEstimatedVBlankAndQueued(token) = output_state.redraw_state
            {
                if let Some(ref handle) = self.loop_handle {
                    handle.remove(token);
                }
            }
        }

        // Check if lock can be confirmed (removed output no longer needs to render)
        ewm.check_lock_on_output_removed();

        // Stop any active screen casts for this output
        #[cfg(feature = "screencast")]
        {
            let sessions_to_stop: Vec<usize> = ewm
                .screen_casts
                .iter()
                .filter(|(_, cast)| cast.output_name == output_name)
                .map(|(id, _)| *id)
                .collect();

            for session_id in sessions_to_stop {
                info!(
                    output = %output_name,
                    session_id,
                    "stopping cast due to output disconnect"
                );
                ewm.stop_cast(session_id);
            }
        }

        // Unmap from space
        ewm.space.unmap_output(&surface.output);

        // Remove from outputs list
        ewm.outputs.retain(|o| o.name != output_name);

        // Remove from D-Bus outputs
        #[cfg(feature = "screencast")]
        {
            let mut dbus_outputs = ewm.dbus_outputs.lock().unwrap();
            dbus_outputs.retain(|o| o.name != output_name);
        }

        // Recalculate output_size
        ewm.recalculate_output_size();

        // Send IPC event
        ewm.send_output_disconnected(&output_name);

        info!("Output disconnected: {}", output_name);
    }
}

/// Initialize DRM device and set up outputs
fn initialize_drm(
    state: &mut State,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
    event_loop_handle: &LoopHandle<'static, State>,
) -> Result<(), Box<dyn std::error::Error>> {
    let drm_backend = state.backend.as_drm_mut().ok_or("Not a DRM backend")?;
    let pending = drm_backend
        .pending
        .take()
        .ok_or("DRM already initialized")?;

    info!("Initializing DRM device (session is now active)");

    // Open DRM device via libseat
    let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
    let session = drm_backend
        .session
        .as_mut()
        .ok_or("Session not available")?;
    let fd = session.open(&pending.gpu_path, open_flags)?;
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
    let api: GbmGlesBackend<GlesRenderer, DrmDeviceFd> = GbmGlesBackend::with_context_priority(
        smithay::backend::egl::context::ContextPriority::High,
    );
    let mut gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>> =
        GpuManager::new(api)?;
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
            let _global = state
                .ewm
                .dmabuf_state
                .create_global_with_default_feedback::<State>(display_handle, &default_feedback);
            info!("Dmabuf global created");
        }
    }

    // Store display handle and device state early so connect_output can use them
    {
        let drm_backend = state.backend.as_drm_mut().unwrap();
        drm_backend.display_handle = Some(display_handle.clone());
        drm_backend.device = Some(DrmDeviceState {
            drm,
            drm_scanner: DrmScanner::new(),
            gbm,
            gpu_manager,
            render_node,
            surfaces: HashMap::new(),
        });
    }

    // Scan connectors (collect results before releasing borrow)
    let connectors: Vec<_> = {
        let device = state
            .backend
            .as_drm_mut()
            .unwrap()
            .device
            .as_mut()
            .unwrap();
        let scan_result = device.drm_scanner.scan_connectors(&device.drm)?;
        scan_result
            .into_iter()
            .filter_map(|event| match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => Some((connector, crtc)),
                DrmScanEvent::Connected {
                    connector,
                    crtc: None,
                } => {
                    warn!(
                        "No available CRTC for connector {:?}-{}",
                        connector.interface(),
                        connector.interface_id()
                    );
                    None
                }
                DrmScanEvent::Disconnected { .. } => None,
            })
            .collect()
    };

    // Connect each discovered output (reuses hotplug connect_output path)
    for (connector, crtc) in connectors {
        let drm_backend = state.backend.as_drm_mut().unwrap();
        if let Err(err) = drm_backend.connect_output(connector, crtc, &mut state.ewm) {
            warn!("Failed to connect output during init: {:?}", err);
        }
    }

    info!(
        "Total output area: {}x{} ({} outputs)",
        state.ewm.output_size.w,
        state.ewm.output_size.h,
        state.ewm.outputs.len()
    );

    // Register DRM event notifier for VBlank
    event_loop_handle.insert_source(drm_notifier, |event, _, state| {
        match event {
            DrmEvent::VBlank(crtc) => {
                crate::tracy_frame_mark!();
                crate::tracy_span!("on_vblank");

                let (redraw_needed, output) = {
                    let Some(device) =
                        &mut state.backend.as_drm_mut().unwrap().device
                    else {
                        return;
                    };
                    let Some(surface) = device.surfaces.get_mut(&crtc) else {
                        return;
                    };

                    match surface.compositor.frame_submitted() {
                        Ok(_) => {}
                        Err(err) => {
                            warn!("Error marking frame as submitted: {:?}", err);
                        }
                    }

                    let output = surface.output.clone();
                    let Some(output_state) = state.ewm.output_state.get_mut(&output)
                    else {
                        return;
                    };

                    // End Tracy frame tracking for VBlank interval
                    output_state.vblank_tracker.end_frame();

                    // Use mem::replace like niri: set to Idle, then decide what to do.
                    // This handles rogue VBlanks that arrive in unexpected states
                    // (can happen on some hardware after sleep/resume).
                    let old_state = std::mem::replace(
                        &mut output_state.redraw_state,
                        RedrawState::Idle,
                    );
                    let redraw_needed = match &old_state {
                        RedrawState::WaitingForVBlank { redraw_needed } => *redraw_needed,
                        _ => {
                            error!(
                                "unexpected redraw state on VBlank for {} \
                                 (should be WaitingForVBlank); can happen when \
                                 resuming from sleep: {}",
                                output.name(),
                                old_state
                            );
                            true // force redraw to recover
                        }
                    };
                    trace!(
                        "{}: {} -> {} (VBlank)",
                        output.name(),
                        old_state,
                        if redraw_needed { "Queued" } else { "Idle" }
                    );

                    (redraw_needed, output)
                };

                if redraw_needed {
                    state.ewm.queue_redraw(&output);
                    state.backend.render_output(crtc, &mut state.ewm);
                } else {
                    state.ewm.send_frame_callbacks(&output);
                }
            }
            DrmEvent::Error(error) => {
                warn!("DRM error: {error}");
                // Reset any stuck WaitingForVBlank states to prevent render stalls
                if let Some(device) = &state.backend.as_drm().unwrap().device {
                    let outputs: Vec<_> =
                        device.surfaces.values().map(|s| s.output.clone()).collect();
                    for output in outputs {
                        if let Some(output_state) = state.ewm.output_state.get_mut(&output) {
                            if matches!(
                                output_state.redraw_state,
                                RedrawState::WaitingForVBlank { .. }
                            ) {
                                warn!(
                                    "Recovering stuck redraw state for {}",
                                    output.name()
                                );
                                output_state.redraw_state = RedrawState::Queued;
                            }
                        }
                    }
                }
            }
        }
    })?;

    info!("DRM initialization complete");

    // connect_output already sent output_detected and working_area events
    // for each output. Send the final completion events.
    state.ewm.queue_event(crate::event::Event::OutputsComplete);
    state.ewm.queue_event(crate::event::Event::Ready);
    info!(
        "Sent {} output_detected events, compositor ready",
        state.ewm.outputs.len()
    );

    // Trigger initial render - collect CRTCs first, then render
    let crtcs: Vec<_> = state
        .backend
        .as_drm()
        .and_then(|drm| drm.device.as_ref())
        .map(|d| d.surfaces.keys().copied().collect())
        .unwrap_or_default();

    for crtc in crtcs {
        state.backend.render_output(crtc, &mut state.ewm);
    }

    Ok(())
}

/// Run EWM with DRM/libinput backend (module mode only)
pub fn run_drm() -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting EWM with DRM backend (module mode)");

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
    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let display_handle = display.handle();

    // Initialize Wayland socket - display is moved into event loop source
    let socket_name = Ewm::init_wayland_listener(display, &event_loop.handle())?;
    let socket_name_str = socket_name.to_string_lossy().to_string();
    info!("Wayland socket: {:?}", socket_name);

    // Set environment variables for child processes and portals
    // SAFETY: We're single-threaded at this point, before spawning any threads
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &socket_name_str);
        std::env::set_var("XDG_CURRENT_DESKTOP", "wlroots");
        std::env::set_var("XDG_SESSION_TYPE", "wayland");
        // Use Wayland-native input method for GTK/Qt apps
        std::env::set_var("GTK_IM_MODULE", "wayland");
        std::env::set_var("QT_IM_MODULE", "wayland");
    }

    // Update D-Bus/systemd environment so portals can find us
    let variables = "WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE";
    match std::process::Command::new("/bin/sh")
        .args([
            "-c",
            &format!(
                "systemctl --user import-environment {variables}; \
                 hash dbus-update-activation-environment 2>/dev/null && \
                 dbus-update-activation-environment {variables}"
            ),
        ])
        .status()
    {
        Ok(status) if !status.success() => {
            warn!("import environment exited with {}", status);
        }
        Err(e) => {
            warn!("Failed to import environment: {}", e);
        }
        _ => {}
    }

    let mut ewm = Ewm::new(display_handle.clone(), event_loop.handle());

    // Connect input method relay to ourselves
    let socket_path = std::env::var("XDG_RUNTIME_DIR")
        .map(|dir| std::path::PathBuf::from(dir).join(&socket_name_str))
        .ok();
    if let Some(ref path) = socket_path {
        ewm.connect_im_relay(path);
    }

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

    // Create backend state (owned directly, no Rc<RefCell<>>)
    let backend = DrmBackendState {
        session: Some(session),
        libinput: libinput.clone(),
        device: None,
        pending: Some(DrmPendingInit {
            gpu_path: gpu_path.clone(),
            seat_name: seat_name.clone(),
        }),
        paused: false,
        session_notifier_token: None, // Set after registering notifier
        init_sender: Some(init_sender),
        loop_handle: Some(event_loop.handle()),
        cursor_buffer: CursorBuffer::new(),
        display_handle: None, // Set during initialize_drm
    };

    let mut state = State {
        backend: Backend::Drm(backend),
        ewm,
    };

    // Initialize PipeWire and D-Bus for screen sharing
    #[cfg(feature = "screencast")]
    {
        use crate::pipewire::PipeWire;
        match PipeWire::new(&event_loop.handle(), || {
            tracing::warn!("PipeWire fatal error callback triggered");
        }) {
            Ok(mut pw) => {
                tracing::info!("PipeWire initialized successfully");

                // Register handler for PipeWire fatal errors
                // Take the channel out of the Option since it can only be consumed once
                if let Some(fatal_error_rx) = pw.fatal_error_rx.take() {
                    event_loop
                        .handle()
                        .insert_source(fatal_error_rx, |event, _, state| {
                            use smithay::reexports::calloop::channel::Event as ChannelEvent;
                            if let ChannelEvent::Msg(()) = event {
                                tracing::error!("PipeWire fatal error, stopping all screen casts");
                                // Clear all screen casts - they will be dropped and cleaned up
                                let count = state.ewm.screen_casts.len();
                                state.ewm.screen_casts.clear();
                                if count > 0 {
                                    tracing::info!(
                                        "Stopped {} screen cast(s) due to PipeWire error",
                                        count
                                    );
                                }
                            }
                        })
                        .expect("Failed to register PipeWire fatal error handler");
                }

                state.ewm.pipewire = Some(pw);
            }
            Err(err) => {
                tracing::warn!("PipeWire initialization failed: {err:?}");
            }
        }

        // Start D-Bus servers
        use crate::dbus::{DBusServers, ScreenCastToCompositor};
        use smithay::reexports::calloop::channel::Event as ChannelEvent;

        let outputs = state.ewm.dbus_outputs.clone();
        let (dbus_servers, receiver) = DBusServers::start(outputs, display_handle.clone());
        // Store D-Bus servers to keep connections alive
        state.ewm.dbus_servers = Some(dbus_servers);

        // Register the receiver to handle D-Bus messages
        event_loop
            .handle()
            .insert_source(receiver, |event, _, state| {
                if let ChannelEvent::Msg(msg) = event {
                    match msg {
                        ScreenCastToCompositor::StartCast {
                            session_id,
                            output_name,
                            signal_ctx,
                        } => {
                            tracing::info!(
                                "StartCast: session={}, output={}",
                                session_id,
                                output_name
                            );

                            // Create PipeWire stream for this output
                            let pw = state.ewm.pipewire.as_ref();
                            let gbm = state.backend.gbm_device();

                            if let (Some(pw), Some(gbm)) = (pw, gbm) {
                                // Find output info
                                let output_info = state
                                    .ewm
                                    .dbus_outputs
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .find(|o| o.name == output_name)
                                    .cloned();

                                if let Some(info) = output_info {
                                    use crate::pipewire::stream::Cast;
                                    use smithay::utils::Size;

                                    match Cast::new(
                                        pw,
                                        gbm,
                                        Size::from((info.width, info.height)),
                                        info.refresh,
                                        output_name.clone(),
                                        signal_ctx,
                                    ) {
                                        Ok(cast) => {
                                            tracing::info!(
                                                "PipeWire stream created, waiting for state change"
                                            );
                                            // Store the cast to keep the stream alive
                                            state.ewm.screen_casts.insert(session_id, cast);
                                        }
                                        Err(err) => {
                                            tracing::warn!(
                                                "Failed to create PipeWire stream: {err:?}"
                                            );
                                        }
                                    }
                                }
                            } else {
                                tracing::warn!("PipeWire or GBM not available for screen cast");
                            }
                        }
                        ScreenCastToCompositor::StopCast { session_id } => {
                            tracing::info!("StopCast: session={}", session_id);
                            state.ewm.stop_cast(session_id);
                        }
                    }
                }
            })
            .expect("Failed to register D-Bus receiver");

        tracing::info!("D-Bus ScreenCast server started");
    }

    // Notify systemd we're ready. This must be outside the #[cfg(feature = "screencast")]
    // block so it fires unconditionally.
    if let Err(err) = sd_notify::notify(true, &[sd_notify::NotifyState::Ready]) {
        tracing::warn!("Error notifying systemd: {err:?}");
    } else {
        tracing::info!("Notified systemd that compositor is ready");
    }

    // Register session notifier and store token for cleanup in Drop
    let session_notifier_token =
        event_loop
            .handle()
            .insert_source(notifier, |event, _, state| match event {
                SessionEvent::PauseSession => {
                    info!("Session paused (VT switch away)");
                    state.backend.pause(&mut state.ewm);
                }
                SessionEvent::ActivateSession => {
                    info!("Session activated");
                    if state
                        .backend
                        .as_drm()
                        .map(|d| d.device.is_none())
                        .unwrap_or(false)
                    {
                        info!("First session activation - triggering DRM init");
                        state.backend.trigger_init();
                    } else {
                        state.backend.resume(&mut state.ewm);
                    }
                }
            })?;
    state.backend.as_drm_mut().unwrap().session_notifier_token = Some(session_notifier_token);

    // Register UdevBackend for hotplug detection
    let udev_backend = UdevBackend::new(&seat_name)?;
    event_loop
        .handle()
        .insert_source(udev_backend, |event, _, state| {
            match event {
                UdevEvent::Changed { device_id: _ } => {
                    // Scan for connector changes
                    state.backend.on_device_changed(&mut state.ewm);
                    state.ewm.queue_redraw_all();
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
    let display_handle_for_init = display_handle.clone();
    let event_loop_handle = event_loop.handle();
    event_loop
        .handle()
        .insert_source(init_receiver, move |event, _, state| {
            if let smithay::reexports::calloop::channel::Event::Msg(DrmMessage::InitializeDrm) =
                event
            {
                info!("Received DRM init message");
                if let Err(e) = initialize_drm(state, &display_handle_for_init, &event_loop_handle)
                {
                    warn!("Failed to initialize DRM: {:?}", e);
                }
            }
        })?;

    // Get loop signal early so input handlers and module can trigger shutdown
    let loop_signal = event_loop.get_signal();
    state.ewm.set_stop_signal(loop_signal.clone());

    // Store signal in module static for ewm-stop to use
    let _ = crate::module::LOOP_SIGNAL.set(loop_signal);

    // Register libinput with event loop (using shared input handlers)
    let libinput_backend = LibinputInputBackend::new(libinput);
    info!("Registering libinput backend with event loop...");
    let _libinput_token =
        event_loop
            .handle()
            .insert_source(libinput_backend, move |mut event, _, state| {
                match event {
                    InputEvent::DeviceAdded { ref mut device } => {
                        handle_device_added(device);
                    }
                    InputEvent::Keyboard { event: kb_event } => {
                        let action = handle_keyboard_event(
                            state,
                            kb_event.key_code().into(),
                            kb_event.state(),
                            Event::time_msec(&kb_event),
                        );
                        match action {
                            KeyboardAction::Shutdown => {
                                info!("Kill combo pressed, shutting down");
                                state.ewm.stop();
                            }
                            KeyboardAction::ChangeVt(vt) => {
                                state.backend.change_vt(vt);
                            }
                            _ => {}
                        }
                    }
                    InputEvent::PointerMotion { event } => {
                        crate::input::handle_pointer_motion::<LibinputInputBackend>(
                            state, event,
                        );
                        state.ewm.queue_redraw_all();
                    }
                    InputEvent::PointerMotionAbsolute { event } => {
                        crate::input::handle_pointer_motion_absolute::<LibinputInputBackend>(
                            state, event,
                        );
                        state.ewm.queue_redraw_all();
                    }
                    InputEvent::PointerButton { event } => {
                        crate::input::handle_pointer_button::<LibinputInputBackend>(
                            state, event,
                        );
                    }
                    InputEvent::PointerAxis { event } => {
                        crate::input::handle_pointer_axis::<LibinputInputBackend>(
                            state, event,
                        );
                    }
                    _ => {}
                }
            })?;

    info!("EWM DRM backend started (waiting for session activation)");
    info!("VT switching: Ctrl+Alt+F1-F7");
    info!("Kill combo: Super+Shift+E");

    // If session is already active, initialize DRM immediately
    if session_active {
        info!("Session already active, initializing DRM now");
        if let Err(e) = initialize_drm(&mut state, &display_handle, &event_loop.handle()) {
            return Err(format!("Failed to initialize DRM: {:?}", e).into());
        }
    }

    let pid = std::process::id();
    info!("Tracking Emacs PID {}", pid);
    state.ewm.set_emacs_pid(pid);

    // Run the event loop with per-frame callback
    event_loop
        .run(None, &mut state, |state| {
            state.refresh_and_flush_clients();
        })
        .map_err(|e| format!("Event loop error: {:?}", e))?;

    info!("EWM DRM backend shutting down");

    // Backend is dropped automatically when state goes out of scope
    // Proper Drop ordering ensures DRM device is released before session

    Ok(())
}

