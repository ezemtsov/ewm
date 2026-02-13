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

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use crate::Ewm;

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
    /// Process all outputs that have queued redraws
    pub fn redraw_queued_outputs(&mut self, ewm: &mut Ewm) {
        match self {
            Backend::Drm(drm) => drm.redraw_queued_outputs(ewm),
            Backend::Headless(headless) => headless.redraw_queued_outputs(ewm),
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
}
