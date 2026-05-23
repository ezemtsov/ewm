//! Headless backend for testing
//!
//! This module provides a mock backend that doesn't require GPU/DRM access,
//! allowing the compositor to run in CI environments and for integration testing.
//!
//! # Design Invariants
//!
//! 1. **No hardware access**: The headless backend never touches real GPUs or displays. The
//!    `render()` method is a no-op that marks the output idle and returns `Submitted`.
//!
//! 2. **Deterministic output**: Virtual outputs have fixed sizes and refresh rates, enabling
//!    reproducible snapshot tests.

use std::collections::HashMap;
use std::time::Duration;

use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::utils::Transform;
use tracing::{debug, info};

use crate::{Ewm, OutputState, RedrawState, State};

/// A virtual output for headless testing.
struct VirtualOutput {
    output: Output,
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
/// The headless backend provides virtual outputs for exercising compositor logic in tests.
pub struct HeadlessBackend {
    /// Virtual outputs indexed by name
    outputs: HashMap<String, VirtualOutput>,
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
        }
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
                serial_number: String::new(),
            },
        );

        let mode = Mode {
            size: (width, height).into(),
            refresh: 60_000, // 60Hz
        };
        // Look up stored config for this output
        let config = ewm.output_config.get(name).cloned();
        let initial_transform = config
            .as_ref()
            .and_then(|c| c.transform)
            .unwrap_or(Transform::Normal);
        let initial_scale = config
            .as_ref()
            .and_then(|c| c.scale)
            .map(|s| smithay::output::Scale::Fractional(super::closest_representable_scale(s)));

        output.change_current_state(Some(mode), Some(initial_transform), initial_scale, None);
        output.set_preferred(mode);

        // Create global for Wayland clients
        output.create_global::<State>(&ewm.display_handle);

        // Calculate position: use config or auto horizontal layout
        let (x_offset, y_offset) = config
            .as_ref()
            .and_then(|c| c.position)
            .unwrap_or((ewm.output_size.w, 0));
        let is_enabled = config.as_ref().map(|c| c.enabled).unwrap_or(true);
        if is_enabled {
            ewm.space.map_output(&output, (x_offset, y_offset));
            output.change_current_state(None, None, None, Some((x_offset, y_offset).into()));
        }

        // Initialize output state in Ewm
        ewm.output_state.insert(
            output.clone(),
            OutputState::new(name, Some(Duration::from_micros(16_667)), (width, height)), // ~60Hz
        );

        self.outputs.insert(
            name.to_string(),
            VirtualOutput {
                output: output.clone(),
            },
        );

        // Build OutputInfo and register via centralized lifecycle method
        let current_scale = output.current_scale().fractional_scale();
        let current_transform = output.current_transform();
        let output_info = crate::OutputInfo {
            name: name.to_string(),
            make: "EWM".to_string(),
            model: "Virtual".to_string(),
            serial: String::new(),
            width_mm: width,
            height_mm: height,
            x: x_offset,
            y: y_offset,
            scale: current_scale,
            transform: crate::backend::transform_to_int(current_transform),
            modes: vec![crate::OutputMode {
                width,
                height,
                refresh: 60_000,
                preferred: true,
                current: true,
            }],
        };

        ewm.add_output(&output, output_info);

        if is_enabled {
            info!(
                "Added virtual output: {} ({}x{}) at ({}, {})",
                name, width, height, x_offset, y_offset
            );
        } else {
            info!(
                "Added virtual output: {} ({}x{}) disabled",
                name, width, height
            );
        }
    }

    /// Remove a virtual output by name
    pub fn remove_output(&mut self, name: &str, ewm: &mut Ewm) {
        if let Some(virtual_output) = self.outputs.remove(name) {
            ewm.remove_output(&virtual_output.output);
        }
    }

    /// Render a single output in headless mode.
    ///
    /// In headless mode, we don't render to a real display. Returns Submitted
    /// for known virtual outputs so the compositor's redraw state machine runs.
    pub fn render(
        &mut self,
        ewm: &mut Ewm,
        output: &smithay::output::Output,
    ) -> super::RenderResult {
        let name = output.name();
        if !self.outputs.contains_key(&name) {
            return super::RenderResult::Skipped;
        }

        // Headless has no VBlank or timers, so transition directly to Idle
        if let Some(output_state) = ewm.output_state.get_mut(output) {
            output_state.redraw_state = RedrawState::Idle;
        }

        debug!("Headless render for output {}", name);

        super::RenderResult::Submitted
    }

    /// Apply output configuration for a live headless output.
    ///
    /// Headless backend supports scale, transform, position, and enabled state.
    /// Mode changes are not supported (virtual outputs have fixed size).
    pub fn apply_output_config(&mut self, ewm: &mut Ewm, output_name: &str) {
        let config = match ewm.output_config.get(output_name) {
            Some(c) => c.clone(),
            None => return,
        };

        let output = ewm.connected_output(output_name);
        let Some(output) = output else {
            info!("apply_output_config: output not found: {}", output_name);
            return;
        };

        // Handle disabled output
        if !config.enabled {
            info!("Disabled output {}", output_name);
            ewm.disable_output(&output);
            return;
        }

        // Build final state and apply in one call (no mode changes for headless)
        let scale = config
            .scale
            .map(|s| smithay::output::Scale::Fractional(super::closest_representable_scale(s)));
        let transform = config.transform;
        let position = config.position.map(|(x, y)| (x, y).into());

        output.change_current_state(None, transform, scale, position);

        super::map_output_for_config(ewm, &output, config.position);

        // Update all backend-agnostic bookkeeping
        ewm.output_config_changed(&output);
    }
}
