//! Until the client commits the buffer a layout asked for, the views of its surface keep
//! presenting the one it has: the view that buffer was made for at its own size, the others
//! covering their windows with it. The client answers within its size hints or with a size it
//! insists on, drawing into its surface, into a subsurface or not at all, and the commit
//! carrying its ack settles the debt.
//!
//! Ported from niri's src/layout/tests.rs: the hints, the forced size and the answer op.

use ewm_core::layout::resize_fade::ViewRect;
use ewm_core::layout::strip::Frame;
use ewm_core::testing::{Fixture, Subsurface, TestClient, Toplevel, nz};
use ewm_core::{LayoutEntry, LayoutEntryId};
use proptest::prelude::*;
use smithay::utils::{Logical, Size};

const OUTPUT: &str = "HEAD-A";
const OUTPUT_SIZE: (i32, i32) = (1920, 1080);
const FRAME: u64 = 500;
const ENTRIES: [u64; 2] = [7, 8];
/// Hints and forced sizes reach this far, past the largest window.
const SIDE: i32 = 1200;

/// A width and height, or a min and max of one side.
type Pair = (i32, i32);

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
    /// The client's min and max size, zero for none.
    Hints {
        min: Pair,
        max: Pair,
    },
    /// A size the client commits whatever it is asked, or none to answer within its hints again.
    Force(Option<Pair>),
    /// The client answers its last configure with a buffer on its surface.
    Answer,
    /// The client acks its last configure and draws nothing.
    Ack,
    /// The client acks its last configure on its surface and draws into a subsurface.
    AnswerBelow,
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

/// A min and max for one side, zero meaning none.
fn min_max_strategy() -> impl Strategy<Value = Pair> {
    prop_oneof![
        Just((0, 0)),
        (1..SIDE).prop_map(|n| (n, n)),
        (1..SIDE).prop_map(|min| (min, 0)),
        (1..SIDE).prop_map(|max| (0, max)),
        (1..SIDE, 1..SIDE).prop_map(|(min, max)| (min, max.max(min))),
    ]
}

/// Hints for both sides, now and then a fixed size.
fn hints_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (min_max_strategy(), min_max_strategy()).prop_map(|((min_w, max_w), (min_h, max_h))| {
            Op::Hints {
                min: (min_w, min_h),
                max: (max_w, max_h),
            }
        }),
        1 => min_max_strategy().prop_map(|size| Op::Hints { min: size, max: size }),
    ]
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (prop::option::of(rect_strategy()), prop::option::of(rect_strategy()))
            .prop_map(|(a, b)| Op::Layout([a, b])),
        2 => (0..2usize, any::<bool>()).prop_map(|(entry, on)| Op::Fullscreen { entry, on }),
        1 => hints_strategy(),
        1 => prop::option::of((1..SIDE, 1..SIDE)).prop_map(Op::Force),
        3 => Just(Op::Answer),
        1 => Just(Op::Ack),
        1 => Just(Op::AnswerBelow),
    ]
}

struct Harness {
    fix: Fixture,
    client: TestClient,
    toplevel: Toplevel,
    sid: u64,
    hints: (Pair, Pair),
    forced: Option<Pair>,
    below: Option<Subsurface>,
}

impl Harness {
    fn new() -> Self {
        let mut fix = Fixture::new().unwrap();
        fix.add_output(OUTPUT, 1920, 1080);
        let mut client = fix.new_client().unwrap();
        fix.roundtrip(&mut client);
        let (toplevel, sid) = fix.open_toplevel_with_id(&mut client, "app", Size::from((400, 300)));
        client.drain_events();
        Self {
            fix,
            client,
            toplevel,
            sid,
            hints: ((0, 0), (0, 0)),
            forced: None,
            below: None,
        }
    }

    /// The views of the surface, as drawn.
    fn views(&self) -> Vec<ViewRect> {
        self.fix.ewm_ref().views_of(self.sid)
    }

    fn synced(&self) -> bool {
        self.fix.ewm_ref().surfaces[&self.sid].synced()
    }

    /// The view the surface presents.
    fn presented(&self) -> Option<LayoutEntryId> {
        self.fix.ewm_ref().surfaces[&self.sid].presented
    }

    /// The size the client last committed.
    fn geometry(&self) -> Size<i32, Logical> {
        self.fix.ewm_ref().surfaces[&self.sid]
            .window
            .geometry()
            .size
    }

    /// What the client commits when asked `asked`: its forced size, else `asked` within its hints.
    fn answer(&self, asked: Pair) -> Size<i32, Logical> {
        let within = |x: i32, min: i32, max: i32| {
            let x = if max > 0 { x.min(max) } else { x };
            if min > 0 { x.max(min) } else { x }
        };
        let (min, max) = self.hints;
        Size::from(
            self.forced
                .unwrap_or((within(asked.0, min.0, max.0), within(asked.1, min.1, max.1))),
        )
    }

    /// The primary view of the surface.
    fn primary(&self) -> Option<&LayoutEntry> {
        self.fix
            .ewm_ref()
            .layout
            .all_frames()
            .flat_map(|frame| &frame.entries)
            .find(|entry| entry.surface_id() == Some(self.sid) && entry.primary())
    }

    /// Whether the frame shows `entry` fullscreen.
    fn shown_fullscreen(&self, entry: LayoutEntryId) -> bool {
        self.fix.ewm_ref().layout.all_frames().any(|frame| {
            frame
                .fullscreen_entry()
                .is_some_and(|shown| shown.id == entry)
        })
    }

    /// Apply `op`, returning the configure an answer committed for.
    fn apply(&mut self, op: &Op) -> Option<Pair> {
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
                None
            }
            Op::Fullscreen { entry, on } => {
                self.fix.set_fullscreen(nz(ENTRIES[*entry]), *on);
                None
            }
            Op::Hints { min, max } => {
                self.hints = (*min, *max);
                self.client
                    .set_size_hints(&self.toplevel, Size::from(*min), Size::from(*max));
                None
            }
            Op::Force(size) => {
                self.forced = *size;
                self.client
                    .set_forced_size(&self.toplevel, size.map(Size::from));
                None
            }
            Op::Answer => Self::asked(self.fix.answer_configure(&mut self.client, &self.toplevel)),
            Op::Ack => Self::asked(self.fix.ack_configure(&mut self.client, &self.toplevel)),
            Op::AnswerBelow => {
                let below = self
                    .below
                    .get_or_insert_with(|| self.client.create_subsurface(&self.toplevel.surface));
                Self::asked(self.fix.answer_configure_below(
                    &mut self.client,
                    &self.toplevel,
                    below,
                ))
            }
        }
    }

    /// The configure the answer drained `sizes` for, the last positive one.
    fn asked(sizes: Vec<Pair>) -> Option<Pair> {
        sizes.into_iter().rev().find(|&(w, h)| w > 0 && h > 0)
    }
}

fn check_ops(ops: &[Op]) -> Result<(), TestCaseError> {
    let mut h = Harness::new();
    for op in ops {
        let presented = h.presented();
        let geometry = h.geometry();
        let answered = h.apply(op);
        h.fix.ewm_ref().verify_invariants();
        if let Some(asked) = answered {
            // A buffer brings the answered size, an ack alone leaves the last one.
            let committed = if matches!(op, Op::Ack) {
                geometry
            } else {
                h.answer(asked)
            };
            prop_assert_eq!(
                h.geometry(),
                committed,
                "the size committed for {:?}",
                asked
            );
            prop_assert!(h.synced(), "answering {:?} left the client behind", asked);
        }
        // Caught up, the primary view presents the buffer at its size and the others cover their
        // windows with it, as from the first configure on. Until then the presented view stays.
        let presents = if h.synced() || presented.is_none() {
            h.primary().map(|entry| entry.id)
        } else {
            presented
        };
        let buffer = h.geometry();
        for view in h.views() {
            prop_assert_eq!(
                view.native,
                Some(view.entry) == presents,
                "view {} presents the wrong contents on {:?}",
                view.entry,
                op
            );
            let window = if h.shown_fullscreen(view.entry) {
                Size::from(OUTPUT_SIZE)
            } else {
                view.window.size
            };
            let scale = if view.native {
                1.
            } else {
                f64::max(
                    window.w as f64 / buffer.w as f64,
                    window.h as f64 / buffer.h as f64,
                )
            };
            prop_assert_eq!(
                view.scale,
                scale,
                "view {} draws at the wrong scale on {:?}",
                view.entry,
                op
            );
            prop_assert_eq!(
                view.rect.size,
                buffer.to_f64().upscale(scale).to_i32_round(),
                "view {} draws the wrong size on {:?}",
                view.entry,
                op
            );
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        ..ProptestConfig::default()
    })]

    #[test]
    fn views_present_the_buffer_they_were_made_for(
        ops in prop::collection::vec(op_strategy(), 1..30)
    ) {
        check_ops(&ops)?;
    }
}

// Sequences the randomised test once found failing.

const fn rect(w: u32, h: u32) -> RectSpec {
    RectSpec { x: 0, y: 0, w, h }
}

#[test]
fn an_answer_before_the_first_layout() {
    check_ops(&[
        Op::Answer,
        Op::Layout([None, Some(rect(300, 200))]),
        Op::Layout([Some(rect(300, 300)), Some(rect(300, 200))]),
    ])
    .unwrap();
}

#[test]
fn fullscreen_on_and_off_answered_each_time() {
    check_ops(&[
        Op::Layout([Some(rect(300, 200)), Some(rect(300, 300))]),
        Op::Fullscreen { entry: 0, on: true },
        Op::Answer,
        Op::Fullscreen {
            entry: 0,
            on: false,
        },
        Op::Answer,
    ])
    .unwrap();
}

#[test]
fn an_ack_without_a_buffer_between_fullscreen_requests() {
    check_ops(&[
        Op::Layout([Some(rect(300, 200)), Some(rect(300, 300))]),
        Op::Answer,
        Op::Fullscreen { entry: 0, on: true },
        Op::Ack,
        Op::Fullscreen {
            entry: 0,
            on: false,
        },
    ])
    .unwrap();
}
