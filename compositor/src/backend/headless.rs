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
    OutputInfos, apply_enabled_output_config, apply_initial_output_config, output_enabled,
};
use crate::{Event, Ewm, OutputState, RedrawState};

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
    /// Plugged virtual connectors by name.
    pub(crate) outputs: HashMap<String, PhysicalProperties>,
    output_infos: OutputInfos,
    /// Renders per output, for tests that watch the redraw loop.
    render_counts: HashMap<String, u32>,
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
            render_counts: HashMap::new(),
        }
    }

    pub(crate) fn output_infos(&self) -> OutputInfos {
        self.output_infos.clone()
    }

    pub fn render_count(&self, output: &str) -> u32 {
        self.render_counts.get(output).copied().unwrap_or(0)
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
        self.output_infos.insert(crate::OutputInfo {
            name: name.to_string(),
            make: make.to_string(),
            model: model.to_string(),
            serial: serial.to_string(),
            width_mm: width,
            height_mm: height,
            x: 0,
            y: 0,
            scale: 1.0,
            transform: 0,
            modes: vec![crate::OutputMode {
                width,
                height,
                refresh: 60_000,
                preferred: true,
                current: true,
            }],
        });
        self.outputs.insert(
            name.to_string(),
            PhysicalProperties {
                size: (width, height).into(),
                subpixel: Subpixel::Unknown,
                make: make.into(),
                model: model.into(),
                serial_number: serial.into(),
            },
        );
        self.connect_connector(name, ewm);
    }

    /// Bring up the plugged connector `name` when config allows, and tell Emacs of it.
    fn connect_connector(&mut self, name: &str, ewm: &mut Ewm) -> bool {
        let Some(properties) = self.outputs.get(name).cloned() else {
            return false;
        };
        let connected = output_enabled(ewm, name);
        if connected {
            let (width, height) = (properties.size.w, properties.size.h);
            let output = Output::new(name.to_string(), properties);
            let mode = Mode {
                size: (width, height).into(),
                refresh: 60_000, // 60Hz
            };
            apply_initial_output_config(&output, mode, ewm.output_config.get(name));
            ewm.output_state.insert(
                output.clone(),
                OutputState::new(name, Some(Duration::from_micros(16_667)), (width, height)), // ~60Hz
            );
            ewm.add_output(&output);
            info!("Added virtual output: {} ({}x{})", name, width, height);
        }
        self.output_infos.announce(name, ewm);
        connected
    }

    /// Remove a virtual output by name
    pub fn remove_output(&mut self, name: &str, ewm: &mut Ewm) {
        self.outputs.remove(name);
        self.output_infos.disconnect(name, ewm);
        if let Some(output) = ewm.connected_output(name) {
            ewm.remove_output(&output);
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
        *self.render_counts.entry(name).or_default() += 1;

        super::RenderResult::Submitted
    }

    /// Apply the config of `output_name`, connecting or dropping the output as enabled says.
    pub fn apply_output_config(&mut self, ewm: &mut Ewm, output_name: &str) {
        let config = match ewm.output_config.get(output_name) {
            Some(c) => c.clone(),
            None => return,
        };

        match (config.enabled, ewm.connected_output(output_name)) {
            (true, Some(output)) => apply_enabled_output_config(ewm, &output, &config, None),
            // Enabled again, a fresh output as a replug brings one.
            (true, None) => {
                if self.connect_connector(output_name, ewm) {
                    ewm.queue_event(Event::OutputsComplete);
                }
            }
            (false, Some(output)) => {
                // Disabled, the output leaves like an unplugged one.
                info!("Disabled output {}", output_name);
                ewm.remove_output(&output);
            }
            (false, None) => {}
        }
    }
}
