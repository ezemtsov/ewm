//! The buffer answering a resize, or a layout refitting a view, fading in over the old contents
//! where they were, on one spring curve.

use smithay::backend::renderer::gles::GlesTexture;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::LayoutEntryId;
use crate::animation::{Animation, Clock, SpringParams};
use crate::layout::snapshot::Snapshot;
use crate::render_helpers::texture::TextureRenderElement;

/// Brisk, as Emacs's borders already sit where the view is going.
pub(super) fn fade_animation(clock: Clock) -> Animation {
    Animation::new(clock, 0., 1., 0., SpringParams::new(1., 3000., 0.0001))
}

/// Size changes up to this many logical pixels snap, with no fade.
const RESIZE_FADE_THRESHOLD: i32 = 10;

#[derive(Debug)]
pub struct ResizeFade {
    pub(super) snapshot: Snapshot,
    /// How far the new contents have come in, 0 to 1, none while held for the answer.
    pub(super) anim: Option<Animation>,
    /// The views as drawn before, where the old contents fade out.
    pub(crate) views: Vec<ViewRect>,
    /// The commit flipped fullscreen, so the backdrop fades along.
    pub(crate) flips: bool,
}

impl ResizeFade {
    /// Whether the fade waits for the answer to a configure.
    pub(crate) fn held(&self) -> bool {
        self.anim.is_none()
    }

    /// Run from now, the backdrop along when `flips`.
    pub(crate) fn start(&mut self, clock: Clock, flips: bool) {
        self.anim = Some(fade_animation(clock));
        self.flips = flips;
    }

    pub(crate) fn is_done(&self) -> bool {
        self.anim.as_ref().is_some_and(Animation::is_done)
    }

    /// Alpha of the new contents over the old, 0 while held.
    pub(crate) fn progress(&self) -> f32 {
        self.anim
            .as_ref()
            .map_or(0., |anim| anim.value().clamp(0., 1.) as f32)
    }

    pub(crate) fn render(
        &self,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> TextureRenderElement<GlesTexture> {
        self.snapshot.render(location, alpha)
    }
}

/// Where a shown view's contents sit on its output and at what scale, with the box they draw in.
#[derive(Debug)]
pub struct ViewRect {
    pub surface: u64,
    pub entry: LayoutEntryId,
    pub output: Output,
    pub rect: Rectangle<i32, Logical>,
    pub scale: f64,
    /// Drawn at its own size, as the view the buffer was made for.
    pub native: bool,
    /// Its box as drawn, output-local.
    pub window: Rectangle<i32, Logical>,
    /// Its frame jumps into place, under a resize grab or after the working area changed.
    pub settles: bool,
}

/// Whether a change from size `a` to `b` fades rather than snaps.
pub(crate) fn resize_fades(a: Size<i32, Logical>, b: Size<i32, Logical>) -> bool {
    (a.w - b.w).abs().max((a.h - b.h).abs()) > RESIZE_FADE_THRESHOLD
}
