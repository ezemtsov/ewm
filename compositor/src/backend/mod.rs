//! Backend abstraction layer
//!
//! This module provides backend implementations for EWM:
//!
//! - **DRM backend** (`drm`): For running EWM standalone on TTY with real hardware.
//!   Requires DRM master access and works with physical displays.
//!
//! - **Headless backend** (`headless`): For testing without hardware access.
//!   Uses software rendering and virtual outputs for CI/integration testing.
//!
//! # Design Invariants
//!
//! 1. **Backend isolation**: Each backend owns its renderer and output management.
//!    The compositor core (Ewm) is backend-agnostic and works through the common
//!    trait interface.
//!
//! 2. **Output state separation**: Redraw state is stored in `Ewm::output_state`,
//!    not in the backend. This allows backend-agnostic redraw scheduling.
//!
//! 3. **Render element collection**: Both backends use the same `collect_render_elements_for_output`
//!    function, ensuring consistent rendering behavior across backends.

pub mod drm;
pub mod headless;

pub use drm::DrmBackendState;
pub use headless::HeadlessBackend;

use crate::Ewm;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Transform;
use std::time::Duration;

/// Result of a backend render operation.
///
/// The backend only handles the GPU/DRM render, and `Ewm::redraw()`
/// orchestrates state transitions based on the result.
#[derive(Debug, PartialEq, Eq)]
pub enum RenderResult {
    /// The frame was submitted to the backend for presentation.
    Submitted,
    /// Rendering succeeded, but there was no damage.
    NoDamage,
    /// The frame was not rendered/submitted, due to an error or otherwise.
    Skipped,
}

/// Backend abstraction enum
///
/// Allows the compositor to run with different backends while maintaining
/// a common interface for core operations like redraw processing.
///
/// # Usage
///
/// ```ignore
/// let backend = Backend::Headless(HeadlessBackend::new());
/// backend.redraw_queued_outputs(&mut ewm);
/// ```
pub enum Backend {
    /// DRM backend for hardware rendering on TTY
    Drm(DrmBackendState),
    /// Headless backend for testing without hardware
    Headless(HeadlessBackend),
}

impl Backend {
    /// Render a single output. Returns the render result.
    ///
    /// This only handles the GPU/DRM render. State transitions, frame callbacks,
    /// screencopy, and screencast are handled by `Ewm::redraw()`.
    pub fn render(
        &mut self,
        ewm: &mut Ewm,
        output: &Output,
        target_presentation_time: Duration,
    ) -> RenderResult {
        match self {
            Backend::Drm(drm) => drm.render(ewm, output, target_presentation_time),
            Backend::Headless(headless) => headless.render(ewm, output),
        }
    }

    /// Process post-render work for an output (screencopy, screencast).
    ///
    /// Called by `Ewm::redraw()` after a successful render. Requires backend-specific
    /// renderer access, so it lives here rather than on Ewm.
    pub fn post_render(&mut self, ewm: &mut Ewm, output: &Output) {
        match self {
            Backend::Drm(drm) => drm.post_render(ewm, output),
            Backend::Headless(_) => {
                // No post-render work for headless
            }
        }
    }

    /// Run a closure with renderer, cursor buffer, and event loop handle.
    ///
    /// Used for immediate screencopy rendering outside the per-output render loop.
    /// No-op for headless backend.
    pub fn with_renderer<F>(&mut self, f: F)
    where
        F: FnOnce(
            &mut smithay::backend::renderer::gles::GlesRenderer,
            &crate::cursor::CursorBuffer,
            &smithay::reexports::calloop::LoopHandle<'static, crate::State>,
        ),
    {
        match self {
            Backend::Drm(drm) => drm.with_renderer(f),
            Backend::Headless(_) => {}
        }
    }

    /// Check if any output has a redraw queued
    pub fn has_queued_redraws(&self, ewm: &Ewm) -> bool {
        match self {
            Backend::Drm(drm) => drm.has_queued_redraws(ewm),
            Backend::Headless(headless) => headless.has_queued_redraws(ewm),
        }
    }

    /// Perform early buffer import for a surface
    ///
    /// This is crucial for DMA-BUF/EGL buffer import on DRM backends.
    /// No-op for headless backend.
    pub fn early_import(&mut self, surface: &WlSurface) {
        match self {
            Backend::Drm(drm) => drm.early_import(surface),
            Backend::Headless(_) => {
                // No early import needed for headless
            }
        }
    }

    /// Get the DRM backend if this is a DRM backend
    ///
    /// Returns `None` for headless backend. Use this for DRM-specific
    /// operations like VT switching or session management.
    pub fn as_drm(&self) -> Option<&DrmBackendState> {
        match self {
            Backend::Drm(drm) => Some(drm),
            Backend::Headless(_) => None,
        }
    }

    /// Get mutable access to the DRM backend
    pub fn as_drm_mut(&mut self) -> Option<&mut DrmBackendState> {
        match self {
            Backend::Drm(drm) => Some(drm),
            Backend::Headless(_) => None,
        }
    }

    /// Get the headless backend if this is a headless backend
    pub fn as_headless(&self) -> Option<&HeadlessBackend> {
        match self {
            Backend::Drm(_) => None,
            Backend::Headless(headless) => Some(headless),
        }
    }

    /// Get mutable access to the headless backend
    pub fn as_headless_mut(&mut self) -> Option<&mut HeadlessBackend> {
        match self {
            Backend::Drm(_) => None,
            Backend::Headless(headless) => Some(headless),
        }
    }

    /// Check if this is a DRM backend
    pub fn is_drm(&self) -> bool {
        matches!(self, Backend::Drm(_))
    }

    /// Check if this is a headless backend
    pub fn is_headless(&self) -> bool {
        matches!(self, Backend::Headless(_))
    }

    /// Get the GBM device for screencasting
    ///
    /// Returns `None` for headless backend or if DRM is not initialized.
    #[cfg(feature = "screencast")]
    pub fn gbm_device(
        &self,
    ) -> Option<smithay::backend::allocator::gbm::GbmDevice<smithay::backend::drm::DrmDeviceFd>>
    {
        match self {
            Backend::Drm(drm) => drm.gbm_device(),
            Backend::Headless(_) => None,
        }
    }

    /// Apply stored output configuration for the named output.
    ///
    /// Looks up `ewm.output_config` and applies mode, scale, transform,
    /// position, and enabled state in one pass. Called when Emacs sends
    /// a `ConfigureOutput` command.
    pub fn apply_output_config(&mut self, ewm: &mut Ewm, output_name: &str) {
        match self {
            Backend::Drm(drm) => drm.apply_output_config(ewm, output_name),
            Backend::Headless(headless) => headless.apply_output_config(ewm, output_name),
        }
    }

    // --- DRM-specific methods (panic on Headless) ---
    // These are only called from DRM backend callbacks

    /// Handle session pause (VT switch away)
    ///
    /// # Panics
    /// Panics if called on Headless backend.
    pub fn pause(&mut self, ewm: &mut Ewm) {
        match self {
            Backend::Drm(drm) => drm.pause(ewm),
            Backend::Headless(_) => panic!("pause() called on Headless backend"),
        }
    }

    /// Handle session resume (VT switch back)
    ///
    /// # Panics
    /// Panics if called on Headless backend.
    pub fn resume(&mut self, ewm: &mut Ewm) {
        match self {
            Backend::Drm(drm) => drm.resume(ewm),
            Backend::Headless(_) => panic!("resume() called on Headless backend"),
        }
    }

    /// Trigger deferred DRM initialization
    ///
    /// # Panics
    /// Panics if called on Headless backend.
    pub fn trigger_init(&self) {
        match self {
            Backend::Drm(drm) => drm.trigger_init(),
            Backend::Headless(_) => panic!("trigger_init() called on Headless backend"),
        }
    }

    /// Change to a different VT (virtual terminal)
    ///
    /// # Panics
    /// Panics if called on Headless backend.
    pub fn change_vt(&mut self, vt: i32) {
        match self {
            Backend::Drm(drm) => drm.change_vt(vt),
            Backend::Headless(_) => panic!("change_vt() called on Headless backend"),
        }
    }

    /// Handle udev device change event (monitor hotplug)
    ///
    /// # Panics
    /// Panics if called on Headless backend.
    pub fn on_device_changed(&mut self, ewm: &mut Ewm) {
        match self {
            Backend::Drm(drm) => drm.on_device_changed(ewm),
            Backend::Headless(_) => panic!("on_device_changed() called on Headless backend"),
        }
    }

    /// Re-apply libinput configuration to all connected devices.
    /// No-op for headless backend.
    pub fn reapply_libinput_config(&mut self, configs: &[crate::input::InputConfigEntry]) {
        match self {
            Backend::Drm(drm) => drm.reapply_libinput_config(configs),
            Backend::Headless(_) => {}
        }
    }

}

/// Round scale to the nearest value representable by the fractional-scale
/// protocol (precision is N/120). E.g. 1.5 → 180/120 = 1.5 (exact),
/// 1.3333 → 160/120 = 1.33333...
pub fn closest_representable_scale(scale: f64) -> f64 {
    const FRACTIONAL_SCALE_DENOM: f64 = 120.0;
    (scale * FRACTIONAL_SCALE_DENOM).round() / FRACTIONAL_SCALE_DENOM
}

/// Convert integer to Smithay Transform.
/// 0=Normal, 1=90, 2=180, 3=270, 4=Flipped, 5=Flipped90, 6=Flipped180, 7=Flipped270.
pub fn int_to_transform(value: i32) -> Transform {
    match value {
        1 => Transform::_90,
        2 => Transform::_180,
        3 => Transform::_270,
        4 => Transform::Flipped,
        5 => Transform::Flipped90,
        6 => Transform::Flipped180,
        7 => Transform::Flipped270,
        _ => Transform::Normal,
    }
}

/// Convert Smithay Transform to integer.
pub fn transform_to_int(transform: Transform) -> i32 {
    match transform {
        Transform::Normal => 0,
        Transform::_90 => 1,
        Transform::_180 => 2,
        Transform::_270 => 3,
        Transform::Flipped => 4,
        Transform::Flipped90 => 5,
        Transform::Flipped180 => 6,
        Transform::Flipped270 => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_closest_representable_scale() {
        // Exact values
        assert_eq!(closest_representable_scale(1.0), 1.0); // 120/120
        assert_eq!(closest_representable_scale(1.5), 1.5); // 180/120
        assert_eq!(closest_representable_scale(2.0), 2.0); // 240/120
        assert_eq!(closest_representable_scale(1.25), 1.25); // 150/120

        // Non-representable values get rounded
        let rounded = closest_representable_scale(1.77);
        assert!((rounded - 212.0 / 120.0).abs() < 1e-10); // 212/120

        let rounded = closest_representable_scale(1.3333);
        assert!((rounded - 160.0 / 120.0).abs() < 1e-10); // 160/120
    }
}
