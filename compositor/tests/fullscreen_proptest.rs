//! Properties for compositor-owned fullscreen lifecycle state.
//!
//! Fullscreen is frame presentation state, not layout input.  Layout refreshes,
//! migrations, output detach/reattach, and surface destruction may preserve or
//! clear an existing fullscreen selection, but they must not manufacture one.

use std::collections::HashSet;

use ewm_core::strip::{Frame, FullscreenEntry};
use ewm_core::testing::Fixture;
use ewm_core::{LayoutEntry, LayoutEntryId};
use proptest::prelude::*;

const OUTPUTS: [&str; 2] = ["HEAD-A", "HEAD-B"];
const FRAME_COUNT: usize = 4;
const ENTRY_SLOTS: usize = 3;

fn nz(n: u64) -> LayoutEntryId {
    std::num::NonZeroU64::new(n).unwrap()
}

fn frame_id(frame_idx: usize) -> u64 {
    100 + frame_idx as u64
}

fn frame_focus_id(frame_idx: usize) -> LayoutEntryId {
    nz(1_000 + frame_idx as u64)
}

fn entry_id(frame_idx: usize, entry_slot: usize) -> LayoutEntryId {
    nz(10_000 + frame_idx as u64 * 100 + entry_slot as u64)
}

fn surface_strategy() -> impl Strategy<Value = u64> {
    1u64..=6
}

#[derive(Debug, Clone)]
enum Op {
    AddOutput(usize),
    RemoveOutput(usize),
    ApplyLayout {
        output: usize,
        frames: Vec<FrameSpec>,
    },
    SetEntryFullscreen {
        frame_idx: usize,
        entry_slot: usize,
        on: bool,
    },
    SetSurfaceFullscreen {
        surface_id: u64,
        output: Option<usize>,
        on: bool,
    },
    DestroySurface(u64),
}

#[derive(Debug, Clone)]
struct FrameSpec {
    frame_idx: usize,
    entries: Vec<EntrySpec>,
    incoming_fullscreen: Option<(LayoutEntryId, u64)>,
    selected_entry_id: Option<LayoutEntryId>,
}

#[derive(Debug, Clone)]
struct EntrySpec {
    entry_slot: usize,
    surface_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ViewKey {
    frame_id: u64,
    entry_id: LayoutEntryId,
    surface_id: u64,
}

fn frame_seed_strategy() -> impl Strategy<Value = (bool, Vec<(bool, u64)>, bool, bool)> {
    (
        any::<bool>(),
        prop::collection::vec((any::<bool>(), surface_strategy()), ENTRY_SLOTS),
        any::<bool>(),
        any::<bool>(),
    )
}

fn layout_strategy() -> impl Strategy<Value = Vec<FrameSpec>> {
    prop::collection::vec(frame_seed_strategy(), FRAME_COUNT).prop_map(|seeds| {
        seeds
            .into_iter()
            .enumerate()
            .filter_map(
                |(frame_idx, (include_frame, entry_seeds, select_first, fullscreen_first))| {
                    include_frame.then(|| {
                        let entries = entry_seeds
                            .into_iter()
                            .enumerate()
                            .filter_map(|(entry_slot, (include_entry, surface_id))| {
                                include_entry.then_some(EntrySpec {
                                    entry_slot,
                                    surface_id,
                                })
                            })
                            .collect::<Vec<_>>();
                        let incoming_fullscreen = fullscreen_first
                            .then(|| {
                                entries.first().map(|entry| {
                                    (entry_id(frame_idx, entry.entry_slot), entry.surface_id)
                                })
                            })
                            .flatten();
                        let selected_entry_id = select_first
                            .then(|| {
                                entries
                                    .first()
                                    .map(|entry| entry_id(frame_idx, entry.entry_slot))
                            })
                            .flatten();
                        FrameSpec {
                            frame_idx,
                            entries,
                            incoming_fullscreen,
                            selected_entry_id,
                        }
                    })
                },
            )
            .collect()
    })
}

fn output_option_strategy() -> impl Strategy<Value = Option<usize>> {
    prop_oneof![Just(None), (0..OUTPUTS.len()).prop_map(Some)]
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        1 => (0..OUTPUTS.len()).prop_map(Op::AddOutput),
        1 => (0..OUTPUTS.len()).prop_map(Op::RemoveOutput),
        5 => (0..OUTPUTS.len(), layout_strategy()).prop_map(|(output, frames)| {
            Op::ApplyLayout { output, frames }
        }),
        4 => (0usize..FRAME_COUNT, 0usize..ENTRY_SLOTS, any::<bool>()).prop_map(
            |(frame_idx, entry_slot, on)| Op::SetEntryFullscreen {
                frame_idx,
                entry_slot,
                on,
            },
        ),
        3 => (surface_strategy(), output_option_strategy(), any::<bool>()).prop_map(
            |(surface_id, output, on)| Op::SetSurfaceFullscreen {
                surface_id,
                output,
                on,
            },
        ),
        2 => surface_strategy().prop_map(Op::DestroySurface),
    ]
}

fn to_frame(spec: &FrameSpec) -> Frame {
    Frame {
        name: String::new(),
        workspace_name: None,
        surface_id: frame_id(spec.frame_idx),
        focus_id: frame_focus_id(spec.frame_idx),
        width: 1280.0,
        height: 0.0,
        entries: spec
            .entries
            .iter()
            .map(|entry| {
                LayoutEntry::surface(
                    entry_id(spec.frame_idx, entry.entry_slot),
                    "entry".to_string(),
                    entry.surface_id,
                    0,
                    0,
                    400,
                    300,
                )
            })
            .collect(),
        fullscreen: spec
            .incoming_fullscreen
            .map(|(entry_id, surface_id)| FullscreenEntry {
                entry_id,
                surface_id,
            }),
        selected_entry_id: spec.selected_entry_id,
        move_anim: None,
        floating_pos: None,
    }
}

fn all_fullscreen_selections(fix: &Fixture) -> HashSet<ViewKey> {
    fix.ewm_ref()
        .frame_set
        .all_frames()
        .filter_map(|frame| {
            frame.fullscreen.map(|fullscreen| ViewKey {
                frame_id: frame.surface_id,
                entry_id: fullscreen.entry_id,
                surface_id: fullscreen.surface_id,
            })
        })
        .collect()
}

fn live_views(fix: &Fixture) -> HashSet<ViewKey> {
    fix.ewm_ref()
        .frame_set
        .all_frames()
        .flat_map(|frame| {
            frame.entries.iter().filter_map(|entry| {
                entry.surface_id().map(|surface_id| ViewKey {
                    frame_id: frame.surface_id,
                    entry_id: entry.id,
                    surface_id,
                })
            })
        })
        .collect()
}

fn mapped_fullscreen_surfaces(fix: &Fixture) -> HashSet<u64> {
    fix.ewm_ref()
        .frame_set
        .mapped_strips()
        .values()
        .flat_map(|strip| strip.frames.iter())
        .filter_map(|frame| frame.fullscreen.map(|fullscreen| fullscreen.surface_id))
        .collect()
}

fn live_view_for_entry(fix: &Fixture, entry_id: LayoutEntryId) -> Option<ViewKey> {
    live_views(fix)
        .into_iter()
        .find(|view| view.entry_id == entry_id)
}

fn assert_fullscreen_invariants(fix: &Fixture) -> Result<(), TestCaseError> {
    let live = live_views(fix);
    let selections = all_fullscreen_selections(fix);
    let mapped_fullscreen_surfaces = mapped_fullscreen_surfaces(fix);
    let mut selected_frames = HashSet::new();

    for selection in &selections {
        prop_assert!(
            live.contains(selection),
            "fullscreen selection must point to a live view: {:?}",
            selection,
        );
        prop_assert!(
            selected_frames.insert(selection.frame_id),
            "frame {} has more than one fullscreen selection",
            selection.frame_id,
        );
    }

    for surface_id in 1..=6 {
        prop_assert_eq!(
            fix.ewm_ref().is_surface_fullscreen(surface_id),
            mapped_fullscreen_surfaces.contains(&surface_id),
            "mapped fullscreen index diverged for surface {}",
            surface_id,
        );
    }

    Ok(())
}

fn assert_no_new_selection(
    before: &HashSet<ViewKey>,
    after: &HashSet<ViewKey>,
    op: &Op,
) -> Result<(), TestCaseError> {
    let unexpected = after
        .difference(before)
        .copied()
        .collect::<HashSet<ViewKey>>();
    prop_assert!(
        unexpected.is_empty(),
        "{:?} manufactured fullscreen selection(s): {:?}",
        op,
        unexpected,
    );
    Ok(())
}

fn apply_op(fix: &mut Fixture, op: &Op) -> Result<(), TestCaseError> {
    let before = all_fullscreen_selections(fix);

    match op {
        Op::AddOutput(output) => {
            let name = OUTPUTS[*output];
            if !fix.has_output(name) {
                fix.add_output(name, 1920, 1080);
            }
            let after = all_fullscreen_selections(fix);
            assert_no_new_selection(&before, &after, op)?;
        }
        Op::RemoveOutput(output) => {
            fix.remove_output(OUTPUTS[*output]);
            let after = all_fullscreen_selections(fix);
            assert_no_new_selection(&before, &after, op)?;
        }
        Op::ApplyLayout { output, frames } => {
            let frames = frames.iter().map(to_frame).collect();
            fix.ewm().apply_output_layout(OUTPUTS[*output], frames);
            let after = all_fullscreen_selections(fix);
            assert_no_new_selection(&before, &after, op)?;
        }
        Op::SetEntryFullscreen {
            frame_idx,
            entry_slot,
            on,
        } => {
            let entry_id = entry_id(*frame_idx, *entry_slot);
            let expected = live_view_for_entry(fix, entry_id);
            let target_surface = fix.ewm().set_layout_entry_fullscreen(entry_id, *on);
            let after = all_fullscreen_selections(fix);

            if *on {
                match expected {
                    Some(view) => {
                        prop_assert_eq!(target_surface, Some(view.surface_id));
                        prop_assert!(
                            after.contains(&view),
                            "set_layout_entry_fullscreen({:?}, true) did not select {:?}",
                            entry_id,
                            view,
                        );
                        prop_assert!(
                            after
                                .difference(&before)
                                .all(|selection| *selection == view),
                            "set_layout_entry_fullscreen({:?}, true) created unrelated selection(s): before={:?} after={:?}",
                            entry_id,
                            before,
                            after,
                        );
                    }
                    None => {
                        prop_assert_eq!(target_surface, None);
                        prop_assert_eq!(after, before);
                    }
                }
            } else {
                assert_no_new_selection(&before, &after, op)?;
                prop_assert!(
                    after.iter().all(|selection| selection.entry_id != entry_id),
                    "set_layout_entry_fullscreen({:?}, false) left that entry selected: {:?}",
                    entry_id,
                    after,
                );
            }
        }
        Op::SetSurfaceFullscreen {
            surface_id,
            output,
            on,
        } => {
            let output_name = output.map(|idx| OUTPUTS[idx]);
            fix.ewm()
                .set_surface_fullscreen_on_output(*surface_id, output_name, *on);
            let after = all_fullscreen_selections(fix);

            if *on {
                prop_assert!(
                    after
                        .difference(&before)
                        .all(|selection| selection.surface_id == *surface_id),
                    "set_surface_fullscreen({}, true) created unrelated selection(s): before={:?} after={:?}",
                    surface_id,
                    before,
                    after,
                );
            } else {
                assert_no_new_selection(&before, &after, op)?;
                prop_assert!(
                    after
                        .iter()
                        .all(|selection| selection.surface_id != *surface_id),
                    "set_surface_fullscreen({}, false) left selection(s): {:?}",
                    surface_id,
                    after,
                );
            }
        }
        Op::DestroySurface(surface_id) => {
            fix.ewm().handle_toplevel_destroyed_by_id(*surface_id);
            let after = all_fullscreen_selections(fix);
            assert_no_new_selection(&before, &after, op)?;
            prop_assert!(
                after
                    .iter()
                    .all(|selection| selection.surface_id != *surface_id),
                "destroying surface {} left fullscreen selection(s): {:?}",
                surface_id,
                after,
            );
        }
    }

    assert_fullscreen_invariants(fix)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        ..ProptestConfig::default()
    })]

    #[test]
    fn fullscreen_lifecycle_invariants_hold(ops in prop::collection::vec(op_strategy(), 1..40)) {
        let mut fix = Fixture::new().unwrap();
        for output in OUTPUTS {
            fix.add_output(output, 1920, 1080);
        }

        assert_fullscreen_invariants(&fix)?;
        for op in &ops {
            apply_op(&mut fix, op)?;
        }
    }
}
