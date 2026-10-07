//! The buffer answering a resize fading in over the old view contents.

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

/// A view still showing the old contents until they fade, as it was.
#[derive(Debug)]
pub(crate) struct FadingView {
    entry: LayoutEntryId,
    rect: Rectangle<i32, Logical>,
    /// Drawn at native size, as a primary view is.
    pub(crate) native: bool,
}

/// What the answer to a size-changing configure will fade, fixed when it goes out.
#[derive(Debug)]
pub struct PendingResize {
    views: Vec<FadingView>,
    /// The highest resolution any view draws the old contents at, capped at their own.
    scale: f64,
}

#[derive(Debug)]
pub struct ResizeFade {
    snapshot: Snapshot,
    /// How far the new contents have come in, 0 to 1.
    anim: Animation,
    views: Vec<FadingView>,
}

impl ResizeFade {
    pub(crate) fn is_done(&self) -> bool {
        self.anim.is_done()
    }

    /// The fading view `entry` still shows, if any.
    pub(crate) fn view(&self, entry: &LayoutEntry) -> Option<&FadingView> {
        self.views
            .iter()
            .find(|view| view.entry == entry.id && view.rect == entry.rect())
    }

    pub(crate) fn size(&self) -> Size<f64, Logical> {
        self.snapshot.size()
    }

    /// Alpha of the new contents over the old.
    pub(crate) fn progress(&self) -> f32 {
        self.anim.value() as f32
    }

    pub(crate) fn render(
        &self,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> TextureRenderElement<GlesTexture> {
        self.snapshot.render(location, alpha)
    }
}

impl Ewm {
    /// What a configure of `id` to `size` will fade, less the secondary views `moved` this layout.
    pub(crate) fn pending_resize(
        &self,
        id: u64,
        size: Size<i32, Logical>,
        moved: &HashSet<LayoutEntryId>,
    ) -> Option<PendingResize> {
        if self.animations_clock.should_complete_instantly() {
            return None;
        }
        let old = self.window(id)?.geometry().size;
        if (size.w - old.w).abs().max((size.h - old.h).abs()) <= RESIZE_FADE_THRESHOLD {
            return None;
        }
        let mut views = Vec::new();
        let mut scale = 0f64;
        for (output, entry) in self
            .surface_views(id)
            .filter(|(_, entry)| entry.primary() || !moved.contains(&entry.id))
        {
            let rect = entry.rect();
            let native = entry.primary();
            let drawn = if native {
                1.
            } else {
                let Some(fill) = fill_scale(old, rect.size) else {
                    continue;
                };
                fill.x.min(1.)
            };
            views.push(FadingView {
                entry: entry.id,
                rect,
                native,
            });
            scale = scale.max(drawn * output.current_scale().fractional_scale());
        }
        (!views.is_empty()).then_some(PendingResize { views, scale })
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
        let clock = ewm.animations_clock.clone();
        self.backend.with_renderer(|renderer, _, _| {
            if let Some(snapshot) =
                render_view_snapshot(&surface.window, renderer, pending.scale.into())
            {
                let anim = Animation::linear(clock, 0., 1., RESIZE_FADE_DURATION);
                surface.resize = Some(ResizeFade {
                    snapshot,
                    anim,
                    views: pending.views,
                });
            }
        });
    }
}
