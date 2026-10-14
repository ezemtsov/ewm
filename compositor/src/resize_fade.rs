//! The buffer answering a resize, or a layout refitting a view, fading in over the old contents
//! where they were, on one spring curve.

use smithay::backend::renderer::gles::GlesTexture;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::animation::{Animation, SpringParams};
use crate::render::render_view_snapshot;
use crate::snapshot::Snapshot;
use crate::texture::TextureRenderElement;
use crate::utils::fill_scale;
use crate::{Ewm, LayoutEntry, LayoutEntryId, State, strip};

/// The crossfade's curve. Brisk, as Emacs's borders already sit where the view is going.
fn fade_spring() -> SpringParams {
    SpringParams::new(1., 3000., 0.0001)
}

/// Size changes up to this many logical pixels snap, with no fade.
pub const RESIZE_FADE_THRESHOLD: i32 = 10;

/// A view the old contents fade in, at the window it had then, drawn where and at the scale they
/// were.
#[derive(Debug)]
struct FadingView {
    entry: LayoutEntryId,
    window: Rectangle<i32, Logical>,
    scale: f64,
    /// Their box as last drawn, output-local.
    at: Option<Rectangle<i32, Logical>>,
}

/// What a fade will blend, fixed when its cause goes out.
#[derive(Debug)]
pub struct PendingResize {
    views: Vec<FadingView>,
    /// The highest resolution any view draws the old contents at, capped at their own.
    scale: f64,
}

impl PendingResize {
    /// Where each view last drew the old contents, from `was`.
    pub(crate) fn place(&mut self, was: &[ViewRect]) {
        for view in &mut self.views {
            view.at = was
                .iter()
                .find(|rect| rect.entry == view.entry)
                .map(|rect| rect.rect);
        }
    }
}

#[derive(Debug)]
pub struct ResizeFade {
    snapshot: Snapshot,
    /// How far the new contents have come in, 0 to 1.
    anim: Animation,
    views: Vec<FadingView>,
    /// The commit flipped fullscreen, so the backdrop fades along.
    flips: bool,
}

impl ResizeFade {
    pub(crate) fn is_done(&self) -> bool {
        self.anim.is_done()
    }

    pub fn flips(&self) -> bool {
        self.flips
    }

    fn view(&self, entry: &LayoutEntry) -> Option<&FadingView> {
        self.views.iter().find(|view| view.entry == entry.id)
    }

    /// Whether `entry` still shows the old contents, in the window it had then.
    pub(crate) fn shows(&self, entry: &LayoutEntry) -> bool {
        self.view(entry)
            .is_some_and(|view| view.window == entry.rect())
    }

    /// Alpha of the new contents over the old.
    pub(crate) fn progress(&self) -> f32 {
        self.anim.value().clamp(0., 1.) as f32
    }

    /// The scale `entry` showed the old contents at.
    pub(crate) fn scale(&self, entry: &LayoutEntry) -> f64 {
        self.view(entry).map_or(1., |view| view.scale)
    }

    /// Where `entry` showed the old contents, output-local.
    pub(crate) fn at(&self, entry: &LayoutEntry) -> Option<Rectangle<i32, Logical>> {
        self.view(entry).and_then(|view| view.at)
    }

    pub(crate) fn render(
        &self,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> TextureRenderElement<GlesTexture> {
        self.snapshot.render(location, alpha)
    }
}

/// Where a shown view's contents sit on its output and at what scale, with its window.
#[derive(Debug)]
pub struct ViewRect {
    pub surface: u64,
    pub entry: LayoutEntryId,
    pub output: Output,
    pub frame: u64,
    pub rect: Rectangle<i32, Logical>,
    pub scale: f64,
    /// Its fit now, which it takes once nothing is owed.
    pub fit: Fit,
    /// Its window as Emacs laid it out.
    pub window: Rectangle<i32, Logical>,
}

/// How a view fits its contents: at their own size when native, else filling its window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    /// The window's size, the slot's when fullscreen.
    pub window: Size<i32, Logical>,
    pub native: bool,
}

impl Fit {
    /// The scale `buffer` is drawn at.
    pub(crate) fn scale(self, buffer: Size<i32, Logical>) -> Option<f64> {
        if self.native {
            Some(1.)
        } else {
            fill_scale(buffer, self.window).map(|fill| fill.x)
        }
    }

    /// Where fullscreen `buffer` starts in a `slot` that size: centered when native.
    pub(crate) fn fullscreen_offset(
        self,
        buffer: Size<i32, Logical>,
        slot: Size<i32, Logical>,
    ) -> Point<i32, Logical> {
        if self.native {
            crate::fullscreen_center_offset(buffer, slot).into()
        } else {
            Point::from((0, 0))
        }
    }
}

/// Where contents sit and at what scale: `buffer` at `loc`, drawn by `fit`.
pub(crate) fn contents_rect(
    fit: Fit,
    buffer: Size<i32, Logical>,
    loc: Point<f64, Logical>,
) -> Option<(Rectangle<f64, Logical>, f64)> {
    let scale = fit.scale(buffer)?;
    Some((Rectangle::new(loc, buffer.to_f64().upscale(scale)), scale))
}

/// The larger side a size changes by.
pub fn size_change(a: Size<i32, Logical>, b: Size<i32, Logical>) -> i32 {
    (a.w - b.w).abs().max((a.h - b.h).abs())
}

impl Ewm {
    /// Fit every shown view, holding the views of a surface owing a buffer until it comes, unless
    /// this is its commit (`answered`). Views no longer shown are forgotten.
    pub(crate) fn fit_views(&mut self, answered: bool) {
        let shown = self.view_rects();
        for now in &shown {
            if let Some(surface) = self.surfaces.get_mut(&now.surface) {
                let hold = !answered && surface.owes_buffer();
                surface.fit(now.entry, now.fit, hold);
            }
        }
        for surface in self.surfaces.values_mut() {
            surface.forget_views(&shown);
        }
    }

    /// The views of `id` as drawn now, with the resolution the old contents need for them.
    fn fading_views(
        &self,
        id: u64,
        mut keep: impl FnMut(&ViewRect) -> Option<FadingView>,
    ) -> Option<PendingResize> {
        let mut views = Vec::new();
        let mut scale = 0f64;
        for now in self.view_rects().iter().filter(|now| now.surface == id) {
            let Some(view) = keep(now) else {
                continue;
            };
            scale = scale.max(view.scale.min(1.) * now.output.current_scale().fractional_scale());
            views.push(view);
        }
        (!views.is_empty()).then_some(PendingResize { views, scale })
    }

    /// What a configure of `id` to `size` will fade.
    pub(crate) fn pending_resize(
        &self,
        id: u64,
        size: Size<i32, Logical>,
    ) -> Option<PendingResize> {
        if self.animations_clock.should_complete_instantly() {
            return None;
        }
        let old = self.window(id)?.geometry().size;
        if size_change(size, old) <= RESIZE_FADE_THRESHOLD {
            return None;
        }
        self.fading_views(id, |now| {
            Some(FadingView {
                entry: now.entry,
                window: now.window,
                scale: now.scale,
                at: None,
            })
        })
    }

    /// What a layout refitting views of `id` since `before` will fade, nothing while it owes a
    /// buffer or its frame settles at once.
    fn pending_refit(&self, id: u64, before: &[ViewRect]) -> Option<PendingResize> {
        if self.animations_clock.should_complete_instantly()
            || self.surfaces.get(&id)?.owes_buffer()
        {
            return None;
        }
        self.fading_views(id, |now| {
            if self
                .layout
                .monitor_of(&now.output)
                .is_some_and(|monitor| monitor.settles_at_once(now.frame))
            {
                return None;
            }
            let was = before
                .iter()
                .find(|was| was.entry == now.entry && was.output == now.output)?;
            (size_change(was.rect.size, now.rect.size) > RESIZE_FADE_THRESHOLD).then(|| {
                FadingView {
                    entry: now.entry,
                    window: now.window,
                    scale: was.scale,
                    at: Some(was.rect),
                }
            })
        })
    }
}

impl State {
    /// Fade the old contents of surface `id` into the new ones `pending` names, the backdrop
    /// along if it `flips`.
    pub(crate) fn start_resize_fade(&mut self, id: u64, pending: PendingResize, flips: bool) {
        let ewm = &mut self.ewm;
        let Some(surface) = ewm.surfaces.get_mut(&id) else {
            return;
        };
        // A fade in flight keeps its snapshot, and an open fade would hide a new one.
        if surface.open.as_ref().is_some_and(|open| !open.is_done())
            || surface.resize.as_ref().is_some_and(|fade| !fade.is_done())
            || ewm.animations_clock.should_complete_instantly()
        {
            return;
        }
        let clock = ewm.animations_clock.clone();
        self.backend.with_renderer(|renderer, _, _| {
            if let Some(snapshot) =
                render_view_snapshot(&surface.window, renderer, pending.scale.into())
            {
                let anim = Animation::new(clock, 0., 1., 0., fade_spring());
                surface.resize = Some(ResizeFade {
                    snapshot,
                    anim,
                    views: pending.views,
                    flips,
                });
            }
        });
    }

    /// Apply Emacs's layout, fading every view it refits.
    pub(crate) fn apply_layout(&mut self, frames: Vec<strip::Frame>) {
        let before = self.ewm.view_rects();
        self.ewm.apply_layout(frames);
        let ids: Vec<u64> = self.ewm.surfaces.keys().copied().collect();
        for id in ids {
            if let Some(pending) = self.ewm.pending_refit(id, &before) {
                self.start_resize_fade(id, pending, false);
            }
        }
    }
}
