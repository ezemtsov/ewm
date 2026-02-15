//! Coordinate and scaling utilities for fractional scale support
//!
//! Precise coordinate conversions at fractional scales.
//! The fractional-scale protocol has N/120 precision, so coordinates must be carefully
//! rounded to avoid subpixel drift.

use smithay::output::{self, Output};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Coordinate, Logical, Size, Transform};
use smithay::wayland::compositor::{send_surface_state, SurfaceData};
use smithay::wayland::fractional_scale::with_fractional_scale;

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

/// Send both integer and fractional scale + transform to a surface.
///
/// Sends integer scale via `send_surface_state` (for legacy clients) and fractional
/// scale via `with_fractional_scale` (for clients that support wp-fractional-scale-v1).
/// Must be called whenever a surface needs to know about its output's scale/transform —
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_physical_precise_round() {
        // At scale 1.5, logical 101 → physical 151.5 → rounds to 152
        let result: i32 = to_physical_precise_round(1.5, 101i32);
        assert_eq!(result, 152);

        // At scale 1.0, no change
        let result: i32 = to_physical_precise_round(1.0, 100i32);
        assert_eq!(result, 100);

        // At scale 2.0, doubles
        let result: i32 = to_physical_precise_round(2.0, 50i32);
        assert_eq!(result, 100);

        // Fractional scale with precise rounding
        let result: i32 = to_physical_precise_round(1.25, 10i32);
        assert_eq!(result, 13); // 10 * 1.25 = 12.5 → rounds to 13
    }

    #[test]
    fn test_round_logical_in_physical() {
        // At scale 1.5: 10.3 * 1.5 = 15.45 → round to 15 → 15 / 1.5 = 10.0
        let result = round_logical_in_physical(1.5, 10.3);
        assert!((result - 10.0).abs() < 1e-10);

        // At scale 1.5: 10.5 * 1.5 = 15.75 → round to 16 → 16 / 1.5 = 10.666...
        let result = round_logical_in_physical(1.5, 10.5);
        assert!((result - 16.0 / 1.5).abs() < 1e-10);

        // At scale 1.0, value stays the same (rounded to integer)
        let result = round_logical_in_physical(1.0, 10.7);
        assert!((result - 11.0).abs() < 1e-10);
    }
}
