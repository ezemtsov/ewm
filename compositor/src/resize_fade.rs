//! The buffer answering a resize fading in over the old view contents, and a view springing from
//! where it sat when its rect changes.

use std::time::Duration;

use smithay::backend::renderer::gles::GlesTexture;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Serial, Size};

use crate::animation::{Animation, Clock};
use crate::render::render_view_snapshot;
use crate::snapshot::Snapshot;
use crate::texture::TextureRenderElement;
use crate::utils::fill_scale;
use crate::{Ewm, LayoutEntry, LayoutEntryId, State};

const RESIZE_FADE_DURATION: Duration = Duration::from_millis(150);

/// Rect changes up to this many logical pixels snap, with no fade and no spring.
pub const RESIZE_FADE_THRESHOLD: i32 = 10;

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

/// Where a shown view and its frame sit on its output, and whether it is fullscreen or in a float.
#[derive(Debug)]
pub struct ViewRect {
    pub surface: u64,
    pub entry: LayoutEntryId,
    pub output: Output,
    pub frame: u64,
    /// Its frame's origin on the output.
    pub origin: Point<i32, Logical>,
    pub rect: Rectangle<i32, Logical>,
    pub fullscreen: bool,
    pub floating: bool,
}

/// A view springing from where it sat to where it is drawn now.
#[derive(Debug)]
pub struct ViewMove {
    pub from: Rectangle<i32, Logical>,
    /// It sat there fullscreen.
    pub from_fullscreen: bool,
    /// It sat there in a float.
    pub from_floating: bool,
    /// Its frame's origin then, so the frame carries it. None across spaces.
    from_origin: Option<Point<i32, Logical>>,
    /// The output it springs on.
    pub output: Output,
    anim: Animation,
}

impl ViewMove {
    pub(crate) fn new(was: &ViewRect, now: &ViewRect, clock: Clock) -> Self {
        let anim = Animation::new(clock, 0., 1., 0., crate::slide_spring());
        Self {
            from: was.rect,
            from_fullscreen: was.fullscreen,
            from_floating: was.floating,
            from_origin: (was.floating == now.floating).then_some(was.origin),
            output: now.output.clone(),
            anim,
        }
    }

    pub(crate) fn is_done(&self) -> bool {
        self.anim.is_done()
    }

    /// How far it has come, 0 to 1.
    pub(crate) fn progress(&self) -> f32 {
        self.anim.value().clamp(0., 1.) as f32
    }

    /// Alpha of the backdrop under it on the way to `to_fullscreen`. A float's comes whole at once.
    pub(crate) fn backdrop_alpha(&self, to_fullscreen: bool) -> f32 {
        if to_fullscreen && self.from_floating {
            return 1.;
        }
        let from = f32::from(u8::from(self.from_fullscreen));
        let to = f32::from(u8::from(to_fullscreen));
        from + (to - from) * self.progress()
    }

    /// Its rect on the way to `to`, carried by its frame now at `origin`.
    pub fn rect(
        &self,
        to: Rectangle<f64, Logical>,
        origin: Option<Point<f64, Logical>>,
    ) -> Rectangle<f64, Logical> {
        let p = self.anim.value();
        let mut from = self.from.to_f64();
        if let (Some(now), Some(then)) = (origin, self.from_origin) {
            from.loc += now - then.to_f64();
        }
        Rectangle::new(
            Point::from((
                from.loc.x + (to.loc.x - from.loc.x) * p,
                from.loc.y + (to.loc.y - from.loc.y) * p,
            )),
            Size::from((
                from.size.w + (to.size.w - from.size.w) * p,
                from.size.h + (to.size.h - from.size.h) * p,
            )),
        )
    }
}

/// The largest side a rect change moves.
pub fn rect_change(a: Rectangle<i32, Logical>, b: Rectangle<i32, Logical>) -> i32 {
    (a.loc.x - b.loc.x)
        .abs()
        .max((a.loc.y - b.loc.y).abs())
        .max((a.size.w - b.size.w).abs())
        .max((a.size.h - b.size.h).abs())
}

impl Ewm {
    /// Spring every view whose rect changed since `before` from where it sat.
    pub(crate) fn animate_view_moves(&mut self, before: &[ViewRect]) {
        if self.animations_clock.should_complete_instantly() {
            return;
        }
        let clock = self.animations_clock.clone();
        let shown = self.view_rects();
        for now in &shown {
            if self
                .layout
                .monitor_of(&now.output)
                .is_some_and(|monitor| monitor.settles_at_once(now.frame))
            {
                continue;
            }
            let was = before.iter().find(|was| {
                was.surface == now.surface && was.entry == now.entry && was.output == now.output
            });
            if let Some(was) = was
                && rect_change(was.rect, now.rect) > RESIZE_FADE_THRESHOLD
                && let Some(surface) = self.surfaces.get_mut(&now.surface)
            {
                surface.start_move(now.entry, was, now, clock.clone());
            }
        }
        for surface in self.surfaces.values_mut() {
            surface.settle_moves(&shown);
        }
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
        if (size.w - old.w).abs().max((size.h - old.h).abs()) <= RESIZE_FADE_THRESHOLD {
            return None;
        }
        let mut views = Vec::new();
        let mut scale = 0f64;
        for (output, entry) in self.surface_views(id) {
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
        if surface.open.is_some()
            || surface.resize.is_some()
            || ewm.animations_clock.should_complete_instantly()
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::animation::Clock;

    fn view(
        rect: Rectangle<i32, Logical>,
        origin: Point<i32, Logical>,
        floating: bool,
    ) -> ViewRect {
        ViewRect {
            surface: 1,
            entry: LayoutEntryId::MIN,
            output: crate::testing::output("HEAD-A"),
            frame: 2,
            origin,
            rect,
            fullscreen: false,
            floating,
        }
    }

    /// A frame moving under a spring carries what is left of it along.
    #[test]
    fn frame_carries_the_spring() {
        let clock = Clock::with_time(Duration::ZERO);
        let was = view(
            Rectangle::new((10, 20).into(), (400, 300).into()),
            (10, 20).into(),
            false,
        );
        let now = view(
            Rectangle::new((0, 0).into(), (1920, 1080).into()),
            (0, 0).into(),
            false,
        );
        let spring = ViewMove::new(&was, &now, clock.clone());
        clock.set(Duration::from_millis(100));
        let to = now.rect.to_f64();
        let at = spring.rect(to, Some((0., 0.).into()));
        let carried = spring.rect(to, Some((30., 20.).into()));
        let left = 1. - f64::from(spring.progress());
        assert!((carried.loc.x - at.loc.x - 30. * left).abs() < 1e-6);
        assert!((carried.loc.y - at.loc.y - 20. * left).abs() < 1e-6);
        // Across spaces the frame does not carry it.
        let floated = ViewMove::new(&view(was.rect, was.origin, true), &now, clock);
        assert_eq!(
            floated.rect(to, Some((30., 20.).into())),
            floated.rect(to, None)
        );
    }
}
