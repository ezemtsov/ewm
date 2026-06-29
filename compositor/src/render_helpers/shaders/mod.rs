use glam::Mat3;
use smithay::backend::renderer::gles::{
    GlesFrame, GlesRenderer, Uniform, UniformName, UniformType, UniformValue,
};
use tracing::{error, warn};

use super::renderer::NiriRenderer;
use super::shader_element::ShaderProgram;

pub struct Shaders {
    pub shadow: Option<ShaderProgram>,
}

#[derive(Debug, Clone, Copy)]
pub enum ProgramType {
    Shadow,
}

impl Shaders {
    fn compile(renderer: &mut GlesRenderer) -> Self {
        crate::tracy_span!("Shaders::compile");

        let shadow = ShaderProgram::compile(
            renderer,
            concat!(
                include_str!("shadow.frag"),
                include_str!("rounding_alpha.frag")
            ),
            &[
                UniformName::new("shadow_color", UniformType::_4f),
                UniformName::new("sigma", UniformType::_1f),
                UniformName::new("input_to_geo", UniformType::Matrix3x3),
                UniformName::new("geo_size", UniformType::_2f),
                UniformName::new("corner_radius", UniformType::_4f),
                UniformName::new("window_input_to_geo", UniformType::Matrix3x3),
                UniformName::new("window_geo_size", UniformType::_2f),
                UniformName::new("window_corner_radius", UniformType::_4f),
            ],
            &[],
        )
        .map_err(|err| {
            warn!("error compiling shadow shader: {err:?}");
        })
        .ok();

        Self { shadow }
    }

    pub fn get_from_frame<'a>(frame: &'a mut GlesFrame<'_, '_>) -> &'a Self {
        let data = frame.egl_context().user_data();
        data.get()
            .expect("shaders::init() must be called when creating the renderer")
    }

    pub fn get(renderer: &mut impl NiriRenderer) -> &Self {
        let renderer = renderer.as_gles_renderer();
        let data = renderer.egl_context().user_data();
        data.get()
            .expect("shaders::init() must be called when creating the renderer")
    }

    pub fn program(&self, program: ProgramType) -> Option<ShaderProgram> {
        match program {
            ProgramType::Shadow => self.shadow.clone(),
        }
    }
}

pub fn init(renderer: &mut GlesRenderer) {
    let data = renderer.egl_context().user_data();
    if data.get::<Shaders>().is_some() {
        return;
    }

    let shaders = Shaders::compile(renderer);
    let data = renderer.egl_context().user_data();
    if !data.insert_if_missing(|| shaders) {
        error!("shaders were already compiled");
    }
}

pub fn mat3_uniform(name: &str, mat: Mat3) -> Uniform<'_> {
    Uniform::new(
        name,
        UniformValue::Matrix3x3 {
            matrices: vec![mat.to_cols_array()],
            transpose: false,
        },
    )
}
