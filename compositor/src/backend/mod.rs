//! Backend abstraction layer
//!
//! This module provides a unified interface for different compositor backends
//! (Winit for nested sessions, DRM for standalone TTY sessions).

pub mod drm;
pub mod winit;

use std::cell::RefCell;
use std::rc::Rc;

use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

pub use drm::DrmBackendState;
pub use winit::WinitBackend;

/// Unified backend enum for runtime dispatch
#[allow(dead_code)]
pub enum Backend {
    /// Winit backend for running nested inside another compositor
    Winit(WinitBackend),
    /// DRM backend for running standalone on TTY
    Drm(Rc<RefCell<DrmBackendState>>),
}

#[allow(dead_code)]
impl Backend {
    /// Check if this is the DRM backend
    pub fn is_drm(&self) -> bool {
        matches!(self, Backend::Drm(_))
    }

    /// Queue a redraw for all outputs (DRM only, no-op for Winit)
    pub fn queue_redraw(&self) {
        if let Backend::Drm(state) = self {
            state.borrow_mut().queue_redraw();
        }
    }

    /// Queue a redraw for a specific output only (DRM only, no-op for Winit)
    pub fn queue_redraw_for_output(&self, output: &Output) {
        if let Backend::Drm(state) = self {
            state.borrow_mut().queue_redraw_for_output(output);
        }
    }

    /// Perform early buffer import (DRM only, no-op for Winit)
    pub fn early_import(&self, surface: &WlSurface) {
        if let Backend::Drm(state) = self {
            state.borrow_mut().early_import(surface);
        }
    }
}

/// Check if we're running inside another compositor/display server
pub fn is_nested() -> bool {
    std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some()
        || std::env::var("DISPLAY")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some()
}
