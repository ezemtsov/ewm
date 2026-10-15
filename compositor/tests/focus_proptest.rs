//! Property tests for the compositor focus state machine.
//!
//! Codifies invariants from `docs/focus-design.md` § "Invariants" and
//! asserts they hold after every randomly generated focus-mutating op.
//! Emacs frames are real toplevels of a client with Emacs's pid, created and
//! destroyed through `new_toplevel` and `toplevel_destroyed`; the ops name
//! them by a logical id and `Frames` maps it to the surface id the
//! compositor assigned.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use ewm_core::event::Event;
use ewm_core::strip::Frame;
use ewm_core::testing::{
    Fixture, TestClient, Toplevel, clear_pending_frames, mark_active_frame_close, nz,
    prepare_frame, strip_with_frame_ids,
};
use ewm_core::{Action, LayoutEntry, LayoutEntryId, OutputConfig, OverviewAction};
use proptest::prelude::*;
use smithay::utils::Size;

const OUTPUTS: [&str; 2] = ["HEAD-A", "HEAD-B"];
const MAX_FRAMES: usize = 3;

/// The pending-frame queue is process-wide, so tests that create frames take turns.
static SERIAL: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone)]
enum Op {
    /// Lisp prepares a frame on `OUTPUTS[output]` and Emacs opens its toplevel,
    /// which enters the strip before any layout arrives.
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
    /// A compositor action, as a key or `ewm-action-module` runs it.
    Action(Action),
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

// Layout-only surface ids and logical frame ids sit above the ids real frames get.
const ENTRY_SID_RANGE: std::ops::RangeInclusive<u64> = 1001..=1008;
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

/// One step along an output axis.
fn direction_strategy() -> impl Strategy<Value = (i32, i32)> {
    prop::sample::select(vec![(-1, 0), (1, 0), (0, -1), (0, 1)])
}

/// Every action but `Spawn`, which would start processes.
fn action_strategy() -> impl Strategy<Value = Action> {
    prop_oneof![
        (-2i32..=2).prop_map(|delta| Action::FocusFrame { delta }),
        (1usize..=4).prop_map(|n| Action::FocusNthFrame { n }),
        FRAME_SID_RANGE.prop_map(|id| Action::ActivateFrame { id }),
        direction_strategy().prop_map(|(dx, dy)| Action::FocusOutput { dx, dy }),
        prop_oneof![Just(-1), Just(1)].prop_map(|delta| Action::MoveFrame { delta }),
        Just(Action::ToggleFullscreen),
        prop_oneof![
            Just(OverviewAction::Toggle),
            Just(OverviewAction::Open),
            Just(OverviewAction::Close)
        ]
        .prop_map(Action::Overview),
        Just(Action::HideToggle),
    ]
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        action_strategy().prop_map(Op::Action),
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

/// The Emacs frames the ops created, by logical id.
struct Frames {
    client: TestClient,
    toplevels: HashMap<u64, (Toplevel, u64)>,
    next_seq: u64,
}

impl Frames {
    fn new(fix: &mut Fixture) -> Self {
        clear_pending_frames();
        fix.ewm().set_emacs_pid(std::process::id());
        let mut client = fix.new_client().unwrap();
        fix.roundtrip(&mut client);
        Self {
            client,
            toplevels: HashMap::new(),
            next_seq: 0,
        }
    }

    /// The surface id of logical frame `id`, or `id` itself for a frame that never existed.
    fn real(&self, id: u64) -> u64 {
        self.toplevels.get(&id).map_or(id, |(_, sid)| *sid)
    }
}

fn to_frame(spec: &FrameSpec, frames: &Frames) -> Frame {
    Frame {
        focus_id: spec.frame_focus_id,
        width: 1920.0,
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
        selected_entry_id: spec.selected_entry_id,
        ..Frame::new(frames.real(spec.surface_id))
    }
}

fn frame_with_entry(frame_id: u64, entry_id: LayoutEntryId, surface_id: u64) -> Frame {
    Frame {
        focus_id: nz(200),
        width: 1920.0,
        entries: vec![LayoutEntry::surface(
            entry_id,
            "entry".to_string(),
            surface_id,
            0,
            0,
            100,
            100,
        )],
        selected_entry_id: Some(entry_id),
        ..Frame::new(frame_id)
    }
}

fn apply_op(fix: &mut Fixture, frames: &mut Frames, op: Op) {
    match op {
        Op::CreateEmacsFrame { output, surface_id } => {
            let output_name = OUTPUTS[output];
            // A frame enters a strip whose output is mapped, once.
            if !is_output_mapped(fix, output_name) || frames.toplevels.contains_key(&surface_id) {
                return;
            }
            let unfocused = fix.ewm_ref().focused_surface_id() == 0;
            frames.next_seq += 1;
            prepare_frame(output_name, frames.next_seq, None);
            let toplevel = fix.open_toplevel(&mut frames.client, "frame", Size::from((400, 300)));
            let sid = fix
                .find_surface_id(&frames.client, &toplevel.surface)
                .expect("the compositor assigns every toplevel a surface id");
            frames.toplevels.insert(surface_id, (toplevel, sid));
            assert_eq!(
                fix.ewm_ref().layout.frame_output(sid),
                Some(output_name),
                "frame {surface_id} ({sid}) did not land on the output Lisp prepared it for",
            );
            if unfocused {
                // The boot regression: the first frame takes focus by itself.
                assert_eq!(
                    fix.ewm_ref().focused_frame_id,
                    sid,
                    "the first Emacs frame {sid} did not take focus",
                );
            }
        }
        Op::ApplyLayout { frames: specs } => {
            let real: Vec<Frame> = specs.iter().map(|spec| to_frame(spec, frames)).collect();
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
            fix.ewm().set_focus_frame(frames.real(f), "proptest", false);
        }
        Op::Action(Action::ActivateFrame { id }) => fix.do_action(Action::ActivateFrame {
            id: frames.real(id),
        }),
        Op::Action(action) => fix.do_action(action),
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
            let Some((toplevel, id)) = frames.toplevels.remove(&id) else {
                return;
            };
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
            toplevel.xdg_toplevel.destroy();
            toplevel.xdg_surface.destroy();
            toplevel.surface.destroy();
            fix.roundtrip(&mut frames.client);
            if let Some(expected) = expected_same_output_refocus {
                assert_eq!(
                    fix.ewm_ref().focused_frame_id,
                    expected,
                    "destroy({}) did not refocus the same-output neighbour",
                    id,
                );
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
            fix.ewm().activate_or_defer_surface(id, "proptest");
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
            // Invariant 13: a view in the focused frame wins.
            if focused {
                assert_focused_frame_view(fix, id, prev.0);
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

/// Views of `surface` in frame `frame`.
fn views_in_frame(fix: &Fixture, surface: u64, frame: u64) -> Vec<LayoutEntryId> {
    all_frames(fix)
        .filter(|f| f.surface_id == frame)
        .flat_map(|f| f.entries.iter())
        .filter(|entry| entry.surface_id() == Some(surface))
        .map(|entry| entry.id)
        .collect()
}

/// Invariant 13: when `frame` showed `surface`, the view that took focus is one of them.
fn assert_focused_frame_view(fix: &Fixture, surface: u64, frame: u64) {
    let views = views_in_frame(fix, surface, frame);
    if views.is_empty() {
        return;
    }
    let chosen = fix.ewm_ref().focused_entry_id;
    assert!(
        chosen.is_some_and(|entry| views.contains(&entry)),
        "invariant 13: activate({surface}) left focused frame {frame} which shows it",
    );
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

/// Invariant 13 over every placement of a surface's views and of the focused frame.
#[test]
fn activation_prefers_the_focused_frame() {
    const SURFACE: u64 = 7;
    for membership in 1u8..16 {
        for focused_frame in 100..104u64 {
            let mut fix = Fixture::new().unwrap();
            // Two frames per output, each with a window of its own and maybe a view of the surface.
            for (output_idx, name) in OUTPUTS.iter().enumerate() {
                fix.add_output(name, 1920, 1080);
                let frames = (0..2)
                    .map(|i| {
                        let idx = 2 * output_idx + i;
                        let frame_id = 100 + idx as u64;
                        let own = entry_entry_id_for(frame_id, 0);
                        let mut frame = frame_with_entry(frame_id, own, 1_000 + idx as u64);
                        if membership & (1 << idx) != 0 {
                            frame.entries.push(LayoutEntry::surface(
                                entry_entry_id_for(frame_id, 1),
                                "view".to_string(),
                                SURFACE,
                                0,
                                100,
                                100,
                                100,
                            ));
                        }
                        frame
                    })
                    .collect();
                fix.tile(name, frames);
            }
            fix.ewm().set_focus_frame(focused_frame, "test", false);
            fix.ewm().activate_or_defer_surface(SURFACE, "test");

            let ewm = fix.ewm_ref();
            assert_eq!(
                ewm.focused_surface_id(),
                SURFACE,
                "membership={membership:#06b} focused={focused_frame}"
            );
            assert_focused_frame_view(&fix, SURFACE, focused_frame);
        }
    }
}

proptest! {
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
}

/// Invariant 11: a layout that leaves a frame out empties it.
#[test]
fn layout_omission_empties_the_frame() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(OUTPUTS[0], 1920, 1080);

    let entry_id = entry_entry_id_for(100, 0);
    fix.tile(OUTPUTS[0], vec![frame_with_entry(100, entry_id, 1001)]);
    fix.tile(OUTPUTS[0], vec![Frame::new(101)]);

    fix.ewm().apply_layout(vec![Frame::new(101)]);

    let omitted = fix
        .strip(OUTPUTS[0])
        .frames()
        .iter()
        .find(|frame| frame.surface_id == 100)
        .unwrap();
    assert!(omitted.entries.is_empty());
    assert_eq!(omitted.selected_entry_id, None);
    assert!(!fix.ewm_ref().surface_in_layout(1001));
}

fn assert_invariants(fix: &Fixture) {
    let ewm = fix.ewm_ref();

    // Invariant 3: no focused frame, no focused view.
    if ewm.focused_frame_id == 0 {
        assert_eq!(
            ewm.focused_entry_id, None,
            "invariant 3: focused_entry_id={:?} while focused_frame_id is 0",
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

    // Invariant 4: the layout's own invariants, one place per frame among them.
    ewm.layout.verify_invariants();

    // Invariant 5: the focused frame is the active frame of its own strip,
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
                "invariant 5: focused frame {} present in a strip but not its active frame",
                ewm.focused_frame_id,
            );
        }
        assert!(
            !ewm.layout.is_homeless(ewm.focused_frame_id),
            "invariant 5: focused frame {} is homeless",
            ewm.focused_frame_id,
        );
    }

    // Invariant 6: a frame's selected view is one of its own entries.
    for frame in all_frames(fix) {
        if let Some(selected) = frame.selected_entry_id {
            assert!(
                frame.entries.iter().any(|entry| entry.id == selected),
                "invariant 6: frame {} selects {:?}, which it does not hold",
                frame.surface_id,
                selected,
            );
        }
    }

    // Invariant 10: frames park only while no output is mapped, and strips
    // exist only for mapped outputs.
    let mapped_outputs = mapped_output_names(fix);
    for name in ewm.layout.strips().map(|(name, _)| name) {
        assert!(
            mapped_outputs.contains(name),
            "invariant 10: layout has a monitor for unmapped output {}",
            name,
        );
    }
    assert!(
        mapped_outputs.is_empty() || ewm.layout.homeless().tiled.is_empty(),
        "invariant 10: homeless frames exist while an output is mapped",
    );

    // Invariant 1: some frame is focused iff some frame exists.
    let any_frame = ewm
        .layout
        .strips()
        .map(|(_, strip)| strip)
        .any(|strip| !strip.frames().is_empty());
    assert_eq!(
        any_frame,
        ewm.focused_frame_id != 0,
        "invariant 1: any_mapped_frame={} but focused_frame_id={}",
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

    // Output membership is the set of strips holding each surface, nothing stale for a parked one.
    assert_eq!(
        ewm.surface_outputs(),
        expected_surface_outputs(fix),
        "surface_outputs diverged from the layout",
    );
}

proptest! {
    #[test]
    fn focus_invariants_hold(ops in prop::collection::vec(op_strategy(), 1..30)) {
        let _serial = SERIAL.lock().unwrap();
        let mut fix = Fixture::new().unwrap();
        fix.enable_event_capture();
        for name in OUTPUTS {
            fix.add_output(name, 1920, 1080);
        }
        let mut frames = Frames::new(&mut fix);
        assert_invariants(&fix);
        for op in ops {
            apply_op(&mut fix, &mut frames, op);
            assert_invariants(&fix);
            // Invariant 15: nothing here asks Emacs to run a command.
            for event in fix.drain_events() {
                prop_assert!(
                    !matches!(event, Event::InterceptedCommand { .. } | Event::Key(_)),
                    "{event:?}"
                );
            }
        }
    }
}
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..ProptestConfig::default()
    })]

    /// Lisp freezes on `FrameHidden` and thaws on `FrameShown`, so a placed frame's last event
    /// must say shown iff it is its strip's active frame or the overview is active.
    #[test]
    fn frame_show_hide_tracks_active_slot(
        ops in prop::collection::vec(op_strategy(), 1..30)
    ) {
        let _serial = SERIAL.lock().unwrap();
        let mut fix = Fixture::new().unwrap();
        fix.ewm().animations_clock.set_complete_instantly(true);
        fix.enable_event_capture();
        for name in OUTPUTS {
            fix.add_output(name, 1920, 1080);
        }
        let mut frames = Frames::new(&mut fix);
        let _ = fix.drain_events();

        let mut shown: HashMap<u64, bool> = HashMap::new();

        for op in ops {
            apply_op(&mut fix, &mut frames, op);
            // The overview refreezes the inactive frames on the redraw that ends it.
            fix.dispatch();

            for event in fix.drain_events() {
                match event {
                    Event::FrameShown { id } => {
                        shown.insert(id, true);
                    }
                    Event::FrameHidden { id } => {
                        shown.insert(id, false);
                    }
                    Event::Close { id } => {
                        shown.remove(&id);
                    }
                    _ => {}
                }
            }

            let thawed = fix.ewm_ref().overview.is_active();
            for (out_name, strip) in fix.ewm_ref().layout.strips() {
                let active = strip.active_frame_id();
                for frame in strip.frames() {
                    let id = frame.surface_id;
                    prop_assert_eq!(
                        shown.get(&id),
                        Some(&(thawed || active == Some(id))),
                        "frame {} on {}: Lisp was last told {:?}, active {:?}, overview active {}",
                        id,
                        out_name,
                        shown.get(&id),
                        active,
                        thawed,
                    );
                }
            }
            for frame in &fix.ewm_ref().layout.homeless().tiled {
                let id = frame.surface_id;
                prop_assert_eq!(
                    shown.get(&id),
                    Some(&false),
                    "homeless frame {}: Lisp was last told {:?}",
                    id,
                    shown.get(&id),
                );
            }
        }
    }
}

// `u64` stands in for `WlSurface`; the resolver is generic over surface type.
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
        dir in direction_strategy(),
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
