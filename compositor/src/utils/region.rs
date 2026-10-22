//! Ported from niri's `utils/region.rs`.

use std::cmp::{max, min};
use std::collections::BTreeSet;
use std::sync::Arc;

use smithay::utils::{Logical, Physical, Point, Rectangle, Scale};
use smithay::wayland::compositor::{RectangleKind, RegionAttributes};

/// Helper for fractionally transforming an i32 region while preserving adjacent rects.
///
/// Naively applying floating point transforms may cause adjacent rects to go misaligned due to
/// rounding differences. This struct helps apply the transforms in such a way as to preserve
/// alignment.
#[derive(Debug, Clone)]
pub struct TransformedRegion {
    /// Non-overlapping rects (usually in surface-local coordinates).
    pub rects: Arc<Vec<Rectangle<i32, Logical>>>,
    /// Scale to apply to each rect.
    pub scale: Scale<f64>,
    /// Translation to apply to each rect after scaling.
    pub offset: Point<f64, Logical>,
}

impl TransformedRegion {
    /// Returns an iterator over the top-left and bottom-right corners of transformed rects.
    pub fn iter(&self) -> impl Iterator<Item = (Point<f64, Logical>, Point<f64, Logical>)> + '_ {
        self.rects.iter().map(|r| {
            // Here we start in a happy i32 world where everything lines up, and rectangle loc +
            // size is exactly equal to the adjacent rectangle's loc.
            //
            // Unfortunately, we're about to descend to the floating point hell. And we *really*
            // want adjacent rects to remain adjacent no matter what. So we'll convert our rects to
            // their extremities (rather than loc and size), and operate on those. Coordinates from
            // adjacent rects will undergo exactly the same floating point operations, so when
            // they're ultimately rounded to physical pixels, they will remain adjacent.
            let r = r.to_f64();

            let mut a = r.loc;
            // f64 is enough to represent this i32 addition exactly.
            let mut b = r.loc + r.size.to_point();

            a = a.upscale(self.scale);
            b = b.upscale(self.scale);

            a += self.offset;
            b += self.offset;

            (a, b)
        })
    }

    /// Intersects damage with this subregion.
    pub fn filter_damage(
        &self,
        // Same coordinate space as self.iter().
        crop: Rectangle<f64, Logical>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        filtered: &mut Vec<Rectangle<i32, Physical>>,
    ) {
        let scale = dst.size.to_f64() / crop.size;

        let cs = crop.size.to_point();

        for (mut a, mut b) in self.iter() {
            // Convert to dst-relative.
            a -= crop.loc;
            b -= crop.loc;

            // Intersect with crop.
            let ia = Point::new(f64::max(a.x, 0.), f64::max(a.y, 0.));
            let ib = Point::new(f64::min(b.x, cs.x), f64::min(b.y, cs.y));
            if ib.x <= ia.x || ib.y <= ia.y {
                // No intersection.
                continue;
            }

            // Round extremities to physical pixels, ensuring that adjacent rectangles stay adjacent
            // at fractional scales.
            let ia = ia.to_physical_precise_round(scale);
            let ib = ib.to_physical_precise_round(scale);

            let r = Rectangle::from_extremities(ia, ib);

            // Intersect with each damage rect.
            for d in damage {
                if let Some(intersection) = r.intersection(*d) {
                    filtered.push(intersection);
                }
            }
        }
    }
}

pub fn region_to_non_overlapping_rects(
    region: &RegionAttributes,
    output: &mut Vec<Rectangle<i32, Logical>>,
) {
    crate::tracy_span!("region_to_non_overlapping_rects");

    output.clear();

    // Collect all unique Y coordinates.
    let ys = BTreeSet::from_iter(
        region
            .rects
            .iter()
            .flat_map(|(_, r)| [r.loc.y, r.loc.y + r.size.h]),
    );

    let mut ys = ys.into_iter();
    let Some(mut lo) = ys.next() else {
        // The region was empty.
        return;
    };

    // Sorted list of non-overlapping [start, end) tuples.
    let mut spans = Vec::<(i32, i32)>::new();

    // Iterate over Y bands.
    for hi in ys {
        spans.clear();

        'region: for (kind, r) in &region.rects {
            // Skip rects that don't overlap with the Y band.
            if hi <= r.loc.y || r.loc.y + r.size.h <= lo {
                continue;
            }

            let mut x1 = r.loc.x;
            let mut x2 = r.loc.x + r.size.w;
            if x1 == x2 {
                // Empty rect.
                continue;
            }

            match *kind {
                RectangleKind::Add => {
                    // Iterate over existing spans backwards.
                    for i in (0..spans.len()).rev() {
                        let (start, end) = spans[i];

                        // New span is to the right.
                        if end < x1 {
                            spans.insert(i + 1, (x1, x2));
                            continue 'region;
                        }

                        // New span is to the left.
                        if x2 < start {
                            continue;
                        }

                        // New span overlaps this span. Merge them.
                        spans.remove(i);
                        x1 = min(x1, start);
                        x2 = max(x2, end);
                    }

                    spans.insert(0, (x1, x2));
                }
                RectangleKind::Subtract => {
                    // Iterate over existing spans backwards.
                    for i in (0..spans.len()).rev() {
                        let (start, end) = spans[i];

                        // Subtract span is to the right.
                        if end <= x1 {
                            continue 'region;
                        }

                        // Subtract span is to the left.
                        if x2 <= start {
                            continue;
                        }

                        // Subtract span overlaps this span.
                        spans.remove(i);
                        if x2 < end {
                            spans.insert(i, (x2, end));
                        }
                        if start < x1 {
                            spans.insert(i, (start, x1));
                        }
                    }
                }
            }
        }

        for (x1, x2) in spans.drain(..) {
            output.push(Rectangle::from_extremities((x1, lo), (x2, hi)));
        }

        lo = hi;
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use smithay::utils::{Logical, Point, Rectangle, Size};
    use smithay::wayland::compositor::{RectangleKind, RegionAttributes};

    use super::region_to_non_overlapping_rects;

    proptest! {
        /// The rectangles are disjoint and together cover exactly the points the region keeps.
        #[test]
        fn non_overlapping_output_covers_the_region(
            rects in proptest::collection::vec(
                (
                    prop_oneof![Just(RectangleKind::Add), Just(RectangleKind::Subtract)],
                    (0..20i32, 0..20i32, 0..20i32, 0..20i32),
                ),
                1..10,
            )
        ) {
            let rects: Vec<_> = rects
                .into_iter()
                .map(|(kind, (x, y, w, h))| (kind, Rectangle::new(Point::new(x, y), Size::new(w, h))))
                .collect();
            let region = RegionAttributes {
                rects: rects.clone(),
            };

            let mut output: Vec<Rectangle<i32, Logical>> = Vec::new();
            region_to_non_overlapping_rects(&region, &mut output);

            for i in 0..output.len() {
                prop_assert!(!output[i].is_empty());

                // Verify no pair of output rectangles overlaps.
                for j in (i + 1)..output.len() {
                    prop_assert!(
                        !output[i].overlaps(output[j]),
                        "rectangles overlap: {:?} and {:?}",
                        output[i],
                        output[j],
                    );
                }
            }

            for x in 0..40 {
                for y in 0..40 {
                    let point = Point::new(x, y);
                    let kept = rects.iter().fold(false, |kept, (kind, rect)| match kind {
                        RectangleKind::Add => kept || rect.contains(point),
                        RectangleKind::Subtract => kept && !rect.contains(point),
                    });
                    prop_assert_eq!(output.iter().any(|r| r.contains(point)), kept, "{:?}", point);
                }
            }
        }
    }
}
