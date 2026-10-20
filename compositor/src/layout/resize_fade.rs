//! The buffer answering a resize, or a layout refitting a view, fading in over the old contents
//! where they were, on one spring curve.

use smithay::backend::renderer::gles::GlesTexture;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::animation::{Animation, SpringParams};
use crate::layout::snapshot::Snapshot;
use crate::render_helpers::texture::TextureRenderElement;
use crate::utils::fill_scale;
use crate::{LayoutEntry, LayoutEntryId};

/// The crossfade's curve. Brisk, as Emacs's borders already sit where the view is going.
pub(super) fn fade_spring() -> SpringParams {
    SpringParams::new(1., 3000., 0.0001)
}

/// Size changes up to this many logical pixels snap, with no fade.
pub const RESIZE_FADE_THRESHOLD: i32 = 10;

/// A view the old contents fade in, at the window it had then, drawn where and at the scale they
/// were.
#[derive(Debug)]
pub(super) struct FadingView {
    pub(super) entry: LayoutEntryId,
    pub(super) window: Rectangle<i32, Logical>,
    pub(super) scale: f64,
    /// Their box as last drawn, output-local.
    pub(super) at: Option<Rectangle<i32, Logical>>,
}

/// What a fade will blend, fixed when its cause goes out.
#[derive(Debug)]
pub struct PendingResize {
    pub(super) views: Vec<FadingView>,
    /// The highest resolution any view draws the old contents at, capped at their own.
    pub(super) scale: f64,
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
    pub(super) snapshot: Snapshot,
    /// How far the new contents have come in, 0 to 1.
    pub(super) anim: Animation,
    pub(super) views: Vec<FadingView>,
    /// The commit flipped fullscreen, so the backdrop fades along.
    pub(super) flips: bool,
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
