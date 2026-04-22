//! Per-output workspace transition animation state.
//!
//! When the active tab changes to a neighbor on an output, a snapshot of
//! the departing workspace content is captured and slid off while the
//! new layout pans in from the opposite side. Layer surfaces stay fixed.
//! This module holds the animation state and the snapshot element
//! injected into the render pipeline.

use std::time::Duration;

use smithay::backend::renderer::element::{
    texture::{TextureBuffer, TextureRenderElement},
    Kind,
};
use smithay::backend::renderer::gles::GlesTexture;
use smithay::utils::{Logical, Point, Scale, Size};

use crate::render::EwmRenderElement;
use crate::utils::get_monotonic_time;

/// In-flight transition for a single output.
pub struct WorkspaceTransition {
    start: Duration,
    duration: Duration,
    /// Frozen snapshot of the departing workspace content (no layers, no cursor).
    buffer: TextureBuffer<GlesTexture>,
    /// Logical output size, used to translate the snapshot across the viewport.
    output_size: Size<i32, Logical>,
    /// +1 when moving to a higher tab index (content pans left),
    /// -1 when moving to a lower tab index (content pans right).
    direction: i32,
}

impl WorkspaceTransition {
    pub fn new(
        buffer: TextureBuffer<GlesTexture>,
        output_size: Size<i32, Logical>,
        direction: i32,
        duration: Duration,
    ) -> Self {
        debug_assert!(direction == 1 || direction == -1);
        Self {
            start: get_monotonic_time(),
            duration,
            buffer,
            output_size,
            direction,
        }
    }

    pub fn is_done(&self) -> bool {
        get_monotonic_time().saturating_sub(self.start) >= self.duration
    }

    /// Linear progress 0.0 → 1.0. Easing comes later.
    pub fn progress(&self) -> f64 {
        let elapsed = get_monotonic_time().saturating_sub(self.start);
        (elapsed.as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0)
    }

    pub fn direction(&self) -> i32 {
        self.direction
    }

    pub fn output_size(&self) -> Size<i32, Logical> {
        self.output_size
    }

    /// Snapshot render element for the current frame.
    ///
    /// At progress=0 the snapshot sits at (0, 0); at progress=1 it has
    /// translated fully off-screen in the `-direction` direction (i.e.
    /// opposite of where the new content pans in from).
    pub fn render_element(&self, scale: Scale<f64>) -> EwmRenderElement {
        let offset_logical = -self.direction as f64 * self.progress() * self.output_size.w as f64;
        let offset_physical_x = (offset_logical * scale.x) as i32;
        let elem = TextureRenderElement::from_texture_buffer(
            Point::<f64, _>::from((offset_physical_x as f64, 0.0)),
            &self.buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        );
        EwmRenderElement::Texture(elem)
    }
}
