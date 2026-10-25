//! The views of surfaces through layouts, fullscreen requests and client answers. Fullscreen is a
//! per-surface bit set by requests and commands alone, shown once its client has committed it.
//! Each strip frame shows one fullscreen view, a float counting for the frame it is over, and a
//! float showing fullscreen pins to that frame and keeps the pin while parked. Until a client
//! commits the buffer a layout asked for, the views of its surface keep presenting the one it has,
//! the view that buffer was made for at its own size and the others covering their windows with it.
//!
//! Ported from niri's src/layout/tests.rs: the hints, the forced size and the answer op.

use std::collections::{BTreeMap, BTreeSet};

use ewm_core::layout::floating::SizeFracPoint;
use ewm_core::layout::strip::{Frame, Strip};
use ewm_core::testing::{
    ClientEvent, Fixture, Subsurface, TestClient, Toplevel, nz, proptest_cases,
};
use ewm_core::{Ewm, LayoutEntry, LayoutEntryId};
use proptest::prelude::*;
use smithay::utils::{Logical, Size};
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
/// Hints and forced sizes reach this far, past the largest window.
const SIDE: i32 = 1200;

/// A width and height, or a min and max of one side.
type Pair = (i32, i32);

fn frame_id(frame_idx: usize) -> u64 {
    100 + frame_idx as u64
}

fn entry_id(frame_idx: usize, entry_slot: usize) -> LayoutEntryId {
    nz(10_000 + frame_idx as u64 * 100 + entry_slot as u64)
}

fn floating(frame_idx: usize) -> bool {
    frame_idx >= FIRST_FLOAT
}

#[derive(Debug, Clone, Copy)]
struct RectSpec {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
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
    /// The client's min and max size, zero for none.
    Hints {
        surface: usize,
        min: Pair,
        max: Pair,
    },
    /// A size the client commits whatever it is asked, or none to answer within its hints again.
    Force {
        surface: usize,
        size: Option<Pair>,
    },
    /// The client answers its last configure with a buffer on its surface.
    Answer(usize),
    /// The client acks its last configure and draws nothing.
    Ack(usize),
    /// The client acks its last configure on its surface and draws into a subsurface.
    AnswerBelow(usize),
    DestroySurface(usize),
}

#[derive(Debug, Clone)]
struct FrameSpec {
    frame_idx: usize,
    /// Slot, surface and window of each entry.
    entries: Vec<(usize, usize, RectSpec)>,
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

/// The primary view of `sid`.
fn primary(ewm: &Ewm, sid: u64) -> Option<&LayoutEntry> {
    ewm.layout
        .all_frames()
        .flat_map(|frame| &frame.entries)
        .find(|entry| entry.surface_id() == Some(sid) && entry.primary())
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
    below: Vec<Option<Subsurface>>,
    /// The view each surface presented before the op.
    presented: Vec<Option<LayoutEntryId>>,
}

impl Clients {
    /// The size `surface` was last asked for, none before its first real configure.
    fn asked(&self, surface: usize) -> Option<Pair> {
        self.configured
            .get(&self.sids[surface])
            .map(|&(w, h, _)| (w, h))
            .filter(|&(w, h)| w > 0 && h > 0)
    }

    fn snapshot(&mut self, ewm: &Ewm) {
        for (index, sid) in self.sids.iter().enumerate() {
            let Some(surface) = ewm.surfaces.get(sid) else {
                continue;
            };
            self.presented[index] = surface.presented;
        }
    }
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

fn rect_strategy() -> impl Strategy<Value = RectSpec> {
    (
        prop::sample::select(vec![0, 8, 20, 400]),
        prop::sample::select(vec![0, 6, 30]),
        prop::sample::select(vec![300u32, 400, 800]),
        prop::sample::select(vec![200u32, 300, 600]),
    )
        .prop_map(|(x, y, w, h)| RectSpec { x, y, w, h })
}

fn layout_strategy() -> impl Strategy<Value = Vec<FrameSpec>> {
    let frame = (
        any::<bool>(),
        prop::collection::vec((any::<bool>(), 0..SURFACES, rect_strategy()), ENTRY_SLOTS),
        0..ENTRY_SLOTS,
    );
    prop::collection::vec(frame, FRAME_COUNT).prop_map(|seeds| {
        seeds
            .into_iter()
            .enumerate()
            .filter(|(_, (present, _, _))| *present)
            .map(|(frame_idx, (_, slots, selected))| {
                let entries: Vec<(usize, usize, RectSpec)> = slots
                    .into_iter()
                    .enumerate()
                    .filter(|(_, (present, _, _))| *present)
                    .map(|(slot, (_, surface, rect))| (slot, surface, rect))
                    .collect();
                let selected = entries
                    .iter()
                    .any(|(slot, _, _)| *slot == selected)
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
        5 => (0..SURFACES, min_max_strategy(), min_max_strategy()).prop_map(
            |(surface, (min_w, max_w), (min_h, max_h))| Op::Hints {
                surface,
                min: (min_w, min_h),
                max: (max_w, max_h),
            }
        ),
        1 => (0..SURFACES, min_max_strategy())
            .prop_map(|(surface, size)| Op::Hints { surface, min: size, max: size }),
    ]
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
        1 => hints_strategy(),
        1 => (0..SURFACES, prop::option::of((1..SIDE, 1..SIDE)))
            .prop_map(|(surface, size)| Op::Force { surface, size }),
        4 => (0..SURFACES).prop_map(Op::Answer),
        1 => (0..SURFACES).prop_map(Op::Ack),
        1 => (0..SURFACES).prop_map(Op::AnswerBelow),
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
        .map(|&(slot, surface, rect)| {
            LayoutEntry::surface(
                entry_id(spec.frame_idx, slot),
                "entry".into(),
                sids[surface],
                rect.x,
                rect.y,
                rect.w,
                rect.h,
            )
        })
        .collect();
    frame.selected_entry_id = spec.selected.map(|slot| entry_id(spec.frame_idx, slot));
    frame
}

fn check(fix: &Fixture, model: &mut Model, clients: &Clients) -> Result<(), TestCaseError> {
    let ewm = fix.ewm_ref();
    ewm.verify_invariants();
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
        prop_assert_eq!(
            frame.fullscreen_entry().map(|entry| entry.id),
            expected_mark(frame, &model.committed),
            "parked frame {} marks the wrong view",
            frame.surface_id
        );
    }
    for float in &homeless.floating {
        prop_assert_eq!(
            float.pinned_to,
            model.pins.get(&float.surface_id).copied(),
            "parked float {} lost its pin",
            float.surface_id
        );
        if let Some(id) = float.pinned_to {
            pins.insert(float.surface_id, id);
        }
    }
    model.pins = pins;

    for (index, &sid) in clients.sids.iter().enumerate() {
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
        let primary = primary(ewm, sid);
        // Once a shown view is primary, the last configure follows the bit: output or view size.
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
        // The primary view presents the buffer at its size once the client has caught up, until
        // then the view presented before the op does, even gone from the layout, and nothing else
        // ever does. The other views cover their windows with the buffer.
        let surface = &ewm.surfaces[&sid];
        let primary = primary.map(|entry| entry.id);
        let presented = surface.presented;
        prop_assert!(
            primary.is_none()
                || presented == primary
                || (!surface.synced() && presented == clients.presented[index]),
            "{} presents {:?} with primary {:?}, {:?} before the op",
            sid,
            presented,
            primary,
            clients.presented[index]
        );
        let buffer = surface.window.geometry().size;
        for view in ewm.views_of(sid) {
            prop_assert_eq!(
                view.native,
                Some(view.entry) == presented,
                "view {} of {} presents the wrong contents",
                view.entry,
                sid
            );
            let scale = if view.native {
                1.
            } else {
                f64::max(
                    view.window.size.w as f64 / buffer.w as f64,
                    view.window.size.h as f64 / buffer.h as f64,
                )
            };
            prop_assert_eq!(
                view.scale,
                scale,
                "view {} of {} draws at the wrong scale",
                view.entry,
                sid
            );
            prop_assert_eq!(
                view.rect.size,
                buffer.to_f64().upscale(scale).to_i32_round(),
                "view {} of {} draws the wrong size",
                view.entry,
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
    clients.snapshot(fix.ewm_ref());
    // The surface that answered, what it was asked and the size it committed.
    let mut answered: Option<(u64, Pair, Size<i32, Logical>)> = None;
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
            // Frames open before the layout lands, as they do for Emacs, each opening settling
            // the float pins, then one layout declares them all.
            fix.open_frames(name, &tiled, false);
            fix.open_frames(name, &floats, true);
            check(fix, model, clients)?;
            fix.layout(name, tiled, floats);
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
                model.request(fix, sid, *on);
            }
        }
        Op::Hints { surface, min, max } => clients.client.set_size_hints(
            &clients.toplevels[*surface],
            Size::from(*min),
            Size::from(*max),
        ),
        Op::Force { surface, size } => clients
            .client
            .set_forced_size(&clients.toplevels[*surface], size.map(Size::from)),
        Op::Answer(surface) | Op::Ack(surface) | Op::AnswerBelow(surface) => {
            let sid = clients.sids[*surface];
            if let Some(asked) = clients
                .asked(*surface)
                .filter(|_| !model.gone.contains(&sid))
            {
                let toplevel = &clients.toplevels[*surface];
                let size = clients.client.answer_size(toplevel, Size::from(asked));
                let kept = fix.ewm_ref().surfaces[&sid].window.geometry().size;
                let committed = match op {
                    Op::Ack(_) => {
                        toplevel.surface.commit();
                        kept
                    }
                    Op::Answer(_) => {
                        toplevel
                            .xdg_surface
                            .set_window_geometry(0, 0, size.w, size.h);
                        clients.client.paint(&toplevel.surface, size);
                        size
                    }
                    _ => {
                        let below = clients.below[*surface].get_or_insert_with(|| {
                            clients.client.create_subsurface(&toplevel.surface)
                        });
                        toplevel
                            .xdg_surface
                            .set_window_geometry(0, 0, size.w, size.h);
                        toplevel.surface.commit();
                        clients.client.paint(&below.surface, size);
                        size
                    }
                };
                if model.requested.contains(&sid) {
                    model.committed.insert(sid);
                } else {
                    model.committed.remove(&sid);
                }
                answered = Some((sid, asked, committed));
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
    if let Some((sid, asked, committed)) = answered {
        let now = &fix.ewm_ref().surfaces[&sid];
        prop_assert_eq!(
            now.window.geometry().size,
            committed,
            "the size {} committed for {:?}",
            sid,
            asked
        );
        prop_assert!(now.synced(), "answering {:?} left {} behind", asked, sid);
    }
    check(fix, model, clients)
}

fn check_ops(ops: &[Op]) -> Result<(), TestCaseError> {
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
        below: std::iter::repeat_with(|| None).take(SURFACES).collect(),
        presented: vec![None; SURFACES],
    };
    let mut model = Model::default();
    drain(&mut fix, &mut clients);
    check(&fix, &mut model, &clients)?;
    for op in ops {
        apply_op(&mut fix, &mut clients, &mut model, op)?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases(48)))]

    #[test]
    fn view_invariants_hold(ops in prop::collection::vec(op_strategy(), 1..40)) {
        check_ops(&ops)?;
    }
}

// Sequences the randomised test once found failing.

const fn rect(w: u32, h: u32) -> RectSpec {
    RectSpec { x: 0, y: 0, w, h }
}

const VIEW_RECT: RectSpec = rect(VIEW.0 as u32, VIEW.1 as u32);

fn frame(
    frame_idx: usize,
    entries: &[(usize, usize, RectSpec)],
    selected: Option<usize>,
) -> FrameSpec {
    FrameSpec {
        frame_idx,
        entries: entries.to_vec(),
        selected,
    }
}

#[test]
fn two_requests_land_in_one_frame() {
    check_ops(&[
        Op::RequestFullscreen {
            surface: 2,
            on: true,
        },
        Op::RequestFullscreen {
            surface: 0,
            on: true,
        },
        Op::ApplyLayout {
            output: 0,
            frames: vec![frame(2, &[(0, 0, VIEW_RECT), (2, 2, VIEW_RECT)], Some(0))],
        },
        Op::RequestFullscreen {
            surface: 0,
            on: true,
        },
    ])
    .unwrap();
}

#[test]
fn a_fullscreen_view_survives_losing_every_output() {
    check_ops(&[
        Op::ApplyLayout {
            output: 1,
            frames: vec![frame(0, &[], None)],
        },
        Op::ApplyLayout {
            output: 0,
            frames: vec![frame(1, &[], None), frame(3, &[(2, 1, VIEW_RECT)], None)],
        },
        Op::RequestFullscreen {
            surface: 1,
            on: true,
        },
        Op::Answer(1),
        Op::RemoveOutput(0),
        Op::RemoveOutput(1),
        Op::AddOutput(0),
    ])
    .unwrap();
}

#[test]
fn a_layout_drops_the_fullscreen_view() {
    check_ops(&[
        Op::RequestFullscreen {
            surface: 1,
            on: true,
        },
        Op::ApplyLayout {
            output: 1,
            frames: vec![frame(3, &[(1, 1, VIEW_RECT)], None)],
        },
        Op::Answer(1),
        Op::ApplyLayout {
            output: 1,
            frames: vec![frame(1, &[], None), frame(3, &[], None)],
        },
    ])
    .unwrap();
}

#[test]
fn an_answer_before_the_first_layout() {
    check_ops(&[
        Op::Answer(0),
        Op::ApplyLayout {
            output: 0,
            frames: vec![frame(0, &[(1, 0, rect(300, 200))], Some(1))],
        },
        Op::ApplyLayout {
            output: 0,
            frames: vec![frame(
                0,
                &[(0, 0, rect(300, 300)), (1, 0, rect(300, 200))],
                Some(0),
            )],
        },
    ])
    .unwrap();
}

#[test]
fn fullscreen_on_and_off_answered_each_time() {
    check_ops(&[
        Op::ApplyLayout {
            output: 0,
            frames: vec![frame(
                0,
                &[(0, 0, rect(300, 200)), (1, 0, rect(300, 300))],
                Some(0),
            )],
        },
        Op::SetEntryFullscreen {
            frame_idx: 0,
            entry_slot: 0,
            on: true,
        },
        Op::Answer(0),
        Op::SetEntryFullscreen {
            frame_idx: 0,
            entry_slot: 0,
            on: false,
        },
        Op::Answer(0),
    ])
    .unwrap();
}

#[test]
fn an_ack_without_a_buffer_between_fullscreen_requests() {
    check_ops(&[
        Op::ApplyLayout {
            output: 0,
            frames: vec![frame(
                0,
                &[(0, 0, rect(300, 200)), (1, 0, rect(300, 300))],
                Some(0),
            )],
        },
        Op::Answer(0),
        Op::SetEntryFullscreen {
            frame_idx: 0,
            entry_slot: 0,
            on: true,
        },
        Op::Ack(0),
        Op::SetEntryFullscreen {
            frame_idx: 0,
            entry_slot: 0,
            on: false,
        },
    ])
    .unwrap();
}
