//! Ported from niri's `layout/closing_window.rs`, without the blocked-out buffers, the
//! transaction blocker, the custom shader and the shrink.

use std::time::Duration;

use smithay::backend::renderer::gles::GlesTexture;
use smithay::utils::{Logical, Point};

use crate::State;
use crate::animation::Animation;
use crate::snapshot::Snapshot;
use crate::texture::TextureRenderElement;

pub const CLOSE_ANIM_DURATION: Duration = Duration::from_millis(150);

#[derive(Debug)]
pub struct ClosingFrame {
    snapshot: Snapshot,
    pos: Point<f64, Logical>,

    /// The closing animation.
    anim: Animation,
}

impl ClosingFrame {
    pub fn new(snapshot: Snapshot, pos: Point<f64, Logical>, anim: Animation) -> Self {
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
        self.snapshot
            .render(location, 1. - self.anim.value() as f32)
    }
}

impl State {
    /// Bake close snapshots for the frames among `ids` that have none yet.
    pub(crate) fn store_close_snapshots(&mut self, ids: &[u64]) {
        let ewm = &mut self.ewm;
        if ewm.animations_clock.should_complete_instantly() {
            return;
        }
        let ids: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|&id| {
                ewm.frame_location(id)
                    .is_some_and(|location| location.frame.close_snapshot.is_none())
            })
            .collect();
        if ids.is_empty() {
            return;
        }
        self.backend.with_renderer(|renderer, _, _| {
            for id in ids {
                ewm.store_frame_close_snapshot(renderer, id);
            }
        });
    }
}
