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
use std::time::{Duration, Instant};

use ewm_core::event::Event;
use ewm_core::handlers::PlacementHint;
use ewm_core::layout::strip::Frame;
use ewm_core::testing::{
    ClientEvent, Fixture, TestClient, Toplevel, clear_pending_frames, mark_active_frame_close, nz,
    prepare_frame, proptest_cases, strip_with_frame_ids,
};
use ewm_core::{
    Action, FocusSource, LayoutEntry, LayoutEntryId, Notify, OutputConfig, OverviewAction,
};
use proptest::prelude::*;
use smithay::utils::Size;
use smithay::wayland::xdg_activation::XdgActivationTokenData;
use wayland_client::Proxy as _;

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
    /// A bell marks the frame.
    MarkUrgent(u64),
    /// Lisp launches an app with a hint in its token and the app redeems it on
    /// `LAUNCH_SIDS[slot]`, before or after mapping. `stale` ages the token past the grant.
    Launch {
        slot: usize,
        hint: Option<(HintFrame, bool)>,
        stale: bool,
    },
    /// The next launched app commits its first buffer, a redeemed one first.
    MapLaunch,
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
/// Layout-only ids that name the app windows `Op::Launch` routes, once they mapped.
const LAUNCH_SIDS: [u64; 3] = [1006, 1007, 1008];
/// A frame id no toplevel ever gets.
const GONE_FRAME: u64 = 999;

/// The frame a launch hint names, resolved against the state at launch.
#[derive(Debug, Clone, Copy)]
enum HintFrame {
    None,
    Focused,
    /// The nth other Emacs frame, gone when there is none.
    Other(usize),
    Gone,
}

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
        Just(Action::MoveFrameToNextOutput),
        Just(Action::ToggleFullscreen),
        prop_oneof![
            Just(OverviewAction::Toggle),
            Just(OverviewAction::Open),
            Just(OverviewAction::Close)
        ]
        .prop_map(Action::Overview),
        Just(Action::HideToggle),
        Just(Action::FloatFocusToggle),
    ]
}

/// No hint, or a quiet or loud one for the focused frame, another one, a gone one or none.
fn hint_strategy() -> impl Strategy<Value = Option<(HintFrame, bool)>> {
    prop::option::weighted(
        0.8,
        (
            prop_oneof![
                Just(HintFrame::None),
                Just(HintFrame::Focused),
                (0..MAX_FRAMES).prop_map(HintFrame::Other),
                Just(HintFrame::Gone),
            ],
            any::<bool>(),
        ),
    )
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        1 => action_strategy().prop_map(Op::Action),
        1 => (0..OUTPUTS.len(), FRAME_SID_RANGE)
            .prop_map(|(output, surface_id)| Op::CreateEmacsFrame { output, surface_id }),
        1 => layout_strategy().prop_map(|frames| Op::ApplyLayout { frames }),
        1 => entry_view_strategy().prop_map(Op::FocusView),
        1 => FRAME_SID_RANGE.prop_map(Op::FocusFrame),
        1 => (0..OUTPUTS.len()).prop_map(Op::DisableOutput),
        1 => (0..OUTPUTS.len()).prop_map(Op::EnableOutput),
        1 => (0..OUTPUTS.len()).prop_map(Op::RemoveOutput),
        1 => (0..OUTPUTS.len()).prop_map(Op::AddOutput),
        1 => FRAME_SID_RANGE.prop_map(Op::DestroyFrame),
        1 => ENTRY_SID_RANGE.prop_map(Op::DestroySurface),
        1 => ENTRY_SID_RANGE.prop_map(Op::ActivateSurface),
        1 => FRAME_SID_RANGE.prop_map(Op::MarkUrgent),
        1 => launch_strategy(),
        1 => Just(Op::MapLaunch),
    ]
}

fn launch_strategy() -> impl Strategy<Value = Op> {
    (0..LAUNCH_SIDS.len(), hint_strategy(), any::<bool>())
        .prop_map(|(slot, hint, stale)| Op::Launch { slot, hint, stale })
}

/// Launches, maps and the focus moves between them, over the frames and layouts they land in.
fn launch_op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => launch_strategy(),
        3 => Just(Op::MapLaunch),
        2 => FRAME_SID_RANGE.prop_map(Op::FocusFrame),
        2 => layout_strategy().prop_map(|frames| Op::ApplyLayout { frames }),
        1 => (0..OUTPUTS.len(), FRAME_SID_RANGE)
            .prop_map(|(output, surface_id)| Op::CreateEmacsFrame { output, surface_id }),
        1 => FRAME_SID_RANGE.prop_map(Op::DestroyFrame),
        1 => ENTRY_SID_RANGE.prop_map(Op::ActivateSurface),
        1 => action_strategy().prop_map(Op::Action),
    ]
}

/// The Emacs frames the ops created, by logical id, and the app windows a launch may route.
struct Frames {
    client: TestClient,
    toplevels: HashMap<u64, (Toplevel, u64)>,
    next_seq: u64,
    apps: TestClient,
    launches: Vec<Launch>,
}

/// An app window of `LAUNCH_SIDS`, opened unmapped so a token can reach it first.
struct Launch {
    toplevel: Toplevel,
    sid: u64,
    redeemed: Option<Redeemed>,
    mapped: bool,
}

/// What a launch redeemed before mapping, for the map to answer.
struct Redeemed {
    token: String,
    hint: PlacementHint,
    fresh: bool,
}

impl Frames {
    fn new(fix: &mut Fixture) -> Self {
        clear_pending_frames();
        // Opened before Emacs's pid is known, so these are app windows.
        let mut apps = fix.new_client().unwrap();
        fix.roundtrip(&mut apps);
        let launches = LAUNCH_SIDS
            .iter()
            .map(|_| {
                let toplevel = fix.open_toplevel_unmapped(&mut apps, "launch", None);
                let sid = fix.find_surface_id(&apps, &toplevel.surface).unwrap();
                Launch {
                    toplevel,
                    sid,
                    redeemed: None,
                    mapped: false,
                }
            })
            .collect();
        fix.ewm().set_emacs_pid(std::process::id());
        let mut client = fix.new_client().unwrap();
        fix.roundtrip(&mut client);
        Self {
            client,
            toplevels: HashMap::new(),
            next_seq: 0,
            apps,
            launches,
        }
    }

    /// The surface id of logical frame `id`, or `id` itself for a frame that never existed.
    fn real(&self, id: u64) -> u64 {
        self.toplevels.get(&id).map_or(id, |(_, sid)| *sid)
    }

    /// The launch a layout-only id stands for.
    fn launched(&self, id: u64) -> Option<&Launch> {
        let slot = LAUNCH_SIDS.iter().position(|&sid| sid == id)?;
        Some(&self.launches[slot])
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
                // Lisp places a launched window once it mapped, never before.
                let surface_id = frames
                    .launched(e.surface_id)
                    .filter(|launch| launch.mapped)
                    .map_or(e.surface_id, |launch| launch.sid);
                LayoutEntry::surface(e.entry_id, "entry".to_string(), surface_id, 0, 0, 100, 100)
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
            let unfocused = fix.ewm_ref().focused_surface_id().is_none();
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
                    fix.ewm_ref().focused_frame_id(),
                    Some(sid),
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
            fix.ewm()
                .set_focus_entry(v, FocusSource::Test, Notify::None);
        }
        Op::FocusFrame(f) => {
            fix.ewm()
                .set_focus_frame(frames.real(f), FocusSource::Test, Notify::None);
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
            let prev_focused = fix.ewm_ref().focused_frame_id();
            let expected_same_output_refocus = if prev_focused == Some(id) {
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
                    fix.ewm_ref().focused_frame_id(),
                    Some(expected),
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
                if prev_focused != Some(id) {
                    assert_eq!(
                        fix.ewm_ref().focused_frame_id(),
                        prev_focused,
                        "destroy({}) changed focus away from inactive frame {:?}",
                        id,
                        prev_focused,
                    );
                }
            }
        }
        Op::ActivateSurface(id) => {
            let id = frames.launched(id).map_or(id, |launch| launch.sid);
            let prev = (
                fix.ewm_ref().focused_frame_id(),
                fix.ewm_ref().focused_entry_id(),
            );
            fix.ewm().activate_or_defer_surface(id, FocusSource::Test);
            let ewm = fix.ewm_ref();
            let focused = ewm.focused_surface_id() == Some(id);
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
                    (ewm.focused_frame_id(), ewm.focused_entry_id()),
                    prev,
                    "activate({}) of a hidden surface moved focus",
                    id,
                );
            }
            // Invariant 13: a view in the focused frame wins.
            if focused && let Some(frame) = prev.0 {
                assert_focused_frame_view(fix, id, frame);
            }
        }
        Op::MarkUrgent(f) => {
            fix.ewm().mark_workspace_urgent(Some(frames.real(f)));
        }
        Op::Launch { slot, hint, stale } => {
            let focused = fix.ewm_ref().focused_frame_id();
            let frame_surface_id = match hint.map(|(frame, _)| frame) {
                None | Some(HintFrame::None) => None,
                Some(HintFrame::Focused) => focused,
                Some(HintFrame::Other(n)) => {
                    let mut others: Vec<u64> = frames
                        .toplevels
                        .values()
                        .map(|(_, sid)| *sid)
                        .filter(|sid| Some(*sid) != focused)
                        .collect();
                    others.sort_unstable();
                    others
                        .get(n % others.len().max(1))
                        .copied()
                        .or(Some(GONE_FRAME))
                }
                Some(HintFrame::Gone) => Some(GONE_FRAME),
            };
            let hint = PlacementHint {
                frame_surface_id,
                quiet: hint.is_some_and(|(_, quiet)| quiet),
            };
            let token = if stale {
                let data = XdgActivationTokenData {
                    timestamp: Instant::now() - Duration::from_secs(11),
                    ..Default::default()
                };
                data.user_data.insert_if_missing(|| hint);
                fix.ewm()
                    .activation_state
                    .create_external_token(data)
                    .0
                    .as_str()
                    .to_owned()
            } else {
                fix.create_activation_token(Some(hint))
            };
            let sid = frames.launches[slot].sid;
            let before = focus(fix);
            let pending_before = pending(fix, sid);
            let urgent_before = fix.ewm_ref().surfaces[&sid].urgent;
            frames
                .apps
                .activate(&token, &frames.launches[slot].toplevel.surface);
            fix.roundtrip(&mut frames.apps);
            let launch = &mut frames.launches[slot];
            let ewm = fix.ewm_ref();
            if !launch.mapped {
                // Before the first buffer the token waits on the record and nothing is judged.
                assert!(
                    ewm.surfaces[&sid].activation.is_some(),
                    "redeem on unmapped {sid} stored no token",
                );
                assert_eq!(
                    (focus(fix), pending(fix, sid), ewm.surfaces[&sid].urgent),
                    (before, pending_before, urgent_before),
                    "redeem on unmapped {sid} acted before map",
                );
                launch.redeemed = Some(Redeemed {
                    token,
                    hint,
                    fresh: !stale,
                });
                return;
            }
            // A mapped surface is judged at once: a stale or quiet token does nothing, a hinted
            // frame without the keyboard flags it, anything else focuses it or defers the grant.
            let away = hint.frame_surface_id.is_some_and(|frame| {
                ewm.layout.frame_location(frame).is_some() && before.frame != Some(frame)
            });
            let focused = ewm.focused_surface_id() == Some(sid);
            let pending_now = pending(fix, sid);
            if stale || hint.quiet {
                assert_eq!(
                    (focus(fix), pending_now, ewm.surfaces[&sid].urgent),
                    (before, pending_before, urgent_before),
                    "a stale or quiet redeem on mapped {sid} acted",
                );
            } else if away {
                assert_eq!(focus(fix), before, "away redeem on {sid} moved focus");
                // Whatever holds the keyboard has been seen, so only an unfocused one stays flagged.
                assert!(
                    focused || ewm.surfaces[&sid].urgent,
                    "away redeem left {sid} unflagged"
                );
                assert_eq!(
                    pending_now, pending_before,
                    "away redeem on {sid} touched the grant"
                );
            } else {
                assert_eq!(
                    pending_now, !focused,
                    "redeem on {sid} must focus the surface or defer it",
                );
                if pending_now {
                    assert_eq!(focus(fix), before, "deferred redeem on {sid} moved focus");
                }
                if focused && let Some(frame) = before.frame {
                    assert_focused_frame_view(fix, sid, frame);
                }
            }
        }
        Op::MapLaunch => {
            let Some(slot) = frames
                .launches
                .iter()
                .position(|launch| !launch.mapped && launch.redeemed.is_some())
                .or_else(|| frames.launches.iter().position(|launch| !launch.mapped))
            else {
                return;
            };
            let sid = frames.launches[slot].sid;
            let redeemed = frames.launches[slot].redeemed.take();
            let hint = redeemed.as_ref().map(|r| r.hint).unwrap_or_default();
            let before = focus(fix);
            let pending_before = pending(fix, sid);
            let ewm = fix.ewm_ref();
            let active_output = ewm.active_output().map(|output| output.name());
            let hinted = hint.frame_surface_id.and_then(|frame| {
                ewm.layout
                    .frame_location(frame)
                    .map(|location| (location.monitor.name().to_owned(), location.floating))
            });
            let away = hinted.is_some() && before.frame != hint.frame_surface_id;
            let over_fullscreen = before
                .surface
                .is_some_and(|surface| ewm.pending_fullscreen(surface));
            let seen = ewm.captured_events.as_ref().map_or(0, Vec::len);
            fix.map_toplevel(
                &mut frames.apps,
                &frames.launches[slot].toplevel,
                Size::from((400, 300)),
            );
            frames.launches[slot].mapped = true;
            let ewm = fix.ewm_ref();
            let mapped = ewm.captured_events.as_deref().unwrap_or(&[])[seen..]
                .iter()
                .find_map(|event| match event {
                    Event::Mapped {
                        id,
                        activate,
                        output,
                        frame_surface_id,
                        token,
                        ..
                    } if *id == sid => {
                        Some((*activate, output.clone(), *frame_surface_id, token.clone()))
                    }
                    _ => None,
                });
            let Some((activate, output, host, token)) = mapped else {
                panic!("launch {sid} mapped without a Mapped event");
            };
            // One shot: the record no longer holds the token and the map itself moves nothing.
            assert!(
                ewm.surfaces[&sid].activation.is_none(),
                "mapped {sid} still holds its token",
            );
            assert_eq!(focus(fix), before, "map of {sid} moved focus");
            // Lisp learns which launch the surface came from, from a stale token too.
            assert_eq!(
                token,
                redeemed.as_ref().map(|r| r.token.clone()),
                "launch {sid} echoed the wrong token",
            );
            // The hint places: its frame while that is tiled, its output while the frame exists.
            match &hinted {
                Some((name, floating)) => {
                    assert_eq!(
                        output.as_deref(),
                        Some(name.as_str()),
                        "launch {sid} missed the hinted output",
                    );
                    if !floating {
                        assert_eq!(host, hint.frame_surface_id, "launch {sid} missed its frame");
                    }
                }
                None => assert_eq!(
                    output, active_output,
                    "launch {sid} without a usable hint left the active output",
                ),
            }
            if let Some(host) = host {
                assert!(
                    ewm.layout.frame_location(host).is_some_and(|location| {
                        !location.floating && Some(location.monitor.name()) == output.as_deref()
                    }),
                    "host {host} of launch {sid} is not a tiled frame on {output:?}",
                );
            }
            // The hint only vetoes: quiet or away never selects, a grant always does, a plain
            // window follows the fullscreen rule. The grant waits for a layout only if it selects.
            let granted = redeemed.as_ref().is_some_and(|r| r.fresh) || pending_before;
            if hint.quiet || away {
                assert!(!activate, "vetoed launch {sid} asked Lisp to select it");
            } else if granted {
                assert!(activate, "granted launch {sid} was not selected");
            } else {
                assert_eq!(activate, !over_fullscreen, "plain launch {sid} selection");
            }
            assert_eq!(
                pending(fix, sid),
                granted && activate,
                "the grant of launch {sid} does not match its selection",
            );
            // A denied loud launch is flagged, nothing else is.
            assert_eq!(
                ewm.surfaces[&sid].urgent,
                away && !hint.quiet,
                "urgency of launch {sid}",
            );
        }
        Op::DestroySurface(id) => {
            let was_present = layout_surface_ids(fix).contains(&id);
            let was_focused = fix.ewm_ref().focused_surface_id() == Some(id);
            let prev_focused_frame = fix.ewm_ref().focused_frame_id();
            let prev_focused_entry = fix.ewm_ref().focused_entry_id();
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
                    Some(id),
                    "destroy({}) left keyboard focus on the dead surface",
                    id,
                );
                if was_focused {
                    assert_eq!(
                        fix.ewm_ref().focused_frame_id(),
                        prev_focused_frame,
                        "destroy({}) changed host frame focus",
                        id,
                    );
                    assert_eq!(
                        fix.ewm_ref().focused_entry_id(),
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
    let chosen = fix.ewm_ref().focused_entry_id();
    assert!(
        chosen.is_some_and(|entry| views.contains(&entry)),
        "invariant 13: activate({surface}) left focused frame {frame} which shows it",
    );
}

/// Where the keyboard is, as frame, view and surface.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Focus {
    frame: Option<u64>,
    entry: Option<LayoutEntryId>,
    surface: Option<u64>,
}

fn focus(fix: &Fixture) -> Focus {
    let ewm = fix.ewm_ref();
    Focus {
        frame: ewm.focused_frame_id(),
        entry: ewm.focused_entry_id(),
        surface: ewm.focused_surface_id(),
    }
}

/// Whether a grant for `sid` waits for a layout to show it.
fn pending(fix: &Fixture, sid: u64) -> bool {
    fix.ewm_ref().pending_activations().any(|id| id == sid)
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

fn all_frames(fix: &Fixture) -> impl Iterator<Item = &ewm_core::layout::strip::Frame> {
    fix.ewm_ref().layout.all_frames()
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
        fix.ewm().set_focus_frame(closing_id, FocusSource::Test, Notify::None);

        mark_active_frame_close(closing_id);
        fix.ewm().set_focus_frame(ids[mru_idx], FocusSource::Test, Notify::None);
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
            .set_focus_frame(refocus.unwrap(), FocusSource::ToplevelDestroyed, Notify::None);
        prop_assert_eq!(fix.ewm_ref().focused_frame_id(), Some(expected));
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

/// Every op keeps the focus invariants, and none asks Emacs to run a command (invariant 15).
fn check_focus_ops(ops: Vec<Op>) -> Result<(), TestCaseError> {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    for name in OUTPUTS {
        fix.add_output(name, 1920, 1080);
    }
    let mut frames = Frames::new(&mut fix);
    fix.ewm_ref().verify_invariants();
    for op in ops {
        apply_op(&mut fix, &mut frames, op);
        fix.ewm_ref().verify_invariants();
        for event in fix.drain_events() {
            prop_assert!(
                !matches!(event, Event::InterceptedCommand { .. } | Event::Key(_)),
                "{event:?}"
            );
        }
    }
    Ok(())
}

/// Lisp freezes on `FrameHidden` and thaws on `FrameShown`, so a placed frame's last event must
/// say shown iff it is its strip's active frame or the overview is active.
fn check_show_hide_ops(ops: Vec<Op>) -> Result<(), TestCaseError> {
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
        fix.ewm_ref().verify_invariants();
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
    Ok(())
}

/// Inside the overview no op clears a mark. Outside it, once refreshed, whatever holds focus is
/// not urgent (invariant 20).
fn check_urgency_ops(ops: Vec<Op>) -> Result<(), TestCaseError> {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    for name in OUTPUTS {
        fix.add_output(name, 1920, 1080);
    }
    let mut frames = Frames::new(&mut fix);
    for op in ops {
        let was_open = fix.ewm_ref().overview.is_open();
        let marked: Vec<u64> = fix
            .ewm_ref()
            .surfaces
            .iter()
            .filter(|(_, surface)| surface.urgent)
            .map(|(id, _)| *id)
            .collect();
        apply_op(&mut fix, &mut frames, op);
        fix.dispatch();
        let ewm = fix.ewm_ref();
        if ewm.overview.is_open() {
            for id in marked
                .iter()
                .filter(|id| was_open && ewm.surfaces.contains_key(id))
            {
                prop_assert!(
                    ewm.surfaces[id].urgent,
                    "frame {id} unmarked inside the overview"
                );
            }
        } else {
            for id in [ewm.focused_surface_id(), ewm.focused_frame_id()]
                .into_iter()
                .flatten()
            {
                prop_assert!(
                    ewm.surfaces.get(&id).is_none_or(|surface| !surface.urgent),
                    "focused {id} is urgent outside the overview"
                );
            }
        }
    }
    Ok(())
}

/// A launch lands as its hint says, selects only when granted and not vetoed, and flags a
/// denied loud one. A frame per output gives the hints somewhere to point.
fn check_launch_ops(ops: Vec<Op>) -> Result<(), TestCaseError> {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    for name in OUTPUTS {
        fix.add_output(name, 1920, 1080);
    }
    let mut frames = Frames::new(&mut fix);
    for (output, surface_id) in [(0, 100), (1, 101)] {
        apply_op(
            &mut fix,
            &mut frames,
            Op::CreateEmacsFrame { output, surface_id },
        );
    }
    for op in ops {
        apply_op(&mut fix, &mut frames, op);
        fix.ewm_ref().verify_invariants();
    }
    Ok(())
}

proptest! {
    #[test]
    fn focus_invariants_hold(ops in prop::collection::vec(op_strategy(), 1..30)) {
        check_focus_ops(ops)?;
    }

    #[test]
    fn launches_follow_their_hints(ops in prop::collection::vec(launch_op_strategy(), 1..30)) {
        check_launch_ops(ops)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases(64)))]

    #[test]
    fn urgency_survives_the_overview(ops in prop::collection::vec(op_strategy(), 1..30)) {
        check_urgency_ops(ops)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases(64)))]

    #[test]
    fn frame_show_hide_tracks_active_slot(
        ops in prop::collection::vec(op_strategy(), 1..30)
    ) {
        check_show_hide_ops(ops)?;
    }
}

// Sequences the randomised tests once found failing.

#[test]
fn frames_opened_under_the_overview() {
    let ops = vec![
        Op::Action(Action::Overview(OverviewAction::Toggle)),
        Op::CreateEmacsFrame {
            output: 1,
            surface_id: 100,
        },
        Op::CreateEmacsFrame {
            output: 1,
            surface_id: 101,
        },
    ];
    check_focus_ops(ops.clone()).unwrap();
    check_show_hide_ops(ops).unwrap();
}

#[test]
fn an_output_removed_under_the_overview() {
    let ops = vec![
        Op::CreateEmacsFrame {
            output: 1,
            surface_id: 101,
        },
        Op::CreateEmacsFrame {
            output: 0,
            surface_id: 100,
        },
        Op::Action(Action::Overview(OverviewAction::Toggle)),
        Op::RemoveOutput(0),
    ];
    check_focus_ops(ops.clone()).unwrap();
    check_show_hide_ops(ops).unwrap();
}

#[test]
fn frames_back_from_parking_under_the_overview() {
    let ops = vec![
        Op::CreateEmacsFrame {
            output: 0,
            surface_id: 100,
        },
        Op::Action(Action::Overview(OverviewAction::Toggle)),
        Op::CreateEmacsFrame {
            output: 0,
            surface_id: 101,
        },
        Op::DisableOutput(1),
        Op::DisableOutput(0),
        Op::EnableOutput(0),
    ];
    check_focus_ops(ops.clone()).unwrap();
    check_show_hide_ops(ops).unwrap();
}

#[derive(Debug, Clone)]
enum SuspendOp {
    Op(Op),
    Unattended(bool),
}

/// The layout entries that show real app windows.
const APP_ENTRY_SIDS: [u64; 2] = [1001, 1002];

proptest! {
    #![proptest_config(ProptestConfig {
        cases: proptest_cases(64),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// A placed toplevel is told it is suspended iff no shown frame holds it, a frame showing
    /// when Lisp was last told so, and none while the frames are hidden or the system is kept
    /// awake behind a closed lid.
    #[test]
    fn suspended_tracks_shown(
        ops in prop::collection::vec(
            prop_oneof![
                4 => op_strategy().prop_map(SuspendOp::Op),
                1 => any::<bool>().prop_map(SuspendOp::Unattended),
            ],
            1..30,
        )
    ) {
        let _serial = SERIAL.lock().unwrap();
        let mut fix = Fixture::new().unwrap();
        fix.ewm().animations_clock.set_complete_instantly(true);
        for name in OUTPUTS {
            fix.add_output(name, 1920, 1080);
        }
        // Opened before Emacs's pid is known, so these are app windows.
        let mut apps = fix.new_client().unwrap();
        fix.roundtrip(&mut apps);
        let app_windows: Vec<(Toplevel, u64)> = APP_ENTRY_SIDS
            .iter()
            .map(|_| fix.open_toplevel_with_id(&mut apps, "app", Size::from((400, 300))))
            .collect();
        let mut frames = Frames::new(&mut fix);
        fix.enable_event_capture();
        let mut told: HashMap<u64, bool> = HashMap::new();
        let mut frame_shown: HashMap<u64, bool> = HashMap::new();
        let mut hidden = false;
        let mut unattended = false;

        for op in ops {
            match op {
                SuspendOp::Unattended(on) => {
                    unattended = on;
                    fix.ewm().set_unattended(on);
                }
                SuspendOp::Op(Op::ApplyLayout { frames: mut specs }) => {
                    for entry in specs.iter_mut().flat_map(|spec| &mut spec.entries) {
                        if let Some(index) =
                            APP_ENTRY_SIDS.iter().position(|&sid| sid == entry.surface_id)
                        {
                            entry.surface_id = app_windows[index].1;
                        }
                    }
                    apply_op(&mut fix, &mut frames, Op::ApplyLayout { frames: specs });
                }
                SuspendOp::Op(op) => apply_op(&mut fix, &mut frames, op),
            }
            fix.ewm_ref().verify_invariants();
            fix.dispatch();
            fix.roundtrip(&mut frames.client);
            fix.roundtrip(&mut frames.apps);
            fix.roundtrip(&mut apps);
            // Proxies of different clients can share an id, so match each client's own.
            let frame_sids = frames.toplevels.values().map(|(top, sid)| (top, *sid));
            let launch_sids = frames
                .launches
                .iter()
                .map(|launch| (&launch.toplevel, launch.sid));
            for (client, toplevels) in [
                (&mut frames.client, frame_sids.collect::<Vec<_>>()),
                (&mut frames.apps, launch_sids.collect()),
                (&mut apps, app_windows.iter().map(|(top, sid)| (top, *sid)).collect()),
            ] {
                for event in client.drain_events() {
                    if let ClientEvent::ToplevelConfigure { toplevel, suspended, .. } = event
                        && let Some((_, sid)) =
                            toplevels.iter().find(|(top, _)| top.xdg_toplevel.id() == toplevel.id())
                    {
                        told.insert(*sid, suspended);
                    }
                }
            }

            for event in fix.drain_events() {
                match event {
                    Event::FrameShown { id } => {
                        frame_shown.insert(id, true);
                    }
                    Event::FrameHidden { id } => {
                        frame_shown.insert(id, false);
                    }
                    Event::Hide { hidden: now } => hidden = now,
                    _ => {}
                }
            }

            let ewm = fix.ewm_ref();
            let mut shown = HashSet::new();
            if !unattended && !hidden {
                for monitor in ewm.layout.monitors() {
                    let frames = monitor.strip.frames().iter().chain(monitor.floating.frames());
                    for frame in frames {
                        if frame_shown.get(&frame.surface_id) == Some(&true) {
                            shown.insert(frame.surface_id);
                            shown.extend(frame.entries.iter().filter_map(LayoutEntry::surface_id));
                        }
                    }
                }
            }
            for (&sid, surface) in &ewm.surfaces {
                if surface.placed {
                    prop_assert_eq!(
                        told.get(&sid).copied().unwrap_or(false),
                        !shown.contains(&sid),
                        "surface {} (emacs {})",
                        sid,
                        surface.emacs,
                    );
                }
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
    focused: Option<u64>,
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
            .set_focus_frame(setup.active_frame(i), FocusSource::Test, Notify::None);
    }
    fix.ewm().set_focus_frame(
        setup.initial_focused_frame(),
        FocusSource::Test,
        Notify::None,
    );
    fix
}

fn nav_run(setup: &NavSetup, dx: i32, dy: i32, cx: f64, cy: f64) -> NavResult {
    let mut fix = nav_build(setup);
    fix.warp_pointer(cx, cy);
    fix.ewm().focus_output_direction(dx, dy);
    NavResult {
        focused: fix.ewm_ref().focused_frame_id(),
        pointer: fix.ewm_ref().pointer_location(),
    }
}

fn nav_round_trip(setup: &NavSetup, dx: i32, dy: i32) -> (Option<u64>, Option<u64>) {
    let mut fix = nav_build(setup);
    fix.ewm().focus_output_direction(dx, dy);
    let after_a = fix.ewm_ref().focused_frame_id();
    fix.ewm().focus_output_direction(-dx, -dy);
    let after_b = fix.ewm_ref().focused_frame_id();
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
            "cursor-dependent: ({},{}) at ({},{}) -> {:?}, at ({},{}) -> {:?}",
            dx, dy, cx1, cy1, a.focused, cx2, cy2, b.focused,
        );

        prop_assert_eq!(a.pointer, (cx1, cy1), "compositor moved pointer during navigation");
        prop_assert_eq!(b.pointer, (cx2, cy2), "compositor moved pointer during navigation");

        let initial = setup.initial_focused_frame();
        if !matches!((dx, dy), (-1, 0) | (1, 0) | (0, -1) | (0, 1)) {
            prop_assert_eq!(
                a.focused, Some(initial),
                "non-axial direction ({},{}) must no-op",
                dx, dy,
            );
        }

        let actives = setup.all_active_frames();
        prop_assert!(
            a.focused.is_some_and(|id| actives.contains(&id)),
            "outcome {:?} is not the active frame of any output (actives: {:?})",
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
        if after_a != Some(initial) {
            prop_assert_eq!(
                after_b, Some(initial),
                "round-trip ({},{}) then ({},{}) from {} went {:?} then {:?}",
                dx, dy, -dx, -dy, initial, after_a, after_b,
            );
        }
    }
}
