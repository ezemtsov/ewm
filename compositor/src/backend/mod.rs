//! Backend abstraction layer
//!
//! This module provides backend implementations for EWM:
//!
//! - **DRM backend** (`drm`): For running EWM standalone on TTY with real hardware. Requires DRM
//!   master access and works with physical displays.
//!
//! - **Headless backend** (`headless`): For testing without hardware access. Provides virtual
//!   outputs for CI/integration testing.
//!
//! # Design Invariants
//!
//! 1. **Backend isolation**: Each backend owns output management and any renderer it needs. The
//!    compositor core (Ewm) is backend-agnostic and works through the `Backend` enum's method
//!    dispatch.
//!
//! 2. **Output state separation**: Redraw state is stored in `Ewm::output_state`, not in the
//!    backend. This allows backend-agnostic redraw scheduling.

pub mod drm;
pub mod headless;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use drm::DrmBackendState;
pub use headless::HeadlessBackend;
use smithay::output::{Mode, Output, Scale};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Transform};

use crate::{Ewm, OutputConfig, OutputInfo, OutputMode};

/// Backend-owned output metadata for reporting protocols and Emacs events.
#[derive(Clone, Default)]
pub(crate) struct OutputInfos {
    infos: Arc<Mutex<Vec<OutputInfo>>>,
}

impl OutputInfos {
    #[cfg(feature = "screencast")]
    pub(crate) fn shared(&self) -> Arc<Mutex<Vec<OutputInfo>>> {
        self.infos.clone()
    }

    pub(crate) fn insert(&self, info: OutputInfo) {
        let mut infos = self.infos.lock().unwrap();
        if let Some(existing) = infos.iter_mut().find(|existing| existing.name == info.name) {
            *existing = info;
        } else {
            infos.push(info);
        }
    }

    pub(crate) fn remove(&self, name: &str) {
        self.infos.lock().unwrap().retain(|info| info.name != name);
    }

    pub(crate) fn update_from_output(&self, output: &Output, loc: Point<i32, Logical>) {
        let name = output.name();

        if let Some(info) = self
            .infos
            .lock()
            .unwrap()
            .iter_mut()
            .find(|info| info.name == name)
        {
            info.scale = output.current_scale().fractional_scale();
            info.transform = transform_to_int(output.current_transform());
            info.x = loc.x;
            info.y = loc.y;
            if let Some(current_mode) = output.current_mode() {
                sync_output_info_mode(&mut info.modes, current_mode);
            }
        }
    }

    pub(crate) fn list(&self) -> Vec<OutputInfo> {
        self.infos.lock().unwrap().clone()
    }

    pub(crate) fn list_in_output_order(&self, sorted_outputs: &[Output]) -> Vec<OutputInfo> {
        let mut by_name = self
            .list()
            .into_iter()
            .map(|info| (info.name.clone(), info))
            .collect::<HashMap<_, _>>();
        let mut ordered = Vec::with_capacity(by_name.len());

        for output in sorted_outputs {
            let name = output.name();
            if let Some(info) = by_name.remove(&name) {
                ordered.push(info);
            }
        }

        let mut remaining = by_name.into_values().collect::<Vec<_>>();
        remaining.sort_by(|a, b| a.name.cmp(&b.name));
        ordered.extend(remaining);
        ordered
    }

    #[cfg(feature = "screencast")]
    pub(crate) fn get(&self, name: &str) -> Option<OutputInfo> {
        self.infos
            .lock()
            .unwrap()
            .iter()
            .find(|info| info.name == name)
            .cloned()
    }

    pub(crate) fn count(&self) -> usize {
        self.infos.lock().unwrap().len()
    }
}

fn sync_output_info_mode(modes: &mut Vec<OutputMode>, current_mode: Mode) {
    let mut found_current = false;
    for mode in modes.iter_mut() {
        mode.current = mode.width == current_mode.size.w
            && mode.height == current_mode.size.h
            && mode.refresh == current_mode.refresh;
        found_current |= mode.current;
    }

    if !found_current {
        modes.push(OutputMode {
            width: current_mode.size.w,
            height: current_mode.size.h,
            refresh: current_mode.refresh,
            preferred: false,
            current: true,
        });
    }
}

/// Map a configured output. If it was disabled and has no explicit
/// position, append it after the currently mapped output bounds.
/// Returns whether the output went from unmapped to mapped.
pub(crate) fn map_output_for_config(
    ewm: &mut Ewm,
    output: &Output,
    position: Option<(i32, i32)>,
) -> bool {
    let was_mapped = ewm.space.output_geometry(output).is_some();
    let position = position
        .map(Point::from)
        .or_else(|| (!was_mapped).then(|| Point::from((ewm.output_size.w, 0))));

    if let Some(position) = position {
        ewm.space.map_output(output, position);
        output.change_current_state(None, None, None, Some(position));
        ewm.reconcile_mapped_output_entries(output);
    }
    !was_mapped && ewm.space.output_geometry(output).is_some()
}

pub(crate) fn apply_initial_output_config(
    output: &Output,
    mode: Mode,
    config: Option<&OutputConfig>,
) {
    let transform = config
        .and_then(|config| config.transform)
        .unwrap_or(Transform::Normal);
    let scale = config
        .and_then(|config| config.scale)
        .map(|scale| Scale::Fractional(closest_representable_scale(scale)));

    output.change_current_state(Some(mode), Some(transform), scale, None);
    output.set_preferred(mode);
}

pub(crate) fn map_initial_output(
    ewm: &mut Ewm,
    output: &Output,
    config: Option<&OutputConfig>,
) -> (Point<i32, Logical>, bool) {
    let position = config
        .and_then(|config| config.position)
        .map(Point::from)
        .unwrap_or_else(|| Point::from((ewm.output_size.w, 0)));
    let enabled = config.map(|config| config.enabled).unwrap_or(true);

    if enabled {
        ewm.space.map_output(output, position);
        output.change_current_state(None, None, None, Some(position));
    }

    (position, enabled)
}

pub(crate) fn apply_enabled_output_config(
    ewm: &mut Ewm,
    output_infos: &OutputInfos,
    output: &Output,
    config: &OutputConfig,
    mode: Option<Mode>,
) {
    let scale = config
        .scale
        .map(|s| Scale::Fractional(closest_representable_scale(s)));
    let position = config.position.map(Point::from);

    output.change_current_state(mode, config.transform, scale, position);
    if let Some(mode) = mode {
        output.set_preferred(mode);
    }

    let became_mapped = map_output_for_config(ewm, output, config.position);
    output_infos.update_from_output(
        output,
        ewm.space
            .output_geometry(output)
            .map(|geo| geo.loc)
            .unwrap_or_default(),
    );
    ewm.output_config_changed(output, became_mapped);
}

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
/// let mut backend = Backend::Headless(HeadlessBackend::new());
/// ewm.redraw_queued_outputs(&mut backend);
/// ```
#[allow(clippy::large_enum_variant)]
pub enum Backend {
    /// DRM backend for hardware rendering on TTY
    Drm(DrmBackendState),
    /// Headless backend for testing without hardware
    Headless(HeadlessBackend),
}

impl Backend {
    pub(crate) fn output_infos(&self) -> OutputInfos {
        match self {
            Backend::Drm(drm) => drm.output_infos(),
            Backend::Headless(headless) => headless.output_infos(),
        }
    }

    pub(crate) fn output_info_list(&self, sorted_outputs: &[Output]) -> Vec<OutputInfo> {
        self.output_infos().list_in_output_order(sorted_outputs)
    }

    #[cfg(feature = "screencast")]
    pub(crate) fn output_info(&self, name: &str) -> Option<OutputInfo> {
        self.output_infos().get(name)
    }

    pub(crate) fn output_info_count(&self) -> usize {
        self.output_infos().count()
    }

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

    /// Run a closure with renderer, cursor texture cache, and event loop handle.
    ///
    /// Used for immediate screencopy rendering outside the per-output render loop.
    /// No-op for headless backend.
    pub fn with_renderer<F>(&mut self, f: F)
    where
        F: FnOnce(
            &mut smithay::backend::renderer::gles::GlesRenderer,
            &crate::cursor::CursorTextureCache,
            &smithay::reexports::calloop::LoopHandle<'static, crate::State>,
        ),
    {
        match self {
            Backend::Drm(drm) => drm.with_renderer(f),
            Backend::Headless(_) => {}
        }
    }

    pub fn clear_cursor_texture_cache(&mut self) {
        match self {
            Backend::Drm(drm) => drm.clear_cursor_texture_cache(),
            Backend::Headless(_) => {}
        }
    }

    pub fn import_environment(&self, env_vars: &HashMap<String, String>) {
        match self {
            Backend::Drm(_) => drm::import_activation_environment(env_vars),
            Backend::Headless(_) => {}
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

    /// DMA-BUF modifiers the renderer can produce for `fourcc`.
    /// Used by screencast format negotiation.
    #[cfg(feature = "screencast")]
    pub fn render_formats_for_fourcc(
        &mut self,
        fourcc: smithay::backend::allocator::Fourcc,
    ) -> Vec<i64> {
        match self {
            Backend::Drm(drm) => drm.render_formats_for_fourcc(fourcc),
            Backend::Headless(_) => {
                vec![u64::from(smithay::reexports::gbm::Modifier::Linear) as i64]
            }
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

    // DRM-specific methods (panic on Headless)
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

    /// Clear all DRM surfaces (sets DPMS off, disables planes).
    /// The next `queue_frame` will re-enable automatically.
    /// No-op for headless backend.
    pub fn clear_all_surfaces(&mut self) {
        match self {
            Backend::Drm(drm) => drm.clear_all_surfaces(),
            Backend::Headless(_) => {}
        }
    }
}

/// Round scale to the nearest value representable by the fractional-scale
/// protocol (precision is N/120). E.g. 1.5 -> 180/120 = 1.5 (exact),
/// 1.3333 -> 160/120 = 1.33333...
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
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn closest_representable_scale_rounds_to_nearest_120th(
            scale_milli in 1i32..=5_000,
        ) {
            let scale = f64::from(scale_milli) / 1_000.0;
            let rounded = closest_representable_scale(scale);
            let units = (scale * 120.0).round();

            prop_assert!((rounded - units / 120.0).abs() < 1e-10);
            prop_assert!((rounded * 120.0 - units).abs() < 1e-10);
            prop_assert!((rounded - scale).abs() <= 1.0 / 240.0 + 1e-10);
            prop_assert_eq!(closest_representable_scale(rounded), rounded);
        }
    }
}
