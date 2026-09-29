//! Coordinate, scaling, and timing utilities
//!
//! Precise coordinate conversions at fractional scales.
//! The fractional-scale protocol has N/120 precision, so coordinates must be carefully
//! rounded to avoid subpixel drift.

pub mod region;
pub mod reply;

use std::time::Duration;

use smithay::backend::renderer::utils::{
    RendererSurfaceStateUserData, with_renderer_surface_state,
};
use smithay::output::{self, Output};
use smithay::reexports::rustix::time::{ClockId, clock_gettime};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Coordinate, Logical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::{SurfaceData, send_surface_state};
use smithay::wayland::fractional_scale::with_fractional_scale;

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
    let props = output.physical_properties();
    let present = |s: &str| !s.is_empty() && s != "Unknown";
    if !present(&props.make) && !present(&props.model) && !present(&props.serial_number) {
        return output.name();
    }
    let field = |s: String| {
        if present(&s) {
            s
        } else {
            "Unknown".to_string()
        }
    };
    format!(
        "{} {} {}",
        field(props.make),
        field(props.model),
        field(props.serial_number),
    )
}

pub fn is_mapped(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
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
