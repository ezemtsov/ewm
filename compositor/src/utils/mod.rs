//! Coordinate, scaling, and timing utilities
//!
//! Precise coordinate conversions at fractional scales.
//! The fractional-scale protocol has N/120 precision, so coordinates must be carefully
//! rounded to avoid subpixel drift.

pub mod region;
pub mod reply;
pub(crate) mod spawning;

use std::os::fd::AsFd;
use std::time::Duration;

use smithay::backend::renderer::utils::{
    RendererSurfaceStateUserData, with_renderer_surface_state,
};
use smithay::desktop::Window;
use smithay::output::{self, Output};
use smithay::reexports::rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use smithay::reexports::rustix::time::{ClockId, clock_gettime};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Coordinate, Logical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::{SurfaceData, send_surface_state};
use smithay::wayland::fractional_scale::with_fractional_scale;

/// Set `O_NONBLOCK` on `fd`.
pub(crate) fn set_nonblocking(fd: impl AsFd) -> std::io::Result<()> {
    fcntl_setfl(&fd, fcntl_getfl(&fd)? | OFlags::NONBLOCK)?;
    Ok(())
}

/// Get monotonic time for timestamps and frame scheduling.
pub fn get_monotonic_time() -> Duration {
    let ts = clock_gettime(ClockId::Monotonic);
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

pub fn center(rect: Rectangle<i32, Logical>) -> Point<i32, Logical> {
    rect.loc + rect.size.downscale(2).to_point()
}

/// Convert a logical coordinate to physical pixels, rounding to the nearest integer.
///
/// This is the scalar equivalent of Smithay's `Point::to_physical_precise_round`.
/// Use when you need a single coordinate converted, not a Point/Size/Rectangle.
pub fn to_physical_precise_round<N: Coordinate>(scale: f64, logical: impl Coordinate) -> N {
    N::from_f64((logical.to_f64() * scale).round())
}

/// Round a logical value so it aligns to a physical pixel boundary.
///
/// Unlike `to_physical_precise_round` which returns an integer physical value,
/// this returns a fractional logical value that, when multiplied by the scale,
/// lands exactly on a pixel. Used for dimensions and offsets that must remain
/// in logical space but be pixel-aligned.
pub fn round_logical_in_physical(scale: f64, logical: f64) -> f64 {
    (logical * scale).round() / scale
}

/// Get the logical size of an output, accounting for fractional scale and transform.
///
/// A 2560x1440 output at scale 1.5 returns 1707x960 (approximately).
/// Transform is applied after scaling (e.g. 90-degree rotation swaps w/h).
pub fn output_size(output: &Output) -> Size<f64, Logical> {
    let scale = output.current_scale().fractional_scale();
    let transform = output.current_transform();
    let mode = output.current_mode().unwrap();
    transform.transform_size(mode.size.to_f64().to_logical(scale))
}

/// Stable identity for an output, surviving connector renumbering across a
/// replug. Prefers EDID make/model/serial and only falls back to the connector
/// name when the display advertises none of them.
pub fn output_identity(output: &Output) -> String {
    let fields = output_identity_fields(output);
    if fields.iter().all(Option::is_none) {
        return output.name();
    }
    let [make, model, serial] = fields.map(|field| field.unwrap_or_else(|| "Unknown".to_string()));
    format!("{make} {model} {serial}")
}

/// EDID make, model and serial, each `None` when the display does not advertise it.
pub fn output_identity_fields(output: &Output) -> [Option<String>; 3] {
    let props = output.physical_properties();
    [props.make, props.model, props.serial_number]
        .map(|field| (!field.is_empty() && field != "Unknown").then_some(field))
}

pub fn is_mapped(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
}

/// Tell `window`'s surfaces the scale and transform of `output`.
pub fn set_preferred_scale_transform(window: &Window, output: &Output) {
    let scale = output.current_scale();
    let transform = output.current_transform();
    window.with_surfaces(|surface, data| {
        send_scale_transform(surface, data, scale, transform);
    });
}

/// Send both integer and fractional scale + transform to a surface.
///
/// Sends integer scale via `send_surface_state` (for legacy clients) and fractional
/// scale via `with_fractional_scale` (for clients that support wp-fractional-scale-v1).
/// Must be called whenever a surface needs to know about its output's scale/transform:
/// on creation, output assignment, and config changes.
pub fn send_scale_transform(
    surface: &WlSurface,
    data: &SurfaceData,
    scale: output::Scale,
    transform: Transform,
) {
    send_surface_state(surface, data, scale.integer_scale(), transform);
    with_fractional_scale(data, |fractional| {
        fractional.set_preferred_scale(scale.fractional_scale());
    });
}

/// Returns the geometry of the surface.
///
/// Returns `None` if the surface isn't mapped.
pub fn surface_geo(states: &SurfaceData) -> Option<Rectangle<i32, Logical>> {
    let data = states.data_map.get::<RendererSurfaceStateUserData>();
    data.and_then(|d| d.lock().unwrap().view())
        .map(|view| Rectangle {
            loc: view.offset,
            size: view.dst,
        })
}

/// `x` within `min` and `max`, zero meaning no bound.
pub(crate) fn ensure_min_max_size(x: i32, min: i32, max: i32) -> i32 {
    let x = if max > 0 { Ord::min(x, max) } else { x };
    if min > 0 { Ord::max(x, min) } else { x }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn physical_rounding_matches_scaled_round(
            scale_hundredths in 25u32..=400,
            logical in -10_000i32..=10_000,
        ) {
            let scale = f64::from(scale_hundredths) / 100.0;
            let physical: i32 = to_physical_precise_round(scale, logical);
            let expected = (f64::from(logical) * scale).round() as i32;

            prop_assert_eq!(physical, expected);
        }

        #[test]
        fn logical_rounding_lands_on_nearest_physical_pixel(
            scale_hundredths in 25u32..=400,
            logical_milli in -1_000_000i32..=1_000_000,
        ) {
            let scale = f64::from(scale_hundredths) / 100.0;
            let logical = f64::from(logical_milli) / 1_000.0;
            let rounded = round_logical_in_physical(scale, logical);
            let physical = rounded * scale;
            let expected_physical = (logical * scale).round();

            prop_assert!((physical - expected_physical).abs() < 1e-9);
            prop_assert!((physical - physical.round()).abs() < 1e-9);
        }
    }
}
