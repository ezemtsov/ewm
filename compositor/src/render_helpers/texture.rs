//! Fractional-scale-aware texture render element.
//!
//! smithay's built-in `TextureBuffer` stores scale as `i32`, so a texture
//! rendered at a fractional output scale (say 1.5x) can't be described
//! correctly -- the mismatch between declared scale and actual pixel size
//! either shrinks or tiles the element. This module mirrors smithay's
//! API but uses `Scale<f64>` and resolves `src` directly from the
//! texture's own dimensions, so fractional-scale outputs render
//! pixel-for-pixel correct.

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, OpaqueRegions};
use smithay::backend::renderer::{ContextId, Frame as _, Renderer, Texture};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use tracing::warn;

/// GPU texture with fractional-scale metadata.
#[derive(Debug, Clone)]
pub struct TextureBuffer<T: Texture> {
    id: Id,
    commit_counter: CommitCounter,
    renderer_context_id: ContextId<T>,
    texture: T,
    scale: Scale<f64>,
    transform: Transform,
    opaque_regions: Vec<Rectangle<i32, Buffer>>,
}

impl<T: Texture> TextureBuffer<T> {
    pub fn from_texture<R: Renderer<TextureId = T>>(
        renderer: &R,
        texture: T,
        scale: impl Into<Scale<f64>>,
        transform: Transform,
        opaque_regions: Vec<Rectangle<i32, Buffer>>,
    ) -> Self {
        Self {
            id: Id::new(),
            commit_counter: CommitCounter::default(),
            renderer_context_id: renderer.context_id(),
            texture,
            scale: scale.into(),
            transform,
            opaque_regions,
        }
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        self.texture
            .size()
            .to_f64()
            .to_logical(self.scale, self.transform)
    }
}

/// Render element that samples from a `TextureBuffer`.
#[derive(Debug, Clone)]
pub struct TextureRenderElement<T: Texture> {
    buffer: TextureBuffer<T>,
    location: Point<f64, Logical>,
    alpha: f32,
    src: Option<Rectangle<f64, Logical>>,
    size: Option<Size<f64, Logical>>,
    kind: Kind,
}

impl<T: Texture> TextureRenderElement<T> {
    pub fn from_texture_buffer(
        buffer: TextureBuffer<T>,
        location: impl Into<Point<f64, Logical>>,
        alpha: f32,
        src: Option<Rectangle<f64, Logical>>,
        size: Option<Size<f64, Logical>>,
        kind: Kind,
    ) -> Self {
        Self {
            buffer,
            location: location.into(),
            alpha,
            src,
            size,
            kind,
        }
    }

    fn logical_size(&self) -> Size<f64, Logical> {
        self.size
            .or_else(|| self.src.map(|src| src.size))
            .unwrap_or_else(|| self.buffer.logical_size())
    }
}

impl<T: Texture> Element for TextureRenderElement<T> {
    fn id(&self) -> &Id {
        &self.buffer.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.buffer.commit_counter
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        Rectangle::new(self.location, self.logical_size()).to_physical_precise_round(scale)
    }

    fn transform(&self) -> Transform {
        self.buffer.transform
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        // Key fix: when no explicit src is given, sample the full texture
        // in buffer coordinates rather than deriving a buffer rect from the
        // logical size × buffer scale (which can exceed the texture bounds
        // on fractional scales and cause tiling).
        self.src
            .map(|src| {
                src.to_buffer(
                    self.buffer.scale,
                    self.buffer.transform,
                    &self.buffer.logical_size(),
                )
            })
            .unwrap_or_else(|| Rectangle::from_size(self.buffer.texture.size()).to_f64())
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        if self.alpha < 1.0 {
            return OpaqueRegions::default();
        }
        let texture_size = self.buffer.texture.size().to_f64();
        let src = self.src();
        self.buffer
            .opaque_regions
            .iter()
            .filter_map(|region| {
                let mut region = region.to_f64().intersection(src)?;
                region.loc -= src.loc;
                region = region.upscale(texture_size / src.size);
                let logical =
                    region.to_logical(self.buffer.scale, self.buffer.transform, &src.size);
                Some(logical.to_physical_precise_down(scale))
            })
            .collect()
    }

    fn alpha(&self) -> f32 {
        self.alpha
    }

    fn kind(&self) -> Kind {
        self.kind
    }
}

impl<R, T> RenderElement<R> for TextureRenderElement<T>
where
    R: Renderer<TextureId = T>,
    T: Texture,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dest: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        if frame.context_id() != self.buffer.renderer_context_id {
            warn!("rendering texture from a different renderer context");
            return Ok(());
        }
        frame.render_texture_from_to(
            &self.buffer.texture,
            src,
            dest,
            damage,
            opaque_regions,
            self.buffer.transform,
            self.alpha,
        )
    }

    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
