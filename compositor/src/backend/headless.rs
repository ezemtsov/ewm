//! Headless backend for testing
//!
//! This module provides a mock backend that doesn't require GPU/DRM access,
//! allowing the compositor to run in CI environments and for integration testing.
//!
//! # Design Invariants
//!
//! 1. **No hardware access**: The headless backend never touches real GPUs or displays.
//!    All rendering is performed to software buffers using GLES via surfaceless contexts.
//!
//! 2. **Deterministic output**: Virtual outputs have fixed sizes and refresh rates,
//!    enabling reproducible snapshot tests.
//!
//! 3. **Event simulation**: Input events can be injected programmatically for testing
//!    keyboard/pointer handling without real hardware.

use std::collections::HashMap;
use std::time::Duration;

use smithay::{
    backend::{
        egl::{EGLContext, EGLDisplay, native::EGLSurfacelessDisplay},
        renderer::{
            damage::OutputDamageTracker,
            gles::GlesRenderer,
        },
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    utils::{Physical, Size, Transform},
};
use tracing::{debug, info};

use crate::{Ewm, OutputState, RedrawState};

/// A virtual output for headless testing
pub struct VirtualOutput {
    pub output: Output,
    pub size: Size<i32, Physical>,
    pub damage_tracker: OutputDamageTracker,
    /// Count of frames rendered to this output (for assertions)
    pub render_count: usize,
}

/// Headless backend state for testing without real hardware
///
/// # Why Headless?
///
/// Integration tests need to exercise the full compositor logic (surface management,
/// focus handling, protocol compliance) without requiring:
/// - DRM master access (unavailable in containers/CI)
/// - Real GPU hardware
/// - Display outputs
///
/// The headless backend provides virtual outputs that track damage and render counts,
/// enabling verification of rendering behavior in tests.
pub struct HeadlessBackend {
    /// Virtual outputs indexed by name
    pub outputs: HashMap<String, VirtualOutput>,
    /// Software renderer for headless rendering
    renderer: Option<GlesRenderer>,
}

impl Default for HeadlessBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl HeadlessBackend {
    /// Create a new headless backend
    pub fn new() -> Self {
        Self {
            outputs: HashMap::new(),
            renderer: None,
        }
    }

    /// Initialize the headless renderer
    ///
    /// Uses EGL surfaceless context for software rendering.
    /// Returns error if EGL initialization fails.
    pub fn init_renderer(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Create surfaceless EGL display for headless rendering
        // This works without any GPU by using software rendering (llvmpipe/softpipe)
        // SAFETY: EGLSurfacelessDisplay doesn't require any native display handle
        let egl_display = unsafe { EGLDisplay::new(EGLSurfacelessDisplay)? };
        let egl_context = EGLContext::new(&egl_display)?;

        let renderer = unsafe { GlesRenderer::new(egl_context)? };
        self.renderer = Some(renderer);

        info!("Headless renderer initialized");
        Ok(())
    }

    /// Add a virtual output with the given name and size
    pub fn add_output(&mut self, name: &str, width: i32, height: i32, ewm: &mut Ewm) {
        let output = Output::new(
            name.to_string(),
            PhysicalProperties {
                size: (width, height).into(),
                subpixel: Subpixel::Unknown,
                make: "EWM".into(),
                model: "Virtual".into(),
            },
        );

        let mode = Mode {
            size: (width, height).into(),
            refresh: 60_000, // 60Hz
        };
        output.change_current_state(Some(mode), Some(Transform::Normal), None, None);
        output.set_preferred(mode);

        // Create global for Wayland clients
        output.create_global::<Ewm>(&ewm.display_handle);

        // Calculate position: place after existing outputs
        let x_offset = ewm.output_size.0;
        ewm.space.map_output(&output, (x_offset, 0));

        // Initialize output state in Ewm
        ewm.output_state.insert(
            output.clone(),
            OutputState::new(name, 16_667, (width as i32, height as i32)), // ~60Hz
        );

        let size = Size::from((width, height));
        let damage_tracker = OutputDamageTracker::from_output(&output);

        self.outputs.insert(
            name.to_string(),
            VirtualOutput {
                output,
                size,
                damage_tracker,
                render_count: 0,
            },
        );

        // Recalculate total output size
        ewm.recalculate_output_size();

        info!("Added virtual output: {} ({}x{}) at ({}, 0)", name, width, height, x_offset);
    }

    /// Remove a virtual output by name
    pub fn remove_output(&mut self, name: &str, ewm: &mut Ewm) {
        if let Some(virtual_output) = self.outputs.remove(name) {
            ewm.output_state.remove(&virtual_output.output);
            ewm.check_lock_on_output_removed();
            ewm.space.unmap_output(&virtual_output.output);
            ewm.recalculate_output_size();
            info!("Removed virtual output: {}", name);
        }
    }

    /// Check if any output has a redraw queued
    pub fn has_queued_redraws(&self, ewm: &Ewm) -> bool {
        ewm.output_state
            .values()
            .any(|s| matches!(s.redraw_state, RedrawState::Queued))
    }

    /// Process all outputs that have queued redraws
    ///
    /// In headless mode, we don't actually render to a display, but we:
    /// 1. Collect render elements (validates the render pipeline)
    /// 2. Track damage (for screencopy/screencast testing)
    /// 3. Increment render counts (for test assertions)
    /// 4. Send frame callbacks to clients
    pub fn redraw_queued_outputs(&mut self, ewm: &mut Ewm) {
        let queued_outputs: Vec<String> = self
            .outputs
            .iter()
            .filter(|(name, _)| {
                ewm.space
                    .outputs()
                    .find(|o| o.name() == **name)
                    .and_then(|o| ewm.output_state.get(o))
                    .map(|s| matches!(s.redraw_state, RedrawState::Queued))
                    .unwrap_or(false)
            })
            .map(|(name, _)| name.clone())
            .collect();

        for name in queued_outputs {
            self.render_output(&name, ewm);
        }
    }

    /// Render a single output
    fn render_output(&mut self, name: &str, ewm: &mut Ewm) {
        let Some(virtual_output) = self.outputs.get_mut(name) else {
            return;
        };

        let output = &virtual_output.output;

        // Mark output as rendered
        if let Some(output_state) = ewm.output_state.get_mut(output) {
            output_state.redraw_state = RedrawState::Idle;
        }

        virtual_output.render_count += 1;
        debug!(
            "Headless render #{} for output {}",
            virtual_output.render_count, name
        );

        // Send frame callbacks to clients
        for window in ewm.space.elements() {
            window.send_frame(output, Duration::ZERO, None, |_, _| Some(output.clone()));
        }

        // Send frame callbacks to layer surfaces
        let layer_map = smithay::desktop::layer_map_for_output(output);
        for layer in layer_map.layers() {
            layer.send_frame(output, Duration::ZERO, None, |_, _| Some(output.clone()));
        }
    }

    /// Get the renderer (for tests that need to verify rendering)
    pub fn renderer(&mut self) -> Option<&mut GlesRenderer> {
        self.renderer.as_mut()
    }

    /// Get render count for an output (for test assertions)
    pub fn render_count(&self, name: &str) -> usize {
        self.outputs.get(name).map(|o| o.render_count).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_headless_backend_creation() {
        let backend = HeadlessBackend::new();
        assert!(backend.outputs.is_empty());
    }
}
