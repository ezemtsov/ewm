//! Fullscreen is a per-surface bit set by requests and commands alone, shown once its client has
//! committed it. Layouts, output changes and migrations place the bit but never change it. Each
//! strip frame shows one fullscreen view, a float counting for the frame it is over, and a float
//! showing fullscreen pins to that frame.

use std::collections::{BTreeMap, BTreeSet};

use ewm_core::floating::SizeFracPoint;
use ewm_core::strip::{Frame, Strip};
use ewm_core::testing::{ClientEvent, Fixture, TestClient, Toplevel, nz};
use ewm_core::{LayoutEntry, LayoutEntryId};
use proptest::prelude::*;
use smithay::utils::Size;
use wayland_client::Proxy as _;

const OUTPUTS: [&str; 2] = ["HEAD-A", "HEAD-B"];
const FRAME_COUNT: usize = 4;
/// Frames from this index on are floats.
const FIRST_FLOAT: usize = 2;
const ENTRY_SLOTS: usize = 3;
const SURFACES: usize = 6;
const VIEW: (i32, i32) = (400, 300);
const OUTPUT_SIZE: (i32, i32) = (1920, 1080);
const FLOAT_AT: [SizeFracPoint; 2] = [
    SizeFracPoint { x: 0.05, y: 0.05 },
    SizeFracPoint { x: 0.45, y: 0.45 },
];

fn frame_id(frame_idx: usize) -> u64 {
    100 + frame_idx as u64
}

fn entry_id(frame_idx: usize, entry_slot: usize) -> LayoutEntryId {
    nz(10_000 + frame_idx as u64 * 100 + entry_slot as u64)
}

fn floating(frame_idx: usize) -> bool {
    frame_idx >= FIRST_FLOAT
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
    /// The client asks for itself.
    RequestFullscreen {
        surface: usize,
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

/// The surfaces and floats as the model sees them.
#[derive(Default)]
struct Model {
    requested: BTreeSet<u64>,
    committed: BTreeSet<u64>,
    gone: BTreeSet<u64>,
    /// The strip frame each float is pinned to.
    pins: BTreeMap<u64, u64>,
}

fn shows(frame: &Frame, sid: u64) -> bool {
    frame
        .entries
        .iter()
        .any(|entry| entry.surface_id() == Some(sid))
}

fn has_views(frame: &Frame) -> bool {
    frame
        .entries
        .iter()
        .any(|entry| entry.surface_id().is_some())
}

/// The view a frame shows fullscreen: its selected one when committed, else the first committed.
fn expected_mark(frame: &Frame, committed: &BTreeSet<u64>) -> Option<LayoutEntryId> {
    let shows = |entry: &&LayoutEntry| entry.surface_id().is_some_and(|id| committed.contains(&id));
    frame
        .entries
        .iter()
        .find(|entry| Some(entry.id) == frame.selected_entry_id && shows(entry))
        .or_else(|| frame.entries.iter().find(shows))
        .map(|entry| entry.id)
}

impl Model {
    /// The strip frame a float counts for: its pin while that frame lasts, else the active one.
    fn frame_of_float(&self, strip: &Strip, float: &Frame) -> Option<u64> {
        self.pins
            .get(&float.surface_id)
            .copied()
            .filter(|&id| strip.index_of(id).is_some())
            .or_else(|| strip.active_frame_id())
    }

    /// Every placed frame with the strip frame its views count for.
    fn placed<'a>(&self, fix: &'a Fixture) -> Vec<(&'a Frame, Option<u64>)> {
        fix.ewm_ref()
            .layout
            .monitors()
            .iter()
            .flat_map(|monitor| {
                let strip = &monitor.strip;
                strip
                    .frames()
                    .iter()
                    .map(|frame| (frame, Some(frame.surface_id)))
                    .chain(
                        monitor
                            .floating
                            .frames()
                            .iter()
                            .map(move |float| (float, self.frame_of_float(strip, float))),
                    )
            })
            .collect()
    }

    /// `sid` asks for fullscreen `on`, and the strip frames its views count for drop their rivals.
    fn request(&mut self, fix: &Fixture, sid: u64, on: bool) {
        if !on {
            self.requested.remove(&sid);
            return;
        }
        let placed = self.placed(fix);
        let frames: BTreeSet<u64> = placed
            .iter()
            .filter(|(frame, _)| shows(frame, sid))
            .filter_map(|(_, strip_frame)| *strip_frame)
            .collect();
        let rivals: Vec<u64> = placed
            .iter()
            .filter(|(_, strip_frame)| strip_frame.is_some_and(|id| frames.contains(&id)))
            .flat_map(|(frame, _)| frame.entries.iter().filter_map(LayoutEntry::surface_id))
            .filter(|&other| other != sid)
            .collect();
        for other in rivals {
            self.requested.remove(&other);
        }
        self.requested.insert(sid);
    }
}

struct Clients {
    client: TestClient,
    toplevels: Vec<Toplevel>,
    sids: Vec<u64>,
    /// The last configure each surface got: its size and whether it says fullscreen.
    configured: BTreeMap<u64, (i32, i32, bool)>,
    /// The surfaces configured during the last op.
    answered: BTreeSet<u64>,
}

/// Deliver the op's configures and note what each surface was told.
fn drain(fix: &mut Fixture, clients: &mut Clients) {
    fix.roundtrip(&mut clients.client);
    clients.answered.clear();
    for event in clients.client.drain_events() {
        let ClientEvent::ToplevelConfigure {
            toplevel,
            width,
            height,
            fullscreen,
            ..
        } = event
        else {
            continue;
        };
        let Some(index) = clients
            .toplevels
            .iter()
            .position(|top| top.xdg_toplevel.id() == toplevel.id())
        else {
            continue;
        };
        let sid = clients.sids[index];
        clients.configured.insert(sid, (width, height, fullscreen));
        clients.answered.insert(sid);
    }
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
        4 => (0..SURFACES, any::<bool>())
            .prop_map(|(surface, on)| Op::RequestFullscreen { surface, on }),
        4 => (0..SURFACES).prop_map(Op::Commit),
        1 => (0..SURFACES).prop_map(Op::DestroySurface),
    ]
}

fn to_frame(spec: &FrameSpec, sids: &[u64]) -> Frame {
    let mut frame = Frame::new(frame_id(spec.frame_idx));
    frame.width = VIEW.0 as f64;
    if floating(spec.frame_idx) {
        frame.height = VIEW.1 as f64;
        frame.floating_pos = Some(FLOAT_AT[spec.frame_idx - FIRST_FLOAT]);
    }
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
                VIEW.0 as u32,
                VIEW.1 as u32,
            )
        })
        .collect();
    frame.selected_entry_id = spec.selected.map(|slot| entry_id(spec.frame_idx, slot));
    frame
}

fn check(fix: &Fixture, model: &mut Model, clients: &Clients) -> Result<(), TestCaseError> {
    let ewm = fix.ewm_ref();
    ewm.layout.verify_invariants();
    let mut shown = BTreeSet::new();
    let mut pins = BTreeMap::new();
    for monitor in ewm.layout.monitors() {
        let strip = &monitor.strip;
        let active = strip.active_frame_id();
        for frame in strip.frames() {
            prop_assert_eq!(
                frame.fullscreen_entry().map(|entry| entry.id),
                expected_mark(frame, &model.committed),
                "strip frame {} marks the wrong view",
                frame.surface_id
            );
            if let Some(entry) = frame.fullscreen_entry() {
                shown.insert(entry.surface_id().unwrap());
            }
        }
        // The topmost fullscreen float over the active frame presents, else the frame's own view.
        let mut presenting = active.filter(|&id| {
            strip
                .frames()
                .iter()
                .any(|frame| frame.surface_id == id && frame.fullscreen_entry().is_some())
        });
        for float in monitor.floating.frames().iter().rev() {
            let mark = expected_mark(float, &model.committed);
            prop_assert_eq!(
                float.fullscreen_entry().map(|entry| entry.id),
                mark,
                "float {} marks the wrong view",
                float.surface_id
            );
            let pin = if mark.is_some() {
                model.frame_of_float(strip, float)
            } else if has_views(float) {
                None
            } else {
                model
                    .pins
                    .get(&float.surface_id)
                    .copied()
                    .filter(|&id| strip.index_of(id).is_some())
            };
            prop_assert_eq!(
                float.pinned_to,
                pin,
                "float {} is pinned to the wrong frame",
                float.surface_id
            );
            if let Some(id) = pin {
                pins.insert(float.surface_id, id);
            }
            if let Some(entry) = float.fullscreen_entry() {
                shown.insert(entry.surface_id().unwrap());
                if pin.is_some() && pin == active {
                    presenting = Some(float.surface_id);
                }
            }
        }
        prop_assert_eq!(
            monitor.presenting_fullscreen(),
            presenting,
            "{} presents the wrong frame",
            monitor.name()
        );
    }
    let homeless = ewm.layout.homeless();
    for frame in homeless.tiled.iter().chain(&homeless.floating) {
        prop_assert!(
            frame.fullscreen_entry().is_none() && frame.pinned_to.is_none(),
            "parked frame {} shows fullscreen or stays pinned",
            frame.surface_id
        );
    }
    model.pins = pins;

    for &sid in &clients.sids {
        let requested = model.requested.contains(&sid);
        prop_assert_eq!(ewm.pending_fullscreen(sid), requested, "{} requested", sid);
        prop_assert_eq!(ewm.layout.is_surface_fullscreen(sid), shown.contains(&sid));
        prop_assert_eq!(
            ewm.surfaces
                .get(&sid)
                .is_some_and(|surface| surface.fullscreen),
            model.committed.contains(&sid),
            "{} committed",
            sid
        );
        if model.gone.contains(&sid) {
            continue;
        }
        // Once a shown view is primary, the last configure follows the bit: output or view size.
        let primary = ewm.layout.monitors().iter().find_map(|monitor| {
            monitor
                .strip
                .frames()
                .iter()
                .chain(monitor.floating.frames())
                .flat_map(|frame| frame.entries.iter())
                .find(|entry| entry.surface_id() == Some(sid) && entry.primary())
        });
        if let Some(primary) = primary {
            let Some(&(width, height, fullscreen)) = clients.configured.get(&sid) else {
                return Err(TestCaseError::fail(format!(
                    "{sid} is shown but was never configured"
                )));
            };
            prop_assert_eq!(fullscreen, requested, "{} was last configured", sid);
            prop_assert_eq!(
                (width, height),
                if requested {
                    OUTPUT_SIZE
                } else {
                    (primary.w as i32, primary.h as i32)
                },
                "{} was last configured to",
                sid
            );
        }
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
            let name = OUTPUTS[*output];
            let (floats, tiled): (Vec<Frame>, Vec<Frame>) = frames
                .iter()
                .map(|spec| to_frame(spec, &clients.sids))
                .partition(|frame| frame.floating_pos.is_some());
            fix.tile(name, tiled);
            fix.float(name, floats);
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
            fix.set_fullscreen(entry, *on);
            if let Some(surface_id) = target {
                model.request(fix, surface_id, *on);
            }
        }
        Op::RequestFullscreen { surface, on } => {
            let sid = clients.sids[*surface];
            if !model.gone.contains(&sid) {
                let toplevel = &clients.toplevels[*surface].xdg_toplevel;
                if *on {
                    toplevel.set_fullscreen(None);
                } else {
                    toplevel.unset_fullscreen();
                }
                fix.roundtrip(&mut clients.client);
                model.request(fix, sid, *on);
            }
        }
        Op::Commit(surface) => {
            let sid = clients.sids[*surface];
            if !model.gone.contains(&sid) {
                fix.roundtrip(&mut clients.client);
                fix.map_toplevel(
                    &mut clients.client,
                    &clients.toplevels[*surface],
                    Size::from(VIEW),
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
                let toplevel = &clients.toplevels[*surface];
                toplevel.xdg_toplevel.destroy();
                toplevel.xdg_surface.destroy();
                toplevel.surface.destroy();
                model.requested.remove(&sid);
                model.committed.remove(&sid);
            }
        }
    }
    drain(fix, clients);
    if let Op::RequestFullscreen { surface, .. } = op {
        let sid = clients.sids[*surface];
        if !model.gone.contains(&sid) {
            prop_assert!(
                clients.answered.contains(&sid),
                "the request of {} went unanswered",
                sid
            );
        }
    }
    check(fix, model, clients)
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
            .map(|i| fix.open_toplevel_with_id(&mut client, &format!("app{i}"), Size::from(VIEW)))
            .unzip();
        let mut clients = Clients {
            client,
            toplevels,
            sids,
            configured: BTreeMap::new(),
            answered: BTreeSet::new(),
        };
        let mut model = Model::default();
        drain(&mut fix, &mut clients);
        check(&fix, &mut model, &clients)?;
        for op in &ops {
            apply_op(&mut fix, &mut clients, &mut model, op)?;
        }
    }
}
