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
use tracing::{debug, info};

use crate::backend::{
    OutputInfos, apply_enabled_output_config, apply_initial_output_config, map_initial_output,
};
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
    output_infos: OutputInfos,
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
            output_infos: OutputInfos::default(),
        }
    }

    pub(crate) fn output_infos(&self) -> OutputInfos {
        self.output_infos.clone()
    }

    /// Add a virtual output with the given name and size. The serial defaults
    /// to the connector name so each virtual output has a unique identity.
    pub fn add_output(&mut self, name: &str, width: i32, height: i32, ewm: &mut Ewm) {
        self.add_output_with_identity(name, width, height, "EWM", "Virtual", name, ewm);
    }

    /// Add a virtual output with an explicit EDID identity. Two outputs sharing
    /// make/model/serial but differing in connector name model the same physical
    /// display re-enumerating on a different connector after a replug.
    #[allow(clippy::too_many_arguments)]
    pub fn add_output_with_identity(
        &mut self,
        name: &str,
        width: i32,
        height: i32,
        make: &str,
        model: &str,
        serial: &str,
        ewm: &mut Ewm,
    ) {
        let output = Output::new(
            name.to_string(),
            PhysicalProperties {
                size: (width, height).into(),
                subpixel: Subpixel::Unknown,
                make: make.into(),
                model: model.into(),
                serial_number: serial.into(),
            },
        );

        let mode = Mode {
            size: (width, height).into(),
            refresh: 60_000, // 60Hz
        };
        // Look up stored config for this output
        let config = ewm.output_config.get(name).cloned();
        apply_initial_output_config(&output, mode, config.as_ref());

        // Create global for Wayland clients
        output.create_global::<State>(&ewm.display_handle);

        let (position, is_enabled) = map_initial_output(ewm, &output, config.as_ref());

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

        let output_info = crate::OutputInfo {
            name: name.to_string(),
            make: make.to_string(),
            model: model.to_string(),
            serial: serial.to_string(),
            width_mm: width,
            height_mm: height,
            x: position.x,
            y: position.y,
            scale: output.current_scale().fractional_scale(),
            transform: crate::backend::transform_to_int(output.current_transform()),
            modes: vec![crate::OutputMode {
                width,
                height,
                refresh: 60_000,
                preferred: true,
                current: true,
            }],
        };

        self.output_infos.insert(output_info.clone());
        ewm.add_output(&output, output_info);

        if is_enabled {
            info!(
                "Added virtual output: {} ({}x{}) at ({}, {})",
                name, width, height, position.x, position.y
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
            self.output_infos.remove(name);
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

        apply_enabled_output_config(ewm, &self.output_infos, &output, &config, None);
    }
}
