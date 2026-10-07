//! Old view contents fading out over the buffer that answered a resize.

use std::collections::HashSet;
use std::time::Duration;

use smithay::backend::renderer::gles::GlesTexture;
use smithay::utils::{Logical, Point, Rectangle, Serial, Size};

use crate::animation::Animation;
use crate::render::render_view_snapshot;
use crate::snapshot::Snapshot;
use crate::texture::TextureRenderElement;
use crate::utils::fill_scale;
use crate::{Ewm, LayoutEntry, LayoutEntryId, State};

const RESIZE_FADE_DURATION: Duration = Duration::from_millis(150);

/// Size changes up to this many logical pixels snap without a fade.
const RESIZE_FADE_THRESHOLD: i32 = 10;

/// What the answer to a size-changing configure will fade, fixed when it goes out.
#[derive(Debug)]
pub struct PendingResize {
    /// The secondary views still showing the old contents, as they are.
    entries: Vec<(LayoutEntryId, Rectangle<i32, Logical>)>,
    /// The highest resolution any of them draws the old contents at, capped at their own.
    scale: f64,
}

#[derive(Debug)]
pub struct ResizeFade {
    snapshot: Snapshot,
    /// Alpha of the old contents, 1 down to 0.
    anim: Animation,
    /// The views that fade, as they were.
    entries: Vec<(LayoutEntryId, Rectangle<i32, Logical>)>,
}

impl ResizeFade {
    pub(crate) fn is_done(&self) -> bool {
        self.anim.is_done()
    }

    pub(crate) fn covers(&self, entry: &LayoutEntry) -> bool {
        self.entries.contains(&(entry.id, entry.rect()))
    }

    pub(crate) fn size(&self) -> Size<f64, Logical> {
        self.snapshot.size()
    }

    pub(crate) fn render(
        &self,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> TextureRenderElement<GlesTexture> {
        self.snapshot
            .render(location, alpha * self.anim.value() as f32)
    }
}

impl Ewm {
    /// What a configure of `id` to `size` will fade, less the views `moved` this layout.
    pub(crate) fn pending_resize(
        &self,
        id: u64,
        size: Size<i32, Logical>,
        moved: &HashSet<LayoutEntryId>,
    ) -> Option<PendingResize> {
        if self.animations_clock.should_complete_instantly() {
            return None;
        }
        let old = self.id_windows.get(&id)?.geometry().size;
        if (size.w - old.w).abs().max((size.h - old.h).abs()) <= RESIZE_FADE_THRESHOLD {
            return None;
        }
        let mut entries = Vec::new();
        let mut scale = 0f64;
        for (output, entry) in self
            .surface_views(id)
            .filter(|(_, entry)| !entry.primary() && !moved.contains(&entry.id))
        {
            let rect = entry.rect();
            let Some(fill) = fill_scale(old, rect.size) else {
                continue;
            };
            entries.push((entry.id, rect));
            scale = scale.max(fill.x.min(1.) * self.output_scale(output));
        }
        (!entries.is_empty()).then_some(PendingResize { entries, scale })
    }
}

impl State {
    /// Fade surface `id` out over the buffer answering configure `serial`.
    pub(crate) fn start_resize_fade(&mut self, id: u64, serial: Serial) {
        let ewm = &mut self.ewm;
        let Some(surface) = ewm.surfaces.get_mut(&id) else {
            return;
        };
        let Some(pending) = surface.take_answered_resize(serial) else {
            return;
        };
        // A fade in flight keeps its snapshot, and an open fade would hide a new one.
        if surface.is_animating() || ewm.animations_clock.should_complete_instantly() {
            return;
        }
        let Some(window) = ewm.id_windows.get(&id) else {
            return;
        };
        let clock = ewm.animations_clock.clone();
        self.backend.with_renderer(|renderer, _, _| {
            if let Some(snapshot) = render_view_snapshot(window, renderer, pending.scale.into()) {
                let anim = Animation::linear(clock, 1., 0., RESIZE_FADE_DURATION);
                surface.resize = Some(ResizeFade {
                    snapshot,
                    anim,
                    entries: pending.entries,
                });
            }
        });
    }
}
