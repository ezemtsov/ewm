//! Zoomed-out view of each output's strip.
//!
//! Ported from niri's overview (`layout/mod.rs`, `layout/monitor.rs`,
//! `rubber_band.rs`), adapted to one strip per output.
//!
//! Every output's content shrinks about its centre so neighbouring frames come
//! into view; layer-shell panels stay put on top. Opened by key or a 3-finger
//! vertical swipe, closed the same way or by clicking a frame.

use std::time::Duration;

use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::shadow_style::{Color, FloatOrInt, Shadow, ShadowOffset};
use crate::strip::{AnimationsClock, DEFAULT_ANIM_DURATION, SwipeTracker, ViewAnimation};
use crate::utils::get_monotonic_time;

/// Touchpad travel in unaccelerated units for a full open or close.
const GESTURE_MOVEMENT: f64 = 300.;
const GESTURE_RUBBER_BAND: RubberBand = RubberBand {
    stiffness: 0.5,
    limit: 0.05,
};

pub const DEFAULT_ZOOM: f64 = 0.5;
pub const DEFAULT_BACKDROP_COLOR: [f32; 4] = [0.15, 0.15, 0.15, 1.];
/// Fill of the output behind its frames, visible where nothing else is.
pub const DEFAULT_BACKGROUND_COLOR: [f32; 4] = [0.25, 0.25, 0.25, 1.];

/// Soft overshoot past a range edge.
#[derive(Debug, Clone, Copy)]
pub struct RubberBand {
    pub stiffness: f64,
    pub limit: f64,
}

impl RubberBand {
    pub fn band(&self, x: f64) -> f64 {
        let c = self.stiffness;
        let d = self.limit;
        (1. - (1. / (x * c / d + 1.))) * d
    }

    pub fn clamp(&self, min: f64, max: f64, x: f64) -> f64 {
        let clamped = x.clamp(min, max);
        let sign = if x < clamped { -1. } else { 1. };
        let diff = (x - clamped).abs();
        clamped + sign * self.band(diff)
    }
}

#[derive(Debug)]
enum Progress {
    Animation(ViewAnimation),
    Gesture(Gesture),
    Open,
}

#[derive(Debug)]
struct Gesture {
    tracker: SwipeTracker,
    start: f64,
    value: f64,
}

/// Overview state machine; progress runs 0 (closed) to 1 (fully zoomed out).
#[derive(Debug)]
pub struct Overview {
    open: bool,
    progress: Option<Progress>,
    zoom: f64,
    pub backdrop_color: [f32; 4],
    clock: AnimationsClock,
}

impl Overview {
    pub fn new(clock: AnimationsClock) -> Self {
        Self {
            open: false,
            progress: None,
            zoom: DEFAULT_ZOOM,
            backdrop_color: DEFAULT_BACKDROP_COLOR,
            clock,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Zoomed out, mid-gesture or mid-animation: content is not at unit scale.
    pub fn is_active(&self) -> bool {
        self.progress.is_some()
    }

    pub fn is_animating(&self) -> bool {
        matches!(self.progress, Some(Progress::Animation(_)))
    }

    pub fn progress(&self) -> Option<f64> {
        self.progress.as_ref().map(|p| match p {
            Progress::Animation(anim) => anim.value(),
            Progress::Gesture(gesture) => gesture.value,
            Progress::Open => 1.,
        })
    }

    pub fn zoom(&self) -> f64 {
        compute_zoom(self.zoom, self.progress())
    }

    pub fn set_zoom(&mut self, zoom: f64) {
        self.zoom = zoom;
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        let from = self.progress().unwrap_or(0.);
        let to = if self.open { 1. } else { 0. };
        self.progress = Some(Progress::Animation(ViewAnimation::new(
            self.clock.clone(),
            from,
            to,
            DEFAULT_ANIM_DURATION,
        )));
    }

    pub fn close(&mut self) -> bool {
        if !self.open {
            return false;
        }
        self.toggle();
        true
    }

    pub fn gesture_begin(&mut self) {
        self.open = true;
        let value = self.progress().unwrap_or(0.);
        self.progress = Some(Progress::Gesture(Gesture {
            tracker: SwipeTracker::new(),
            start: value,
            value,
        }));
    }

    /// Push a swipe delta; positive opens. `Some(true)` when the view changed.
    pub fn gesture_update(&mut self, delta: f64, timestamp: Duration) -> Option<bool> {
        let Some(Progress::Gesture(gesture)) = &mut self.progress else {
            return None;
        };
        gesture.tracker.push(delta, timestamp);
        let pos = gesture.tracker.pos() / GESTURE_MOVEMENT;
        let value = GESTURE_RUBBER_BAND.clamp(0., 1., gesture.start + pos);
        if gesture.value == value {
            return Some(false);
        }
        gesture.value = value;
        Some(true)
    }

    /// Settle to whichever end the projected swipe is closer to.
    pub fn gesture_end(&mut self) -> bool {
        let Some(Progress::Gesture(gesture)) = &mut self.progress else {
            return false;
        };
        gesture.tracker.push(0., get_monotonic_time());
        let pos = gesture.tracker.projected_end_pos() / GESTURE_MOVEMENT;
        let target = (gesture.start + pos).clamp(0., 1.).round();
        self.open = target == 1.;
        self.progress = Some(Progress::Animation(ViewAnimation::new(
            self.clock.clone(),
            gesture.value,
            target,
            DEFAULT_ANIM_DURATION,
        )));
        true
    }

    /// Settle a finished animation. Call once per tick.
    pub fn advance(&mut self) {
        if let Some(Progress::Animation(anim)) = &self.progress
            && anim.is_done()
        {
            self.progress = self.open.then_some(Progress::Open);
        }
    }
}

fn compute_zoom(zoom: f64, progress: Option<f64>) -> f64 {
    let zoom = zoom.clamp(0.0001, 0.75);
    match progress {
        Some(p) => (1. - p * (1. - zoom)).max(0.0001),
        None => 1.,
    }
}

/// Shadow under the zoomed output, sized relative to a 1080px-tall output.
pub fn shadow_config(view_height: f64) -> Shadow {
    let norm = view_height / 1080.;
    Shadow {
        on: true,
        offset: ShadowOffset {
            x: FloatOrInt(0.),
            y: FloatOrInt(10. * norm),
        },
        softness: 40. * norm,
        spread: 10. * norm,
        draw_behind_window: false,
        color: Color::from_rgba8_unpremul(0, 0, 0, 0x50),
        inactive_color: None,
    }
}

/// Where an output's unzoomed content lands: scaled by `zoom` about the output
/// origin, then moved by `offset`. The default leaves content where it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub zoom: f64,
    pub offset: Point<f64, Logical>,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            zoom: 1.,
            offset: Point::from((0., 0.)),
        }
    }
}

impl Geometry {
    /// Centre the zoomed output, moved right by `shift_x`; rounded to physical pixels.
    pub fn new(output_size: Size<f64, Logical>, scale: f64, zoom: f64, shift_x: f64) -> Self {
        let centred = (output_size.to_point() - output_size.upscale(zoom).to_point()).downscale(2.);
        let offset = centred + Point::from((shift_x, 0.));
        let offset = offset.to_physical_precise_round(scale).to_logical(scale);
        Self { zoom, offset }
    }

    pub fn to_content(&self, p: Point<f64, Logical>) -> Point<f64, Logical> {
        (p - self.offset).downscale(self.zoom)
    }

    /// Content whose zoomed image is the screen rectangle `r`.
    pub fn to_content_rect(&self, r: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        Rectangle::new(self.to_content(r.loc), r.size.downscale(self.zoom))
    }

    /// Content shown on `output_rect`, in the same global coordinates.
    pub fn visible_content(&self, output_rect: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
        let mut r = self.to_content_rect(Rectangle::from_size(output_rect.size.to_f64()));
        r.loc += output_rect.loc.to_f64();
        Rectangle::new(r.loc.to_i32_floor(), r.size.to_i32_ceil())
    }

    /// Strip-space range whose zoomed image falls inside `working_area`.
    pub fn strip_viewport(&self, working_area: Rectangle<i32, Logical>) -> Rectangle<f64, Logical> {
        let wa = working_area.to_f64();
        let mut r = self.to_content_rect(wa);
        r.loc -= wa.loc;
        r
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn overview() -> Overview {
        let clock = AnimationsClock::default();
        clock.set_complete_instantly(true);
        Overview::new(clock)
    }

    fn to_screen(geo: &Geometry, p: Point<f64, Logical>) -> Point<f64, Logical> {
        p.upscale(geo.zoom) + geo.offset
    }

    proptest! {
        #[test]
        fn rubber_band_clamp_is_monotonic_and_bounded(
            a in -3f64..=4.,
            b in -3f64..=4.,
        ) {
            let band = GESTURE_RUBBER_BAND;
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            let (clo, chi) = (band.clamp(0., 1., lo), band.clamp(0., 1., hi));
            prop_assert!(clo <= chi);
            prop_assert!(-band.limit < clo && chi < 1. + band.limit);
        }

        // A swipe of at least half the travel settles on the end it points to.
        #[test]
        fn gesture_settles_toward_the_swipe(
            steps in prop::collection::vec(10f64..=60., 15..20),
            opens in any::<bool>(),
        ) {
            let mut overview = overview();
            if !opens {
                overview.toggle();
                overview.advance();
            }
            overview.gesture_begin();
            let sign = if opens { 1. } else { -1. };
            for (i, delta) in steps.iter().enumerate() {
                overview.gesture_update(sign * delta, Duration::from_millis(i as u64 * 8));
            }
            overview.gesture_end();
            prop_assert_eq!(overview.is_open(), opens);
            overview.advance();
            prop_assert_eq!(overview.is_active(), opens);
            prop_assert_eq!(overview.progress(), opens.then_some(1.));
        }

        #[test]
        fn identity_geometry_keeps_working_area_viewport(
            w in 640i32..=3840,
            h in 480i32..=2160,
            wa_x in 0i32..=200,
            wa_y in 0i32..=200,
        ) {
            let geo = Geometry::new(Size::from((f64::from(w), f64::from(h))), 1., 1., 0.);
            prop_assert_eq!(geo.offset, Point::from((0., 0.)));
            let wa = Rectangle::new(Point::from((wa_x, wa_y)), Size::from((w - wa_x, h - wa_y)));
            let viewport = geo.strip_viewport(wa);
            prop_assert_eq!(viewport, Rectangle::from_size(wa.size.to_f64()));
        }

        #[test]
        fn zoomed_viewport_maps_onto_working_area(
            w in 640f64..=3840.,
            h in 480f64..=2160.,
            zoom in 0.1f64..=0.75,
            wa_x in 0i32..=200,
        ) {
            let geo = Geometry::new(Size::from((w, h)), 1., zoom, 0.);
            let wa = Rectangle::new(
                Point::from((wa_x, 0)),
                Size::from((w as i32 - wa_x, h as i32)),
            );
            let viewport = geo.strip_viewport(wa);
            let wa_f = wa.to_f64();
            let left = to_screen(&geo, Point::from((wa_f.loc.x + viewport.loc.x, 0.))).x;
            let right = to_screen(
                &geo,
                Point::from((wa_f.loc.x + viewport.loc.x + viewport.size.w, 0.)),
            )
            .x;
            prop_assert!((left - wa_f.loc.x).abs() < 1e-6);
            prop_assert!((right - (wa_f.loc.x + wa_f.size.w)).abs() < 1e-6);
        }
    }

    #[test]
    fn toggle_flips_open_and_settles() {
        let mut overview = overview();
        assert!(!overview.is_active());
        overview.toggle();
        assert!(overview.is_open());
        assert!(overview.is_animating());
        overview.advance();
        assert!(!overview.is_animating());
        assert_eq!(overview.progress(), Some(1.));
        assert!(overview.close());
        overview.advance();
        assert!(!overview.is_active());
        assert_eq!(overview.zoom(), 1.);
    }
}
