//! Property tests for the compositor focus state machine.
//!
//! Codifies invariants from `docs/focus-design.md` § "Invariants" and
//! asserts they hold after every randomly generated focus-mutating op.

use std::collections::{HashMap, HashSet};

use ewm_core::event::Event;
use ewm_core::strip::Frame;
use ewm_core::testing::{Fixture, mark_active_frame_close, strip_with_frame_ids};
use ewm_core::{LayoutEntry, LayoutEntryId, OutputConfig, resolve_keyboard_focus_target};
use proptest::prelude::*;

const OUTPUTS: [&str; 2] = ["HEAD-A", "HEAD-B"];
const MAX_FRAMES: usize = 3;

fn nz(n: u64) -> LayoutEntryId {
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
    /// Declare frame contents. Placement is the compositor's: frames the
    /// layout names but has not placed are ignored, placed ones keep their slot.
    ApplyLayout {
        frames: Vec<FrameSpec>,
    },
    FocusView(LayoutEntryId),
    FocusFrame(u64),
    DisableOutput(usize),
    EnableOutput(usize),
    RemoveOutput(usize),
    AddOutput(usize),
    DestroyFrame(u64),
    DestroySurface(u64),
    ActivateSurface(u64),
}

#[derive(Debug, Clone)]
struct FrameSpec {
    surface_id: u64,
    frame_focus_id: LayoutEntryId,
    selected_entry_id: Option<LayoutEntryId>,
    entries: Vec<EntrySpec>,
}

#[derive(Debug, Clone)]
struct EntrySpec {
    surface_id: u64,
    entry_id: LayoutEntryId,
}

// Frame surface_ids / entry_ids live in a disjoint range from entry ids so
// `set_focus_entry(v)` and `set_focus_frame(f)` never collide.
const ENTRY_SID_RANGE: std::ops::RangeInclusive<u64> = 1..=8;
const FRAME_SID_RANGE: std::ops::RangeInclusive<u64> = 100..=102;
const MAX_ENTRIES_PER_LAYOUT: usize = 8;

#[derive(Debug, Clone)]
struct EntrySeed {
    surface_id: u64,
}

fn entry_entry_id_for(frame_surface_id: u64, entry_idx: usize) -> LayoutEntryId {
    nz(10_000 + frame_surface_id * 100 + entry_idx as u64)
}

fn entry_view_strategy() -> impl Strategy<Value = LayoutEntryId> {
    (FRAME_SID_RANGE, 0usize..MAX_ENTRIES_PER_LAYOUT)
        .prop_map(|(frame_id, entry_idx)| entry_entry_id_for(frame_id, entry_idx))
}

fn entry_strategy() -> impl Strategy<Value = EntrySeed> {
    ENTRY_SID_RANGE.prop_map(|surface_id| EntrySeed { surface_id })
}

/// Generates layouts satisfying the Emacs side's invariants:
/// - frame surface_ids and entry_ids unique within the layout (assigned by index);
/// - entry entry_ids unique across all frames and retained old frames;
/// - `selected_entry_id` either `None` or one of the frame's own entries.
fn layout_strategy() -> impl Strategy<Value = Vec<FrameSpec>> {
    (
        1usize..=MAX_FRAMES,
        prop::collection::vec(entry_strategy(), 0..=MAX_ENTRIES_PER_LAYOUT),
        prop::collection::vec(any::<bool>(), MAX_FRAMES),
    )
        .prop_map(|(n_frames, entries, has_sel_flags)| {
            let mut buckets: Vec<Vec<EntrySeed>> = (0..n_frames).map(|_| Vec::new()).collect();
            for (i, e) in entries.into_iter().enumerate() {
                buckets[i % n_frames].push(e);
            }

            buckets
                .into_iter()
                .enumerate()
                .map(|(i, entries)| {
                    let frame_surface_id = 100 + i as u64;
                    let entries: Vec<EntrySpec> = entries
                        .into_iter()
                        .enumerate()
                        .map(|(entry_idx, entry)| EntrySpec {
                            surface_id: entry.surface_id,
                            entry_id: entry_entry_id_for(frame_surface_id, entry_idx),
                        })
                        .collect();
                    let selected_entry_id = if has_sel_flags[i] {
                        entries.first().map(|e| e.entry_id)
                    } else {
                        None
                    };
                    FrameSpec {
                        surface_id: frame_surface_id,
                        frame_focus_id: nz(200 + i as u64),
                        selected_entry_id,
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
        layout_strategy().prop_map(|frames| Op::ApplyLayout { frames }),
        entry_view_strategy().prop_map(Op::FocusView),
        FRAME_SID_RANGE.prop_map(Op::FocusFrame),
        (0..OUTPUTS.len()).prop_map(Op::DisableOutput),
        (0..OUTPUTS.len()).prop_map(Op::EnableOutput),
        (0..OUTPUTS.len()).prop_map(Op::RemoveOutput),
        (0..OUTPUTS.len()).prop_map(Op::AddOutput),
        FRAME_SID_RANGE.prop_map(Op::DestroyFrame),
        ENTRY_SID_RANGE.prop_map(Op::DestroySurface),
        ENTRY_SID_RANGE.prop_map(Op::ActivateSurface),
    ]
}

fn to_frame(spec: &FrameSpec) -> Frame {
    Frame {
        name: String::new(),
        workspace_name: None,
        surface_id: spec.surface_id,
        focus_id: spec.frame_focus_id,
        width: 1920.0,
        height: 0.0,
        entries: spec
            .entries
            .iter()
            .map(|e| {
                LayoutEntry::surface(
                    e.entry_id,
                    "entry".to_string(),
                    e.surface_id,
                    0,
                    0,
                    100,
                    100,
                )
            })
            .collect(),
        fullscreen: None,
        selected_entry_id: spec.selected_entry_id,
        move_anim: None,
        floating_pos: None,
        close_snapshot: None,
    }
}

fn frame_with_entry(frame_id: u64, entry_id: LayoutEntryId, surface_id: u64) -> Frame {
    to_frame(&FrameSpec {
        surface_id: frame_id,
        frame_focus_id: nz(200),
        selected_entry_id: Some(entry_id),
        entries: vec![EntrySpec {
            surface_id,
            entry_id,
        }],
    })
}

fn apply_op(fix: &mut Fixture, op: Op) {
    match op {
        Op::CreateEmacsFrame { output, surface_id } => {
            // Mirrors `new_toplevel`'s eager-insert + auto-focus.  If the frame
            // already exists on any output, skip the insert (real `new_toplevel`
            // wouldn't be called twice for the same id either).
            let output_name = OUTPUTS[output];
            // Mirrors production: frames only enter a strip whose output is mapped.
            if !is_output_mapped(fix, output_name) {
                return;
            }
            let already_present = strip_frame_ids(fix).contains(&surface_id);
            if already_present && fix.ewm_ref().layout.frame_output(surface_id) != Some(output_name)
            {
                return;
            }
            if !already_present {
                let strip = fix.strip_mut(output_name);
                strip.push_frame(Frame::new(surface_id));
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
        Op::ApplyLayout { frames } => {
            let real: Vec<Frame> = frames.iter().map(to_frame).collect();
            fix.ewm().apply_layout(real.clone());
            // A frame on a strip that the layout names carries the submitted
            // contents. Homeless frames wait for an output.
            for frame in &real {
                let Some(location) = fix.ewm_ref().layout.frame_location(frame.surface_id) else {
                    continue;
                };
                let live = location.frame;
                let ids = |f: &Frame| f.entries.iter().map(|e| e.id).collect::<Vec<_>>();
                assert_eq!(
                    ids(live),
                    ids(frame),
                    "layout did not replace the contents of frame {}",
                    frame.surface_id,
                );
            }
            for id in fix.ewm_ref().pending_activations() {
                assert!(
                    !fix.ewm_ref().focus_target_ready(id),
                    "layout left activation of {} pending although it can be focused",
                    id,
                );
            }
        }
        Op::FocusView(v) => {
            fix.ewm().set_focus_entry(v, "proptest", false);
        }
        Op::FocusFrame(f) => {
            fix.ewm().set_focus_frame(f, "proptest", false);
        }
        Op::DisableOutput(output) => {
            set_output_enabled(fix, OUTPUTS[output], false);
        }
        Op::EnableOutput(output) => {
            set_output_enabled(fix, OUTPUTS[output], true);
        }
        Op::RemoveOutput(output) => {
            let name = OUTPUTS[output];
            let before = strip_frame_ids(fix);
            fix.remove_output(name);
            // I10: output topology changes may migrate or park frames, but
            // only explicit frame destruction may drop them.
            let after = strip_frame_ids(fix);
            assert!(
                before.is_subset(&after),
                "invariant 10: remove_output({}) dropped {:?}",
                name,
                before.difference(&after).collect::<Vec<_>>(),
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
            let prev_focused = fix.ewm_ref().focused_frame_id;
            let expected_same_output_refocus = if prev_focused == id {
                fix.ewm_ref()
                    .layout
                    .frame_location(id)
                    .and_then(|location| {
                        let idx = location.frame_idx;
                        let frames = location.monitor.strip.frames();
                        frames
                            .get(idx + 1)
                            .or_else(|| idx.checked_sub(1).and_then(|i| frames.get(i)))
                            .map(|frame| frame.surface_id)
                    })
            } else {
                None
            };
            let refocus = fix.ewm().handle_toplevel_destroyed_by_id(id);
            if let Some(expected) = expected_same_output_refocus {
                assert_eq!(
                    refocus,
                    Some(expected),
                    "destroy({}) returned wrong same-output refocus target",
                    id,
                );
            } else if prev_focused != id {
                assert_eq!(
                    refocus, None,
                    "destroy({}) requested refocus for an inactive frame",
                    id,
                );
            }
            if let Some(target) = refocus {
                fix.ewm()
                    .set_focus_frame(target, "toplevel_destroyed", false);
            }
            if was_present {
                assert!(
                    !strip_frame_ids(fix).contains(&id),
                    "invariant 12: destroy({}) left the id in some strip",
                    id,
                );
                if prev_focused != id {
                    assert_eq!(
                        fix.ewm_ref().focused_frame_id,
                        prev_focused,
                        "destroy({}) changed focus away from inactive frame {}",
                        id,
                        prev_focused,
                    );
                }
            }
        }
        Op::ActivateSurface(id) => {
            let prev = (
                fix.ewm_ref().focused_frame_id,
                fix.ewm_ref().focused_entry_id,
            );
            fix.ewm().activate_or_defer_surface(id, true, "proptest");
            let ewm = fix.ewm_ref();
            let focused = ewm.focused_surface_id() == id;
            let pending = ewm.pending_activations().any(|pending| pending == id);
            // Only a surface with a record can wait. A layout-only id focuses or is dropped.
            assert_eq!(
                pending,
                !focused && ewm.surfaces.contains_key(&id),
                "activate({}) must focus the surface or defer it: focused={} pending={}",
                id,
                focused,
                pending,
            );
            if pending {
                assert_eq!(
                    (ewm.focused_frame_id, ewm.focused_entry_id),
                    prev,
                    "activate({}) of a hidden surface moved focus",
                    id,
                );
            }
        }
        Op::DestroySurface(id) => {
            let was_present = layout_surface_ids(fix).contains(&id);
            let was_focused = fix.ewm_ref().focused_surface_id() == id;
            let prev_focused_frame = fix.ewm_ref().focused_frame_id;
            let prev_focused_entry = fix.ewm_ref().focused_entry_id;
            let destroyed_entry_ids: HashSet<LayoutEntryId> = all_frames(fix)
                .flat_map(|frame| frame.entries.iter())
                .filter(|entry| entry.surface_id() == Some(id))
                .map(|entry| entry.id)
                .collect();
            let refocus = fix.ewm().handle_toplevel_destroyed_by_id(id);
            assert_eq!(
                refocus, None,
                "destroying embedded surface {} must not request frame refocus",
                id,
            );
            assert!(
                !fix.ewm_ref()
                    .pending_activations()
                    .any(|pending| pending == id),
                "destroy({}) left a pending activation",
                id,
            );
            if was_present {
                assert!(
                    !layout_surface_ids(fix).contains(&id),
                    "destroy({}) left the id in a layout entry",
                    id,
                );
                assert!(
                    !fix.ewm_ref().surface_in_layout(id),
                    "destroy({}) left stale output membership",
                    id,
                );
                let detached: HashSet<LayoutEntryId> = all_frames(fix)
                    .flat_map(|frame| frame.entries.iter())
                    .filter(|entry| entry.surface.is_none())
                    .map(|entry| entry.id)
                    .collect();
                assert!(
                    destroyed_entry_ids.is_subset(&detached),
                    "destroy({}) removed the windows that showed it",
                    id,
                );
                assert_ne!(
                    fix.ewm_ref().focused_surface_id(),
                    id,
                    "destroy({}) left keyboard focus on the dead surface",
                    id,
                );
                if was_focused {
                    assert_eq!(
                        fix.ewm_ref().focused_frame_id,
                        prev_focused_frame,
                        "destroy({}) changed host frame focus",
                        id,
                    );
                    assert_eq!(
                        fix.ewm_ref().focused_entry_id,
                        prev_focused_entry,
                        "destroy({}) moved focus off the window",
                        id,
                    );
                }
            }
        }
    }
}

fn strip_frame_ids(fix: &Fixture) -> HashSet<u64> {
    all_frames(fix)
        .map(|f| f.surface_id)
        .filter(|&id| id != 0)
        .collect()
}

fn layout_surface_ids(fix: &Fixture) -> HashSet<u64> {
    all_frames(fix)
        .flat_map(|frame| frame.entries.iter().filter_map(LayoutEntry::surface_id))
        .collect()
}

fn all_frames(fix: &Fixture) -> impl Iterator<Item = &ewm_core::strip::Frame> {
    fix.ewm_ref().layout.all_frames()
}

fn mapped_output_names(fix: &Fixture) -> HashSet<String> {
    fix.ewm_ref()
        .space
        .outputs()
        .map(|output| output.name())
        .collect()
}

fn is_output_mapped(fix: &Fixture, output_name: &str) -> bool {
    fix.ewm_ref()
        .space
        .outputs()
        .any(|output| output.name() == output_name)
}

fn set_output_enabled(fix: &mut Fixture, output_name: &str, enabled: bool) {
    fix.ewm().output_config.insert(
        output_name.to_string(),
        OutputConfig {
            enabled,
            ..Default::default()
        },
    );
    if fix.has_output(output_name) {
        fix.apply_output_config(output_name);
    }
}

fn expected_surface_outputs(fix: &Fixture) -> HashMap<u64, HashSet<String>> {
    let mut expected: HashMap<u64, HashSet<String>> = HashMap::new();

    for monitor in fix.ewm_ref().layout.monitors() {
        for surface_id in monitor
            .frames()
            .flat_map(|frame| {
                std::iter::once(frame.surface_id)
                    .chain(frame.entries.iter().filter_map(LayoutEntry::surface_id))
            })
            .filter(|id| fix.ewm_ref().surfaces.contains_key(id))
        {
            expected
                .entry(surface_id)
                .or_default()
                .insert(monitor.name().to_string());
        }
    }

    expected
}

proptest! {
    #[test]
    fn destroying_focused_frame_refocuses_same_output_neighbor(
        len in 2usize..=6,
        raw_idx in 0usize..6,
    ) {
        let idx = raw_idx % len;
        let ids: Vec<u64> = (0..len).map(|i| 10_000 + i as u64).collect();
        let mut fix = Fixture::new().unwrap();
        fix.add_output(OUTPUTS[0], 1920, 1080);
        fix.install_strip(OUTPUTS[0], strip_with_frame_ids(ids.to_vec(), 0));
        fix.ewm().set_focus_frame(ids[idx], "proptest", false);

        let expected = if idx + 1 < len {
            ids[idx + 1]
        } else {
            ids[idx - 1]
        };
        let refocus = fix.ewm().handle_toplevel_destroyed_by_id(ids[idx]);

        prop_assert_eq!(refocus, Some(expected));
        fix.ewm()
            .set_focus_frame(refocus.unwrap(), "toplevel_destroyed", false);
        prop_assert_eq!(fix.ewm_ref().focused_frame_id, expected);
        let strip = fix.strip(OUTPUTS[0]);
        prop_assert_eq!(strip.active_frame_id(), Some(expected));
    }

    #[test]
    fn destroying_inactive_frame_preserves_focused_frame(
        len in 2usize..=6,
        raw_focus_idx in 0usize..6,
        raw_destroy_idx in 0usize..6,
    ) {
        let focus_idx = raw_focus_idx % len;
        let mut destroy_idx = raw_destroy_idx % len;
        if destroy_idx == focus_idx {
            destroy_idx = (destroy_idx + 1) % len;
        }
        let ids: Vec<u64> = (0..len).map(|i| 20_000 + i as u64).collect();
        let mut fix = Fixture::new().unwrap();
        fix.add_output(OUTPUTS[0], 1920, 1080);
        fix.install_strip(OUTPUTS[0], strip_with_frame_ids(ids.to_vec(), 0));
        fix.ewm().set_focus_frame(ids[focus_idx], "proptest", false);

        let refocus = fix.ewm().handle_toplevel_destroyed_by_id(ids[destroy_idx]);

        prop_assert_eq!(refocus, None);
        prop_assert_eq!(fix.ewm_ref().focused_frame_id, ids[focus_idx]);
        let strip = fix.strip(OUTPUTS[0]);
        prop_assert_eq!(
            strip.active_frame_id(),
            Some(ids[focus_idx]),
        );
    }

    #[test]
    fn active_close_intent_uses_destroyed_slot_after_focus_or_layout_race(
        len in 2usize..=6,
        raw_close_idx in 0usize..6,
        raw_mru_idx in 0usize..6,
        omit_closing_frame_from_layout in any::<bool>(),
    ) {
        let close_idx = raw_close_idx % len;
        let mut mru_idx = raw_mru_idx % len;
        if mru_idx == close_idx {
            mru_idx = (mru_idx + 1) % len;
        }
        let ids: Vec<u64> = (0..len).map(|i| 30_000 + i as u64).collect();
        let closing_id = ids[close_idx];
        let expected = if close_idx + 1 < len {
            ids[close_idx + 1]
        } else {
            ids[close_idx - 1]
        };
        let mut fix = Fixture::new().unwrap();
        fix.add_output(OUTPUTS[0], 1920, 1080);
        fix.install_strip(OUTPUTS[0], strip_with_frame_ids(ids.to_vec(), 0));
        fix.ewm().set_focus_frame(closing_id, "proptest", false);

        mark_active_frame_close(closing_id);
        fix.ewm().set_focus_frame(ids[mru_idx], "pgtk_mru", false);
        if omit_closing_frame_from_layout {
            fix.ewm().apply_layout(
                ids.iter()
                    .copied()
                    .filter(|&id| id != closing_id)
                    .map(Frame::new)
                    .collect(),
            );
            prop_assert!(
                strip_frame_ids(&fix).contains(&closing_id),
                "pending active close must keep the closing frame until destroy",
            );
        }

        let refocus = fix.ewm().handle_toplevel_destroyed_by_id(closing_id);

        prop_assert_eq!(refocus, Some(expected));
        fix.ewm()
            .set_focus_frame(refocus.unwrap(), "toplevel_destroyed", false);
        prop_assert_eq!(fix.ewm_ref().focused_frame_id, expected);
        prop_assert!(!strip_frame_ids(&fix).contains(&closing_id));
        let strip = fix.strip(OUTPUTS[0]);
        prop_assert_eq!(strip.active_frame_id(), Some(expected));
    }

    #[test]
    fn layout_omission_keeps_frame_chrome_only(
        surface_id in ENTRY_SID_RANGE,
        entry_idx in 0usize..MAX_ENTRIES_PER_LAYOUT,
    ) {
        let mut fix = Fixture::new().unwrap();
        fix.add_output(OUTPUTS[0], 1920, 1080);

        let entry_id = entry_entry_id_for(100, entry_idx);
        fix.tile(OUTPUTS[0], vec![frame_with_entry(100, entry_id, surface_id)]);
        fix.tile(OUTPUTS[0], vec![Frame::new(101)]);

        // A layout that leaves frame 100 out empties it.
        fix.ewm().apply_layout(vec![Frame::new(101)]);

        let omitted = fix
            .strip(OUTPUTS[0])
            .frames()
            .iter()
            .find(|frame| frame.surface_id == 100)
            .unwrap();
        prop_assert!(omitted.entries.is_empty());
        prop_assert_eq!(omitted.selected_entry_id, None);
        prop_assert!(!fix.ewm_ref().surface_in_layout(surface_id));
    }
}

fn assert_invariants(fix: &Fixture) {
    let ewm = fix.ewm_ref();

    // Invariant 1: focused_frame_id == 0 ⟹ focused_entry_id == None.
    if ewm.focused_frame_id == 0 {
        assert_eq!(
            ewm.focused_entry_id, None,
            "invariant 1: focused_entry_id={:?} while focused_frame_id is 0",
            ewm.focused_entry_id,
        );
    }

    // Invariant 2: focused_entry_id == Some(v) ⟹ v is the entry_id of some entry
    // on the focused frame.
    if let Some(v) = ewm.focused_entry_id {
        let in_focused_frame = all_frames(fix)
            .find(|f| f.surface_id == ewm.focused_frame_id)
            .map(|f| f.entries.iter().any(|e| e.id == v))
            .unwrap_or(false);
        assert!(
            in_focused_frame,
            "invariant 2: focused_entry_id={:?} not in focused frame {}",
            v, ewm.focused_frame_id,
        );
    }

    // Invariant 3: the layout's own invariants, one place per frame among them.
    ewm.layout.verify_invariants();

    // Invariant 4: the focused frame is the active frame of its own strip,
    // and never a homeless frame.
    if ewm.focused_frame_id != 0 {
        for strip in ewm.layout.strips().map(|(_, strip)| strip) {
            let active_id = strip.active_frame_id();
            if active_id == Some(ewm.focused_frame_id) {
                continue;
            }
            assert!(
                !strip
                    .frames()
                    .iter()
                    .any(|f| f.surface_id == ewm.focused_frame_id),
                "invariant 4: focused frame {} present in a strip but not its active frame",
                ewm.focused_frame_id,
            );
        }
        assert!(
            !ewm.layout.is_homeless(ewm.focused_frame_id),
            "invariant 4: focused frame {} is homeless",
            ewm.focused_frame_id,
        );
    }

    // Invariant 5: at least one mapped frame exists ⟺ focused_frame_id is
    // non-zero. Frames may be parked in the homeless pool only while no
    // output is mapped.
    let mapped_outputs = mapped_output_names(fix);
    for name in ewm.layout.strips().map(|(name, _)| name) {
        assert!(
            mapped_outputs.contains(name),
            "invariant 5: layout has a monitor for unmapped output {}",
            name,
        );
    }
    assert!(
        mapped_outputs.is_empty() || ewm.layout.homeless().tiled.is_empty(),
        "invariant 5: homeless frames exist while an output is mapped",
    );
    let any_frame = ewm
        .layout
        .strips()
        .map(|(_, strip)| strip)
        .any(|strip| !strip.frames().is_empty());
    assert_eq!(
        any_frame,
        ewm.focused_frame_id != 0,
        "invariant 5: any_mapped_frame={} but focused_frame_id={}",
        any_frame,
        ewm.focused_frame_id,
    );

    // Invariant 7: every live entry entry_id identifies exactly one layout occurrence.
    let mut entry_ids = HashMap::new();
    for (frame_idx, frame) in all_frames(fix).enumerate() {
        for (entry_idx, entry) in frame.entries.iter().enumerate() {
            let occurrence = (frame.surface_id, frame_idx, entry_idx);
            assert!(
                entry_ids.insert(entry.id, occurrence).is_none(),
                "invariant 7: duplicate entry_id {:?} at {:?}",
                entry.id,
                occurrence,
            );
        }
    }

    // Invariant 8: output membership is exactly the set of mapped strips that
    // currently contain each embedded surface. Homeless frames must not keep
    // stale wl_surface output membership.
    assert_eq!(
        ewm.surface_outputs(),
        expected_surface_outputs(fix),
        "invariant 8: surface_outputs diverged from the layout",
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

/// Strategy without `CreateEmacsFrame` -- the fixture variant bypasses
/// reconcile and would break the event-balance invariant.
fn reconcile_op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        layout_strategy().prop_map(|frames| Op::ApplyLayout { frames }),
        entry_view_strategy().prop_map(Op::FocusView),
        FRAME_SID_RANGE.prop_map(Op::FocusFrame),
        (0..OUTPUTS.len()).prop_map(Op::DisableOutput),
        (0..OUTPUTS.len()).prop_map(Op::EnableOutput),
        (0..OUTPUTS.len()).prop_map(Op::RemoveOutput),
        (0..OUTPUTS.len()).prop_map(Op::AddOutput),
        FRAME_SID_RANGE.prop_map(Op::DestroyFrame),
    ]
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
        let _ = fix.drain_events();

        let mut net: HashMap<u64, i32> = HashMap::new();

        for op in ops {
            apply_op(&mut fix, op);

            let mapped_outputs = mapped_output_names(&fix);
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

            for (out_name, strip) in fix.ewm_ref().layout.strips() {
                let active = strip.active_frame_id();
                for frame in strip.frames() {
                    let id = frame.surface_id;
                    let expected: i32 = if mapped_outputs.contains(out_name)
                        && active == Some(id)
                    {
                        1
                    } else {
                        0
                    };
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
            for frame in &fix.ewm_ref().layout.homeless().tiled {
                let id = frame.surface_id;
                let actual = net.get(&id).copied().unwrap_or(0);
                prop_assert_eq!(
                    actual,
                    0,
                    "homeless frame {}: net shown-hidden = {} but expected 0",
                    id,
                    actual,
                );
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
    fn resolver_follows_lock_layer_toplevel_priority(
        locked in any::<bool>(),
        lock in prop::option::of(1u64..=999),
        layer in prop::option::of(1u64..=999),
        focused_id in 0u64..=20,
        map in surface_map_strategy(),
    ) {
        let toplevel = map.get(&focused_id).copied();
        let fallback_calls = std::cell::Cell::new(0);
        let expected = if locked {
            lock
        } else {
            layer.or(toplevel)
        };
        let expected_fallback_calls = usize::from(!locked && layer.is_none());

        let resolved = resolve_keyboard_focus_target(
            locked, lock, layer,
            || {
                fallback_calls.set(fallback_calls.get() + 1);
                toplevel
            },
        );

        prop_assert_eq!(resolved, expected);
        prop_assert_eq!(fallback_calls.get(), expected_fallback_calls);
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

    for i in 0..setup.n() {
        let strip = fix.strip_mut(&setup.output_name(i));
        for f in 0..setup.frames_per_output[i] {
            strip.push_frame(Frame::new(setup.frame_id(i, f)));
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
