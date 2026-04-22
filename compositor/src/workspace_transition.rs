//! Per-output workspace transition animation state.
//!
//! When the active tab changes on an output, a snapshot of the departing
//! workspace is captured and slid off while the new layout renders
//! underneath. This module holds the animation state and the element
//! injected into the render pipeline; the capture and trigger live
//! elsewhere.

use std::time::Duration;

use smithay::backend::renderer::element::{
    solid::{SolidColorBuffer, SolidColorRenderElement},
    Kind,
};
use smithay::utils::{Logical, Point, Scale, Size};

use crate::render::EwmRenderElement;
use crate::utils::get_monotonic_time;

/// In-flight transition for a single output.
///
/// The `buffer` field is a stub: a semi-transparent red rectangle that
/// slides to visualize the animation. It will be replaced by a captured
/// output texture in a later commit.
pub struct WorkspaceTransition {
    start: Duration,
    duration: Duration,
    buffer: SolidColorBuffer,
    output_size: Size<i32, Logical>,
}

impl WorkspaceTransition {
    pub fn new(output_size: Size<i32, Logical>, duration: Duration) -> Self {
        Self {
            start: get_monotonic_time(),
            duration,
            buffer: SolidColorBuffer::new((output_size.w, output_size.h), [1.0, 0.0, 0.0, 0.5]),
            output_size,
        }
    }

    pub fn is_done(&self) -> bool {
        get_monotonic_time().saturating_sub(self.start) >= self.duration
    }

    /// Linear progress 0.0 → 1.0. Easing comes later.
    fn progress(&self) -> f64 {
        let elapsed = get_monotonic_time().saturating_sub(self.start);
        (elapsed.as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0)
    }

    /// Render element for the current frame: slides the snapshot left.
    pub fn render_element(&self, scale: Scale<f64>) -> EwmRenderElement {
        let offset_physical_x = -(self.progress() * self.output_size.w as f64 * scale.x) as i32;
        let elem = SolidColorRenderElement::from_buffer(
            &self.buffer,
            Point::from((offset_physical_x, 0)),
            scale,
            1.0,
            Kind::Unspecified,
        );
        EwmRenderElement::SolidColor(elem)
    }
}
