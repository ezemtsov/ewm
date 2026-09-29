//! DRM/libinput backend for running EWM as a standalone Wayland session
//!
//! Inspired by niri's `backend/tty.rs` for DRM initialization, VBlank
//! synchronization, and session pause/resume patterns. This module provides
//! the backend for running directly on hardware without another compositor.
//!
//! # Design Invariants
//!
//! 1. **Deferred DRM initialization**: DRM master can only be acquired when the session is active.
//!    Session activation happens asynchronously via libseat, so we defer all DRM operations until
//!    we receive an ActivateSession event.
//!
//! 2. **Field ordering for Drop**: The order of fields in DrmBackendState and
//!    DrmDeviceState is critical. Surfaces must be dropped before drm/gbm to
//!    avoid use-after-free. See https://github.com/Smithay/smithay/issues/1102
//!
//! 3. **Session notifier cleanup**: The session notifier must be removed from the event loop BEFORE
//!    the session is dropped. This is essential for embedded mode where process exit doesn't clean
//!    up resources automatically.
//!
//! 4. **Per-output rendering**: Each output has independent redraw state and VBlank
//!    synchronization. Outputs never share frame timing.

use std::collections::HashMap;
use std::iter::zip;
use std::num::NonZeroU64;
use std::os::fd::AsFd;

use crate::output_mode::{ConfiguredMode, HSyncPolarity, Modeline, VSyncPolarity};
use crate::tracy_span;
use anyhow::{Context as _, ensure};
use bytemuck::cast_slice_mut;
use drm_ffi::drm_mode_modeinfo;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, error, info, trace, warn};

use smithay::input::tablet::{TabletDescriptor, TabletSeatTrait};
#[cfg(feature = "screencast")]
use smithay::utils::Size;
use smithay::{
    backend::{
        allocator::{
            Modifier,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmEventTime, DrmNode, NodeType,
            compositor::{DrmCompositor, FrameFlags, PrimaryPlaneElement},
            exporter::gbm::GbmFramebufferExporter,
        },
        egl::{EGLDevice, EGLDisplay},
        input::{Event, InputEvent, KeyboardKeyEvent},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportDma, ImportEgl,
            gles::GlesRenderer,
            multigpu::{GpuManager, gbm::GbmGlesBackend},
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, primary_gpu},
    },
    output::{Mode, Output, OutputModeSource, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            EventLoop, LoopHandle, RegistrationToken,
            channel::{Sender, channel},
            timer::{TimeoutAction, Timer},
        },
        drm::control::{
            Device as ControlDevice, Mode as DrmMode, ModeFlags, ModeTypeFlags, ResourceHandle,
            connector, crtc, property,
        },
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::{
            Display, DisplayHandle, backend::GlobalId, protocol::wl_surface::WlSurface,
        },
    },
    utils::{DeviceFd, Scale},
    wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::reexports::wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::wayland::presentation::Refresh;

use crate::{
    Ewm, LockRenderState, OutputInfo, OutputMode, OutputState, RedrawState, State,
    backend::{
        Backend, OutputInfos, apply_enabled_output_config, apply_initial_output_config,
        map_initial_output,
    },
    cursor::{CursorConfig, CursorTextureCache},
    input::{KeyboardAction, apply_libinput_settings, handle_keyboard_event},
    render::{collect_render_elements_for_output, process_screencopies_for_output},
    vblank_throttle::VBlankThrottle,
};

const SUPPORTED_COLOR_FORMATS: [smithay::backend::allocator::Fourcc; 4] = [
    smithay::backend::allocator::Fourcc::Xrgb8888,
    smithay::backend::allocator::Fourcc::Xbgr8888,
    smithay::backend::allocator::Fourcc::Argb8888,
    smithay::backend::allocator::Fourcc::Abgr8888,
];

const CLOSED_LID_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
const MODE_REFRESH_TOLERANCE_MILLIHERTZ: u32 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LidPolicyReason {
    Startup,
    Resume,
    Hotplug,
    LidEvent,
    SettlingTimeout,
}

impl LidPolicyReason {
    fn allows_closed_lid_settle(self) -> bool {
        matches!(self, Self::Startup | Self::Resume)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClosedLidNoExternalAction {
    StartSettling,
    KeepSettling,
    Suspend,
}

fn closed_lid_no_external_action(
    reason: LidPolicyReason,
    settling: bool,
) -> ClosedLidNoExternalAction {
    if settling && reason != LidPolicyReason::SettlingTimeout {
        ClosedLidNoExternalAction::KeepSettling
    } else if reason.allows_closed_lid_settle() {
        ClosedLidNoExternalAction::StartSettling
    } else {
        ClosedLidNoExternalAction::Suspend
    }
}

fn should_suppress_laptop_panels(lid_supported: bool, lid_closed: bool) -> bool {
    lid_supported && lid_closed
}

fn parse_acpi_lid_state(contents: &str) -> Option<bool> {
    let lower = contents.to_ascii_lowercase();
    if lower.contains("closed") {
        Some(true)
    } else if lower.contains("open") {
        Some(false)
    } else {
        None
    }
}

fn current_lid_closed_from_acpi() -> Option<bool> {
    let mut saw_open = false;

    for entry in std::fs::read_dir("/proc/acpi/button/lid").ok()?.flatten() {
        let state_path = entry.path().join("state");
        let Ok(contents) = std::fs::read_to_string(&state_path) else {
            continue;
        };
        match parse_acpi_lid_state(&contents) {
            Some(true) => return Some(true),
            Some(false) => saw_open = true,
            None => {}
        }
    }

    saw_open.then_some(false)
}

pub fn calculate_drm_mode_from_modeline(modeline: &Modeline) -> anyhow::Result<DrmMode> {
    ensure!(
        modeline.hdisplay < modeline.hsync_start,
        "hdisplay {} must be < hsync_start {}",
        modeline.hdisplay,
        modeline.hsync_start
    );
    ensure!(
        modeline.hsync_start < modeline.hsync_end,
        "hsync_start {} must be < hsync_end {}",
        modeline.hsync_start,
        modeline.hsync_end
    );
    ensure!(
        modeline.hsync_end < modeline.htotal,
        "hsync_end {} must be < htotal {}",
        modeline.hsync_end,
        modeline.htotal
    );
    ensure!(
        modeline.vdisplay < modeline.vsync_start,
        "vdisplay {} must be < vsync_start {}",
        modeline.vdisplay,
        modeline.vsync_start
    );
    ensure!(
        modeline.vsync_start < modeline.vsync_end,
        "vsync_start {} must be < vsync_end {}",
        modeline.vsync_start,
        modeline.vsync_end
    );
    ensure!(
        modeline.vsync_end < modeline.vtotal,
        "vsync_end {} must be < vtotal {}",
        modeline.vsync_end,
        modeline.vtotal
    );

    let pixel_clock_kilo_hertz = modeline.clock * 1000.0;
    // Calculated as documented in the CVT 1.2 standard:
    // https://app.box.com/s/vcocw3z73ta09txiskj7cnk6289j356b/file/93518784646
    let vrefresh_hertz = (pixel_clock_kilo_hertz * 1000.0)
        / (modeline.htotal as u64 * modeline.vtotal as u64) as f64;
    ensure!(
        vrefresh_hertz.is_finite(),
        "calculated refresh rate is not finite"
    );
    let vrefresh_rounded = vrefresh_hertz.round() as u32;

    let flags = match modeline.hsync_polarity {
        HSyncPolarity::PHSync => ModeFlags::PHSYNC,
        HSyncPolarity::NHSync => ModeFlags::NHSYNC,
    } | match modeline.vsync_polarity {
        VSyncPolarity::PVSync => ModeFlags::PVSYNC,
        VSyncPolarity::NVSync => ModeFlags::NVSYNC,
    };

    let mode_name = format!(
        "{}x{}@{:.2}",
        modeline.hdisplay, modeline.vdisplay, vrefresh_hertz
    );
    let name = modeinfo_name_slice_from_string(&mode_name);

    // https://www.kernel.org/doc/html/v6.17/gpu/drm-uapi.html#c.drm_mode_modeinfo
    Ok(DrmMode::from(drm_mode_modeinfo {
        clock: pixel_clock_kilo_hertz.round() as u32,
        hdisplay: modeline.hdisplay,
        hsync_start: modeline.hsync_start,
        hsync_end: modeline.hsync_end,
        htotal: modeline.htotal,
        vdisplay: modeline.vdisplay,
        vsync_start: modeline.vsync_start,
        vsync_end: modeline.vsync_end,
        vtotal: modeline.vtotal,
        vrefresh: vrefresh_rounded,
        flags: flags.bits(),
        name,
        // Defaults
        type_: drm_ffi::DRM_MODE_TYPE_USERDEF,
        hskew: 0,
        vscan: 0,
    }))
}

pub fn calculate_mode_cvt(width: u16, height: u16, refresh: f64) -> DrmMode {
    // Cross-checked with sway's implementation:
    // https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/22528542970687720556035790212df8d9bb30bb/backend/drm/util.c#L251

    let options = libdisplay_info::cvt::Options {
        red_blank_ver: libdisplay_info::cvt::ReducedBlankingVersion::None,
        h_pixels: width as i32,
        v_lines: height as i32,
        ip_freq_rqd: refresh,

        // Defaults
        video_opt: false,
        vblank: 0f64,
        additional_hblank: 0,
        early_vsync_rqd: false,
        int_rqd: false,
        margins_rqd: false,
    };
    let cvt_timing = libdisplay_info::cvt::Timing::compute(options);

    let hsync_start = width + cvt_timing.h_front_porch as u16;
    let vsync_start = (cvt_timing.v_lines_rnd + cvt_timing.v_front_porch) as u16;
    let hsync_end = hsync_start + cvt_timing.h_sync as u16;
    let vsync_end = vsync_start + cvt_timing.v_sync as u16;

    let htotal = hsync_end + cvt_timing.h_back_porch as u16;
    let vtotal = vsync_end + cvt_timing.v_back_porch as u16;

    let clock = f64::round(cvt_timing.act_pixel_freq * 1000f64) as u32;
    let vrefresh = f64::round(cvt_timing.act_frame_rate) as u32;

    let flags = drm_ffi::DRM_MODE_FLAG_NHSYNC | drm_ffi::DRM_MODE_FLAG_PVSYNC;

    let mode_name = format!("{width}x{height}@{:.2}", cvt_timing.act_frame_rate);
    let name = modeinfo_name_slice_from_string(&mode_name);

    let drm_ffi_mode = drm_mode_modeinfo {
        clock,

        hdisplay: width,
        hsync_start,
        hsync_end,
        htotal,

        vdisplay: height,
        vsync_start,
        vsync_end,
        vtotal,

        vrefresh,

        flags,
        type_: drm_ffi::DRM_MODE_TYPE_USERDEF,
        name,

        // Defaults
        hskew: 0,
        vscan: 0,
    };

    DrmMode::from(drm_ffi_mode)
}

// Returns a c-string of maximally 31 Rust string chars + null terminator. Excess characters are
// dropped.
fn modeinfo_name_slice_from_string(mode_name: &str) -> [core::ffi::c_char; 32] {
    let mut name: [core::ffi::c_char; 32] = [0; 32];

    for (a, b) in zip(&mut name[..31], mode_name.as_bytes()) {
        // Can be u8 on aarch64 and i8 on x86_64.
        *a = *b as _;
    }

    name
}

fn refresh_hz_to_millihertz(refresh_hz: f64) -> i32 {
    (refresh_hz * 1000.).round() as i32
}

fn mode_refresh_millihertz(mode: DrmMode) -> i32 {
    Mode::from(mode).refresh
}

fn highest_refresh_mode<I>(modes: I) -> Option<DrmMode>
where
    I: Iterator<Item = DrmMode>,
{
    modes.max_by_key(|mode| mode_refresh_millihertz(*mode))
}

fn pick_advertised_mode(modes: &[DrmMode], target: ConfiguredMode) -> Option<DrmMode> {
    let candidates = modes.iter().copied().filter(|mode| {
        mode.size() == (target.width, target.height) && !mode.flags().contains(ModeFlags::INTERLACE)
    });

    if let Some(refresh_hz) = target.refresh {
        let target_millihertz = refresh_hz_to_millihertz(refresh_hz);
        candidates
            .filter_map(|mode| {
                let delta = mode_refresh_millihertz(mode).abs_diff(target_millihertz);
                (delta < MODE_REFRESH_TOLERANCE_MILLIHERTZ).then_some((delta, mode))
            })
            .min_by_key(|(delta, _)| *delta)
            .map(|(_, mode)| mode)
    } else {
        highest_refresh_mode(candidates)
    }
}

/// Pick a DRM mode for `connector` matching `target`, or fall back to preferred.
///
/// A custom target is CVT-generated and bypasses the advertised mode list. The
/// returned bool is true when the requested mode was not found and a preferred
/// mode was substituted.
fn pick_mode(
    connector: &connector::Info,
    target: Option<crate::output_mode::Mode>,
) -> Option<(DrmMode, bool)> {
    let mut mode = None;
    let mut fallback = false;

    if let Some(target) = target {
        let target_mode = target.mode;

        if target.custom {
            if let Some(refresh) = target_mode.refresh {
                let custom_mode =
                    calculate_mode_cvt(target_mode.width, target_mode.height, refresh);
                return Some((custom_mode, false));
            } else {
                warn!("ignoring custom mode without refresh rate");
            }
        }

        mode = pick_advertised_mode(connector.modes(), target_mode);

        if mode.is_none() {
            fallback = true;
        }
    }

    if mode.is_none() {
        // Pick a preferred mode.
        mode = highest_refresh_mode(
            connector
                .modes()
                .iter()
                .copied()
                .filter(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED)),
        );
    }

    if mode.is_none() {
        // Last attempt.
        mode = connector.modes().first().copied();
    }

    mode.map(|m| (m, fallback))
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
    primary_formats: FormatSet,
    primary_render_node: DrmNode,
    surface_render_node: Option<DrmNode>,
    surface_scanout_node: DrmNode,
) -> Result<SurfaceDmabufFeedback, std::io::Error> {
    let surface = compositor.surface();
    let planes = surface.planes();

    let primary_plane_formats = surface.plane_info().formats.clone();
    let primary_or_overlay_formats: FormatSet = primary_plane_formats
        .iter()
        .chain(planes.overlay.iter().flat_map(|p| p.formats.iter()))
        .copied()
        .collect();

    // Limit scanout formats to those we can also render, ensuring a fallback path.
    let mut primary_scanout_formats: Vec<_> = primary_plane_formats
        .intersection(&primary_formats)
        .copied()
        .collect();
    let mut overlay_scanout_formats: Vec<_> = primary_or_overlay_formats
        .intersection(&primary_formats)
        .copied()
        .collect();

    // Cross-device scanout with shared non-linear modifiers can produce glitched
    // output on some AMD iGPU+dGPU systems. Only advertise Linear scanout
    // modifiers when the output is not driven by the primary render node.
    if surface_render_node != Some(primary_render_node) {
        primary_scanout_formats.retain(|format| format.modifier == Modifier::Linear);
        overlay_scanout_formats.retain(|format| format.modifier == Modifier::Linear);
    }

    let builder = DmabufFeedbackBuilder::new(primary_render_node.dev_id(), primary_formats);

    // Scanout feedback: prefer primary-plane formats, then overlay-plane formats.
    let scanout = builder
        .clone()
        .add_preference_tranche(
            surface_scanout_node.dev_id(),
            TrancheFlags::Scanout,
            primary_scanout_formats,
            4..=6,
        )
        .add_preference_tranche(
            surface_scanout_node.dev_id(),
            TrancheFlags::Scanout,
            overlay_scanout_formats,
            4..=6,
        )
        .build()?;

    // On the primary render node, include scanout tranches in render feedback too.
    // On secondary outputs, keep render feedback to the primary renderer only;
    // screen sharing/damage detection relies on clients continuing to allocate
    // buffers compatible with the primary render path.
    let render = if surface_render_node == Some(primary_render_node) {
        scanout.clone()
    } else {
        builder.build()?
    };

    Ok(SurfaceDmabufFeedback { render, scanout })
}

/// Data passed through `queue_frame()` -> `frame_submitted()` for presentation feedback.
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
                if blob_id != 0
                    && let Err(err) = device.destroy_property_blob(blob_id)
                {
                    warn!("error destroying GAMMA_LUT property blob: {err:?}");
                }
            })
            .context("error setting GAMMA_LUT")?;

        // Destroy previous blob after successfully setting the new one
        if let Some(previous) = std::mem::replace(&mut self.previous_blob, blob)
            && let Err(err) = device.destroy_property_blob(previous.get())
        {
            warn!("error destroying previous GAMMA_LUT blob: {err:?}");
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
    primary_path: PathBuf,
    primary_node: DrmNode,
    primary_render_node: DrmNode,
    seat_name: String,
}

/// DRM device state (only present after session activation)
///
/// Field order is critical for safe Drop: surfaces must be dropped before drm/gbm.
/// See https://github.com/Smithay/smithay/issues/1102
#[allow(dead_code)]
struct DrmDeviceState {
    node: DrmNode,
    render_node: DrmNode,
    drm_scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, OutputSurface>,
    notifier_token: RegistrationToken,
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
/// We implement Drop to remove the session notifier from the event
/// loop BEFORE the session is dropped. The notifier holds references to session
/// internals that become invalid after session drop. This is critical for
/// embedded mode where process exit doesn't clean up resources.
#[allow(dead_code)]
pub struct DrmBackendState {
    /// Channel to trigger deferred initialization
    init_sender: Option<Sender<DrmMessage>>,
    /// Event loop handle for scheduling timers
    loop_handle: Option<LoopHandle<'static, State>>,
    /// Cursor texture cache for rendering named cursor images.
    cursor_texture_cache: CursorTextureCache,
    /// Display handle for creating output globals on hotplug
    display_handle: Option<DisplayHandle>,
    /// Pending initialization data - Some until DRM is initialized
    pending: Option<DrmPendingInit>,
    /// Whether this system has a lid device. Desktops bypass clamshell policy.
    pub(crate) lid_supported: bool,
    /// Whether the laptop lid is currently closed (from libinput switch events)
    pub lid_closed: bool,
    /// Bounded wait after closed-lid startup/resume for slow USB-C displays.
    closed_lid_settle_timer: Option<RegistrationToken>,
    /// Logind session: inhibitor fd + D-Bus connection for suspend.
    /// When present, we manage lid-close policy (macOS-like: stay awake with
    /// external monitor, suspend via D-Bus when no display remains).
    logind: Option<LogindState>,
    /// Token for session notifier - must be removed before session drops
    session_notifier_token: Option<RegistrationToken>,
    /// Connected libinput devices for re-applying configuration on change
    libinput_devices: std::collections::HashSet<smithay::reexports::input::Device>,
    /// Primary KMS node and render node. Set once DRM initialization succeeds.
    primary_node: Option<DrmNode>,
    primary_render_node: Option<DrmNode>,
    /// Global GPU manager used by all DRM devices.
    gpu_manager: Option<GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>>,
    /// zwp_linux_dmabuf global; `Some` once the primary render node is up.
    dmabuf_global: Option<DmabufGlobal>,
    /// Backend-owned reporting metadata for connected outputs.
    output_infos: OutputInfos,
    // SAFETY: Fields below are dropped in declaration order.
    // devices must drop before session (surfaces -> drm -> libseat).
    // See https://github.com/Smithay/smithay/issues/1102
    devices: HashMap<DrmNode, DrmDeviceState>,
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
        if let (Some(handle), Some(token)) =
            (&self.loop_handle, self.closed_lid_settle_timer.take())
        {
            handle.remove(token);
        }
        info!("DrmBackendState dropping - session will be released");
        // After this, fields drop in declaration order: gpu_manager -> devices -> libinput ->
        // session
    }
}

impl DrmBackendState {
    pub(crate) fn output_infos(&self) -> OutputInfos {
        self.output_infos.clone()
    }

    pub(crate) fn clear_cursor_texture_cache(&mut self) {
        self.cursor_texture_cache.clear();
    }

    /// Check if the libseat session is currently active (not on another VT).
    fn session_active(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.is_active())
    }

    /// Check if DRM is initialized and ready.
    pub fn is_initialized(&self) -> bool {
        self.pending.is_none() && self.gpu_manager.is_some()
    }

    /// Get the primary render node (if DRM is initialized).
    pub fn render_node(&self) -> Option<DrmNode> {
        self.primary_render_node
    }

    /// Get the primary GBM device for screencasting (if DRM is initialized).
    #[cfg(feature = "screencast")]
    pub fn gbm_device(&self) -> Option<GbmDevice<DrmDeviceFd>> {
        let device = self
            .devices
            .values()
            .find(|d| Some(d.render_node) == self.primary_render_node);
        let device = device.or_else(|| self.primary_node.and_then(|node| self.devices.get(&node)));

        Some(device?.gbm.clone())
    }

    fn find_output_device(&self, output: &Output) -> Option<(DrmNode, crtc::Handle)> {
        self.devices.iter().find_map(|(node, device)| {
            device
                .surfaces
                .iter()
                .find(|(_, surface)| surface.output == *output)
                .map(|(crtc, _)| (*node, *crtc))
        })
    }

    /// DMA-BUF modifiers the renderer can produce for `fourcc`. Used by
    /// screencast format negotiation.
    #[cfg(feature = "screencast")]
    pub fn render_formats_for_fourcc(
        &mut self,
        fourcc: smithay::backend::allocator::Fourcc,
    ) -> Vec<i64> {
        let (Some(gpu_manager), Some(render_node)) =
            (&mut self.gpu_manager, self.primary_render_node)
        else {
            return vec![u64::from(smithay::reexports::gbm::Modifier::Linear) as i64];
        };
        let Ok(renderer) = gpu_manager.single_renderer(&render_node) else {
            return vec![u64::from(smithay::reexports::gbm::Modifier::Linear) as i64];
        };
        renderer
            .as_ref()
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .filter(|f| f.code == fourcc)
            .map(|f| u64::from(f.modifier) as i64)
            .collect()
    }

    /// Perform early buffer import for a surface.
    /// This is crucial for proper dmabuf/EGL buffer import on DRM backends.
    pub fn early_import(&mut self, surface: &WlSurface) {
        let (Some(gpu_manager), Some(render_node)) =
            (&mut self.gpu_manager, self.primary_render_node)
        else {
            debug!("DRM not initialized yet, skipping early_import");
            return;
        };
        // Early import for DMA-BUF surfaces (errors are expected for SHM surfaces).
        let _ = gpu_manager.early_import(render_node, surface);
    }

    /// Handle session pause (VT switch away)
    pub(crate) fn pause(&mut self, ewm: &mut Ewm) {
        debug!("Pausing DRM session");
        self.libinput.suspend();
        for device in self.devices.values_mut() {
            device.drm.pause();
            // Cancel any pending estimated VBlank timers and reset states to Idle.
            for surface in device.surfaces.values() {
                if let Some(output_state) = ewm.output_state.get_mut(&surface.output) {
                    if let RedrawState::WaitingForEstimatedVBlank(token)
                    | RedrawState::WaitingForEstimatedVBlankAndQueued(token) =
                        output_state.redraw_state
                        && let Some(ref handle) = self.loop_handle
                    {
                        handle.remove(token);
                    }
                    output_state.redraw_state = RedrawState::Idle;
                    output_state.unfinished_animations_remain = false;
                }
            }
        }
        ewm.cancel_idle_timer();
    }

    /// Handle session resume (VT switch back)
    pub(crate) fn resume(&mut self, ewm: &mut Ewm) {
        debug!("Resuming DRM session");
        self.cancel_closed_lid_settle_timer();

        self.resume_input();

        // GPUs may have been added or removed during the pause; reconcile the
        // device map with what udev currently reports.
        let mut device_list: HashMap<libc::dev_t, PathBuf> = match self
            .session
            .as_ref()
            .map(|s| s.seat())
            .and_then(|seat| UdevBackend::new(&seat).ok())
        {
            Some(udev) => udev
                .device_list()
                .map(|(id, path)| (id, path.to_owned()))
                .collect(),
            None => HashMap::new(),
        };

        let removed: Vec<libc::dev_t> = self
            .devices
            .keys()
            .copied()
            .filter(|node| !device_list.contains_key(&node.dev_id()))
            .map(|node| node.dev_id())
            .collect();
        for device_id in removed {
            device_list.remove(&device_id);
            self.device_removed(device_id, ewm);
        }

        for device in self.devices.values_mut() {
            device_list.remove(&device.node.dev_id());
            if let Err(err) = device.drm.activate(true) {
                warn!("Error activating DRM device {:?}: {:?}", device.node, err);
            } else {
                info!(
                    "DRM device {:?} activated successfully (DRM master acquired)",
                    device.node
                );
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

                // Apply any pending gamma changes that were queued while session was paused.
                if let Some(ramp) = surface.pending_gamma_change.take() {
                    if let Some(ref mut gamma_props) = surface.gamma_props {
                        if let Err(err) = gamma_props.set_gamma(&device.drm, ramp.as_deref()) {
                            warn!(
                                "error applying pending gamma change for {}: {err:?}",
                                surface.output.name()
                            );
                        }
                    } else {
                        // Legacy fallback.
                        let crtc = surface.compositor.surface().crtc();
                        if let Err(err) = set_gamma_for_crtc(&device.drm, crtc, ramp.as_deref()) {
                            warn!(
                                "error applying pending gamma change for {}: {err:?}",
                                surface.output.name()
                            );
                        }
                    }
                } else {
                    // No pending change; restore previous gamma state.
                    if let Some(ref gamma_props) = surface.gamma_props
                        && let Err(err) = gamma_props.restore_gamma(&device.drm)
                    {
                        warn!(
                            "error restoring gamma for {}: {err:?}",
                            surface.output.name()
                        );
                    }
                }
            }
        }

        // Add devices that appeared during the pause, primary first so later
        // nodes can depend on the primary render node being available.
        let primary_id = self.primary_node.map(|n| n.dev_id());
        let mut new_devices: Vec<_> = device_list.into_iter().collect();
        new_devices.sort_by_key(|(id, _)| if Some(*id) == primary_id { 0 } else { 1 });
        for (device_id, path) in new_devices {
            if let Err(err) = self.device_added(device_id, &path, ewm) {
                warn!("Error adding device {device_id} on resume: {err:?}");
            }
        }

        // Re-scan connectors to detect monitors added/removed during suspend.
        let lid_state_changed = self.refresh_lid_state_from_acpi();
        self.scan_all_device_connectors(ewm);

        // Verify output globals survived the session pause/resume cycle
        self.verify_output_globals();

        let lid_policy_handled = self.lid_supported
            && (self.lid_closed || lid_state_changed)
            && self.apply_lid_policy(ewm, LidPolicyReason::Resume);
        if self.closed_lid_settle_timer.is_some() {
            return;
        }

        // Reactivate monitors in case they were deactivated (e.g., lid closed)
        ewm.activate_monitors();

        // Queue redraws for all outputs to resume rendering
        ewm.queue_redraw_all();

        // Reset idle timers so we don't immediately trigger idle timeout after wake
        ewm.idle_notifier_state.notify_activity(&ewm.seat);
        ewm.reset_idle_timer();

        // Always notify Emacs so it re-syncs layout and focus after resume,
        // even if no output topology changed.
        if !lid_policy_handled {
            ewm.queue_event(crate::event::Event::OutputsComplete);
        }
    }

    /// Re-open the input devices after the session became active.
    pub(crate) fn resume_input(&mut self) {
        if self.libinput.resume().is_err() {
            warn!("Error resuming libinput");
        }
    }

    /// Trigger deferred DRM initialization (called when session becomes active)
    pub(crate) fn trigger_init(&self) {
        if let Some(sender) = &self.init_sender
            && let Err(e) = sender.send(DrmMessage::InitializeDrm)
        {
            warn!("Failed to send DRM init message: {:?}", e);
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
        for device in self.devices.values_mut() {
            for surface in device.surfaces.values_mut() {
                if let Err(err) = surface.compositor.clear() {
                    warn!("Error clearing DRM surface: {:?}", err);
                }
            }
        }
    }

    /// Get gamma ramp size for an output
    pub fn get_gamma_size(&mut self, output: &Output) -> anyhow::Result<u32> {
        let (node, crtc) = self
            .find_output_device(output)
            .context("output not found")?;
        let device = self
            .devices
            .get(&node)
            .context("DRM device not initialized")?;
        let surface = device.surfaces.get(&crtc).context("output not found")?;

        if let Some(ref gamma_props) = surface.gamma_props {
            gamma_props.gamma_size(&device.drm)
        } else {
            // Legacy fallback: read gamma_length from CRTC info.
            let crtc_info = device
                .drm
                .get_crtc(crtc)
                .context("error getting CRTC info")?;
            Ok(crtc_info.gamma_length())
        }
    }

    /// Set gamma ramp for an output (or None to reset to identity)
    pub fn set_gamma(&mut self, output: &Output, ramp: Option<Vec<u16>>) -> anyhow::Result<()> {
        let session_active = self.session_active();
        let (node, crtc) = self
            .find_output_device(output)
            .context("output not found")?;
        let device = self
            .devices
            .get_mut(&node)
            .context("DRM device not initialized")?;
        let surface = device.surfaces.get_mut(&crtc).context("output not found")?;

        // If session is paused, store the change to apply on resume.
        if !session_active {
            trace!("Session paused, queuing gamma change for {}", output.name());
            surface.pending_gamma_change = Some(ramp);
            return Ok(());
        }

        // Apply immediately.
        if let Some(ref mut gamma_props) = surface.gamma_props {
            gamma_props.set_gamma(&device.drm, ramp.as_deref())
        } else {
            // Legacy fallback.
            set_gamma_for_crtc(&device.drm, crtc, ramp.as_deref())
        }
    }
}

impl DrmBackendState {
    /// Apply output configuration for a live output.
    ///
    /// Resolves the final state for mode, scale, transform, and position,
    /// then applies everything in one pass. Updates reporting metadata,
    /// refresh interval, working areas.
    pub fn apply_output_config(&mut self, ewm: &mut Ewm, output_name: &str) {
        let config = match ewm.output_config.get(output_name) {
            Some(c) => c.clone(),
            None => return,
        };

        let output = ewm.connected_output(output_name);
        let Some(output) = output else {
            warn!("apply_output_config: output not found: {}", output_name);
            return;
        };

        // Handle disabled output
        if !config.enabled {
            info!("Disabled output {}", output_name);
            ewm.disable_output(&output);
            return;
        }

        let Some((node, crtc)) = self.find_output_device(&output) else {
            warn!("No DRM surface for output: {}", output_name);
            return;
        };
        let Some(device) = self.devices.get_mut(&node) else {
            warn!("DRM device not found for output: {}", output_name);
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            warn!("No DRM surface for output: {}", output_name);
            return;
        };

        // Resolve and apply DRM mode. A modeline takes precedence over `mode`;
        // when neither is configured, keep the current mode (scale-only change).
        let new_drm_mode = if config.mode.is_some() || config.modeline.is_some() {
            let connector_info = match device.drm.get_connector(surface.connector, false) {
                Ok(info) => info,
                Err(e) => {
                    warn!("Failed to get connector info for {}: {:?}", output_name, e);
                    return;
                }
            };
            let mut mode = None;
            if let Some(modeline) = &config.modeline {
                match calculate_drm_mode_from_modeline(modeline) {
                    Ok(x) => mode = Some(x),
                    Err(err) => warn!(
                        "invalid custom modeline for {}; falling back to advertised modes: {:?}",
                        output_name, err
                    ),
                }
            }
            if let Some(x) = mode {
                Some(x)
            } else if let Some((m, fallback)) = pick_mode(&connector_info, config.mode) {
                if fallback {
                    let t = config.mode.unwrap();
                    warn!(
                        "configured mode {}x{} not found for {}, falling back to preferred",
                        t.mode.width, t.mode.height, output_name
                    );
                }
                Some(m)
            } else {
                warn!("no mode available for {}, keeping current", output_name);
                None
            }
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

        let smithay_mode = new_drm_mode.map(Mode::from);
        apply_enabled_output_config(ewm, &self.output_infos, &output, &config, smithay_mode);

        // Update frame clock refresh interval
        if let Some(drm_mode) = new_drm_mode
            && let Some(output_state) = ewm.output_state.get_mut(&output)
        {
            output_state.frame_clock =
                crate::frame_clock::FrameClock::new(Some(refresh_interval(drm_mode)));
        }

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

        let Some((node, crtc)) = self.find_output_device(output) else {
            return super::RenderResult::Skipped;
        };
        let Some(device) = self.devices.get(&node) else {
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

        // Get a renderer for the GPU that owns this output.
        let Some(gpu_manager) = &mut self.gpu_manager else {
            return super::RenderResult::Skipped;
        };

        let Ok(mut renderer) = gpu_manager.single_renderer(&render_node) else {
            warn!(
                "Failed to get renderer from GPU manager for {:?}",
                render_node
            );
            return super::RenderResult::Skipped;
        };

        // Collect render elements for this specific output
        let (mut content, cursor) = collect_render_elements_for_output(
            ewm,
            renderer.as_mut(),
            output_scale,
            &self.cursor_texture_cache,
            output_pos,
            output_size,
            true, // include_cursor
            true, // include_layers
            output,
        );
        // DRM render needs all elements merged (cursor in front)
        content.splice(0..0, cursor);
        let elements = content;

        // Frame flags for proper plane scanout
        let flags =
            FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY | FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT;

        // Render the frame
        let Some(device) = self.devices.get_mut(&node) else {
            return super::RenderResult::Skipped;
        };
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
                if result.needs_sync()
                    && let PrimaryPlaneElement::Swapchain(element) = &result.primary_element
                    && let Err(err) = element.sync.wait()
                {
                    warn!("error waiting for frame completion: {err:?}");
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

        let Some((node, crtc)) = self.find_output_device(output) else {
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

        // Don't set a zero timer; frame callbacks are sent right after render anyway
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
                    drm.on_estimated_vblank_timer(node, crtc, &mut state.ewm);
                }
                TimeoutAction::Drop
            })
            .unwrap();
        output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
    }

    /// Run a closure with renderer, cursor texture cache, and event loop handle.
    ///
    /// Used for immediate screencopy rendering outside the per-output render loop.
    pub fn with_renderer<F>(&mut self, f: F)
    where
        F: FnOnce(
            &mut GlesRenderer,
            &crate::cursor::CursorTextureCache,
            &LoopHandle<'static, State>,
        ),
    {
        let Some(ref event_loop) = self.loop_handle else {
            return;
        };
        let event_loop = event_loop.clone();
        let (Some(gpu_manager), Some(render_node)) =
            (&mut self.gpu_manager, self.primary_render_node)
        else {
            return;
        };
        let Ok(mut renderer) = gpu_manager.single_renderer(&render_node) else {
            warn!("Failed to get renderer for with_renderer");
            return;
        };
        f(renderer.as_mut(), &self.cursor_texture_cache, &event_loop);
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
        let cursor_texture_cache = &self.cursor_texture_cache;

        let (Some(gpu_manager), Some(render_node)) =
            (&mut self.gpu_manager, self.primary_render_node)
        else {
            return;
        };
        let Ok(mut renderer) = gpu_manager.single_renderer(&render_node) else {
            return;
        };
        let renderer = renderer.as_mut();

        // Process pending screencopy requests (skip setup cost when no requests pending)
        if ewm.screencopy_state.has_pending_for_output(output) {
            process_screencopies_for_output(
                ewm,
                renderer,
                output,
                cursor_texture_cache,
                &event_loop,
            );
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

            ewm.render_for_screen_cast(
                renderer,
                output,
                cursor_texture_cache,
                output_pos,
                output_size,
                output_size_physical,
                output_scale,
                target_frame_time,
            );
            ewm.render_layout_entries_for_screen_cast(
                renderer,
                output,
                cursor_texture_cache,
                output_scale,
                target_frame_time,
            );
        }
    }

    /// Handle estimated VBlank timer firing
    pub(crate) fn on_estimated_vblank_timer(
        &mut self,
        node: DrmNode,
        crtc: crtc::Handle,
        ewm: &mut Ewm,
    ) {
        let Some(device) = self.devices.get(&node) else {
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
        node: DrmNode,
        crtc: crtc::Handle,
        meta: DrmEventMetadata,
        ewm: &mut Ewm,
    ) {
        let now = crate::utils::get_monotonic_time();

        let presentation_time = match meta.time {
            DrmEventTime::Monotonic(time) if !time.is_zero() => time,
            _ => now,
        };

        let Some(device) = self.devices.get_mut(&node) else {
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

        let lid_state_changed = self.refresh_lid_state_from_acpi();
        let changed = self.scan_all_device_connectors(ewm);

        // Signal Emacs that output topology is settled so it can
        // re-sync layout, focus, and frame-output parity.
        if self.lid_supported
            && (lid_state_changed
                || (changed && self.lid_closed)
                || self.closed_lid_settle_timer.is_some())
            && self.apply_lid_policy(ewm, LidPolicyReason::Hotplug)
        {
        } else if changed {
            ewm.queue_event(crate::event::Event::OutputsComplete);
        }
    }

    fn scan_all_device_connectors(&mut self, ewm: &mut Ewm) -> bool {
        let nodes: Vec<_> = self.devices.keys().copied().collect();
        let mut changed = false;
        for node in nodes {
            if self.scan_device_connectors(node, ewm) {
                changed = true;
            }
        }
        changed
    }

    fn scan_device_connectors(&mut self, node: DrmNode, ewm: &mut Ewm) -> bool {
        let Some(device) = self.devices.get_mut(&node) else {
            return false;
        };

        // DrmScanner will preserve any existing connector-CRTC mapping.
        let scan_result = match device.drm_scanner.scan_connectors(&device.drm) {
            Ok(x) => x,
            Err(err) => {
                warn!("error scanning connectors on {:?}: {:?}", node, err);
                return false;
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
                        "connector connected on {:?}: {}-{}",
                        node,
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
            return false;
        }

        let mut changed = false;

        for crtc in removed {
            self.disconnect_output(node, crtc, ewm);
            changed = true;
        }

        let Some(device) = self.devices.get(&node) else {
            return changed;
        };
        let disable_laptop_panels =
            should_suppress_laptop_panels(self.lid_supported, self.lid_closed);
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
            if let Err(err) = self.connect_output(node, connector, crtc, ewm) {
                warn!("failed to connect output on {:?}: {:?}", node, err);
            } else {
                changed = true;
            }
        }

        changed
    }

    /// Connect a new output
    ///
    /// Creates the DRM surface, Smithay output, and DrmCompositor.
    /// Reads `ewm.output_config` for mode/scale/transform/position.
    /// Sends OutputDetected and WorkingArea events to Emacs.
    fn connect_output(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
        ewm: &mut Ewm,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(device) = self.devices.get_mut(&node) else {
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

        // Select mode: modeline takes precedence, then the configured mode, then
        // the connector's preferred mode (handled inside pick_mode).
        let mut mode = None;
        if let Some(modeline) = config.as_ref().and_then(|c| c.modeline.as_ref()) {
            match calculate_drm_mode_from_modeline(modeline) {
                Ok(x) => mode = Some(x),
                Err(err) => warn!(
                    "invalid custom modeline for {}; falling back to advertised modes: {:?}",
                    connector_name, err
                ),
            }
        }
        let (mode, fallback) = match mode {
            Some(x) => (x, false),
            None => pick_mode(&connector, config.as_ref().and_then(|c| c.mode))
                .ok_or("No mode available")?,
        };
        if fallback {
            let t = config.as_ref().and_then(|c| c.mode).unwrap();
            warn!(
                "configured mode {}x{} not found for {}, using preferred",
                t.mode.width, t.mode.height, connector_name
            );
        }

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

        // Get render formats from the GPU manager for the GPU that owns this output.
        let gpu_manager = self
            .gpu_manager
            .as_mut()
            .ok_or("GPU manager not initialized")?;
        let render_formats: FormatSet = {
            let renderer = gpu_manager.single_renderer(&device.render_node)?;
            let raw_render_formats = renderer.as_ref().egl_context().dmabuf_render_formats();

            // Filter out problematic modifiers.
            raw_render_formats
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
                .collect()
        };
        let primary_render_node = self
            .primary_render_node
            .ok_or("Primary render node not initialized")?;
        let primary_formats = if primary_render_node == device.render_node {
            render_formats.clone()
        } else {
            gpu_manager
                .single_renderer(&primary_render_node)?
                .dmabuf_formats()
                .clone()
        };

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
        apply_initial_output_config(&output, smithay_mode, config.as_ref());
        let global_id = output.create_global::<State>(display_handle);
        warn!(
            "Created wl_output global for {}: {:?}",
            connector_name, global_id
        );

        // Create DrmCompositor
        let cursor_size = device.drm.cursor_size();
        let compositor = match DrmCompositor::new(
            OutputModeSource::Auto(output.downgrade()),
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
                    OutputModeSource::Auto(output.downgrade()),
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

        let vblank_throttle =
            VBlankThrottle::new(self.loop_handle.clone().unwrap(), connector_name.clone());

        // Build per-surface DMA-BUF feedback (scanout hints for clients)
        let dmabuf_feedback = match build_surface_dmabuf_feedback(
            &compositor,
            primary_formats,
            primary_render_node,
            Some(device.render_node),
            node,
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

        let (position, is_enabled) = map_initial_output(ewm, &output, config.as_ref());
        if is_enabled {
            info!(
                "Mapped output {} at position ({}, {}), size {}x{}",
                connector_name,
                position.x,
                position.y,
                mode.size().0,
                mode.size().1
            );
        } else {
            info!("Output {} connected but disabled by config", connector_name);
        }

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
                    current: smithay == smithay_mode,
                }
            })
            .collect();

        let output_info = OutputInfo {
            name: connector_name.clone(),
            make,
            model,
            serial: serial_number,
            width_mm: physical_size.0 as i32,
            height_mm: physical_size.1 as i32,
            x: position.x,
            y: position.y,
            scale: output.current_scale().fractional_scale(),
            transform: super::transform_to_int(output.current_transform()),
            modes: output_modes,
        };

        self.output_infos.insert(output_info.clone());
        ewm.add_output(&output, output_info);

        info!("Output connected: {}", connector_name);

        Ok(())
    }

    /// Disconnect an output
    fn disconnect_output(&mut self, node: DrmNode, crtc: crtc::Handle, ewm: &mut Ewm) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };

        let Some(surface) = device.surfaces.remove(&crtc) else {
            return;
        };

        // Remove the wl_output global before dropping the surface.
        // Without this, each reconnect cycle creates a new global while the
        // old one remains advertised to clients, causing duplicates.
        if let Some(dh) = &self.display_handle {
            dh.remove_global::<State>(surface.global_id);
        }

        self.output_infos.remove(&surface.output.name());
        ewm.remove_output(&surface.output);
    }

    /// Verify all connected outputs have valid wl_output globals.
    /// Re-creates any that are missing (defensive against silent global loss).
    fn verify_output_globals(&mut self) {
        let Some(dh) = &self.display_handle else {
            return;
        };
        for device in self.devices.values_mut() {
            for surface in device.surfaces.values_mut() {
                let backend = dh.backend_handle();
                if backend.global_info(surface.global_id.clone()).is_err() {
                    warn!(
                        "Output {} has invalid wl_output global {:?}, re-creating",
                        surface.output.name(),
                        surface.global_id
                    );
                    // Remove the old (invalid) global before creating a new one
                    // to avoid duplicate wl_output globals.
                    dh.remove_global::<State>(surface.global_id.clone());
                    surface.global_id = surface.output.create_global::<State>(dh);
                    warn!(
                        "Re-created wl_output global for {}: {:?}",
                        surface.output.name(),
                        surface.global_id
                    );
                }
            }
        }
    }

    /// Handle lid open/close by reconciling the current connector topology.
    pub fn on_lid_state_changed(&mut self, ewm: &mut Ewm) {
        self.lid_supported = true;
        self.apply_lid_policy(ewm, LidPolicyReason::LidEvent);
    }

    fn apply_lid_policy(&mut self, ewm: &mut Ewm, reason: LidPolicyReason) -> bool {
        if !self.lid_supported {
            self.cancel_closed_lid_settle_timer();
            return false;
        }

        if self.lid_closed {
            if self.has_external_monitor() {
                self.cancel_closed_lid_settle_timer();
                self.disconnect_laptop_panels(ewm);
                ewm.kill_idle_child();
                ewm.activate_monitors();
                ewm.output_management_state.output_heads_changed = true;
                ewm.queue_event(crate::event::Event::OutputsComplete);
                return true;
            }

            match closed_lid_no_external_action(reason, self.closed_lid_settle_timer.is_some()) {
                ClosedLidNoExternalAction::StartSettling => {
                    self.start_closed_lid_settle_timer(ewm);
                    true
                }
                ClosedLidNoExternalAction::KeepSettling => true,
                ClosedLidNoExternalAction::Suspend => {
                    self.cancel_closed_lid_settle_timer();
                    self.suspend_closed_lid_without_external(ewm, reason);
                    ewm.output_management_state.output_heads_changed = true;
                    ewm.queue_event(crate::event::Event::OutputsComplete);
                    true
                }
            }
        } else {
            self.cancel_closed_lid_settle_timer();
            self.reconnect_laptop_panels(ewm);
            ewm.activate_monitors();
            ewm.wake_from_idle();
            ewm.output_management_state.output_heads_changed = true;
            ewm.queue_event(crate::event::Event::OutputsComplete);
            true
        }
    }

    fn disconnect_laptop_panels(&mut self, ewm: &mut Ewm) -> bool {
        let to_disconnect: Vec<(DrmNode, crtc::Handle)> = self
            .devices
            .iter()
            .flat_map(|(node, device)| {
                device
                    .surfaces
                    .iter()
                    .filter(|(_, surface)| crate::is_laptop_panel(&surface.output.name()))
                    .map(|(crtc, _)| (*node, *crtc))
                    .collect::<Vec<_>>()
            })
            .collect();
        let changed = !to_disconnect.is_empty();
        for (node, crtc) in to_disconnect {
            self.disconnect_output(node, crtc, ewm);
        }
        changed
    }

    fn reconnect_laptop_panels(&mut self, ewm: &mut Ewm) -> bool {
        // Lid opened: reconnect laptop panels that the scanner still knows
        // about but that we disconnected. on_device_changed() won't help
        // here because the scanner sees no state change (the connector was
        // physically connected the whole time).
        let to_connect: Vec<(DrmNode, connector::Info, crtc::Handle)> = self
            .devices
            .iter()
            .flat_map(|(node, device)| {
                device
                    .drm_scanner
                    .crtcs()
                    .filter(|(conn, _)| conn.state() == connector::State::Connected)
                    .filter(|(_, crtc)| !device.surfaces.contains_key(crtc))
                    .filter(|(conn, _)| {
                        let name = format!("{}-{}", conn.interface().as_str(), conn.interface_id());
                        crate::is_laptop_panel(&name)
                    })
                    .map(|(conn, crtc)| (*node, conn.clone(), crtc))
                    .collect::<Vec<_>>()
            })
            .collect();
        let changed = !to_connect.is_empty();
        for (node, connector, crtc) in to_connect {
            if let Err(err) = self.connect_output(node, connector, crtc, ewm) {
                warn!("Failed to reconnect laptop panel: {:?}", err);
            }
        }
        changed
    }

    fn refresh_lid_state_from_acpi(&mut self) -> bool {
        let Some(is_closed) = current_lid_closed_from_acpi() else {
            return false;
        };
        self.lid_supported = true;
        if self.lid_closed == is_closed {
            return false;
        }

        self.lid_closed = is_closed;
        info!(
            "Lid {} (ACPI state refresh)",
            if is_closed { "closed" } else { "opened" }
        );
        true
    }

    fn has_external_monitor(&self) -> bool {
        self.output_infos
            .list()
            .iter()
            .any(|o| !crate::is_laptop_panel(&o.name))
    }

    fn start_closed_lid_settle_timer(&mut self, ewm: &mut Ewm) {
        ewm.deactivate_monitors();
        if self.closed_lid_settle_timer.is_some() {
            return;
        }

        info!(
            "Lid closed with no external display; settling for {:?}",
            CLOSED_LID_SETTLE_TIMEOUT
        );

        let Some(handle) = self.loop_handle.clone() else {
            self.suspend_closed_lid_without_external(ewm, LidPolicyReason::Startup);
            return;
        };

        match handle.insert_source(
            Timer::from_duration(CLOSED_LID_SETTLE_TIMEOUT),
            move |_, _, state| {
                if let Some(drm) = state.backend.as_drm_mut() {
                    drm.closed_lid_settle_timer = None;
                    drm.on_closed_lid_settle_timeout(&mut state.ewm);
                }
                TimeoutAction::Drop
            },
        ) {
            Ok(token) => self.closed_lid_settle_timer = Some(token),
            Err(err) => {
                warn!("Failed to start closed-lid settle timer: {:?}", err);
                self.suspend_closed_lid_without_external(ewm, LidPolicyReason::Startup);
            }
        }
    }

    fn cancel_closed_lid_settle_timer(&mut self) {
        if let Some(token) = self.closed_lid_settle_timer.take()
            && let Some(handle) = &self.loop_handle
        {
            handle.remove(token);
        }
    }

    fn on_closed_lid_settle_timeout(&mut self, ewm: &mut Ewm) {
        self.refresh_lid_state_from_acpi();
        self.scan_all_device_connectors(ewm);
        self.apply_lid_policy(ewm, LidPolicyReason::SettlingTimeout);
    }

    fn suspend_closed_lid_without_external(&self, ewm: &mut Ewm, reason: LidPolicyReason) {
        ewm.deactivate_monitors();
        if let Some(logind) = &self.logind {
            info!(
                "Lid closed with no external display after {:?}, suspending",
                reason
            );
            logind.suspend();
        }
    }

    fn device_added(
        &mut self,
        device_id: libc::dev_t,
        path: &Path,
        ewm: &mut Ewm,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.session_active() {
            return Ok(());
        }

        let node = DrmNode::from_dev_id(device_id)?;
        if node.ty() != NodeType::Primary {
            debug!("skipping non-primary DRM node {:?}", node);
            return Ok(());
        }
        if self.devices.contains_key(&node) {
            self.scan_device_connectors(node, ewm);
            return Ok(());
        }

        info!("Adding DRM device {:?} at {:?}", node, path);

        let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
        let session = self.session.as_mut().ok_or("Session not available")?;
        let fd = session.open(path, open_flags)?;
        let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (mut drm, drm_notifier) = DrmDevice::new(device_fd.clone(), true)?;
        let gbm = GbmDevice::new(device_fd.clone())?;

        if let Err(err) = drm.activate(true) {
            warn!("Failed to activate DRM device {:?}: {:?}", node, err);
        }

        let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
        let egl_device = EGLDevice::device_for_display(&egl_display)?;
        if egl_device.is_software() {
            return Err("software EGL renderers are skipped".into());
        }
        let render_node = egl_device.try_get_render_node()?.unwrap_or(node);
        info!("DRM device {:?} uses render node {:?}", node, render_node);

        // A display-only device has no render node, so the startup probe fell
        // back to the primary node, which cannot render.
        if self.primary_render_node == Some(node) {
            self.primary_render_node = Some(render_node);
        }

        let gpu_manager = self
            .gpu_manager
            .as_mut()
            .ok_or("GPU manager not initialized")?;
        let render_node_already_added = self
            .devices
            .values()
            .any(|device| device.render_node == render_node);
        if !render_node_already_added {
            gpu_manager.as_mut().add_node(render_node, gbm.clone())?;
        }

        if self.primary_render_node == Some(render_node) && self.dmabuf_global.is_none() {
            let display_handle = self
                .display_handle
                .as_ref()
                .ok_or("Display handle not available")?;
            let mut renderer = gpu_manager.single_renderer(&render_node)?;
            if let Err(err) = renderer.bind_wl_display(display_handle) {
                // wl_drm is on its way out so this is expected on most modern distros.
                trace!("error binding legacy EGL to wl_display: {err:?}");
            } else {
                debug!("bound legacy EGL to wl_display");
            }

            let dmabuf_formats = renderer.dmabuf_formats().clone();
            if let Ok(default_feedback) =
                DmabufFeedbackBuilder::new(render_node.dev_id(), dmabuf_formats).build()
            {
                let global = ewm
                    .dmabuf_state
                    .create_global_with_default_feedback::<State>(
                        display_handle,
                        &default_feedback,
                    );
                self.dmabuf_global = Some(global);
                info!("Dmabuf global created");
            }
        }

        let handle = self
            .loop_handle
            .clone()
            .ok_or("event loop handle unavailable")?;
        let token =
            handle.insert_source(drm_notifier, move |event, metadata, state| match event {
                DrmEvent::VBlank(crtc) => {
                    crate::tracy_frame_mark!();
                    crate::tracy_span!("on_vblank");

                    let now = crate::utils::get_monotonic_time();
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

                    {
                        let Some(drm) = state.backend.as_drm_mut() else {
                            return;
                        };
                        let Some(device) = drm.devices.get_mut(&node) else {
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
                                let meta = DrmEventMetadata {
                                    sequence: seq,
                                    time: DrmEventTime::Monotonic(Duration::ZERO),
                                };
                                let drm = state.backend.as_drm_mut().unwrap();
                                drm.process_vblank(node, crtc, meta, &mut state.ewm);
                            })
                        {
                            return;
                        }
                    }

                    let drm = state.backend.as_drm_mut().unwrap();
                    drm.process_vblank(node, crtc, meta, &mut state.ewm);
                }
                DrmEvent::Error(error) => {
                    warn!("DRM error on {:?}: {error}", node);
                    let outputs: Vec<_> = state
                        .backend
                        .as_drm()
                        .and_then(|drm| drm.devices.get(&node))
                        .map(|device| device.surfaces.values().map(|s| s.output.clone()).collect())
                        .unwrap_or_default();
                    for output in outputs {
                        if let Some(output_state) = state.ewm.output_state.get_mut(&output)
                            && matches!(
                                output_state.redraw_state,
                                RedrawState::WaitingForVBlank { .. }
                            )
                        {
                            warn!("Recovering stuck redraw state for {}", output.name());
                            output_state.redraw_state = RedrawState::Queued;
                        }
                    }
                }
            })?;

        self.devices.insert(
            node,
            DrmDeviceState {
                node,
                render_node,
                drm_scanner: DrmScanner::new(),
                surfaces: HashMap::new(),
                notifier_token: token,
                drm,
                gbm,
            },
        );

        self.scan_device_connectors(node, ewm);
        Ok(())
    }

    fn device_removed(&mut self, device_id: libc::dev_t, ewm: &mut Ewm) {
        let Ok(node) = DrmNode::from_dev_id(device_id) else {
            return;
        };
        let Some(device) = self.devices.remove(&node) else {
            return;
        };

        let crtcs: Vec<_> = device.surfaces.keys().copied().collect();
        // Put it back while disconnect_output removes surfaces and Wayland globals.
        self.devices.insert(node, device);
        for crtc in crtcs {
            self.disconnect_output(node, crtc, ewm);
        }
        let Some(device) = self.devices.remove(&node) else {
            return;
        };

        if let Some(handle) = &self.loop_handle {
            handle.remove(device.notifier_token);
        }

        if let Some(gpu_manager) = &mut self.gpu_manager {
            let render_node = device.render_node;
            let was_last = !self
                .devices
                .values()
                .any(|other| other.render_node == render_node);
            if was_last {
                if self.primary_render_node == Some(render_node) {
                    debug!("destroying the primary renderer");
                    match gpu_manager.single_renderer(&render_node) {
                        Ok(mut renderer) => renderer.unbind_wl_display(),
                        Err(err) => warn!("error creating renderer during device removal: {err}"),
                    }

                    if let (Some(global), Some(display_handle)) =
                        (self.dmabuf_global.take(), self.display_handle.clone())
                    {
                        ewm.dmabuf_state
                            .disable_global::<State>(&display_handle, &global);
                        if let Some(handle) = &self.loop_handle {
                            let _ = handle.insert_source(
                                Timer::from_duration(Duration::from_secs(10)),
                                move |_, _, state| {
                                    state
                                        .ewm
                                        .dmabuf_state
                                        .destroy_global::<State>(&state.ewm.display_handle, global);
                                    TimeoutAction::Drop
                                },
                            );
                        }

                        for device in self.devices.values_mut() {
                            for surface in device.surfaces.values_mut() {
                                surface.dmabuf_feedback = None;
                            }
                        }
                    }
                }

                gpu_manager.as_mut().remove_node(&render_node);
                let _ = gpu_manager.devices();
            }
        }
    }
}

/// Logind connection with lid-switch inhibitor.
/// Dropping this releases the inhibitor (fd closes).
struct LogindState {
    conn: zbus::blocking::Connection,
    _inhibitor: std::os::fd::OwnedFd,
}

impl LogindState {
    /// Connect to logind and acquire a `handle-lid-switch` inhibitor.
    /// Returns None if logind is unavailable (non-systemd system).
    fn new() -> Option<Self> {
        use zbus::blocking::Connection;
        use zbus::zvariant::OwnedFd as ZbusFd;

        let conn = match Connection::system() {
            Ok(c) => c,
            Err(e) => {
                warn!("No system D-Bus, lid switch managed by system default: {e}");
                return None;
            }
        };

        let reply = conn.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Inhibit",
            &(
                "handle-lid-switch",
                "ewm",
                "Compositor manages lid switch",
                "block",
            ),
        );

        match reply {
            Ok(msg) => match msg.body().deserialize::<ZbusFd>() {
                Ok(fd) => {
                    info!("Acquired logind handle-lid-switch inhibitor");
                    Some(Self {
                        conn,
                        _inhibitor: std::os::fd::OwnedFd::from(fd),
                    })
                }
                Err(e) => {
                    warn!("Failed to deserialize inhibitor fd: {e}");
                    None
                }
            },
            Err(e) => {
                warn!("Failed to acquire lid switch inhibitor: {e}");
                None
            }
        }
    }

    /// Ask logind to suspend the system.
    fn suspend(&self) {
        if let Err(e) = self.conn.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Suspend",
            &(false,),
        ) {
            warn!("Failed to suspend via logind: {e}");
        }
    }
}

/// Initialize DRM devices and set up outputs.
fn initialize_drm(
    state: &mut State,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
    _event_loop_handle: &LoopHandle<'static, State>,
) -> Result<(), Box<dyn std::error::Error>> {
    let pending = {
        let drm_backend = state.backend.as_drm_mut().ok_or("Not a DRM backend")?;
        drm_backend
            .pending
            .take()
            .ok_or("DRM already initialized")?
    };

    info!(
        "Initializing DRM devices (primary {:?}, render {:?})",
        pending.primary_node, pending.primary_render_node
    );

    let api: GbmGlesBackend<GlesRenderer, DrmDeviceFd> = GbmGlesBackend::with_context_priority(
        smithay::backend::egl::context::ContextPriority::High,
    );
    let gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>> = GpuManager::new(api)?;

    {
        let drm_backend = state.backend.as_drm_mut().unwrap();
        drm_backend.display_handle = Some(display_handle.clone());
        drm_backend.primary_node = Some(pending.primary_node);
        drm_backend.primary_render_node = Some(pending.primary_render_node);
        drm_backend.gpu_manager = Some(gpu_manager);
    }

    // Enumerate all DRM primary nodes on the seat, adding the primary device first.
    let udev_backend = UdevBackend::new(&pending.seat_name)?;
    let mut devices: Vec<(libc::dev_t, PathBuf)> = udev_backend
        .device_list()
        .map(|(device_id, path)| (device_id, path.to_owned()))
        .collect();

    devices.sort_by_key(|(device_id, _)| {
        if *device_id == pending.primary_node.dev_id() {
            0
        } else {
            1
        }
    });

    if !devices
        .iter()
        .any(|(device_id, _)| *device_id == pending.primary_node.dev_id())
    {
        devices.insert(0, (pending.primary_node.dev_id(), pending.primary_path));
    }

    for (device_id, path) in devices {
        let drm_backend = state.backend.as_drm_mut().unwrap();
        if let Err(err) = drm_backend.device_added(device_id, &path, &mut state.ewm) {
            warn!(
                "Failed to add DRM device {:?} at {:?}: {:?}",
                device_id, path, err
            );
        }
    }

    // Verify all output globals are valid (defensive against silent global loss).
    state.backend.as_drm_mut().unwrap().verify_output_globals();

    let lid_policy_handled = {
        let drm_backend = state.backend.as_drm_mut().unwrap();
        drm_backend.lid_supported
            && drm_backend.lid_closed
            && drm_backend.apply_lid_policy(&mut state.ewm, LidPolicyReason::Startup)
    };
    let closed_lid_settling = state
        .backend
        .as_drm()
        .is_some_and(|drm| drm.closed_lid_settle_timer.is_some());

    info!(
        "Total output area: {}x{} ({} outputs)",
        state.ewm.output_size.w,
        state.ewm.output_size.h,
        state.backend.output_info_count()
    );

    info!("DRM initialization complete");

    // Place pointer at center of first output instead of (0, 0).
    state.center_pointer_on_first_output();

    if !lid_policy_handled {
        state.ewm.queue_event(crate::event::Event::OutputsComplete);
    }
    info!(
        "Sent {} output_detected events",
        state.backend.output_info_count()
    );

    // Trigger initial render via redraw_queued_outputs (all outputs start in Queued state).
    if !closed_lid_settling {
        state.ewm.redraw_queued_outputs(&mut state.backend);
    }

    Ok(())
}

pub(crate) fn import_activation_environment(env_vars: &HashMap<String, String>) {
    let import_names: Vec<&str> = env_vars.keys().map(|k| k.as_str()).collect();
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
}

/// Run EWM with DRM/libinput backend (module mode only)
pub fn run_drm(cursor_config: CursorConfig) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting EWM with DRM backend (module mode)");
    cursor_config.apply_env();

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

    // Increase the buffer size so that it's harder to crash a frozen client.
    display_handle.set_default_max_buffer_size(1024 * 1024);

    // Initialize Wayland socket - display is moved into event loop source
    let socket_name = Ewm::init_wayland_listener(display, &event_loop.handle())?;
    let socket_name_str = socket_name.to_string_lossy().to_string();
    info!("Wayland socket: {:?}", socket_name);

    let mut ewm = Ewm::new(
        display_handle.clone(),
        event_loop.handle(),
        true,
        cursor_config.clone(),
    );

    // Connect input method relay to ourselves
    ewm.connect_im_relay();

    // Find primary GPU: env override or udev detection
    let gpu_path = match std::env::var("EWM_RENDER_DEVICE") {
        Ok(ref p) if std::path::Path::new(p).exists() => {
            info!("Using render device from EWM_RENDER_DEVICE: {:?}", p);
            std::path::PathBuf::from(p)
        }
        Ok(p) => {
            warn!(
                "EWM_RENDER_DEVICE {:?} not found, falling back to auto-detection",
                p
            );
            primary_gpu(&seat_name)?.ok_or("No GPU found")?
        }
        Err(_) => primary_gpu(&seat_name)?.ok_or("No GPU found")?,
    };
    let configured_node = DrmNode::from_path(&gpu_path)?;
    let primary_node = if configured_node.ty() == NodeType::Primary {
        configured_node
    } else {
        configured_node
            .node_with_type(NodeType::Primary)
            .and_then(Result::ok)
            .ok_or("Configured DRM node has no primary node")?
    };
    let primary_path = primary_node.dev_path().unwrap_or_else(|| gpu_path.clone());
    let primary_render_node = primary_node
        .node_with_type(NodeType::Render)
        .and_then(Result::ok)
        .unwrap_or(primary_node);
    info!(
        "Primary GPU: {:?} (node {:?}, render {:?})",
        primary_path, primary_node, primary_render_node
    );

    // Initialize libinput
    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|()| "Failed to assign seat to libinput")?;
    if !session_active {
        // logind revokes the evdev fds of an inactive session, and only a
        // suspended context re-enumerates them when libinput is resumed.
        libinput.suspend();
    }

    // Create channel for deferred DRM initialization
    let (init_sender, init_receiver) = channel::<DrmMessage>();

    let initial_lid_state = current_lid_closed_from_acpi();

    // Create backend state (owned directly, no Rc<RefCell<>>)
    let backend = DrmBackendState {
        session: Some(session),
        libinput: libinput.clone(),
        pending: Some(DrmPendingInit {
            primary_path: primary_path.clone(),
            primary_node,
            primary_render_node,
            seat_name: seat_name.clone(),
        }),
        lid_supported: initial_lid_state.is_some(),
        lid_closed: initial_lid_state.unwrap_or(false),
        closed_lid_settle_timer: None,
        logind: LogindState::new(),
        session_notifier_token: None, // Set after registering notifier
        init_sender: Some(init_sender),
        loop_handle: Some(event_loop.handle()),
        cursor_texture_cache: CursorTextureCache::default(),
        display_handle: None, // Set during initialize_drm
        libinput_devices: std::collections::HashSet::new(),
        primary_node: None,
        primary_render_node: None,
        gpu_manager: None,
        dmabuf_global: None,
        output_infos: OutputInfos::default(),
        devices: HashMap::new(),
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
    ]);
    env_vars.extend(cursor_config.env_vars());
    if let Some(satellite) = &state.ewm.satellite {
        let display_name = satellite.display_name().to_owned();
        info!("listening on X11 socket: {display_name}");
        env_vars.insert("DISPLAY".into(), display_name);
    } else {
        // SAFETY: still single-threaded at this point
        unsafe { std::env::remove_var("DISPLAY") };
    }

    // Set C-level environment (pgtk needs WAYLAND_DISPLAY for wl_display_connect,
    // satellite needs WAYLAND_DISPLAY to connect)
    // SAFETY: still single-threaded at this point
    unsafe {
        for (k, v) in &env_vars {
            std::env::set_var(k, v);
        }
    }

    // Propagate to D-Bus/systemd so portals and apps launched via D-Bus inherit them
    import_activation_environment(&env_vars);

    // Send to Emacs for process-environment (so child processes inherit)
    state
        .ewm
        .queue_event(crate::event::Event::Environment { vars: env_vars });

    // Initialize D-Bus for screen sharing. The PipeWire connection
    // and its event channel are owned by `Ewm.casting` and were set up
    // in `Ewm::new`. PipeWire itself is created lazily on first cast.
    #[cfg(feature = "screencast")]
    {
        use smithay::reexports::calloop::channel::Event as ChannelEvent;

        use crate::dbus::{CompositorToIntrospect, DBusServers, IntrospectToCompositor};

        let outputs = state.backend.output_infos().shared();
        let (dbus_servers, sc_receiver, introspect_receiver, introspect_reply_tx) =
            DBusServers::start(outputs, display_handle.clone());
        // Store D-Bus servers to keep connections alive
        state.ewm.dbus_servers = Some(dbus_servers);
        state.ewm.introspect_reply_tx = Some(introspect_reply_tx);

        // Register the ScreenCast receiver - dispatches to State::on_screen_cast_msg
        event_loop
            .handle()
            .insert_source(sc_receiver, |event, _, state| {
                if let ChannelEvent::Msg(msg) = event {
                    state.on_screen_cast_msg(msg);
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
                            let windows = state.ewm.introspect_windows();
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

    // Start org.freedesktop.ScreenSaver D-Bus interface for idle inhibition.
    // This allows apps like Firefox and Chrome to inhibit the screensaver via D-Bus.
    let screen_saver =
        crate::dbus_screensaver::ScreenSaver::new(state.ewm.is_fdo_idle_inhibited.clone());
    match screen_saver.start() {
        Ok(conn) => {
            state.ewm.dbus_screensaver_conn = Some(conn);
        }
        Err(err) => {
            warn!("failed to start org.freedesktop.ScreenSaver D-Bus interface: {err:?}");
        }
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
                        .map(|d| !d.is_initialized())
                        .unwrap_or(false)
                    {
                        info!("First session activation - triggering DRM init");
                        state.backend.trigger_init();
                        // trigger_init() covers DRM only.
                        if let Some(drm) = state.backend.as_drm_mut() {
                            drm.resume_input();
                        }
                    } else {
                        state.backend.resume(&mut state.ewm);
                    }
                }
            })?;
    state.backend.as_drm_mut().unwrap().session_notifier_token = Some(session_notifier_token);

    // Fallback frame callback timer for surfaces that somehow
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
        .insert_source(udev_backend, |event, _, state| match event {
            UdevEvent::Changed { device_id } => {
                debug!("UDev device changed: {:?}", device_id);
                state.backend.on_device_changed(&mut state.ewm);
            }
            UdevEvent::Added { device_id, path } => {
                debug!("UDev device added: {:?} at {:?}", device_id, path);
                if let Some(drm) = state.backend.as_drm_mut()
                    && let Err(err) = drm.device_added(device_id, &path, &mut state.ewm)
                {
                    warn!("Failed to add DRM device from udev: {:?}", err);
                }
            }
            UdevEvent::Removed { device_id } => {
                debug!("UDev device removed: {:?}", device_id);
                if let Some(drm) = state.backend.as_drm_mut() {
                    drm.device_removed(device_id, &mut state.ewm);
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
                    // Ready is already out, so nothing else reports this.
                    error!("Failed to initialize DRM: {:?}", e);
                    state.ewm.stop();
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
                if device.has_capability(smithay::reexports::input::DeviceCapability::TabletTool) {
                    if device.size().is_none() {
                        warn!("tablet tool device has no size");
                    }

                    let tablet_seat = state.ewm.seat.tablet_seat();
                    let desc = TabletDescriptor::from(&*device);
                    tablet_seat.add_wp_tablet(&state.ewm.display_handle, &desc);
                }
                if device.has_capability(smithay::reexports::input::DeviceCapability::Touch)
                    && state.ewm.touch.is_none()
                {
                    state.ewm.touch = Some(state.ewm.seat.add_touch());
                }
                if let Backend::Drm(drm) = &mut state.backend {
                    drm.libinput_devices.insert(device.clone());
                }
            }
            InputEvent::Keyboard { event: kb_event } => {
                let action = handle_keyboard_event(
                    state,
                    kb_event.key_code().into(),
                    kb_event.state(),
                    Event::time(&kb_event),
                );
                if let KeyboardAction::ChangeVt(vt) = action {
                    state.backend.change_vt(vt);
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
            InputEvent::GestureSwipeBegin { event } => {
                crate::input::handle_gesture_swipe_begin::<LibinputInputBackend>(state, event);
            }
            InputEvent::GestureSwipeUpdate { event } => {
                crate::input::handle_gesture_swipe_update::<LibinputInputBackend>(state, event);
            }
            InputEvent::GestureSwipeEnd { event } => {
                crate::input::handle_gesture_swipe_end::<LibinputInputBackend>(state, event);
            }
            InputEvent::TouchDown { event } => {
                crate::input::handle_touch_down::<LibinputInputBackend>(state, event);
            }
            InputEvent::TouchMotion { event } => {
                crate::input::handle_touch_motion::<LibinputInputBackend>(state, event);
            }
            InputEvent::TouchUp { event } => {
                crate::input::handle_touch_up::<LibinputInputBackend>(state, event);
            }
            InputEvent::TouchCancel { event } => {
                crate::input::handle_touch_cancel::<LibinputInputBackend>(state, event);
            }
            InputEvent::TouchFrame { event } => {
                crate::input::handle_touch_frame::<LibinputInputBackend>(state, event);
            }
            InputEvent::TabletToolAxis { event } => {
                crate::input::handle_tablet_tool_axis::<LibinputInputBackend>(state, event);
            }
            InputEvent::TabletToolTip { event } => {
                crate::input::handle_tablet_tool_tip::<LibinputInputBackend>(state, event);
            }
            InputEvent::TabletToolProximity { event } => {
                crate::input::handle_tablet_tool_proximity::<LibinputInputBackend>(state, event);
            }
            InputEvent::TabletToolButton { event } => {
                crate::input::handle_tablet_tool_button::<LibinputInputBackend>(state, event);
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
                if device.has_capability(smithay::reexports::input::DeviceCapability::TabletTool) {
                    let tablet_seat = state.ewm.seat.tablet_seat();
                    let desc = TabletDescriptor::from(device);
                    tablet_seat.remove_tablet(&desc);
                    if tablet_seat.count_tablets() == 0 {
                        tablet_seat.clear_tools();
                    }
                }
                let was_touch =
                    device.has_capability(smithay::reexports::input::DeviceCapability::Touch);
                if let Backend::Drm(drm) = &mut state.backend {
                    drm.libinput_devices.remove(device);
                    if was_touch
                        && !drm.libinput_devices.iter().any(|device| {
                            device
                                .has_capability(smithay::reexports::input::DeviceCapability::Touch)
                        })
                    {
                        state.ewm.seat.remove_touch();
                        state.ewm.touch = None;
                    }
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

    // Ready means the socket is up, not that DRM is initialised: an inactive
    // session defers the init above, and the Lisp side would time out waiting.
    state.ewm.queue_event(crate::event::Event::Ready);
    info!("Compositor ready");

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

#[cfg(test)]
mod tests {
    use super::{
        ClosedLidNoExternalAction, LidPolicyReason, ModeFlags, ModeTypeFlags,
        calculate_drm_mode_from_modeline, calculate_mode_cvt, closed_lid_no_external_action,
        drm_mode_modeinfo, mode_refresh_millihertz, parse_acpi_lid_state, pick_advertised_mode,
        should_suppress_laptop_panels,
    };
    use crate::output_mode::{ConfiguredMode, HSyncPolarity, Modeline, VSyncPolarity};

    fn test_mode(width: u16, height: u16, refresh_millihertz: i32) -> super::DrmMode {
        test_mode_with_flags(width, height, refresh_millihertz, ModeFlags::empty())
    }

    fn test_mode_with_flags(
        width: u16,
        height: u16,
        refresh_millihertz: i32,
        flags: ModeFlags,
    ) -> super::DrmMode {
        super::DrmMode::from(drm_mode_modeinfo {
            clock: refresh_millihertz as u32,
            hdisplay: width,
            hsync_start: 900,
            hsync_end: 950,
            htotal: 1000,
            vdisplay: height,
            vsync_start: 700,
            vsync_end: 750,
            vtotal: 1000,
            vrefresh: (refresh_millihertz as f64 / 1000.0).round() as u32,
            flags: flags.bits(),
            type_: 0,
            name: [0; 32],
            hskew: 0,
            vscan: 0,
        })
    }

    // Expected values cross-checked against niri's snapshots and the `cvt` utility.
    #[test]
    fn cvt_mode_matches_reference() {
        let m = calculate_mode_cvt(1920, 1080, 60.0);
        assert_eq!(m.size(), (1920, 1080));
        assert_eq!(m.clock(), 173000);
        assert_eq!(m.hsync(), (2048, 2248, 2576));
        assert_eq!(m.vsync(), (1083, 1088, 1120));
        assert_eq!(m.vrefresh(), 60);
        assert!(m.mode_type().contains(ModeTypeFlags::USERDEF));

        let m = calculate_mode_cvt(1920, 1080, 144.0);
        assert_eq!(m.clock(), 452500);
        assert_eq!(m.hsync(), (2088, 2296, 2672));
        assert_eq!(m.vsync(), (1083, 1088, 1177));
        assert_eq!(m.vrefresh(), 144);
    }

    #[test]
    fn modeline_mode_matches_reference() {
        let modeline = Modeline {
            clock: 173.0,
            hdisplay: 1920,
            hsync_start: 2048,
            hsync_end: 2248,
            htotal: 2576,
            vdisplay: 1080,
            vsync_start: 1083,
            vsync_end: 1088,
            vtotal: 1120,
            hsync_polarity: HSyncPolarity::NHSync,
            vsync_polarity: VSyncPolarity::PVSync,
        };
        let m = calculate_drm_mode_from_modeline(&modeline).unwrap();
        assert_eq!(m.size(), (1920, 1080));
        assert_eq!(m.clock(), 173000);
        assert_eq!(m.hsync(), (2048, 2248, 2576));
        assert_eq!(m.vsync(), (1083, 1088, 1120));
        assert_eq!(m.vrefresh(), 60);
        assert!(m.mode_type().contains(ModeTypeFlags::USERDEF));
    }

    #[test]
    fn modeline_parses_x11_form() {
        let m: Modeline = "173.0 1920 2048 2248 2576 1080 1083 1088 1120 -hsync +vsync"
            .parse()
            .unwrap();
        assert_eq!(m.clock, 173.0);
        assert_eq!(m.hdisplay, 1920);
        assert_eq!(m.htotal, 2576);
        assert_eq!(m.vtotal, 1120);
        assert_eq!(m.hsync_polarity, HSyncPolarity::NHSync);
        assert_eq!(m.vsync_polarity, VSyncPolarity::PVSync);
        assert!("173.0 1920".parse::<Modeline>().is_err());
    }

    #[test]
    fn advertised_mode_with_refresh_picks_closest_precise_refresh() {
        let modes = [test_mode(800, 600, 60_000), test_mode(800, 600, 59_940)];
        let selected = pick_advertised_mode(
            &modes,
            ConfiguredMode {
                width: 800,
                height: 600,
                refresh: Some(60.0),
            },
        )
        .unwrap();

        assert_eq!(mode_refresh_millihertz(selected), 60_000);
    }

    #[test]
    fn advertised_mode_with_refresh_accepts_precise_timing_delta() {
        let modes = [calculate_mode_cvt(1920, 1080, 60.0)];
        assert_ne!(mode_refresh_millihertz(modes[0]), 60_000);

        let selected = pick_advertised_mode(
            &modes,
            ConfiguredMode {
                width: 1920,
                height: 1080,
                refresh: Some(60.0),
            },
        )
        .unwrap();

        assert_eq!(selected.size(), (1920, 1080));
    }

    #[test]
    fn advertised_mode_without_refresh_picks_highest_precise_refresh() {
        let modes = [test_mode(800, 600, 59_940), test_mode(800, 600, 60_000)];
        let selected = pick_advertised_mode(
            &modes,
            ConfiguredMode {
                width: 800,
                height: 600,
                refresh: None,
            },
        )
        .unwrap();

        assert_eq!(mode_refresh_millihertz(selected), 60_000);
    }

    #[test]
    fn advertised_mode_ignores_interlaced_modes() {
        let modes = [
            test_mode_with_flags(800, 600, 30_000, ModeFlags::INTERLACE),
            test_mode(800, 600, 59_940),
        ];
        let selected = pick_advertised_mode(
            &modes,
            ConfiguredMode {
                width: 800,
                height: 600,
                refresh: Some(60.0),
            },
        )
        .unwrap();

        assert_eq!(mode_refresh_millihertz(selected), 59_940);
    }

    #[test]
    fn parses_acpi_lid_state() {
        assert_eq!(parse_acpi_lid_state("state:      closed\n"), Some(true));
        assert_eq!(parse_acpi_lid_state("state:      open\n"), Some(false));
        assert_eq!(parse_acpi_lid_state("state:      unknown\n"), None);
    }

    #[test]
    fn closed_lid_without_external_only_settles_after_startup_or_resume() {
        assert_eq!(
            closed_lid_no_external_action(LidPolicyReason::Startup, false),
            ClosedLidNoExternalAction::StartSettling
        );
        assert_eq!(
            closed_lid_no_external_action(LidPolicyReason::Resume, false),
            ClosedLidNoExternalAction::StartSettling
        );
        assert_eq!(
            closed_lid_no_external_action(LidPolicyReason::Hotplug, false),
            ClosedLidNoExternalAction::Suspend
        );
        assert_eq!(
            closed_lid_no_external_action(LidPolicyReason::LidEvent, false),
            ClosedLidNoExternalAction::Suspend
        );
        assert_eq!(
            closed_lid_no_external_action(LidPolicyReason::SettlingTimeout, true),
            ClosedLidNoExternalAction::Suspend
        );
        assert_eq!(
            closed_lid_no_external_action(LidPolicyReason::Hotplug, true),
            ClosedLidNoExternalAction::KeepSettling
        );
    }

    #[test]
    fn laptop_panel_suppression_requires_lid_support() {
        assert!(!should_suppress_laptop_panels(false, false));
        assert!(!should_suppress_laptop_panels(false, true));
        assert!(!should_suppress_laptop_panels(true, false));
        assert!(should_suppress_laptop_panels(true, true));
    }
}
