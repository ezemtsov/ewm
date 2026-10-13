//! A view whose rect changes by more than the snap threshold springs from where it sat, carrying
//! whether it was fullscreen there, whether a layout or a client's fullscreen commit moved it. An
//! unchanged rect, or one changed by less, starts no spring, and a spring already under way hands
//! over from wherever it has got to.

use ewm_core::LayoutEntry;
use ewm_core::resize_fade::{RESIZE_FADE_THRESHOLD, rect_change};
use ewm_core::strip::Frame;
use ewm_core::testing::{Fixture, TestClient, Toplevel, nz};
use proptest::prelude::*;
use smithay::utils::{Logical, Rectangle, Size};

const OUTPUT: &str = "HEAD-A";
const FRAME: u64 = 500;
const ENTRIES: [u64; 2] = [7, 8];

type Rect = Rectangle<i32, Logical>;

#[derive(Debug, Clone, Copy)]
struct RectSpec {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

#[derive(Debug, Clone)]
enum Op {
    Layout([Option<RectSpec>; 2]),
    Fullscreen {
        entry: usize,
        on: bool,
    },
    /// The client answers its last configure with a buffer of that size.
    Answer,
}

fn rect_strategy() -> impl Strategy<Value = RectSpec> {
    (
        prop::sample::select(vec![0, 8, 20, 400]),
        prop::sample::select(vec![0, 6, 30]),
        prop::sample::select(vec![300u32, 400, 800]),
        prop::sample::select(vec![200u32, 300, 600]),
    )
        .prop_map(|(x, y, w, h)| RectSpec { x, y, w, h })
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (prop::option::of(rect_strategy()), prop::option::of(rect_strategy()))
            .prop_map(|(a, b)| Op::Layout([a, b])),
        2 => (0..2usize, any::<bool>()).prop_map(|(entry, on)| Op::Fullscreen { entry, on }),
        3 => Just(Op::Answer),
    ]
}

/// Whether `x` lies between `a` and `b` on every side, give or take rounding.
fn between(a: Rect, b: Rect, x: Rect) -> bool {
    let sides = |r: Rect| [r.loc.x, r.loc.y, r.size.w, r.size.h];
    sides(a)
        .into_iter()
        .zip(sides(b))
        .zip(sides(x))
        .all(|((a, b), x)| a.min(b) - 1 <= x && x <= a.max(b) + 1)
}

struct Harness {
    fix: Fixture,
    client: TestClient,
    toplevel: Toplevel,
    sid: u64,
}

impl Harness {
    fn new() -> Self {
        let mut fix = Fixture::new().unwrap();
        fix.add_output(OUTPUT, 1920, 1080);
        let mut client = fix.new_client().unwrap();
        fix.roundtrip(&mut client);
        let (toplevel, sid) = fix.open_toplevel_with_id(&mut client, "app", Size::from((400, 300)));
        Self {
            fix,
            client,
            toplevel,
            sid,
        }
    }

    /// Each view's rect with its fullscreen flag, while shown.
    fn rects(&self) -> [Option<(Rect, bool)>; 2] {
        let views = self.fix.ewm_ref().view_rects();
        ENTRIES.map(|entry| {
            views
                .iter()
                .find(|view| view.surface == self.sid && view.entry == nz(entry))
                .map(|view| (view.rect, view.fullscreen))
        })
    }

    fn moves(&self) -> [Option<(Rect, bool)>; 2] {
        ENTRIES.map(|entry| {
            self.fix.ewm_ref().surfaces[&self.sid]
                .view_move(nz(entry))
                .map(|view| (view.from, view.from_fullscreen))
        })
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::Layout(specs) => {
                let mut frame = Frame::new(FRAME);
                frame.width = 1920.0;
                frame.entries = specs
                    .iter()
                    .zip(ENTRIES)
                    .filter_map(|(spec, entry)| {
                        let spec = (*spec)?;
                        Some(LayoutEntry::surface(
                            nz(entry),
                            "view".into(),
                            self.sid,
                            spec.x,
                            spec.y,
                            spec.w,
                            spec.h,
                        ))
                    })
                    .collect();
                frame.selected_entry_id = frame.entries.first().map(|entry| entry.id);
                self.fix.tile(OUTPUT, vec![frame]);
            }
            Op::Fullscreen { entry, on } => self.fix.set_fullscreen(nz(ENTRIES[*entry]), *on),
            Op::Answer => {
                self.fix.answer_configure(&mut self.client, &self.toplevel);
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        ..ProptestConfig::default()
    })]

    #[test]
    fn views_spring_from_where_they_sat(ops in prop::collection::vec(op_strategy(), 1..30)) {
        let mut h = Harness::new();
        for op in &ops {
            let before = h.rects();
            let moving = h.moves();
            h.apply(op);
            let after = h.rects();
            let moves = h.moves();
            for i in 0..2 {
                match (before[i], after[i]) {
                    (Some((was, was_fullscreen)), Some((now, _)))
                        if rect_change(was, now) > RESIZE_FADE_THRESHOLD =>
                    {
                        let Some((from, from_fullscreen)) = moves[i] else {
                            return Err(TestCaseError::fail(format!(
                                "view {} went {:?} -> {:?} on {:?} without a spring",
                                ENTRIES[i], was, now, op
                            )));
                        };
                        prop_assert_eq!(from_fullscreen, was_fullscreen, "view {} on {:?}", ENTRIES[i], op);
                        // A spring under way hands over from wherever it has got to.
                        let origin = moving[i].map_or(was, |(origin, _)| origin);
                        prop_assert!(
                            between(origin, was, from),
                            "view {} springs from {:?}, not between {:?} and {:?}, on {:?}",
                            ENTRIES[i],
                            from,
                            origin,
                            was,
                            op
                        );
                    }
                    (Some(_), Some(_)) => prop_assert!(
                        moves[i].is_none() || moves[i] == moving[i],
                        "view {} sprang without a rect change on {:?}",
                        ENTRIES[i],
                        op
                    ),
                    _ => {}
                }
            }
        }
    }
}
