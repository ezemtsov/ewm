//! DRM/libinput backend for running EWM as a standalone Wayland session
//!
//! Inspired by niri's `backend/tty.rs` for DRM initialization, VBlank
//! synchronization, and session pause/resume patterns. This module provides
//! the backend for running directly on hardware without another compositor.
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
use std::iter::zip;
use std::num::NonZeroU64;
use std::os::fd::AsFd;

use crate::tracy_span;
use anyhow::{ensure, Context as _};
use bytemuck::cast_slice_mut;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, error, info, trace, warn};

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
            exporter::gbm::GbmFramebufferExporter,
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmEventTime, DrmNode,
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
            connector, crtc, property, Device as ControlDevice, Mode as DrmMode, ModeFlags,
            ModeTypeFlags, ResourceHandle,
        },
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::{
            backend::GlobalId, protocol::wl_surface::WlSurface, Display, DisplayHandle,
        },
    },
    utils::{DeviceFd, Scale, Transform},
    wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::reexports::wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::wayland::presentation::Refresh;

use crate::{
    backend::Backend,
    cursor::CursorBuffer,
    input::{apply_libinput_settings, handle_keyboard_event, KeyboardAction},
    render::{collect_render_elements_for_output, process_screencopies_for_output, RenderTarget},
    vblank_throttle::VBlankThrottle,
    Ewm, LockRenderState, OutputInfo, OutputMode, OutputState, RedrawState, State,
};

const SUPPORTED_COLOR_FORMATS: [smithay::backend::allocator::Fourcc; 4] = [
    smithay::backend::allocator::Fourcc::Xrgb8888,
    smithay::backend::allocator::Fourcc::Xbgr8888,
    smithay::backend::allocator::Fourcc::Argb8888,
    smithay::backend::allocator::Fourcc::Abgr8888,
];

/// Find a DRM mode matching the requested resolution and optional refresh rate.
///
/// `refresh_hz`: target refresh rate in Hz (e.g., 60 for 60Hz).
/// When specified, picks the mode whose refresh is closest (within 1Hz).
/// When omitted, picks the highest available refresh rate.
fn resolve_drm_mode(
    modes: &[DrmMode],
    width: i32,
    height: i32,
    refresh_hz: Option<i32>,
) -> Option<DrmMode> {
    let matching = modes
        .iter()
        .filter(|m| m.size().0 as i32 == width && m.size().1 as i32 == height);

    if let Some(target_hz) = refresh_hz {
        let target_mhz = target_hz * 1000;
        // Pick mode closest to target, within 1Hz tolerance
        matching
            .filter(|m| (Mode::from(**m).refresh - target_mhz).abs() < 1000)
            .min_by_key(|m| (Mode::from(**m).refresh - target_mhz).abs())
            .copied()
    } else {
        // No refresh specified — pick highest available
        matching.max_by_key(|m| Mode::from(**m).refresh).copied()
    }
}

/// Compute precise refresh interval from DRM mode timing parameters.
///
/// Uses raw pixel clock and total line counts instead of the rounded `vrefresh()`
/// value, giving nanosecond precision (e.g. 4167291ns for 239.964Hz instead of
/// 4166µs from integer division).
fn refresh_interval(mode: DrmMode) -> Duration {
    let clock = mode.clock() as u64;
    let htotal = mode.hsync().2 as u64;
    let vtotal = mode.vsync().2 as u64;

    if clock == 0 || htotal == 0 || vtotal == 0 {
        return Duration::from_micros(16_667);
    }

    let mut numerator = htotal * vtotal * 1_000_000;
    let mut denominator = clock;

    if mode.flags().contains(ModeFlags::INTERLACE) {
        denominator *= 2;
    }
    if mode.flags().contains(ModeFlags::DBLSCAN) {
        numerator *= 2;
    }
    if mode.vscan() > 1 {
        numerator *= mode.vscan() as u64;
    }

    let refresh_interval_ns = (numerator + denominator / 2) / denominator;
    Duration::from_nanos(refresh_interval_ns)
}

/// Build per-surface DMA-BUF feedback with scanout tranche hints.
///
/// Creates two feedback sets: `render` (default compositing path) and `scanout`
/// (direct scanout via primary/overlay planes). Clients that allocate DMA-BUFs
/// in scanout-compatible formats can skip GPU composition entirely.
fn build_surface_dmabuf_feedback(
    compositor: &GbmDrmCompositor,
    render_formats: FormatSet,
    render_node: DrmNode,
) -> Result<SurfaceDmabufFeedback, std::io::Error> {
    let surface = compositor.surface();
    let planes = surface.planes();

    let primary_plane_formats = surface.plane_info().formats.clone();
    let primary_or_overlay_formats: FormatSet = primary_plane_formats
        .iter()
        .chain(planes.overlay.iter().flat_map(|p| p.formats.iter()))
        .copied()
        .collect();

    // Limit scanout formats to those we can also render — ensures a fallback path
    let primary_scanout_formats: Vec<_> = primary_plane_formats
        .intersection(&render_formats)
        .copied()
        .collect();
    let overlay_scanout_formats: Vec<_> = primary_or_overlay_formats
        .intersection(&render_formats)
        .copied()
        .collect();

    let builder = DmabufFeedbackBuilder::new(render_node.dev_id(), render_formats);

    // Scanout feedback: prefer primary-plane formats, then overlay-plane formats
    let scanout = builder
        .clone()
        .add_preference_tranche(
            render_node.dev_id(),
            Some(TrancheFlags::Scanout),
            primary_scanout_formats,
        )
        .add_preference_tranche(
            render_node.dev_id(),
            Some(TrancheFlags::Scanout),
            overlay_scanout_formats,
        )
        .build()?;

    // Render feedback: just the default tranche (no scanout hints).
    // Single-GPU: render == scanout since same device handles both.
    let render = scanout.clone();

    Ok(SurfaceDmabufFeedback { render, scanout })
}

/// Find the preferred DRM mode, falling back to the first available.
fn preferred_drm_mode(modes: &[DrmMode]) -> Option<DrmMode> {
    modes
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| modes.first())
        .copied()
}

/// Data passed through `queue_frame()` → `frame_submitted()` for presentation feedback.
type FrameData = (OutputPresentationFeedback, Duration);

/// Type alias for our DRM compositor
type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    FrameData,
    DrmDeviceFd,
>;

/// Per-output surface state (DRM-specific, redraw state is in Ewm::output_state)
struct OutputSurface {
    output: Output,
    /// wl_output global ID (stored for verification/lifecycle)
    global_id: GlobalId,
    compositor: GbmDrmCompositor,
    /// Connector handle for mode lookups
    connector: connector::Handle,
    /// Throttles buggy drivers that deliver VBlanks too early
    vblank_throttle: VBlankThrottle,
    /// DMA-BUF feedback for direct scanout hints to clients
    dmabuf_feedback: Option<SurfaceDmabufFeedback>,
    /// Gamma control properties (if hardware supports it)
    gamma_props: Option<GammaProps>,
    /// Pending gamma change to apply when session becomes active
    pending_gamma_change: Option<Option<Vec<u16>>>,
}

/// Per-surface DMA-BUF feedback: render path vs direct scanout path.
/// Clients use this to allocate buffers in formats the compositor can
/// scanout directly, avoiding GPU composition copies.
pub struct SurfaceDmabufFeedback {
    pub render: DmabufFeedback,
    pub scanout: DmabufFeedback,
}

/// Look up a DRM property value for a resource handle.
fn get_drm_property(
    drm: &DrmDevice,
    resource: impl ResourceHandle,
    prop: property::Handle,
) -> Option<property::RawValue> {
    let props = match drm.get_properties(resource) {
        Ok(props) => props,
        Err(err) => {
            warn!("error getting properties: {err:?}");
            return None;
        }
    };
    props
        .into_iter()
        .find_map(|(handle, value)| (handle == prop).then_some(value))
}

/// DRM gamma correction properties for a CRTC
struct GammaProps {
    crtc: crtc::Handle,
    gamma_lut: property::Handle,
    gamma_lut_size: property::Handle,
    previous_blob: Option<NonZeroU64>,
}

impl GammaProps {
    /// Query CRTC properties and create GammaProps if hardware supports gamma control
    fn new(device: &DrmDevice, crtc: crtc::Handle) -> anyhow::Result<Self> {
        let mut gamma_lut = None;
        let mut gamma_lut_size = None;

        let props = device
            .get_properties(crtc)
            .context("error getting CRTC properties")?;
        for (prop, _) in props {
            let Ok(info) = device.get_property(prop) else {
                continue;
            };

            let Ok(name) = info.name().to_str() else {
                continue;
            };

            match name {
                "GAMMA_LUT" => {
                    ensure!(
                        matches!(info.value_type(), property::ValueType::Blob),
                        "GAMMA_LUT has unexpected type {:?}",
                        info.value_type()
                    );
                    gamma_lut = Some(prop);
                }
                "GAMMA_LUT_SIZE" => {
                    ensure!(
                        matches!(info.value_type(), property::ValueType::UnsignedRange(_, _)),
                        "GAMMA_LUT_SIZE has unexpected type {:?}",
                        info.value_type()
                    );
                    gamma_lut_size = Some(prop);
                }
                _ => (),
            }
        }

        Ok(Self {
            crtc,
            gamma_lut: gamma_lut.context("GAMMA_LUT property not found")?,
            gamma_lut_size: gamma_lut_size.context("GAMMA_LUT_SIZE property not found")?,
            previous_blob: None,
        })
    }

    /// Get the gamma ramp size supported by hardware
    fn gamma_size(&self, device: &DrmDevice) -> anyhow::Result<u32> {
        get_drm_property(device, self.crtc, self.gamma_lut_size)
            .map(|v| v as u32)
            .context("error getting GAMMA_LUT_SIZE")
    }

    /// Set gamma ramp (or None to reset to identity)
    fn set_gamma(&mut self, device: &DrmDevice, gamma: Option<&[u16]>) -> anyhow::Result<()> {
        tracy_span!("GammaProps::set_gamma");

        let blob = if let Some(gamma) = gamma {
            let gamma_size = self.gamma_size(device)? as usize;

            ensure!(
                gamma.len() == gamma_size * 3,
                "wrong gamma length: got {}, expected {}",
                gamma.len(),
                gamma_size * 3
            );

            // Convert flat [R,G,B] array to drm_color_lut structs
            #[allow(non_camel_case_types)]
            #[repr(C)]
            #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
            struct drm_color_lut {
                red: u16,
                green: u16,
                blue: u16,
                reserved: u16,
            }

            let (red, rest) = gamma.split_at(gamma_size);
            let (blue, green) = rest.split_at(gamma_size);
            let mut data = zip(zip(red, blue), green)
                .map(|((&red, &green), &blue)| drm_color_lut {
                    red,
                    green,
                    blue,
                    reserved: 0,
                })
                .collect::<Vec<_>>();
            let data = cast_slice_mut(&mut data);

            let blob = drm_ffi::mode::create_property_blob(device.as_fd(), data)
                .context("error creating GAMMA_LUT blob")?;
            NonZeroU64::new(u64::from(blob.blob_id))
        } else {
            None
        };

        let blob_id = blob.map(NonZeroU64::get).unwrap_or(0);

        device
            .set_property(
                self.crtc,
                self.gamma_lut,
                property::Value::Blob(blob_id).into(),
            )
            .inspect_err(|_| {
                // Clean up the blob we just created on failure
                if blob_id != 0 {
                    if let Err(err) = device.destroy_property_blob(blob_id) {
                        warn!("error destroying GAMMA_LUT property blob: {err:?}");
                    }
                }
            })
            .context("error setting GAMMA_LUT")?;

        // Destroy previous blob after successfully setting the new one
        if let Some(previous) = std::mem::replace(&mut self.previous_blob, blob) {
            if let Err(err) = device.destroy_property_blob(previous.get()) {
                warn!("error destroying previous GAMMA_LUT blob: {err:?}");
            }
        }

        Ok(())
    }

    /// Restore the previously-active gamma blob (e.g. after session resume)
    fn restore_gamma(&self, device: &DrmDevice) -> anyhow::Result<()> {
        let blob = self.previous_blob.map(NonZeroU64::get).unwrap_or(0);
        device
            .set_property(
                self.crtc,
                self.gamma_lut,
                property::Value::Blob(blob).into(),
            )
            .context("error setting GAMMA_LUT")?;
        Ok(())
    }
}

/// Legacy gamma fallback for hardware without GAMMA_LUT property.
/// Uses the ioctl-based `set_gamma` on the CRTC directly.
fn set_gamma_for_crtc(
    device: &DrmDevice,
    crtc: crtc::Handle,
    ramp: Option<&[u16]>,
) -> anyhow::Result<()> {
    let crtc_info = device.get_crtc(crtc).context("error getting CRTC info")?;
    let gamma_length = crtc_info.gamma_length() as usize;
    ensure!(gamma_length > 0, "CRTC reports zero gamma length");

    let mut temp;
    let ramp = if let Some(ramp) = ramp {
        ensure!(
            ramp.len() == gamma_length * 3,
            "wrong gamma length: got {}, expected {}",
            ramp.len(),
            gamma_length * 3
        );
        ramp
    } else {
        // Generate linear ramp
        temp = vec![0u16; gamma_length * 3];
        let (red, rest) = temp.split_at_mut(gamma_length);
        let (green, blue) = rest.split_at_mut(gamma_length);
        let denom = gamma_length as u64 - 1;
        for (i, ((r, g), b)) in zip(zip(red, green), blue).enumerate() {
            let value = (0xFFFFu64 * i as u64 / denom) as u16;
            *r = value;
            *g = value;
            *b = value;
        }
        &temp
    };

    let (red, rest) = ramp.split_at(gamma_length);
    let (green, blue) = rest.split_at(gamma_length);
    device
        .set_gamma(crtc, red, green, blue)
        .context("error setting legacy gamma")?;

    Ok(())
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
    /// Whether the laptop lid is currently closed (from libinput switch events)
    pub lid_closed: bool,
    /// Token for session notifier - must be removed before session drops
    session_notifier_token: Option<RegistrationToken>,
    /// Connected libinput devices for re-applying configuration on change
    libinput_devices: std::collections::HashSet<smithay::reexports::input::Device>,
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
    /// Check if the libseat session is currently active (not on another VT).
    fn session_active(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.is_active())
    }

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
        ewm.cancel_idle_timer();
    }

    /// Handle session resume (VT switch back)
    pub(crate) fn resume(&mut self, ewm: &mut Ewm) {
        debug!("Resuming DRM session");

        if self.libinput.resume().is_err() {
            warn!("Error resuming libinput");
        }

        if let Some(device) = &mut self.device {
            if let Err(err) = device.drm.activate(true) {
                warn!("Error activating DRM device: {:?}", err);
            } else {
                info!("DRM device activated successfully (DRM master acquired)");
            }

            // Reset DRM compositor state on all surfaces. After a session resume
            // the compositor needs to re-read hardware state and do a full damage
            // repaint. Without this, stale buffer references from before the pause
            // can cause rendering artifacts.
            for surface in device.surfaces.values_mut() {
                if let Err(err) = surface.compositor.reset_state() {
                    warn!("Error resetting DrmCompositor state: {:?}", err);
                }
                surface.compositor.reset_buffers();

                // Apply any pending gamma changes that were queued while session was paused
                if let Some(ramp) = surface.pending_gamma_change.take() {
                    if let Some(ref mut gamma_props) = surface.gamma_props {
                        if let Err(err) = gamma_props.set_gamma(&device.drm, ramp.as_deref()) {
                            warn!(
                                "error applying pending gamma change for {}: {err:?}",
                                surface.output.name()
                            );
                        }
                    } else {
                        // Legacy fallback
                        let crtc = surface.compositor.surface().crtc();
                        if let Err(err) = set_gamma_for_crtc(&device.drm, crtc, ramp.as_deref()) {
                            warn!(
                                "error applying pending gamma change for {}: {err:?}",
                                surface.output.name()
                            );
                        }
                    }
                } else {
                    // No pending change — restore previous gamma state
                    if let Some(ref gamma_props) = surface.gamma_props {
                        if let Err(err) = gamma_props.restore_gamma(&device.drm) {
                            warn!(
                                "error restoring gamma for {}: {err:?}",
                                surface.output.name()
                            );
                        }
                    }
                }
            }
        }

        // Re-scan connectors to detect monitors added/removed during VT switch.
        // If topology changed, on_device_changed sends OutputsComplete.
        self.on_device_changed(ewm);

        // Verify output globals survived the session pause/resume cycle
        self.verify_output_globals();

        // Reactivate monitors in case they were deactivated (e.g., lid closed)
        ewm.activate_monitors();

        // If lid was closed during suspend, ensure laptop panel stays off
        if self.lid_closed {
            self.on_lid_state_changed(ewm);
        }

        // Queue redraws for all outputs to resume rendering
        ewm.queue_redraw_all();

        // Reset idle timers so we don't immediately trigger idle timeout after wake
        ewm.idle_notifier_state.notify_activity(&ewm.seat);
        ewm.reset_idle_timer();

        // Always notify Emacs so it re-syncs layout and focus after resume,
        // even if no output topology changed.
        ewm.queue_event(crate::event::Event::OutputsComplete);
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

    /// Re-apply libinput settings to all connected devices.
    pub fn reapply_libinput_config(&mut self, configs: &[crate::input::InputConfigEntry]) {
        for mut device in self.libinput_devices.iter().cloned() {
            crate::input::apply_libinput_settings(&mut device, configs);
        }
    }

    /// Clear all DRM surfaces (DPMS off). Re-enabled on next queue_frame.
    pub fn clear_all_surfaces(&mut self) {
        let Some(device) = &mut self.device else {
            return;
        };
        for surface in device.surfaces.values_mut() {
            if let Err(err) = surface.compositor.clear() {
                warn!("Error clearing DRM surface: {:?}", err);
            }
        }
    }

    /// Get gamma ramp size for an output
    pub fn get_gamma_size(&mut self, output: &Output) -> anyhow::Result<u32> {
        let device = self.device.as_ref().context("DRM device not initialized")?;

        let (crtc, surface) = device
            .surfaces
            .iter()
            .find(|(_, s)| &s.output == output)
            .context("output not found")?;

        if let Some(ref gamma_props) = surface.gamma_props {
            gamma_props.gamma_size(&device.drm)
        } else {
            // Legacy fallback: read gamma_length from CRTC info
            let crtc_info = device
                .drm
                .get_crtc(*crtc)
                .context("error getting CRTC info")?;
            Ok(crtc_info.gamma_length())
        }
    }

    /// Set gamma ramp for an output (or None to reset to identity)
    pub fn set_gamma(&mut self, output: &Output, ramp: Option<Vec<u16>>) -> anyhow::Result<()> {
        let session_active = self.session_active();
        let device = self.device.as_mut().context("DRM device not initialized")?;

        let (&crtc, surface) = device
            .surfaces
            .iter_mut()
            .find(|(_, s)| &s.output == output)
            .context("output not found")?;

        // If session is paused, store the change to apply on resume
        if !session_active {
            trace!("Session paused, queuing gamma change for {}", output.name());
            surface.pending_gamma_change = Some(ramp);
            return Ok(());
        }

        // Apply immediately
        if let Some(ref mut gamma_props) = surface.gamma_props {
            gamma_props.set_gamma(&device.drm, ramp.as_deref())
        } else {
            // Legacy fallback
            set_gamma_for_crtc(&device.drm, crtc, ramp.as_deref())
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
                    warn!("Failed to get connector info for {}: {:?}", output_name, e);
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
        let smithay_mode = new_drm_mode.map(Mode::from);
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

        // --- Update frame clock refresh interval ---
        if let Some(drm_mode) = new_drm_mode {
            if let Some(output_state) = ewm.output_state.get_mut(&output) {
                output_state.frame_clock =
                    crate::frame_clock::FrameClock::new(Some(refresh_interval(drm_mode)));
            }
        }

        // --- Update all backend-agnostic bookkeeping ---
        ewm.output_config_changed(&output);

        info!(
            "Applied config for {}: mode={:?}, scale={:?}, transform={:?}, pos={:?}",
            output_name, config.mode, config.scale, config.transform, config.position,
        );
    }

    /// Render a frame to the given output
    /// Render a single output via DRM. Returns the render result.
    ///
    /// This only handles the GPU render + DRM queue. State transitions, frame
    /// callbacks, screencopy, and screencast are handled by `Ewm::redraw()`.
    pub(crate) fn render(
        &mut self,
        ewm: &mut Ewm,
        output: &smithay::output::Output,
        target_presentation_time: Duration,
    ) -> super::RenderResult {
        tracy_span!("drm_render");

        let Some(device) = &self.device else {
            return super::RenderResult::Skipped;
        };

        // Find CRTC for this output
        let Some((&crtc, _)) = device.surfaces.iter().find(|(_, s)| s.output == *output) else {
            return super::RenderResult::Skipped;
        };

        if !device.drm.is_active() {
            // This branch hits any time we try to render while the user has
            // switched to a different VT, so don't print anything here.
            return super::RenderResult::Skipped;
        }

        let render_node = device.render_node;

        let output_scale = Scale::from(output.current_scale().fractional_scale());

        // Get output geometry in global space
        let output_geo = ewm.space.output_geometry(output).unwrap_or_default();
        let output_pos = output_geo.loc;
        let output_size = output_geo.size;

        // Get a renderer from the GPU manager
        let Some(device) = &mut self.device else {
            return super::RenderResult::Skipped;
        };

        let Ok(mut renderer) = device.gpu_manager.single_renderer(&render_node) else {
            warn!("Failed to get renderer from GPU manager");
            return super::RenderResult::Skipped;
        };

        // Collect render elements for this specific output
        let (mut content, cursor) = collect_render_elements_for_output(
            ewm,
            renderer.as_mut(),
            output_scale,
            &self.cursor_buffer,
            output_pos,
            output_size,
            true, // include_cursor
            output,
            RenderTarget::Output,
        );
        // DRM render needs all elements merged (cursor in front)
        content.splice(0..0, cursor);
        let elements = content;

        // Frame flags for proper plane scanout
        let flags =
            FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY | FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT;

        // Render the frame
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return super::RenderResult::Skipped;
        };

        let render_result = surface.compositor.render_frame::<_, _>(
            renderer.as_mut(),
            &elements,
            [0.1, 0.1, 0.1, 1.0], // Dark gray background
            flags,
        );

        let mut rv = super::RenderResult::Skipped;

        match render_result {
            Ok(result) => {
                // Wait for GPU completion if the kernel can't handle fencing.
                if result.needs_sync() {
                    if let PrimaryPlaneElement::Swapchain(element) = &result.primary_element {
                        if let Err(err) = element.sync.wait() {
                            warn!("error waiting for frame completion: {err:?}");
                        }
                    }
                }

                // Update primary scanout output tracking (for frame callback throttling)
                ewm.update_primary_scanout_output(output, &result.states);

                // Send DMA-BUF feedback to clients (scanout hints for direct display)
                if let Some(feedback) = surface.dmabuf_feedback.as_ref() {
                    ewm.send_dmabuf_feedbacks(output, feedback, &result.states);
                }

                if !result.is_empty {
                    // Collect presentation feedback from surfaces before queueing
                    let presentation_feedbacks =
                        ewm.take_presentation_feedbacks(output, &result.states);
                    let frame_data = (presentation_feedbacks, target_presentation_time);

                    // Queue frame to DRM with presentation feedback data
                    match surface.compositor.queue_frame(frame_data) {
                        Ok(()) => {
                            let output_state = ewm.output_state.get_mut(output).unwrap();

                            trace!(
                                "{}: {} -> WaitingForVBlank",
                                output.name(),
                                output_state.redraw_state
                            );

                            let new_state = RedrawState::WaitingForVBlank {
                                redraw_needed: false,
                            };
                            match std::mem::replace(&mut output_state.redraw_state, new_state) {
                                RedrawState::Idle => unreachable!(),
                                RedrawState::Queued => (),
                                RedrawState::WaitingForVBlank { .. } => unreachable!(),
                                RedrawState::WaitingForEstimatedVBlank(_) => unreachable!(),
                                RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
                                    self.loop_handle.as_ref().unwrap().remove(token);
                                }
                            }

                            output_state.frame_callback_sequence =
                                output_state.frame_callback_sequence.wrapping_add(1);
                            output_state.vblank_tracker.begin_frame();

                            rv = super::RenderResult::Submitted;
                        }
                        Err(err) => {
                            warn!("{}: Error queueing frame: {:?}", output.name(), err);
                        }
                    }
                } else {
                    rv = super::RenderResult::NoDamage;
                }
            }
            Err(err) => {
                warn!("{}: Error rendering frame: {:?}", output.name(), err);
            }
        }

        // Queue estimated VBlank timer when no frame was submitted
        if rv != super::RenderResult::Submitted {
            self.queue_estimated_vblank_timer(output, ewm, target_presentation_time);
        }

        rv
    }

    /// Queue an estimated VBlank timer when no frame was submitted.
    ///
    /// Uses the target presentation time from FrameClock for accurate timing,
    /// falling back to refresh interval if target has already passed.
    fn queue_estimated_vblank_timer(
        &mut self,
        output: &smithay::output::Output,
        ewm: &mut Ewm,
        target_presentation_time: Duration,
    ) {
        let Some(handle) = self.loop_handle.clone() else {
            warn!("No loop handle available for estimated VBlank timer");
            return;
        };

        let Some(device) = &self.device else {
            return;
        };
        // Find CRTC for this output
        let Some((&crtc, _)) = device.surfaces.iter().find(|(_, s)| s.output == *output) else {
            return;
        };

        let Some(output_state) = ewm.output_state.get_mut(output) else {
            return;
        };

        match std::mem::take(&mut output_state.redraw_state) {
            RedrawState::Idle => unreachable!(),
            RedrawState::Queued => (),
            RedrawState::WaitingForVBlank { .. } => unreachable!(),
            RedrawState::WaitingForEstimatedVBlank(token)
            | RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
                output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
                return;
            }
        }

        let now = crate::utils::get_monotonic_time();
        let mut duration = target_presentation_time.saturating_sub(now);

        // Don't set a zero timer — frame callbacks are sent right after render anyway
        if duration.is_zero() {
            duration = output_state
                .frame_clock
                .refresh_interval()
                .unwrap_or(Duration::from_micros(16_667));
        }

        trace!(
            "{}: queueing estimated vblank timer to fire in {duration:?}",
            output.name()
        );

        let token = handle
            .insert_source(Timer::from_duration(duration), move |_, _, state| {
                if let Some(drm) = state.backend.as_drm_mut() {
                    drm.on_estimated_vblank_timer(crtc, &mut state.ewm);
                }
                TimeoutAction::Drop
            })
            .unwrap();
        output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
    }

    /// Run a closure with renderer, cursor buffer, and event loop handle.
    ///
    /// Used for immediate screencopy rendering outside the per-output render loop.
    pub fn with_renderer<F>(&mut self, f: F)
    where
        F: FnOnce(&mut GlesRenderer, &crate::cursor::CursorBuffer, &LoopHandle<'static, State>),
    {
        let Some(ref event_loop) = self.loop_handle else {
            return;
        };
        let event_loop = event_loop.clone();
        let Some(device) = &mut self.device else {
            return;
        };
        let render_node = device.render_node;
        let Ok(mut renderer) = device.gpu_manager.single_renderer(&render_node) else {
            warn!("Failed to get renderer for with_renderer");
            return;
        };
        f(renderer.as_mut(), &self.cursor_buffer, &event_loop);
    }

    /// Process post-render work: screencopy and screencast for an output.
    ///
    /// Acquires the GPU renderer once and uses it for all post-render work
    /// (screencopy + screencast), avoiding repeated mutex locks on the GPU manager.
    pub(crate) fn post_render(&mut self, ewm: &mut Ewm, output: &smithay::output::Output) {
        let Some(ref event_loop) = self.loop_handle else {
            return;
        };
        let event_loop = event_loop.clone();
        let cursor_buffer = &self.cursor_buffer;

        let Some(device) = &mut self.device else {
            return;
        };
        let render_node = device.render_node;
        let Ok(mut renderer) = device.gpu_manager.single_renderer(&render_node) else {
            return;
        };
        let renderer = renderer.as_mut();

        // Process pending screencopy requests (skip setup cost when no requests pending)
        if ewm.screencopy_state.has_pending_for_output(output) {
            process_screencopies_for_output(ewm, renderer, output, cursor_buffer, &event_loop);
        }

        // Render to active screen casts
        #[cfg(feature = "screencast")]
        {
            use crate::utils::get_monotonic_time;

            let output_scale = Scale::from(output.current_scale().fractional_scale());
            let output_geo = ewm.space.output_geometry(output).unwrap_or_default();
            let output_pos = output_geo.loc;
            let output_size = output_geo.size;

            let output_size_physical = output
                .current_mode()
                .map(|m| Size::from((m.size.w, m.size.h)))
                .unwrap_or_else(|| Size::from((1920, 1080)));

            let target_frame_time = get_monotonic_time();

            let mut screen_casts = std::mem::take(&mut ewm.screen_casts);
            let mut sc_elements = None;
            let mut errored_sessions = Vec::new();

            let valid_outputs: std::collections::HashSet<String> =
                ewm.space.outputs().map(|o| o.name()).collect();

            for (session_id, cast) in screen_casts.iter_mut() {
                if cast.has_error() {
                    errored_sessions.push(*session_id);
                    continue;
                }

                if !cast.is_streaming() {
                    continue;
                }

                match &cast.target {
                    crate::dbus::CastTarget::Output { name } => {
                        // Output cast: only render if this is the matching output
                        if !valid_outputs.contains(name) {
                            trace!(output = %name, "skipping orphaned cast");
                            continue;
                        }
                        if *name != output.name() {
                            continue;
                        }

                        if cast.is_resize_pending() {
                            trace!("cast is resize pending, skipping");
                            continue;
                        }

                        if cast.check_time_and_schedule(output, target_frame_time) {
                            continue;
                        }

                        let include_cursor = cast.cursor_mode != 0;

                        let (content, cursor) = sc_elements.get_or_insert_with(|| {
                            collect_render_elements_for_output(
                                ewm,
                                renderer,
                                output_scale,
                                cursor_buffer,
                                output_pos,
                                output_size,
                                include_cursor,
                                output,
                                RenderTarget::Screencast,
                            )
                        });

                        let cursor_location = {
                            let (px, py) = ewm.pointer_location();
                            smithay::utils::Point::<i32, smithay::utils::Physical>::from((
                                px as i32, py as i32,
                            ))
                        };

                        if cast.dequeue_buffer_and_render(
                            renderer,
                            content,
                            cursor,
                            cursor_location,
                            output_size_physical,
                            output_scale,
                        ) {
                            cast.last_frame_time = target_frame_time;
                        }
                    }
                    crate::dbus::CastTarget::Window { id } => {
                        // Window cast: check if the window is on this output
                        let window_output = ewm.window_output_name(*id);
                        if window_output.as_deref() != Some(&output.name()) {
                            continue;
                        }

                        ewm.render_window_for_screen_cast(
                            renderer,
                            cast,
                            *id,
                            output,
                            cursor_buffer,
                            output_scale,
                            target_frame_time,
                        );
                    }
                }
            }

            ewm.screen_casts = screen_casts;

            // Stop errored casts
            for session_id in errored_sessions {
                warn!(session_id, "stopping errored screen cast");
                ewm.stop_cast(session_id);
            }
        }
    }

    /// Handle estimated VBlank timer firing
    pub(crate) fn on_estimated_vblank_timer(&mut self, crtc: crtc::Handle, ewm: &mut Ewm) {
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
        output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);

        match std::mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
            RedrawState::Idle => unreachable!(),
            RedrawState::Queued => unreachable!(),
            RedrawState::WaitingForVBlank { .. } => unreachable!(),
            RedrawState::WaitingForEstimatedVBlank(_) => (),
            RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
                output_state.redraw_state = RedrawState::Queued;
                return;
            }
        }

        if output_state.unfinished_animations_remain {
            ewm.queue_redraw(&output);
        } else {
            ewm.send_frame_callbacks(&output);
        }
    }

    /// Process a VBlank event for a CRTC.
    ///
    /// Handles: frame_submitted with presentation feedback, FrameClock update,
    /// redraw state transitions, and queuing the next redraw or sending frame callbacks.
    pub(crate) fn process_vblank(
        &mut self,
        crtc: crtc::Handle,
        meta: DrmEventMetadata,
        ewm: &mut Ewm,
    ) {
        let now = crate::utils::get_monotonic_time();

        let presentation_time = match meta.time {
            DrmEventTime::Monotonic(time) if !time.is_zero() => time,
            _ => now,
        };

        let Some(device) = &mut self.device else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };
        let output = surface.output.clone();

        let Some(output_state) = ewm.output_state.get_mut(&output) else {
            return;
        };

        // End Tracy frame tracking
        output_state.vblank_tracker.end_frame();

        // Transition state BEFORE frame_submitted(). frame_submitted() may
        // submit a queued frame (generating another VBlank), so the state
        // machine must be settled first.
        let redraw_needed =
            match std::mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
                RedrawState::WaitingForVBlank { redraw_needed } => redraw_needed,
                state @ (RedrawState::Idle
                | RedrawState::Queued
                | RedrawState::WaitingForEstimatedVBlank(_)
                | RedrawState::WaitingForEstimatedVBlankAndQueued(_)) => {
                    error!(
                        "{}: unexpected redraw state on VBlank \
                     (should be WaitingForVBlank); can happen when \
                     resuming from sleep or powering on monitors: {}",
                        output.name(),
                        state
                    );
                    true
                }
            };

        // Record presentation time in frame clock
        output_state.frame_clock.presented(presentation_time);

        // Mark the last frame as submitted and process presentation feedback.
        // This may submit a queued frame internally (generating another VBlank).
        let refresh_interval = output_state.frame_clock.refresh_interval();
        match surface.compositor.frame_submitted() {
            Ok(Some((mut feedback, target_presentation_time))) => {
                let refresh = match refresh_interval {
                    Some(r) => Refresh::Fixed(r),
                    None => Refresh::Unknown,
                };
                let seq = meta.sequence as u64;
                let mut flags = wp_presentation_feedback::Kind::Vsync
                    | wp_presentation_feedback::Kind::HwCompletion;
                if matches!(meta.time, DrmEventTime::Monotonic(t) if !t.is_zero()) {
                    flags.insert(wp_presentation_feedback::Kind::HwClock);
                }
                feedback.presented::<_, smithay::utils::Monotonic>(
                    presentation_time,
                    refresh,
                    seq,
                    flags,
                );
                let _ = target_presentation_time; // available for Tracy plots
            }
            Ok(None) => {}
            Err(err) => {
                warn!("Error marking frame as submitted: {:?}", err);
            }
        }

        if redraw_needed || output_state.unfinished_animations_remain {
            ewm.queue_redraw(&output);
        } else {
            ewm.send_frame_callbacks(&output);
        }
    }

    /// Handle udev device change event (monitor hotplug)
    pub fn on_device_changed(&mut self, ewm: &mut Ewm) {
        if !self.session_active() {
            return;
        }

        let Some(device) = &mut self.device else {
            return;
        };

        // DrmScanner will preserve any existing connector-CRTC mapping.
        let scan_result = match device.drm_scanner.scan_connectors(&device.drm) {
            Ok(x) => x,
            Err(err) => {
                warn!("error scanning connectors: {:?}", err);
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
                        "connector connected: {}-{}",
                        connector.interface().as_str(),
                        connector.interface_id()
                    );
                    added.push((connector, crtc));
                }
                DrmScanEvent::Disconnected {
                    crtc: Some(crtc), ..
                } => {
                    removed.push(crtc);
                }
                _ => (),
            }
        }

        if added.is_empty() && removed.is_empty() {
            return;
        }

        let mut changed = false;

        // Process disconnections first.
        for crtc in removed {
            self.disconnect_output(crtc, ewm);
            changed = true;
        }

        // Re-acquire device reference after disconnections (borrow was
        // consumed by disconnect_output).
        let Some(device) = &self.device else {
            return;
        };

        // Skip laptop panels when the lid is closed and an external monitor
        // is present (mirrors on_lid_state_changed logic). Without this,
        // udev Changed events after lid-close re-report the panel as
        // Connected, causing a connect/disconnect cycle every second.
        let disable_laptop_panels = self.lid_closed && self.has_external_monitor(ewm);

        // Process new connections, skipping CRTCs that already have a
        // surface (guards against spurious scanner re-reports).
        let added: Vec<_> = added
            .into_iter()
            .filter(|(connector, crtc)| {
                if device.surfaces.contains_key(crtc) {
                    return false;
                }
                if disable_laptop_panels {
                    let name = format!(
                        "{}-{}",
                        connector.interface().as_str(),
                        connector.interface_id()
                    );
                    if crate::is_laptop_panel(&name) {
                        return false;
                    }
                }
                true
            })
            .collect();

        for (connector, crtc) in added {
            if let Err(err) = self.connect_output(connector, crtc, ewm) {
                warn!("failed to connect output: {:?}", err);
            } else {
                changed = true;
            }
        }

        // Signal Emacs that output topology is settled so it can
        // re-sync layout, focus, and frame-output parity.
        if changed {
            ewm.queue_event(crate::event::Event::OutputsComplete);
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
        let connector_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
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

        // Read EDID for manufacturer/model/serial
        let (make, model, serial_number) = if let Some(info) =
            smithay_drm_extras::display_info::for_connector(&device.drm, connector.handle())
        {
            (
                info.make().unwrap_or_else(|| "Unknown".to_string()),
                info.model().unwrap_or_else(|| "Unknown".to_string()),
                info.serial().unwrap_or_default(),
            )
        } else {
            ("Unknown".to_string(), "Unknown".to_string(), String::new())
        };

        // Create Smithay output
        let output = Output::new(
            connector_name.clone(),
            PhysicalProperties {
                size: connector
                    .size()
                    .map(|(w, h)| (w as i32, h as i32).into())
                    .unwrap_or_default(),
                subpixel: Subpixel::Unknown,
                make: make.clone(),
                model: model.clone(),
                serial_number: serial_number.clone(),
            },
        );

        let smithay_mode = Mode::from(mode);
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
        let global_id = output.create_global::<State>(display_handle);
        warn!(
            "Created wl_output global for {}: {:?}",
            connector_name, global_id
        );

        // Create DrmCompositor
        let cursor_size = device.drm.cursor_size();
        let compositor = match DrmCompositor::new(
            OutputModeSource::Auto(output.clone()),
            drm_surface,
            None,
            allocator.clone(),
            GbmFramebufferExporter::new(device.gbm.clone(), device.render_node.into()),
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
                    GbmFramebufferExporter::new(device.gbm.clone(), device.render_node.into()),
                    SUPPORTED_COLOR_FORMATS,
                    fallback_formats,
                    cursor_size,
                    Some(device.gbm.clone()),
                )?
            }
        };

        info!("DrmCompositor created for {}", connector_name);

        let refresh_interval = refresh_interval(mode);

        // Calculate position: use config or auto horizontal layout
        let (x_offset, y_offset) = config
            .as_ref()
            .and_then(|c| c.position)
            .unwrap_or((ewm.output_size.w, 0));

        let vblank_throttle =
            VBlankThrottle::new(self.loop_handle.clone().unwrap(), connector_name.clone());

        // Build per-surface DMA-BUF feedback (scanout hints for clients)
        let dmabuf_feedback = match build_surface_dmabuf_feedback(
            &compositor,
            render_formats.clone(),
            device.render_node,
        ) {
            Ok(feedback) => Some(feedback),
            Err(err) => {
                warn!("Failed to build surface DMA-BUF feedback: {:?}", err);
                None
            }
        };

        // Initialize gamma control if hardware supports it
        let mut gamma_props = match GammaProps::new(&device.drm, crtc) {
            Ok(props) => Some(props),
            Err(err) => {
                debug!("no GAMMA_LUT support for {connector_name}: {err:?}");
                None
            }
        };

        // Reset to identity gamma
        if let Some(ref mut gamma_props) = gamma_props {
            if let Err(err) = gamma_props.set_gamma(&device.drm, None) {
                debug!("failed to reset gamma for {connector_name}: {err:?}");
            }
        } else if let Err(err) = set_gamma_for_crtc(&device.drm, crtc, None) {
            debug!("failed to reset legacy gamma for {connector_name}: {err:?}");
        }

        device.surfaces.insert(
            crtc,
            OutputSurface {
                output: output.clone(),
                global_id,
                compositor,
                connector: connector.handle(),
                vblank_throttle,
                dmabuf_feedback,
                gamma_props,
                pending_gamma_change: None,
            },
        );

        // Initialize output state in Ewm (redraw state, refresh interval)
        let logical_size = crate::utils::output_size(&output);
        let mut output_state = OutputState::new(
            &connector_name,
            Some(refresh_interval),
            (logical_size.w as i32, logical_size.h as i32),
        );
        // If the session is locked, mark the new output as locked immediately
        // so it shows the solid color fallback and doesn't block lock confirmation.
        if ewm.is_locked() {
            output_state.lock_render_state = LockRenderState::Locked;
        }
        ewm.output_state.insert(output.clone(), output_state);

        // Check if output should be enabled (skip mapping if disabled)
        let is_enabled = config.as_ref().map(|c| c.enabled).unwrap_or(true);
        if is_enabled {
            ewm.space.map_output(&output, (x_offset, y_offset));
            // Notify Wayland clients (including xwayland-satellite) of the
            // output position via wl_output.geometry. space.map_output only
            // tracks position internally.
            output.change_current_state(None, None, None, Some((x_offset, y_offset).into()));
            info!(
                "Mapped output {} at position ({}, {}), size {}x{}",
                connector_name,
                x_offset,
                y_offset,
                mode.size().0,
                mode.size().1
            );
        } else {
            info!("Output {} connected but disabled by config", connector_name);
        }

        // Build OutputInfo and register via centralized lifecycle method
        let physical_size = connector.size().unwrap_or((0, 0));
        let output_modes: Vec<OutputMode> = connector
            .modes()
            .iter()
            .map(|m| {
                let smithay = Mode::from(*m);
                OutputMode {
                    width: smithay.size.w,
                    height: smithay.size.h,
                    refresh: smithay.refresh,
                    preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
                }
            })
            .collect();

        let applied_scale = super::closest_representable_scale(
            config.as_ref().and_then(|c| c.scale).unwrap_or(1.0),
        );
        let applied_transform = config
            .as_ref()
            .and_then(|c| c.transform)
            .unwrap_or(Transform::Normal);

        let output_info = OutputInfo {
            name: connector_name.clone(),
            make,
            model,
            width_mm: physical_size.0 as i32,
            height_mm: physical_size.1 as i32,
            x: x_offset,
            y: y_offset,
            scale: applied_scale,
            transform: super::transform_to_int(applied_transform),
            modes: output_modes,
        };

        ewm.add_output(&output, output_info);

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

        ewm.remove_output(&surface.output);
    }

    /// Verify all connected outputs have valid wl_output globals.
    /// Re-creates any that are missing (defensive against silent global loss).
    fn verify_output_globals(&mut self) {
        let Some(device) = &mut self.device else {
            return;
        };
        let Some(dh) = &self.display_handle else {
            return;
        };
        for surface in device.surfaces.values_mut() {
            let backend = dh.backend_handle();
            if backend.global_info(surface.global_id.clone()).is_err() {
                warn!(
                    "Output {} has invalid wl_output global {:?}, re-creating",
                    surface.output.name(),
                    surface.global_id
                );
                surface.global_id = surface.output.create_global::<State>(dh);
                warn!(
                    "Re-created wl_output global for {}: {:?}",
                    surface.output.name(),
                    surface.global_id
                );
            }
        }
    }

    /// Handle lid open/close by disconnecting or reconnecting the laptop panel.
    ///
    /// When closed with an external monitor: disconnect laptop panel only.
    /// When closed without external: deactivate all monitors.
    /// When opened: re-scan connectors to reconnect laptop panel.
    pub fn on_lid_state_changed(&mut self, ewm: &mut Ewm) {
        if self.lid_closed {
            if self.has_external_monitor(ewm) {
                // Disconnect laptop panel outputs, keep external displays
                let Some(device) = &self.device else { return };
                let to_disconnect: Vec<crtc::Handle> = device
                    .surfaces
                    .iter()
                    .filter(|(_, surface)| crate::is_laptop_panel(&surface.output.name()))
                    .map(|(crtc, _)| *crtc)
                    .collect();
                for crtc in to_disconnect {
                    self.disconnect_output(crtc, ewm);
                }
            }

            // If no outputs remain (no external monitor), deactivate all
            if ewm.outputs.is_empty() {
                ewm.deactivate_monitors();
            }

            // Kill idle child process (e.g., screensaver) on lid close
            ewm.kill_idle_child();
        } else {
            // Lid opened: reconnect laptop panels that the scanner still knows
            // about but that we disconnected. on_device_changed() won't help
            // here because the scanner sees no state change (the connector was
            // physically connected the whole time).
            let Some(device) = &self.device else { return };
            let to_connect: Vec<(connector::Info, crtc::Handle)> = device
                .drm_scanner
                .crtcs()
                .filter(|(conn, _)| conn.state() == connector::State::Connected)
                .filter(|(_, crtc)| !device.surfaces.contains_key(crtc))
                .filter(|(conn, _)| {
                    let name = format!("{}-{}", conn.interface().as_str(), conn.interface_id());
                    crate::is_laptop_panel(&name)
                })
                .map(|(conn, crtc)| (conn.clone(), crtc))
                .collect();
            for (connector, crtc) in to_connect {
                if let Err(err) = self.connect_output(connector, crtc, ewm) {
                    warn!("Failed to reconnect laptop panel: {:?}", err);
                }
            }
            ewm.activate_monitors();
            ewm.wake_from_idle();
        }

        ewm.output_management_state.output_heads_changed = true;

        ewm.queue_event(crate::event::Event::OutputsComplete);
    }

    fn has_external_monitor(&self, ewm: &Ewm) -> bool {
        ewm.outputs.iter().any(|o| !crate::is_laptop_panel(&o.name))
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
        let device = state.backend.as_drm_mut().unwrap().device.as_mut().unwrap();
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
                        "No available CRTC for connector {}-{}",
                        connector.interface().as_str(),
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

    // Verify all output globals are valid (defensive against silent global loss)
    state.backend.as_drm_mut().unwrap().verify_output_globals();

    info!(
        "Total output area: {}x{} ({} outputs)",
        state.ewm.output_size.w,
        state.ewm.output_size.h,
        state.ewm.outputs.len()
    );

    // Register DRM event notifier for VBlank
    event_loop_handle.insert_source(drm_notifier, |event, metadata, state| {
        match event {
            DrmEvent::VBlank(crtc) => {
                crate::tracy_frame_mark!();
                crate::tracy_span!("on_vblank");

                let now = crate::utils::get_monotonic_time();

                // Extract presentation time from DRM metadata
                let meta = metadata.take().unwrap_or(DrmEventMetadata {
                    time: DrmEventTime::Monotonic(Duration::ZERO),
                    sequence: 0,
                });
                let presentation_time = match meta.time {
                    DrmEventTime::Monotonic(time) => time,
                    DrmEventTime::Realtime(_) => Duration::ZERO,
                };
                let time = if presentation_time.is_zero() {
                    now
                } else {
                    presentation_time
                };

                // Throttle buggy drivers that deliver VBlanks too early
                {
                    let Some(device) = &mut state.backend.as_drm_mut().unwrap().device else {
                        return;
                    };
                    let Some(surface) = device.surfaces.get_mut(&crtc) else {
                        return;
                    };
                    let refresh_interval = state
                        .ewm
                        .output_state
                        .get(&surface.output)
                        .and_then(|s| s.frame_clock.refresh_interval());

                    let seq = meta.sequence;
                    if surface
                        .vblank_throttle
                        .throttle(refresh_interval, time, move |state| {
                            // Re-enter on_vblank with zeroed time (throttled)
                            let meta = DrmEventMetadata {
                                sequence: seq,
                                time: DrmEventTime::Monotonic(Duration::ZERO),
                            };
                            let drm = state.backend.as_drm_mut().unwrap();
                            drm.process_vblank(crtc, meta, &mut state.ewm);
                        })
                    {
                        return; // Throttled — deferred via timer
                    }
                }

                // Process the VBlank normally
                let drm = state.backend.as_drm_mut().unwrap();
                drm.process_vblank(crtc, meta, &mut state.ewm);
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
                                warn!("Recovering stuck redraw state for {}", output.name());
                                output_state.redraw_state = RedrawState::Queued;
                            }
                        }
                    }
                }
            }
        }
    })?;

    info!("DRM initialization complete");

    // Place pointer at center of first output instead of (0, 0).
    state.center_pointer_on_first_output();

    // connect_output already sent output_detected and working_area events
    // for each output. Send the final completion events.
    state.ewm.queue_event(crate::event::Event::OutputsComplete);
    state.ewm.queue_event(crate::event::Event::Ready);
    info!(
        "Sent {} output_detected events, compositor ready",
        state.ewm.outputs.len()
    );

    // Trigger initial render via redraw_queued_outputs (all outputs start in Queued state)
    state.ewm.redraw_queued_outputs(&mut state.backend);

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

    let mut ewm = Ewm::new(display_handle.clone(), event_loop.handle(), true);

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
        lid_closed: false,
        session_notifier_token: None, // Set after registering notifier
        init_sender: Some(init_sender),
        loop_handle: Some(event_loop.handle()),
        cursor_buffer: CursorBuffer::new(),
        display_handle: None, // Set during initialize_drm
        libinput_devices: std::collections::HashSet::new(),
    };

    let mut state = State {
        backend: Backend::Drm(backend),
        ewm,
    };

    // Set up xwayland-satellite for X11 support (on-demand)
    crate::xwayland::satellite::setup(&mut state);

    // Build complete environment
    let mut env_vars = std::collections::HashMap::<String, String>::from([
        ("WAYLAND_DISPLAY".into(), socket_name_str),
        ("XDG_CURRENT_DESKTOP".into(), "ewm".into()),
        ("XDG_SESSION_TYPE".into(), "wayland".into()),
        ("GTK_IM_MODULE".into(), "wayland".into()),
        ("QT_IM_MODULE".into(), "wayland".into()),
    ]);
    if let Some(satellite) = &state.ewm.satellite {
        let display_name = satellite.display_name().to_owned();
        info!("listening on X11 socket: {display_name}");
        env_vars.insert("DISPLAY".into(), display_name);
    } else {
        // SAFETY: still single-threaded at this point
        unsafe { std::env::remove_var("DISPLAY") };
    }

    // Set C-level environment (pgtk needs WAYLAND_DISPLAY for wl_display_connect,
    // GTK/Qt read IM_MODULE at init, satellite needs WAYLAND_DISPLAY to connect)
    // SAFETY: still single-threaded at this point
    unsafe {
        for (k, v) in &env_vars {
            std::env::set_var(k, v);
        }
    }

    // Propagate to D-Bus/systemd so portals and apps launched via D-Bus inherit them
    let import_names: Vec<&str> = env_vars.iter().map(|(k, _)| k.as_str()).collect();
    let import_list = import_names.join(" ");
    match std::process::Command::new("/bin/sh")
        .args([
            "-c",
            &format!(
                "hash systemctl 2>/dev/null && \
                 systemctl --user import-environment {import_list}; \
                 hash dbus-update-activation-environment 2>/dev/null && \
                 dbus-update-activation-environment {import_list}"
            ),
        ])
        .status()
    {
        Ok(status) if !status.success() => {
            warn!("import-environment exited with {}", status);
        }
        Err(e) => {
            warn!("Failed to import-environment: {}", e);
        }
        _ => {}
    }

    // Send to Emacs for process-environment (so child processes inherit)
    crate::module::push_event(crate::event::Event::Environment { vars: env_vars });

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
        use crate::dbus::{
            CastTarget, CompositorToIntrospect, DBusServers, IntrospectToCompositor,
            ScreenCastToCompositor, WindowProperties,
        };
        use smithay::reexports::calloop::channel::Event as ChannelEvent;

        let outputs = state.ewm.dbus_outputs.clone();
        let (dbus_servers, sc_receiver, introspect_receiver, introspect_reply_tx) =
            DBusServers::start(outputs, display_handle.clone());
        // Store D-Bus servers to keep connections alive
        state.ewm.dbus_servers = Some(dbus_servers);
        state.ewm.introspect_reply_tx = Some(introspect_reply_tx);

        // Register the ScreenCast receiver to handle D-Bus messages
        event_loop
            .handle()
            .insert_source(sc_receiver, |event, _, state| {
                if let ChannelEvent::Msg(msg) = event {
                    match msg {
                        ScreenCastToCompositor::StartCast {
                            session_id,
                            target,
                            signal_ctx,
                            cursor_mode,
                        } => {
                            tracing::info!(
                                "StartCast: session={}, target={:?}, cursor_mode={}",
                                session_id,
                                target,
                                cursor_mode,
                            );

                            let alpha = matches!(target, CastTarget::Window { .. });
                            let filter_fourcc = if alpha {
                                smithay::backend::allocator::Fourcc::Argb8888
                            } else {
                                smithay::backend::allocator::Fourcc::Xrgb8888
                            };

                            // Extract modifiers from render formats for the appropriate fourcc
                            let render_formats: Vec<i64> = state
                                .backend
                                .as_drm_mut()
                                .and_then(|drm| drm.device.as_mut())
                                .map(|device| {
                                    let Ok(renderer) =
                                        device.gpu_manager.single_renderer(&device.render_node)
                                    else {
                                        return vec![u64::from(Modifier::Linear) as i64];
                                    };
                                    renderer
                                        .as_ref()
                                        .egl_context()
                                        .dmabuf_render_formats()
                                        .iter()
                                        .filter(|f| f.code == filter_fourcc)
                                        .map(|f| u64::from(f.modifier) as i64)
                                        .collect()
                                })
                                .unwrap_or_else(|| vec![u64::from(Modifier::Linear) as i64]);

                            let pw = state.ewm.pipewire.as_ref();
                            let gbm = state.backend.gbm_device();

                            if let (Some(pw), Some(gbm)) = (pw, gbm) {
                                // Determine size and refresh based on target
                                let cast_info: Option<(smithay::utils::Size<i32, smithay::utils::Physical>, u32)> =
                                    match &target {
                                        CastTarget::Output { name } => {
                                            state
                                                .ewm
                                                .dbus_outputs
                                                .lock()
                                                .unwrap()
                                                .iter()
                                                .find(|o| o.name == *name)
                                                .map(|info| {
                                                    (
                                                        smithay::utils::Size::from((info.width, info.height)),
                                                        info.refresh,
                                                    )
                                                })
                                        }
                                        CastTarget::Window { id } => {
                                            let window = state.ewm.id_windows.get(id);
                                            if let Some(window) = window {
                                                // Use bbox_with_popups for accurate size including popups
                                                let output_name = state.ewm.window_output_name(*id);
                                                let scale = output_name
                                                    .as_ref()
                                                    .and_then(|name| {
                                                        state.ewm.space.outputs()
                                                            .find(|o| o.name() == *name)
                                                    })
                                                    .map(|o| smithay::utils::Scale::from(o.current_scale().fractional_scale()))
                                                    .unwrap_or(smithay::utils::Scale::from(1.0));
                                                let bbox = window.bbox_with_popups().to_physical_precise_up(scale);
                                                let size = bbox.size;
                                                // Find the output this window is on for refresh rate
                                                let refresh = output_name
                                                    .and_then(|name| {
                                                        state
                                                            .ewm
                                                            .space
                                                            .outputs()
                                                            .find(|o| o.name() == name)
                                                            .and_then(|o| o.current_mode())
                                                            .map(|m| (m.refresh / 1000) as u32)
                                                    })
                                                    .unwrap_or(60);
                                                Some((size, refresh))
                                            } else {
                                                tracing::warn!("StartCast: window {} not found", id);
                                                None
                                            }
                                        }
                                    };

                                if let Some((size, refresh)) = cast_info {
                                    use crate::pipewire::stream::Cast;

                                    let event_loop_handle = state
                                        .backend
                                        .as_drm()
                                        .and_then(|drm| drm.loop_handle.clone())
                                        .expect("loop_handle must exist for screencast");

                                    match Cast::new(
                                        pw,
                                        event_loop_handle,
                                        session_id,
                                        gbm,
                                        size,
                                        refresh,
                                        target.clone(),
                                        alpha,
                                        signal_ctx,
                                        render_formats,
                                        cursor_mode,
                                    ) {
                                        Ok(cast) => {
                                            tracing::info!(
                                                "PipeWire stream created for {:?}, waiting for state change",
                                                target,
                                            );
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
            .expect("Failed to register D-Bus ScreenCast receiver");

        // Register the Introspect receiver to handle GetWindows requests
        event_loop
            .handle()
            .insert_source(introspect_receiver, |event, _, state| {
                if let ChannelEvent::Msg(msg) = event {
                    match msg {
                        IntrospectToCompositor::GetWindows => {
                            let mut windows = std::collections::HashMap::new();
                            for (&id, window) in &state.ewm.id_windows {
                                let (title, app_id) = crate::window_title_and_app_id(window);
                                windows.insert(id, WindowProperties { title, app_id });
                            }
                            if let Some(ref tx) = state.ewm.introspect_reply_tx {
                                let _ = tx.send_blocking(CompositorToIntrospect::Windows(windows));
                            }
                        }
                    }
                }
            })
            .expect("Failed to register D-Bus Introspect receiver");

        tracing::info!("D-Bus ScreenCast and Introspect servers started");
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

    // Fallback frame callback timer — safety net for surfaces that somehow
    // didn't receive frame callbacks through the normal render path.
    // Fires every second and sends callbacks to all surfaces unconditionally,
    // relying on FRAME_CALLBACK_THROTTLE (995ms) to prevent busy-looping.
    event_loop.handle().insert_source(
        Timer::from_duration(Duration::from_secs(1)),
        |_, _, state| {
            state.ewm.send_frame_callbacks_on_fallback_timer();
            TimeoutAction::ToDuration(Duration::from_secs(1))
        },
    )?;

    // Register UdevBackend for hotplug detection
    let udev_backend = UdevBackend::new(&seat_name)?;
    event_loop
        .handle()
        .insert_source(udev_backend, |event, _, state| {
            match event {
                UdevEvent::Changed { device_id: _ } => {
                    // Scan for connector changes (redraws queued per-output as needed)
                    state.backend.on_device_changed(&mut state.ewm);
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
    let _libinput_token = event_loop.handle().insert_source(
        libinput_backend,
        move |mut event, _, state| match event {
            InputEvent::DeviceAdded { ref mut device } => {
                apply_libinput_settings(device, &state.ewm.input_configs);
                if let Backend::Drm(drm) = &mut state.backend {
                    drm.libinput_devices.insert(device.clone());
                }
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
                crate::input::handle_pointer_motion::<LibinputInputBackend>(state, event);
                state.ewm.queue_redraw_for_pointer();
            }
            InputEvent::PointerMotionAbsolute { event } => {
                crate::input::handle_pointer_motion_absolute::<LibinputInputBackend>(state, event);
                state.ewm.queue_redraw_for_pointer();
            }
            InputEvent::PointerButton { event } => {
                crate::input::handle_pointer_button::<LibinputInputBackend>(state, event);
            }
            InputEvent::PointerAxis { event } => {
                crate::input::handle_pointer_axis::<LibinputInputBackend>(state, event);
            }
            InputEvent::SwitchToggle { event } => {
                use smithay::backend::input::{
                    Switch, SwitchState, SwitchToggleEvent as SwitchEvt,
                };
                if SwitchEvt::<LibinputInputBackend>::switch(&event) == Some(Switch::Lid) {
                    let is_closed =
                        SwitchEvt::<LibinputInputBackend>::state(&event) == SwitchState::On;
                    info!("Lid {}", if is_closed { "closed" } else { "opened" });
                    state.handle_lid_state(is_closed);
                }
            }
            InputEvent::DeviceRemoved { ref device } => {
                if let Backend::Drm(drm) = &mut state.backend {
                    drm.libinput_devices.remove(device);
                }
            }
            _ => {}
        },
    )?;

    info!("EWM DRM backend started (waiting for session activation)");
    info!("VT switching: Ctrl+Alt+F1-F7");
    info!("Kill combo: Ctrl+Alt+Backspace");

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
