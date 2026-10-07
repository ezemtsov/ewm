//! An output's old contents dissolving after a fullscreen toggle.

use std::time::Duration;

use smithay::backend::renderer::gles::GlesTexture;
use smithay::output::Output;
use smithay::utils::{Point, Scale};

use crate::animation::Animation;
use crate::snapshot::Snapshot;
use crate::texture::TextureRenderElement;
use crate::{LayoutEntryId, State};

const FULLSCREEN_FADE_DURATION: Duration = Duration::from_millis(150);

#[derive(Debug)]
pub struct FullscreenFade {
    snapshot: Snapshot,
    /// Alpha of the old contents, 1 down to 0.
    anim: Animation,
}

impl FullscreenFade {
    pub fn is_done(&self) -> bool {
        self.anim.is_done()
    }

    /// The old contents over the whole output.
    pub fn render(&self) -> TextureRenderElement<GlesTexture> {
        self.snapshot
            .render(Point::from((0., 0.)), self.anim.value() as f32)
    }
}

impl State {
    /// Set entry `entry_id` fullscreen or back, dissolving its output from the old contents.
    pub(crate) fn set_fullscreen(&mut self, entry_id: LayoutEntryId, fullscreen: bool) {
        if let Some(output) = self.ewm.fullscreen_change_output(entry_id, fullscreen) {
            self.start_fullscreen_fade(&output);
        }
        self.ewm.set_layout_entry_fullscreen(entry_id, fullscreen);
        self.refresh_pointer_after_layout();
    }

    /// Bake `output` as it shows now, to dissolve it over what a fullscreen toggle puts there.
    fn start_fullscreen_fade(&mut self, output: &Output) {
        let ewm = &mut self.ewm;
        if ewm.animations_clock.should_complete_instantly() {
            return;
        }
        let scale = Scale::from(output.current_scale().fractional_scale());
        let geometry = ewm.space.output_geometry(output).unwrap_or_default();
        let clock = ewm.animations_clock.clone();
        self.backend
            .with_renderer(|renderer, cursor_texture_cache, _| {
                let (elements, _) = crate::render::collect_render_elements_for_output(
                    ewm,
                    renderer,
                    scale,
                    cursor_texture_cache,
                    geometry.loc,
                    geometry.size,
                    false,
                    true,
                    output,
                );
                let Some(snapshot) = Snapshot::new(renderer, &elements, scale) else {
                    return;
                };
                let anim = Animation::linear(clock, 1., 0., FULLSCREEN_FADE_DURATION);
                if let Some(state) = ewm.output_state.get_mut(output) {
                    state.fullscreen_fade = Some(FullscreenFade { snapshot, anim });
                }
            });
    }
}
