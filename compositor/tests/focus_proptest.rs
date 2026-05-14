//! Property tests for the compositor focus state machine.
//!
//! Codifies invariants from `docs/focus-design.md` § "Compositor Focus State Machine"
//! and asserts they hold after every focus-mutating operation in a randomly
//! generated sequence.

use std::collections::HashSet;

use ewm_core::strip::Frame;
use ewm_core::testing::Fixture;
use ewm_core::{LayoutEntry, ViewId};
use proptest::prelude::*;

const OUTPUTS: [&str; 2] = ["HEAD-A", "HEAD-B"];
const MAX_FRAMES: usize = 3;

fn nz(n: u64) -> ViewId {
    std::num::NonZeroU64::new(n).unwrap()
}

#[derive(Debug, Clone)]
enum Op {
    /// Mimics `new_toplevel`'s eager-insert path for Emacs frames: at boot only
    /// Emacs frames exist and they enter the strip before any layout arrives.
    /// `output` indexes into `OUTPUTS`.
    CreateEmacsFrame {
        output: usize,
        surface_id: u64,
    },
    /// Apply a layout to `OUTPUTS[output]`.  If a frame in `frames` is currently
    /// on a different output, the compositor migrates it.
    ApplyLayout {
        output: usize,
        frames: Vec<FrameSpec>,
    },
    FocusView(ViewId),
    FocusFrame(u64),
    EnterPrefixRedirect(u64),
}

#[derive(Debug, Clone)]
struct FrameSpec {
    surface_id: u64,
    frame_view_id: ViewId,
    selected_view_id: Option<ViewId>,
    entries: Vec<EntrySpec>,
}

#[derive(Debug, Clone)]
struct EntrySpec {
    surface_id: u64,
    view_id: ViewId,
}

// Frame surface_ids / view_ids live in a disjoint range from entry ids so
// `set_focus_view(v)` and `set_focus_frame(f)` never collide.
const ENTRY_SID_RANGE: std::ops::RangeInclusive<u64> = 1..=8;
const ENTRY_VID_RANGE: std::ops::RangeInclusive<u64> = 1..=8;
const FRAME_SID_RANGE: std::ops::RangeInclusive<u64> = 100..=102;

fn entry_strategy() -> impl Strategy<Value = EntrySpec> {
    (ENTRY_SID_RANGE, ENTRY_VID_RANGE.prop_map(nz)).prop_map(|(sid, vid)| EntrySpec {
        surface_id: sid,
        view_id: vid,
    })
}

/// Generates layouts satisfying the Emacs side's invariants:
/// - frame surface_ids and view_ids unique within the layout (assigned by index);
/// - entry view_ids unique across all frames (dedup + round-robin distribution);
/// - `selected_view_id` either `None` or one of the frame's own entries.
fn layout_strategy() -> impl Strategy<Value = Vec<FrameSpec>> {
    (
        1usize..=MAX_FRAMES,
        prop::collection::vec(entry_strategy(), 0..=8),
        prop::collection::vec(any::<bool>(), MAX_FRAMES),
    )
        .prop_map(|(n_frames, entries, has_sel_flags)| {
            let mut seen = HashSet::new();
            let entries: Vec<EntrySpec> = entries
                .into_iter()
                .filter(|e| seen.insert(e.view_id))
                .collect();

            let mut buckets: Vec<Vec<EntrySpec>> = (0..n_frames).map(|_| Vec::new()).collect();
            for (i, e) in entries.into_iter().enumerate() {
                buckets[i % n_frames].push(e);
            }

            buckets
                .into_iter()
                .enumerate()
                .map(|(i, entries)| {
                    let selected_view_id = if has_sel_flags[i] {
                        entries.first().map(|e| e.view_id)
                    } else {
                        None
                    };
                    FrameSpec {
                        surface_id: 100 + i as u64,
                        frame_view_id: nz(200 + i as u64),
                        selected_view_id,
                        entries,
                    }
                })
                .collect()
        })
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..OUTPUTS.len(), FRAME_SID_RANGE)
            .prop_map(|(output, surface_id)| Op::CreateEmacsFrame { output, surface_id }),
        (0..OUTPUTS.len(), layout_strategy())
            .prop_map(|(output, frames)| Op::ApplyLayout { output, frames }),
        ENTRY_VID_RANGE.prop_map(|v| Op::FocusView(nz(v))),
        FRAME_SID_RANGE.prop_map(Op::FocusFrame),
        FRAME_SID_RANGE.prop_map(Op::EnterPrefixRedirect),
    ]
}

fn to_frame(spec: &FrameSpec) -> Frame {
    Frame {
        name: String::new(),
        surface_id: spec.surface_id,
        view_id: spec.frame_view_id,
        width: 1920.0,
        surfaces: spec
            .entries
            .iter()
            .map(|e| LayoutEntry {
                view_id: e.view_id,
                surface_id: e.surface_id,
                x: 0,
                y: 0,
                w: 100,
                h: 100,
                primary: false,
                fullscreen: false,
            })
            .collect(),
        selected_view_id: spec.selected_view_id,
        move_anim: None,
    }
}

fn apply_op(fix: &mut Fixture, op: Op) {
    match op {
        Op::CreateEmacsFrame { output, surface_id } => {
            // Mirrors `new_toplevel`'s eager-insert + auto-focus.  If the frame
            // already exists on any output, skip the insert (real `new_toplevel`
            // wouldn't be called twice for the same id either).
            let output_name = OUTPUTS[output];
            let already_present = fix
                .ewm_ref()
                .output_strips
                .values()
                .any(|s| s.frames.iter().any(|f| f.surface_id == surface_id));
            if !already_present {
                let strip = fix
                    .ewm()
                    .output_strips
                    .entry(output_name.to_string())
                    .or_default();
                strip.frames.push(Frame::new(surface_id));
            }
            let do_auto_focus = fix.ewm().focused_surface_id() == 0;
            if do_auto_focus {
                fix.ewm()
                    .set_focus_frame(surface_id, "initial_surface", false);
                // Catches the boot regression: auto-focus must land on `surface_id`.
                assert_eq!(
                    fix.ewm().focused_frame_id,
                    surface_id,
                    "auto-focus for new Emacs frame {} did not set focused_frame_id",
                    surface_id,
                );
            }
        }
        Op::ApplyLayout { output, frames } => {
            let real: Vec<Frame> = frames.iter().map(to_frame).collect();
            fix.ewm().apply_output_layout(OUTPUTS[output], real);
        }
        Op::FocusView(v) => {
            fix.ewm().set_focus_view(v, "proptest", false);
        }
        Op::FocusFrame(f) => {
            fix.ewm().set_focus_frame(f, "proptest", false);
        }
        Op::EnterPrefixRedirect(f) => {
            fix.ewm().enter_prefix_redirect(f);
        }
    }
}

fn assert_invariants(fix: &Fixture) {
    let ewm = fix.ewm_ref();

    // Invariant 1: focused_frame_id == 0 ⟹ focused_view_id == None.
    if ewm.focused_frame_id == 0 {
        assert_eq!(
            ewm.focused_view_id, None,
            "invariant 1: focused_view_id={:?} while focused_frame_id is 0",
            ewm.focused_view_id,
        );
    }

    // Invariant 2: focused_view_id == Some(v) ⟹ v is the view_id of some entry
    // on the focused frame.
    if let Some(v) = ewm.focused_view_id {
        let in_focused_frame = ewm
            .output_strips
            .values()
            .flat_map(|s| s.frames.iter())
            .find(|f| f.surface_id == ewm.focused_frame_id)
            .map(|f| f.surfaces.iter().any(|e| e.view_id == v))
            .unwrap_or(false);
        assert!(
            in_focused_frame,
            "invariant 2: focused_view_id={:?} not in focused frame {}",
            v, ewm.focused_frame_id,
        );
    }

    // Invariant 3: a frame's surface_id appears in at most one strip.  Cross-output
    // migration in `apply_output_layout` is supposed to enforce this.
    let mut seen: HashSet<u64> = HashSet::new();
    for strip in ewm.output_strips.values() {
        for f in &strip.frames {
            assert!(
                seen.insert(f.surface_id),
                "invariant 3: frame {} appears in multiple strips",
                f.surface_id,
            );
        }
    }

    // Invariant 4: the focused frame is the active frame of its own strip.
    if ewm.focused_frame_id != 0 {
        for strip in ewm.output_strips.values() {
            let active_id = strip.frames.get(strip.active_idx()).map(|f| f.surface_id);
            if active_id == Some(ewm.focused_frame_id) {
                continue;
            }
            assert!(
                !strip
                    .frames
                    .iter()
                    .any(|f| f.surface_id == ewm.focused_frame_id),
                "invariant 4: focused frame {} present in a strip but not its active frame",
                ewm.focused_frame_id,
            );
        }
    }

    // Invariant 5: at least one frame exists ⟺ focused_frame_id is non-zero.
    // EWM always has exactly one focused frame whenever any frame is mapped.
    let any_frame = ewm.output_strips.values().any(|s| !s.frames.is_empty());
    assert_eq!(
        any_frame,
        ewm.focused_frame_id != 0,
        "invariant 5: any_frame={} but focused_frame_id={}",
        any_frame,
        ewm.focused_frame_id,
    );
}

proptest! {
    #[test]
    fn focus_invariants_hold(ops in prop::collection::vec(op_strategy(), 1..30)) {
        let mut fix = Fixture::new().unwrap();
        for name in OUTPUTS {
            fix.add_output(name, 1920, 1080);
        }
        assert_invariants(&fix);
        for op in ops {
            apply_op(&mut fix, op);
            assert_invariants(&fix);
        }
    }
}
