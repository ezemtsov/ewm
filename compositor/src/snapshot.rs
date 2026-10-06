//! Contents baked to a texture while they were still mapped.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Logical, Point, Scale, Transform};
use tracing::warn;

use crate::render::render_to_encompassing_texture;
use crate::texture::{TextureBuffer, TextureRenderElement};

#[derive(Debug, Clone)]
pub struct Snapshot {
    buffer: TextureBuffer<GlesTexture>,

    /// How much the texture should be offset.
    buffer_offset: Point<f64, Logical>,
}

impl Snapshot {
    /// `elements` baked at `scale`, `None` when there is nothing or the render fails.
    pub fn new<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        elements: &[E],
        scale: Scale<f64>,
    ) -> Option<Self> {
        crate::tracy_span!("Snapshot::new");

        if elements.is_empty() {
            return None;
        }
        let (texture, _sync_point, geo) = render_to_encompassing_texture(
            renderer,
            scale,
            Transform::Normal,
            Fourcc::Abgr8888,
            elements,
        )
        .map_err(|err| warn!("error rendering snapshot: {err:?}"))
        .ok()?;

        let buffer =
            TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, Vec::new());

        Some(Self {
            buffer,
            buffer_offset: geo.loc.to_f64().to_logical(scale),
        })
    }

    /// The contents with their origin at `location`.
    pub fn render(
        &self,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> TextureRenderElement<GlesTexture> {
        TextureRenderElement::from_texture_buffer(
            self.buffer.clone(),
            location + self.buffer_offset,
            alpha,
            None,
            None,
            Kind::Unspecified,
        )
    }
}
