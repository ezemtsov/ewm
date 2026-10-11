//! Fullscreen is a per-surface bit set by requests and commands alone, shown once its client has
//! committed it. Layouts, output changes and migrations place the bit but never change it.

use std::collections::BTreeSet;

use ewm_core::strip::Frame;
use ewm_core::testing::{Fixture, TestClient, Toplevel, nz};
use ewm_core::{LayoutEntry, LayoutEntryId};
use proptest::prelude::*;
use smithay::utils::Size;

const OUTPUTS: [&str; 2] = ["HEAD-A", "HEAD-B"];
const FRAME_COUNT: usize = 4;
const ENTRY_SLOTS: usize = 3;
const SURFACES: usize = 6;

fn frame_id(frame_idx: usize) -> u64 {
    100 + frame_idx as u64
}

fn entry_id(frame_idx: usize, entry_slot: usize) -> LayoutEntryId {
    nz(10_000 + frame_idx as u64 * 100 + entry_slot as u64)
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
    /// The client answers everything configured so far with a buffer.
    Commit(usize),
    DestroySurface(usize),
}

#[derive(Debug, Clone)]
struct FrameSpec {
    frame_idx: usize,
    entries: Vec<(usize, usize)>,
    selected: Option<usize>,
}

/// The surfaces as the model sees them.
#[derive(Default)]
struct Model {
    requested: BTreeSet<u64>,
    committed: BTreeSet<u64>,
    gone: BTreeSet<u64>,
}

struct Clients {
    client: TestClient,
    toplevels: Vec<Toplevel>,
    sids: Vec<u64>,
}

fn layout_strategy() -> impl Strategy<Value = Vec<FrameSpec>> {
    let frame = (
        any::<bool>(),
        prop::collection::vec((any::<bool>(), 0..SURFACES), ENTRY_SLOTS),
        0..ENTRY_SLOTS,
    );
    prop::collection::vec(frame, FRAME_COUNT).prop_map(|seeds| {
        seeds
            .into_iter()
            .enumerate()
            .filter(|(_, (present, _, _))| *present)
            .map(|(frame_idx, (_, slots, selected))| {
                let entries: Vec<(usize, usize)> = slots
                    .into_iter()
                    .enumerate()
                    .filter(|(_, (present, _))| *present)
                    .map(|(slot, (_, surface))| (slot, surface))
                    .collect();
                let selected = entries
                    .iter()
                    .any(|(slot, _)| *slot == selected)
                    .then_some(selected);
                FrameSpec {
                    frame_idx,
                    entries,
                    selected,
                }
            })
            .collect()
    })
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        1 => (0..OUTPUTS.len()).prop_map(Op::AddOutput),
        1 => (0..OUTPUTS.len()).prop_map(Op::RemoveOutput),
        5 => (0..OUTPUTS.len(), layout_strategy())
            .prop_map(|(output, frames)| Op::ApplyLayout { output, frames }),
        4 => (0usize..FRAME_COUNT, 0usize..ENTRY_SLOTS, any::<bool>()).prop_map(
            |(frame_idx, entry_slot, on)| Op::SetEntryFullscreen {
                frame_idx,
                entry_slot,
                on,
            },
        ),
        4 => (0..SURFACES).prop_map(Op::Commit),
        1 => (0..SURFACES).prop_map(Op::DestroySurface),
    ]
}

fn to_frame(spec: &FrameSpec, sids: &[u64]) -> Frame {
    let mut frame = Frame::new(frame_id(spec.frame_idx));
    frame.width = 400.0;
    frame.entries = spec
        .entries
        .iter()
        .map(|&(slot, surface)| {
            LayoutEntry::surface(
                entry_id(spec.frame_idx, slot),
                "entry".into(),
                sids[surface],
                0,
                0,
                400,
                300,
            )
        })
        .collect();
    frame.selected_entry_id = spec.selected.map(|slot| entry_id(spec.frame_idx, slot));
    frame
}

fn check(fix: &Fixture, model: &Model, sids: &[u64]) -> Result<(), TestCaseError> {
    let ewm = fix.ewm_ref();
    ewm.layout.verify_invariants();
    let mut shown = BTreeSet::new();
    let strip_frames = ewm
        .layout
        .strips()
        .flat_map(|(_, strip)| strip.frames().iter());
    let floats = ewm
        .layout
        .monitors()
        .iter()
        .flat_map(|monitor| monitor.floating.frames().iter());
    for frame in strip_frames.chain(floats) {
        let marked: Vec<&LayoutEntry> = frame
            .entries
            .iter()
            .filter(|entry| entry.fullscreen())
            .collect();
        let shows = frame.entries.iter().any(|entry| {
            entry
                .surface_id()
                .is_some_and(|id| model.committed.contains(&id))
        });
        prop_assert_eq!(
            marked.len(),
            usize::from(shows),
            "frame {} marks {:?} while showing a committed surface is {}",
            frame.surface_id,
            marked.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            shows
        );
        if let Some(entry) = marked.first() {
            let surface_id = entry.surface_id().unwrap();
            prop_assert!(
                model.committed.contains(&surface_id),
                "mark on {surface_id} before its commit"
            );
            shown.insert(surface_id);
        }
    }
    for &sid in sids {
        prop_assert_eq!(ewm.pending_fullscreen(sid), model.requested.contains(&sid));
        prop_assert_eq!(ewm.layout.is_surface_fullscreen(sid), shown.contains(&sid));
        prop_assert_eq!(
            ewm.surfaces
                .get(&sid)
                .is_some_and(|surface| surface.fullscreen),
            model.committed.contains(&sid),
            "{} committed",
            sid
        );
    }
    Ok(())
}

fn apply_op(
    fix: &mut Fixture,
    clients: &mut Clients,
    model: &mut Model,
    op: &Op,
) -> Result<(), TestCaseError> {
    match op {
        Op::AddOutput(output) => {
            let name = OUTPUTS[*output];
            if !fix.has_output(name) {
                fix.add_output(name, 1920, 1080);
            }
        }
        Op::RemoveOutput(output) => fix.remove_output(OUTPUTS[*output]),
        Op::ApplyLayout { output, frames } => {
            let frames = frames
                .iter()
                .map(|spec| to_frame(spec, &clients.sids))
                .collect();
            fix.tile(OUTPUTS[*output], frames);
        }
        Op::SetEntryFullscreen {
            frame_idx,
            entry_slot,
            on,
        } => {
            let entry = entry_id(*frame_idx, *entry_slot);
            let target = fix
                .ewm_ref()
                .layout
                .entry_surface_id(entry)
                .filter(|id| !model.gone.contains(id));
            if let Some(surface_id) = target
                && *on
            {
                // One fullscreen per frame: the frames showing it drop their other requests.
                let ewm = fix.ewm_ref();
                let rivals: Vec<u64> = ewm
                    .layout
                    .strips()
                    .flat_map(|(_, strip)| strip.frames())
                    .filter(|frame| {
                        frame
                            .entries
                            .iter()
                            .any(|entry| entry.surface_id() == Some(surface_id))
                    })
                    .flat_map(|frame| frame.entries.iter().filter_map(LayoutEntry::surface_id))
                    .filter(|&other| other != surface_id)
                    .collect();
                for other in rivals {
                    model.requested.remove(&other);
                }
            }
            fix.set_fullscreen(entry, *on);
            if let Some(surface_id) = target {
                if *on {
                    model.requested.insert(surface_id);
                } else {
                    model.requested.remove(&surface_id);
                }
            }
        }
        Op::Commit(surface) => {
            let sid = clients.sids[*surface];
            if !model.gone.contains(&sid) {
                fix.roundtrip(&mut clients.client);
                fix.map_toplevel(
                    &mut clients.client,
                    &clients.toplevels[*surface],
                    Size::from((400, 300)),
                );
                if model.requested.contains(&sid) {
                    model.committed.insert(sid);
                } else {
                    model.committed.remove(&sid);
                }
            }
        }
        Op::DestroySurface(surface) => {
            let sid = clients.sids[*surface];
            if model.gone.insert(sid) {
                fix.ewm().handle_toplevel_destroyed_by_id(sid);
                model.requested.remove(&sid);
                model.committed.remove(&sid);
            }
        }
    }
    check(fix, model, &clients.sids)
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
        let mut client = fix.new_client().unwrap();
        fix.roundtrip(&mut client);
        let (toplevels, sids) = (0..SURFACES)
            .map(|i| {
                fix.open_toplevel_with_id(&mut client, &format!("app{i}"), Size::from((400, 300)))
            })
            .unzip();
        let mut clients = Clients { client, toplevels, sids };
        let mut model = Model::default();
        check(&fix, &model, &clients.sids)?;
        for op in &ops {
            apply_op(&mut fix, &mut clients, &mut model, op)?;
        }
    }
}
