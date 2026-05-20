//! Property tests for the compositor focus state machine.
//!
//! Codifies invariants from `docs/focus-design.md` § "Invariants" and
//! asserts they hold after every randomly generated focus-mutating op.

use std::collections::{HashMap, HashSet};

use ewm_core::event::Event;
use ewm_core::resolve_keyboard_focus_target;
use ewm_core::strip::Frame;
use ewm_core::testing::Fixture;
use ewm_core::{LayoutEntry, OutputConfig, ViewId};
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
    RemoveOutput(usize),
    AddOutput(usize),
    DestroyFrame(u64),
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
    fullscreen: bool,
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
        fullscreen: false,
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
        (0..OUTPUTS.len()).prop_map(Op::RemoveOutput),
        (0..OUTPUTS.len()).prop_map(Op::AddOutput),
        FRAME_SID_RANGE.prop_map(Op::DestroyFrame),
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
                fullscreen: e.fullscreen,
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
            // Mirrors production: frames only enter a strip whose output is mapped.
            if !fix.has_output(output_name) {
                return;
            }
            if !strip_frame_ids(fix).contains(&surface_id) {
                let clock = fix.ewm().animations_clock.clone();
                let strip = fix
                    .ewm()
                    .output_strips
                    .entry(output_name.to_string())
                    .or_insert_with(|| ewm_core::strip::Strip::new(clock));
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
            let dest = OUTPUTS[output];
            let real: Vec<Frame> = frames.iter().map(to_frame).collect();
            let before = strip_frame_ids(fix);
            let placed: Vec<u64> = real
                .iter()
                .map(|f| f.surface_id)
                .filter(|&id| id != 0)
                .collect();
            fix.ewm().apply_output_layout(dest, real);
            // I8: apply_output_layout never drops frames.
            let after = strip_frame_ids(fix);
            assert!(
                before.is_subset(&after),
                "invariant 8: apply_output_layout dropped {:?}",
                before.difference(&after).collect::<Vec<_>>(),
            );
            // I10/I12: layout-named frames are present and in layout order.
            if let Some(strip) = fix.ewm_ref().output_strips.get(dest) {
                for id in &placed {
                    assert!(
                        strip.frames.iter().any(|f| f.surface_id == *id),
                        "invariant 10: frame {} missing from {} after layout",
                        id,
                        dest,
                    );
                }
                let subseq: Vec<u64> = strip
                    .frames
                    .iter()
                    .map(|f| f.surface_id)
                    .filter(|id| placed.contains(id))
                    .collect();
                assert_eq!(
                    subseq, placed,
                    "invariant 12: strip order on {} doesn't match layout order",
                    dest,
                );
            }
        }
        Op::FocusView(v) => {
            fix.ewm().set_focus_view(v, "proptest", false);
        }
        Op::FocusFrame(f) => {
            fix.ewm().set_focus_frame(f, "proptest", false);
        }
        Op::EnterPrefixRedirect(f) => {
            let known = strip_frame_ids(fix).contains(&f);
            fix.ewm().enter_prefix_redirect(f);
            if known {
                // I7: prefix-redirect parks focus on the frame (no view).
                assert_eq!(
                    fix.ewm_ref().focused_frame_id,
                    f,
                    "invariant 7: enter_prefix_redirect({}) didn't set focused_frame_id",
                    f,
                );
                assert_eq!(
                    fix.ewm_ref().focused_view_id,
                    None,
                    "invariant 7: enter_prefix_redirect({}) didn't clear focused_view_id",
                    f,
                );
            }
        }
        Op::RemoveOutput(output) => {
            let name = OUTPUTS[output];
            fix.remove_output(name);
            // I9: remove_output evicts the strip for that output.
            assert!(
                !fix.ewm_ref().output_strips.contains_key(name),
                "invariant 9: remove_output({}) left a strip entry behind",
                name,
            );
        }
        Op::AddOutput(output) => {
            let name = OUTPUTS[output];
            // Production never re-adds an already-mapped output.
            if !fix.has_output(name) {
                fix.add_output(name, 1920, 1080);
            }
        }
        Op::DestroyFrame(id) => {
            let was_present = strip_frame_ids(fix).contains(&id);
            let refocus = fix.ewm().handle_toplevel_destroyed_by_id(id);
            if let Some(target) = refocus {
                fix.ewm()
                    .set_focus_frame(target, "toplevel_destroyed", false);
            }
            if was_present {
                assert!(
                    !strip_frame_ids(fix).contains(&id),
                    "invariant 11: destroy({}) left the id in some strip",
                    id,
                );
            }
        }
    }
}

fn strip_frame_ids(fix: &Fixture) -> HashSet<u64> {
    fix.ewm_ref()
        .output_strips
        .values()
        .flat_map(|s| s.frames.iter().map(|f| f.surface_id))
        .filter(|&id| id != 0)
        .collect()
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

    // Invariant 6: selected_view_id, if set, names one of the frame's own entries.
    for strip in ewm.output_strips.values() {
        for f in &strip.frames {
            if let Some(sel) = f.selected_view_id {
                assert!(
                    f.surfaces.iter().any(|e| e.view_id == sel),
                    "invariant 6: frame {} selected_view_id={:?} not in its own entries",
                    f.surface_id,
                    sel,
                );
            }
        }
    }
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

/// Strategy without `CreateEmacsFrame` -- the fixture variant bypasses
/// reconcile and would break the event-balance invariant.
fn reconcile_op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..OUTPUTS.len(), layout_strategy())
            .prop_map(|(output, frames)| Op::ApplyLayout { output, frames }),
        ENTRY_VID_RANGE.prop_map(|v| Op::FocusView(nz(v))),
        FRAME_SID_RANGE.prop_map(Op::FocusFrame),
        FRAME_SID_RANGE.prop_map(Op::EnterPrefixRedirect),
        (0..OUTPUTS.len()).prop_map(Op::RemoveOutput),
        (0..OUTPUTS.len()).prop_map(Op::AddOutput),
        FRAME_SID_RANGE.prop_map(Op::DestroyFrame),
    ]
}

/// Snapshot in `apply_output_layout` misses outputs that have no strip yet.
fn ensure_strips(fix: &mut Fixture) {
    for name in OUTPUTS {
        if fix.has_output(name) {
            let clock = fix.ewm().animations_clock.clone();
            fix.ewm()
                .output_strips
                .entry(name.to_string())
                .or_insert_with(|| ewm_core::strip::Strip::new(clock));
        }
    }
}

proptest! {
    /// For every frame currently in some strip, the running net of
    /// `FrameShown - FrameHidden` events should be 1 iff it's that strip's
    /// active frame, else 0.
    #[test]
    fn frame_show_hide_net_tracks_active_slot(
        ops in prop::collection::vec(reconcile_op_strategy(), 1..30)
    ) {
        let mut fix = Fixture::new().unwrap();
        fix.enable_event_capture();
        for name in OUTPUTS {
            fix.add_output(name, 1920, 1080);
        }
        ensure_strips(&mut fix);
        let _ = fix.drain_events();

        let mut net: HashMap<u64, i32> = HashMap::new();

        for op in ops {
            apply_op(&mut fix, op);
            ensure_strips(&mut fix);

            for event in fix.drain_events() {
                match event {
                    Event::FrameShown { id } => *net.entry(id).or_insert(0) += 1,
                    Event::FrameHidden { id } => *net.entry(id).or_insert(0) -= 1,
                    // Production surface ids are monotonic; the test fixture
                    // recycles them, so drop net state when the id goes away.
                    Event::Close { id } => {
                        net.remove(&id);
                    }
                    _ => {}
                }
            }

            for (out_name, strip) in &fix.ewm_ref().output_strips {
                let active = strip.active_frame().map(|f| f.surface_id);
                for frame in &strip.frames {
                    let id = frame.surface_id;
                    let expected: i32 = if active == Some(id) { 1 } else { 0 };
                    let actual = net.get(&id).copied().unwrap_or(0);
                    prop_assert_eq!(
                        actual,
                        expected,
                        "frame {} on {}: net shown-hidden = {} but expected {}",
                        id,
                        out_name,
                        actual,
                        expected,
                    );
                }
            }
        }
    }
}

// `u64` stands in for `WlSurface`; the resolver is generic over surface type.
fn surface_map_strategy() -> impl Strategy<Value = HashMap<u64, u64>> {
    prop::collection::hash_map(1u64..=8, 100u64..=200, 0..=8)
}

proptest! {
    #[test]
    fn locked_with_lock_surface_wins(
        lock in 1u64..=999,
        layer in prop::option::of(1u64..=999),
        focused_id in 0u64..=20,
        map in surface_map_strategy(),
    ) {
        let resolved = resolve_keyboard_focus_target(
            true, Some(lock), layer,
            || map.get(&focused_id).copied(),
        );
        prop_assert_eq!(resolved, Some(lock));
    }

    #[test]
    fn locked_without_lock_surface_yields_none(
        layer in prop::option::of(1u64..=999),
        focused_id in 0u64..=20,
        map in surface_map_strategy(),
    ) {
        let resolved = resolve_keyboard_focus_target::<u64, _>(
            true, None, layer,
            || map.get(&focused_id).copied(),
        );
        prop_assert_eq!(resolved, None);
    }

    #[test]
    fn layer_focus_beats_toplevel(
        lock in prop::option::of(1u64..=999),
        layer in 1u64..=999,
        focused_id in 0u64..=20,
        map in surface_map_strategy(),
    ) {
        let resolved = resolve_keyboard_focus_target(
            false, lock, Some(layer),
            || map.get(&focused_id).copied(),
        );
        prop_assert_eq!(resolved, Some(layer));
    }

    #[test]
    fn toplevel_fallback(
        lock in prop::option::of(1u64..=999),
        focused_id in 0u64..=20,
        map in surface_map_strategy(),
    ) {
        let expected = map.get(&focused_id).copied();
        let resolved = resolve_keyboard_focus_target::<u64, _>(
            false, lock, None,
            || map.get(&focused_id).copied(),
        );
        prop_assert_eq!(resolved, expected);
    }

    #[test]
    fn resolver_is_total(
        locked in any::<bool>(),
        lock in prop::option::of(1u64..=999),
        layer in prop::option::of(1u64..=999),
        focused_id in 0u64..=20,
        map in surface_map_strategy(),
    ) {
        let _ = resolve_keyboard_focus_target(
            locked, lock, layer,
            || map.get(&focused_id).copied(),
        );
    }
}

/// 3x3 grid of 1920x1080 output cells; subsets exercise rows, columns,
/// stacks, gaps, and the full grid.
const NAV_GRID: [(i32, i32); 9] = [
    (0, 0),
    (1920, 0),
    (3840, 0),
    (0, 1080),
    (1920, 1080),
    (3840, 1080),
    (0, 2160),
    (1920, 2160),
    (3840, 2160),
];

#[derive(Debug, Clone)]
struct NavSetup {
    positions: Vec<(i32, i32)>,
    frames_per_output: Vec<usize>,
    active_idx_per_output: Vec<usize>,
    focused_output: usize,
}

impl NavSetup {
    fn n(&self) -> usize {
        self.positions.len()
    }

    fn output_name(&self, out: usize) -> String {
        format!("V{}", out)
    }

    fn frame_id(&self, out: usize, frame: usize) -> u64 {
        (out as u64 + 1) * 10 + frame as u64
    }

    fn active_frame(&self, out: usize) -> u64 {
        let frame = self.active_idx_per_output[out] % self.frames_per_output[out];
        self.frame_id(out, frame)
    }

    fn initial_focused_frame(&self) -> u64 {
        self.active_frame(self.focused_output)
    }

    fn all_active_frames(&self) -> HashSet<u64> {
        (0..self.n()).map(|i| self.active_frame(i)).collect()
    }
}

fn nav_setup() -> impl Strategy<Value = NavSetup> {
    prop::sample::subsequence(NAV_GRID.to_vec(), 2..=4)
        .prop_flat_map(|positions| {
            let n = positions.len();
            (
                Just(positions),
                prop::collection::vec(1usize..=3, n..=n),
                prop::collection::vec(0usize..3, n..=n),
                0..n,
            )
        })
        .prop_map(
            |(positions, frames_per_output, active_idx_per_output, focused_output)| NavSetup {
                positions,
                frames_per_output,
                active_idx_per_output,
                focused_output,
            },
        )
}

struct NavResult {
    focused: u64,
    pointer: (f64, f64),
}

fn nav_build(setup: &NavSetup) -> Fixture {
    let mut fix = Fixture::new().unwrap();
    for i in 0..setup.n() {
        fix.ewm().output_config.insert(
            setup.output_name(i),
            OutputConfig {
                position: Some(setup.positions[i]),
                ..Default::default()
            },
        );
        fix.add_output(&setup.output_name(i), 1920, 1080);
    }

    let clock = fix.ewm().animations_clock.clone();
    for i in 0..setup.n() {
        let strip = fix
            .ewm()
            .output_strips
            .entry(setup.output_name(i))
            .or_insert_with(|| ewm_core::strip::Strip::new(clock.clone()));
        for f in 0..setup.frames_per_output[i] {
            strip.frames.push(Frame::new(setup.frame_id(i, f)));
        }
    }

    // focused_output last so its frame wins keyboard focus for navigation.
    for i in (0..setup.n()).filter(|&i| i != setup.focused_output) {
        fix.ewm()
            .set_focus_frame(setup.active_frame(i), "nav_setup", false);
    }
    fix.ewm()
        .set_focus_frame(setup.initial_focused_frame(), "nav_setup", false);
    fix
}

fn nav_run(setup: &NavSetup, dx: i32, dy: i32, cx: f64, cy: f64) -> NavResult {
    let mut fix = nav_build(setup);
    fix.warp_pointer(cx, cy);
    fix.ewm().focus_output_direction(dx, dy);
    NavResult {
        focused: fix.ewm_ref().focused_frame_id,
        pointer: fix.ewm_ref().pointer_location(),
    }
}

fn nav_round_trip(setup: &NavSetup, dx: i32, dy: i32) -> (u64, u64) {
    let mut fix = nav_build(setup);
    fix.ewm().focus_output_direction(dx, dy);
    let after_a = fix.ewm_ref().focused_frame_id;
    fix.ewm().focus_output_direction(-dx, -dy);
    let after_b = fix.ewm_ref().focused_frame_id;
    (after_a, after_b)
}

proptest! {
    /// `focus_output_direction` outcome is cursor-independent, non-axial inputs no-op, the result is some output's active frame, and the pointer never moves (cursor warping is MFF's job in lisp, not the compositor's).
    #[test]
    fn focus_output_direction_properties(
        setup in nav_setup(),
        dx in -1i32..=1,
        dy in -1i32..=1,
        cx1 in -5000.0f64..10000.0,
        cy1 in -5000.0f64..10000.0,
        cx2 in -5000.0f64..10000.0,
        cy2 in -5000.0f64..10000.0,
    ) {
        let a = nav_run(&setup, dx, dy, cx1, cy1);
        let b = nav_run(&setup, dx, dy, cx2, cy2);
        prop_assert_eq!(
            a.focused, b.focused,
            "cursor-dependent: ({},{}) at ({},{}) -> {}, at ({},{}) -> {}",
            dx, dy, cx1, cy1, a.focused, cx2, cy2, b.focused,
        );

        prop_assert_eq!(a.pointer, (cx1, cy1), "compositor moved pointer during navigation");
        prop_assert_eq!(b.pointer, (cx2, cy2), "compositor moved pointer during navigation");

        let initial = setup.initial_focused_frame();
        if !matches!((dx, dy), (-1, 0) | (1, 0) | (0, -1) | (0, 1)) {
            prop_assert_eq!(
                a.focused, initial,
                "non-axial direction ({},{}) must no-op",
                dx, dy,
            );
        }

        let actives = setup.all_active_frames();
        prop_assert!(
            actives.contains(&a.focused),
            "outcome {} is not the active frame of any output (actives: {:?})",
            a.focused, actives,
        );
    }
}

proptest! {
    /// `focus_output_direction(dx, dy)` followed by `(-dx, -dy)` returns focus to the start when the first leg actually moved.
    #[test]
    fn focus_output_direction_round_trip(
        setup in nav_setup(),
        dir in prop_oneof![Just((-1i32, 0i32)), Just((1, 0)), Just((0, -1)), Just((0, 1))],
    ) {
        let (dx, dy) = dir;
        let initial = setup.initial_focused_frame();
        let (after_a, after_b) = nav_round_trip(&setup, dx, dy);
        if after_a != initial {
            prop_assert_eq!(
                after_b, initial,
                "round-trip ({},{}) then ({},{}) from {} went {} then {}",
                dx, dy, -dx, -dy, initial, after_a, after_b,
            );
        }
    }
}
