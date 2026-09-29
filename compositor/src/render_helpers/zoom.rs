use smithay::backend::renderer::Renderer;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Transform};

/// Scales an element about the origin and moves it by `offset`.
///
/// Edges are rounded rather than location and size, so elements that share
/// an edge before zooming still share the pixel after it.
#[derive(Debug)]
pub struct ZoomRenderElement<E> {
    element: E,
    zoom: f64,
    offset: Point<i32, Physical>,
}

impl<E: Element> ZoomRenderElement<E> {
    pub fn new(element: E, zoom: f64, offset: Point<i32, Physical>) -> Self {
        Self {
            element,
            zoom,
            offset,
        }
    }
}

/// Zoom `rect` about the origin, rounding each edge to a pixel.
fn zoom_rect(rect: Rectangle<i32, Physical>, zoom: f64) -> Rectangle<i32, Physical> {
    let rect = rect.to_f64().upscale(zoom);
    let start = rect.loc.to_i32_round::<i32>();
    let end = (rect.loc + rect.size.to_point()).to_i32_round::<i32>();
    Rectangle::new(start, (end - start).to_size())
}

impl<E: Element> Element for ZoomRenderElement<E> {
    fn id(&self) -> &Id {
        self.element.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.element.current_commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.element.src()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        let mut geometry = zoom_rect(self.element.geometry(scale), self.zoom);
        geometry.loc += self.offset;
        geometry
    }

    fn transform(&self) -> Transform {
        self.element.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.element
            .damage_since(scale, commit)
            .into_iter()
            .map(|rect| rect.to_f64().upscale(self.zoom).to_i32_up())
            .collect()
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.element
            .opaque_regions(scale)
            .into_iter()
            .map(|rect| rect.to_f64().upscale(self.zoom).to_i32_down())
            .filter(|rect| !rect.is_empty())
            .collect()
    }

    fn alpha(&self) -> f32 {
        self.element.alpha()
    }

    fn kind(&self) -> Kind {
        self.element.kind()
    }

    fn is_framebuffer_effect(&self) -> bool {
        self.element.is_framebuffer_effect()
    }
}

impl<R: Renderer, E: RenderElement<R>> RenderElement<R> for ZoomRenderElement<E> {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        self.element
            .draw(frame, src, dst, damage, opaque_regions, cache)
    }

    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        self.element.underlying_storage(renderer)
    }

    fn capture_framebuffer(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), R::Error> {
        self.element.capture_framebuffer(frame, src, dst, cache)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use smithay::utils::Size;

    use super::*;

    proptest! {
        #[test]
        fn shared_edges_stay_shared(
            x in -4000i32..=4000,
            y in -4000i32..=4000,
            w in 1i32..=4000,
            h in 1i32..=4000,
            split in 0.0f64..=1.0,
            zoom in 0.0001f64..=1.0,
        ) {
            let whole = Rectangle::new(Point::from((x, y)), Size::from((w, h)));
            let cut = y + (f64::from(h) * split) as i32;
            let top = Rectangle::new(Point::from((x, y)), Size::from((w, cut - y)));
            let bottom = Rectangle::new(Point::from((x, cut)), Size::from((w, y + h - cut)));

            let (whole, top, bottom) = (
                zoom_rect(whole, zoom),
                zoom_rect(top, zoom),
                zoom_rect(bottom, zoom),
            );
            prop_assert_eq!(top.loc.y + top.size.h, bottom.loc.y);
            prop_assert_eq!(top.loc.y, whole.loc.y);
            prop_assert_eq!(bottom.loc.y + bottom.size.h, whole.loc.y + whole.size.h);
            prop_assert_eq!((top.loc.x, top.size.w), (whole.loc.x, whole.size.w));
        }
    }
}
