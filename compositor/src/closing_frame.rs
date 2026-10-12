//! Ported from niri's `layout/closing_window.rs`, without the blocked-out buffers, the
//! transaction blocker, the custom shader and the shrink.

use std::time::Duration;

use smithay::backend::renderer::gles::GlesTexture;
use smithay::utils::{Logical, Point};

use crate::State;
use crate::animation::{Animation, Clock};
use crate::snapshot::Snapshot;
use crate::texture::TextureRenderElement;

pub const CLOSE_ANIM_DURATION: Duration = Duration::from_millis(150);

/// Contents fading out where they were drawn, a frame's or a view's.
#[derive(Debug, Clone)]
pub struct Closing {
    snapshot: Snapshot,
    anim: Animation,
}

impl Closing {
    pub fn new(snapshot: Snapshot, clock: Clock) -> Self {
        Self {
            snapshot,
            anim: Animation::ease(clock, 0., 1., CLOSE_ANIM_DURATION),
        }
    }

    /// Hold still for `delay` before fading.
    pub(crate) fn delayed(mut self, delay: Duration) -> Self {
        self.anim = self.anim.delayed(delay);
        self
    }

    pub fn are_animations_ongoing(&self) -> bool {
        !self.anim.is_done()
    }

    /// `origin` is where the frame origin lands on the output.
    pub fn render(&self, origin: Point<f64, Logical>) -> TextureRenderElement<GlesTexture> {
        self.snapshot.render(origin, 1. - self.anim.value() as f32)
    }
}

impl State {
    /// Bake close snapshots for the frames among `ids` lacking one, and for the other ids' views.
    pub(crate) fn store_close_snapshots(&mut self, ids: &[u64]) {
        let ewm = &mut self.ewm;
        if ewm.animations_clock.should_complete_instantly() {
            return;
        }
        let ids: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|&id| match ewm.layout.frame_location(id) {
                Some(location) => location.frame.close_snapshot.is_none(),
                None => ewm.surface_entry_locations(id).next().is_some(),
            })
            .collect();
        if ids.is_empty() {
            return;
        }
        self.backend.with_renderer(|renderer, _, _| {
            for id in ids {
                ewm.store_close_snapshot(renderer, id);
            }
        });
    }
}
