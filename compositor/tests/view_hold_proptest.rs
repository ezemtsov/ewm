//! Until the buffer a layout asked for comes, the views of its surface keep showing what they
//! showed, whatever Emacs does to their windows meanwhile.

use ewm_core::LayoutEntry;
use ewm_core::resize_fade::{RESIZE_FADE_THRESHOLD, size_change};
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

    /// Each view's contents' rect, while shown.
    fn rects(&self) -> [Option<Rect>; 2] {
        let views = self.fix.ewm_ref().view_rects();
        ENTRIES.map(|entry| {
            views
                .iter()
                .find(|view| view.surface == self.sid && view.entry == nz(entry))
                .map(|view| view.rect)
        })
    }

    fn owes(&self) -> bool {
        self.fix.ewm_ref().surfaces[&self.sid].owes_buffer()
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
    fn views_hold_until_the_buffer_comes(ops in prop::collection::vec(op_strategy(), 1..30)) {
        let mut h = Harness::new();
        for op in &ops {
            let before = h.rects();
            h.apply(op);
            let after = h.rects();
            // A layout or a request that leaves a buffer owed changes no view's contents' size.
            if matches!(op, Op::Answer) || !h.owes() {
                continue;
            }
            for i in 0..2 {
                let (Some(was), Some(now)) = (before[i], after[i]) else {
                    continue;
                };
                prop_assert!(
                    size_change(was.size, now.size) <= RESIZE_FADE_THRESHOLD,
                    "view {} owed a buffer yet went from {:?} to {:?} on {:?}",
                    ENTRIES[i],
                    was,
                    now,
                    op
                );
            }
        }
    }
}
