//! Ported from niri's `layout/closing_window.rs`, without the blocked-out buffers, the
//! transaction blocker, the custom shader and the shrink.

use std::time::Duration;

use anyhow::Context as _;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Logical, Point, Scale, Transform};

use crate::animation::Animation;
use crate::render::render_to_encompassing_texture;
use crate::texture::{TextureBuffer, TextureRenderElement};

pub const CLOSE_ANIM_DURATION: Duration = Duration::from_millis(150);

/// A frame's contents baked to a texture while they were still mapped.
#[derive(Debug, Clone)]
pub struct FrameSnapshot {
    /// Contents of the frame.
    buffer: TextureBuffer<GlesTexture>,

    /// How much the texture should be offset.
    buffer_offset: Point<f64, Logical>,
}

impl FrameSnapshot {
    pub fn new<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        elements: &[E],
        scale: Scale<f64>,
    ) -> anyhow::Result<Self> {
        crate::tracy_span!("FrameSnapshot::new");

        let (texture, _sync_point, geo) = render_to_encompassing_texture(
            renderer,
            scale,
            Transform::Normal,
            Fourcc::Abgr8888,
            elements,
        )
        .context("error rendering to texture")?;

        let buffer =
            TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, Vec::new());

        Ok(Self {
            buffer,
            buffer_offset: geo.loc.to_f64().to_logical(scale),
        })
    }
}

#[derive(Debug)]
pub struct ClosingFrame {
    snapshot: FrameSnapshot,
    pos: Point<f64, Logical>,

    /// The closing animation.
    anim: Animation,
}

impl ClosingFrame {
    pub fn new(snapshot: FrameSnapshot, pos: Point<f64, Logical>, anim: Animation) -> Self {
        Self {
            snapshot,
            pos,
            anim,
        }
    }

    pub fn pos(&self) -> Point<f64, Logical> {
        self.pos
    }

    pub fn are_animations_ongoing(&self) -> bool {
        !self.anim.is_done()
    }

    /// `location` is where the frame origin lands on the output.
    pub fn render(&self, location: Point<f64, Logical>) -> TextureRenderElement<GlesTexture> {
        TextureRenderElement::from_texture_buffer(
            self.snapshot.buffer.clone(),
            location + self.snapshot.buffer_offset,
            1. - self.anim.value() as f32,
            None,
            None,
            Kind::Unspecified,
        )
    }
}
