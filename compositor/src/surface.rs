//! Compositor-side state of one toplevel, from creation to destruction.

use std::time::Duration;

use crate::Ewm;
use crate::animation::Animation;

const OPEN_ANIM_DURATION: Duration = Duration::from_millis(150);
/// Holds alpha at 0 for one vblank so the empty first buffer some clients ship never shows.
const OPEN_ANIM_DELAY: Duration = Duration::from_millis(16);

#[derive(Debug, Default)]
pub struct SurfaceState {
    /// An Emacs frame rather than an app window.
    pub emacs: bool,
    /// Whether a buffer has ever been committed.
    pub mapped: bool,
    /// Fade-in after the first buffer.
    pub open: Option<Animation>,
}

impl SurfaceState {
    pub(crate) fn new(emacs: bool) -> Self {
        Self {
            emacs,
            ..Self::default()
        }
    }

    /// Records a committed buffer, true only for the first one.
    pub(crate) fn mark_mapped(&mut self) -> bool {
        !std::mem::replace(&mut self.mapped, true)
    }

    pub(crate) fn is_animating(&self) -> bool {
        self.open.is_some()
    }

    /// Drop finished animations.
    pub(crate) fn advance(&mut self) {
        if self.open.as_ref().is_some_and(Animation::is_done) {
            self.open = None;
        }
    }

    /// Alpha of the open fade, 1 once it is over.
    pub(crate) fn open_alpha(&self) -> f32 {
        self.open.as_ref().map_or(1., |anim| anim.value() as f32)
    }
}

impl Ewm {
    /// Fade surface `id` in from its first buffer.
    pub(crate) fn start_open_fade(&mut self, id: u64) {
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        let anim = Animation::ease(self.animations_clock.clone(), 0., 1., OPEN_ANIM_DURATION);
        surface.open = Some(anim.delayed(OPEN_ANIM_DELAY));
    }
}
