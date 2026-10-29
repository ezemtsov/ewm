//! Ported from niri's `render_helpers/shadow.rs`, drawn with the renderer's pixel shader API.

use std::rc::Rc;

use glam::{Mat3, Vec2};
use smithay::backend::renderer::element::{Element, Id, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, Uniform};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::gpu_span_location;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size};

use super::renderer::EwmRenderer;
use super::shaders::{Shaders, mat3_uniform};
use crate::render_helpers::shadow_style::{Color, CornerRadius};

/// Renders a rounded rectangle shadow.
#[derive(Debug, Clone)]
pub struct ShadowRenderElement {
    id: Id,
    commit_counter: CommitCounter,
    area: Rectangle<f64, Logical>,
    alpha: f32,
    uniforms: Rc<[Uniform<'static>]>,
    params: Parameters,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Parameters {
    size: Size<f64, Logical>,
    geometry: Rectangle<f64, Logical>,
    color: Color,
    sigma: f32,
    corner_radius: CornerRadius,
    // Should only be used for visual improvements, i.e. corner radius anti-aliasing.
    scale: f32,
    alpha: f32,

    window_geometry: Rectangle<f64, Logical>,
    window_corner_radius: CornerRadius,
}

impl ShadowRenderElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        size: Size<f64, Logical>,
        geometry: Rectangle<f64, Logical>,
        color: Color,
        sigma: f32,
        corner_radius: CornerRadius,
        scale: f32,
        window_geometry: Rectangle<f64, Logical>,
        window_corner_radius: CornerRadius,
        alpha: f32,
    ) -> Self {
        let mut rv = Self {
            id: Id::new(),
            commit_counter: CommitCounter::default(),
            area: Rectangle::default(),
            alpha: 1.,
            uniforms: Rc::new([]),
            params: Parameters {
                size,
                geometry,
                color,
                sigma,
                corner_radius,
                scale,
                alpha,
                window_geometry,
                window_corner_radius,
            },
        };
        rv.update_inner();
        rv
    }

    pub fn empty() -> Self {
        Self {
            id: Id::new(),
            commit_counter: CommitCounter::default(),
            area: Rectangle::default(),
            alpha: 1.,
            uniforms: Rc::new([]),
            params: Parameters {
                size: Default::default(),
                geometry: Default::default(),
                color: Default::default(),
                sigma: 0.,
                corner_radius: Default::default(),
                scale: 1.,
                alpha: 1.,
                window_geometry: Default::default(),
                window_corner_radius: Default::default(),
            },
        }
    }

    pub fn damage_all(&mut self) {
        self.commit_counter.increment();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        size: Size<f64, Logical>,
        geometry: Rectangle<f64, Logical>,
        color: Color,
        sigma: f32,
        corner_radius: CornerRadius,
        scale: f32,
        window_geometry: Rectangle<f64, Logical>,
        window_corner_radius: CornerRadius,
        alpha: f32,
    ) {
        let params = Parameters {
            size,
            geometry,
            color,
            sigma,
            alpha,
            corner_radius,
            scale,
            window_geometry,
            window_corner_radius,
        };
        if self.params == params {
            return;
        }

        self.params = params;
        self.update_inner();
    }

    fn update_inner(&mut self) {
        let Parameters {
            size,
            geometry,
            color,
            sigma,
            alpha,
            corner_radius,
            scale,
            window_geometry,
            window_corner_radius,
        } = self.params;

        let area_size = Vec2::new(size.w as f32, size.h as f32);

        let geo_loc = Vec2::new(geometry.loc.x as f32, geometry.loc.y as f32);
        let geo_size = Vec2::new(geometry.size.w as f32, geometry.size.h as f32);

        let input_to_geo =
            Mat3::from_scale(area_size) * Mat3::from_translation(-geo_loc / area_size);

        let window_geo_loc = Vec2::new(window_geometry.loc.x as f32, window_geometry.loc.y as f32);
        let window_geo_size =
            Vec2::new(window_geometry.size.w as f32, window_geometry.size.h as f32);

        let window_input_to_geo =
            Mat3::from_scale(area_size) * Mat3::from_translation(-window_geo_loc / area_size);

        self.area.size = size;
        self.alpha = alpha;
        self.uniforms = Rc::new([
            Uniform::new("ewm_scale", scale),
            Uniform::new("shadow_color", color.to_array_premul()),
            Uniform::new("sigma", sigma),
            mat3_uniform("input_to_geo", input_to_geo),
            Uniform::new("geo_size", geo_size.to_array()),
            Uniform::new("corner_radius", <[f32; 4]>::from(corner_radius)),
            mat3_uniform("window_input_to_geo", window_input_to_geo),
            Uniform::new("window_geo_size", window_geo_size.to_array()),
            Uniform::new(
                "window_corner_radius",
                <[f32; 4]>::from(window_corner_radius),
            ),
        ]);
        self.commit_counter.increment();
    }

    pub fn with_location(mut self, location: Point<f64, Logical>) -> Self {
        self.area.loc = location;
        self
    }

    pub fn with_alpha(mut self, alpha: f32) -> Self {
        self.alpha = alpha;
        self
    }

    pub fn has_shader(renderer: &mut impl EwmRenderer) -> bool {
        Shaders::get(renderer).shadow.is_some()
    }
}

impl Default for ShadowRenderElement {
    fn default() -> Self {
        Self::empty()
    }
}

impl Element for ShadowRenderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit_counter
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(Size::from((1., 1.)))
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.area.to_physical_precise_round(scale)
    }

    fn alpha(&self) -> f32 {
        self.alpha
    }
}

impl RenderElement<GlesRenderer> for ShadowRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        crate::tracy_span!("ShadowRenderElement::draw");
        let Some(program) = Shaders::get_from_frame(frame).shadow.clone() else {
            return Ok(());
        };
        frame.with_gpu_span(gpu_span_location!("ShadowRenderElement::draw"), |frame| {
            // A unit source over a unit texture spreads `v_coords` across the whole destination.
            frame.render_pixel_shader_to(
                &program,
                src,
                dst,
                Size::from((1, 1)),
                Some(damage),
                self.alpha,
                &self.uniforms,
            )
        })
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
