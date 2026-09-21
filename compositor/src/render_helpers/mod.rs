use smithay::backend::renderer::gles::GlesRenderer;

pub mod renderer;
pub mod resources;
pub mod shader_element;
pub mod shaders;
pub mod shadow;
pub mod zoom;

pub fn init(renderer: &mut GlesRenderer) {
    resources::init(renderer);
    shaders::init(renderer);
}
