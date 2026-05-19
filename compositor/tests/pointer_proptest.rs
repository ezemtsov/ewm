//! Property tests for pointer routing and layout invariants.
//!
//! Each numbered invariant is checked by `assert_invariants` after every op
//! in a random `Op` sequence. New invariants are appended; old numbers are
//! never reused. Mirrors `focus_proptest`'s shape.
//!
//! L1  - Surface-index parity. `ewm.surface_outputs` equals the set of
//!       `(surface_id, output_name)` pairs derived from
//!       `output_strips[output].surfaces()`.
//!
//! L2  - View origin formula. `surface_view_origins(S)` lists exactly one
//!       origin per non-fullscreen layout entry of `S`, each equal to
//!       `output_geo.loc + working_area.loc + frame_dx + entry.{x,y}`, in
//!       the strip / frame iteration order.
//!
//! L3  - Pointer focus matches hit-test. After every `WarpPointer(p)` op,
//!       `ewm.pointer_focus.map(|(s, _)| s)` equals
//!       `ewm.surface_under_point(p).map(|(s, _)| s)`.
//!
//! P1  - Enter target. After every `WarpPointer(p)` op followed by a
//!       roundtrip, the most recent `PointerEnter` event the client
//!       received names the surface returned by `surface_under_point(p)`
//!       (or no enter was delivered, when focus didn't change).
//!
//! PP1 - Multi-view popup placement count. For every popup parented to a
//!       toplevel S currently in `id_windows`, `collect_popup_placements`
//!       emits exactly `count(views_of(S))` placements per popup, with
//!       `surface == popup.wl_surface()`.
//!
//! PP2 - Popup placement is hit-testable. The centre of every popup
//!       placement hits *some* popup surface (one of our `popups` set),
//!       never the parent toplevel. Strict "hits its own surface" doesn't
//!       hold when popups overlap; the topmost wins per `popups_for_surface`
//!       iteration order.
//!
//! P3  - Button delivery. After a `PointerButton(pressed)` op + roundtrip,
//!       the client receives a `PointerButton` event with the same
//!       `pressed` value iff `ewm.pointer_focus` is on a surface owned by
//!       this client.
//!
//! C1  - Configure tracks primary. If the toplevel has a primary entry,
//!       the most recent `xdg_toplevel.configure` the client received
//!       carries `(width, height)` equal to that entry's
//!       `(entry.w, entry.h)`.
//!
//! C2  - Output-membership parity. The set of output names the client
//!       believes the toplevel is on (running `wl_surface.enter` minus
//!       `leave`) equals `ewm.surface_outputs[sid]`.

use std::collections::{HashMap, HashSet};

use ewm_core::render::collect_popup_placements;
use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, Popup, TestClient, Toplevel};
use ewm_core::{LayoutEntry, ViewId};
use proptest::prelude::*;
use smithay::desktop::PopupManager;
use smithay::reexports::wayland_server::Resource as _;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::seat::WaylandFocus;
use wayland_client::Proxy as _;

/// `BTN_LEFT` from linux/input-event-codes.h.
const BTN_LEFT: u32 = 0x110;

const OUTPUTS: [(&str, i32, i32); 2] = [("HEAD-A", 1920, 1080), ("HEAD-B", 1920, 1080)];
const VIEW_W: i32 = 400;
const VIEW_H: i32 = 400;

fn nz(n: u64) -> ViewId {
    std::num::NonZeroU64::new(n).unwrap()
}

fn view_size() -> Size<i32, Logical> {
    Size::from((VIEW_W, VIEW_H))
}

/// Wide enough to overlap every output in `OUTPUTS`; used by
/// `collect_popup_placements`.
fn world_rect() -> Rectangle<i32, Logical> {
    Rectangle::new(Point::from((-100, -100)), Size::from((5000, 5000)))
}

#[derive(Debug, Clone)]
enum Op {
    AddOutput(usize),
    RemoveOutput(usize),
    ApplyLayout { output: usize, n_views: usize },
    WarpPointer { x: i32, y: i32 },
    PointerButton { pressed: bool },
    CreatePopup { ax: i32, ay: i32 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..OUTPUTS.len()).prop_map(Op::AddOutput),
        (0..OUTPUTS.len()).prop_map(Op::RemoveOutput),
        ((0..OUTPUTS.len()), 0usize..=4)
            .prop_map(|(output, n_views)| Op::ApplyLayout { output, n_views }),
        (-200i32..2200, -200i32..1300).prop_map(|(x, y)| Op::WarpPointer { x, y }),
        any::<bool>().prop_map(|pressed| Op::PointerButton { pressed }),
        (50i32..350, 50i32..350).prop_map(|(ax, ay)| Op::CreatePopup { ax, ay }),
    ]
}

struct Harness {
    fix: Fixture,
    client: TestClient,
    toplevel: Toplevel,
    sid: u64,
    popups: Vec<Popup>,
    /// Latest `xdg_toplevel.configure` size the client has been told, accumulated
    /// across drain windows. `None` until the first configure arrives.
    last_configure: Option<(i32, i32)>,
    /// Output names the client believes the toplevel is on, accumulated from
    /// `wl_surface.enter` / `leave` events.
    toplevel_on_outputs: HashSet<String>,
}

impl Harness {
    fn new() -> Self {
        let mut fix = Fixture::new().expect("fixture");
        let mut client = fix.new_client().expect("client");
        fix.roundtrip(&mut client);
        let toplevel = fix.open_toplevel(&mut client, "ewm-test-toplevel", view_size());
        let sid = fix
            .find_surface_id(&toplevel.surface)
            .expect("server did not assign a surface_id");
        let mut h = Harness {
            fix,
            client,
            toplevel,
            sid,
            popups: Vec::new(),
            last_configure: None,
            toplevel_on_outputs: HashSet::new(),
        };
        // Drain setup events into the harness so the first op starts clean.
        let setup_events = h.client.drain_events();
        h.fold_events(&setup_events);
        h
    }

    /// Apply enter/leave/configure events from a drain window to harness state.
    fn fold_events(&mut self, events: &[ClientEvent]) {
        let toplevel_id = self.toplevel.surface.id().protocol_id();
        for ev in events {
            match ev {
                ClientEvent::ToplevelConfigure { width, height } => {
                    self.last_configure = Some((*width, *height));
                }
                ClientEvent::SurfaceEnter { surface, output }
                    if surface.id().protocol_id() == toplevel_id =>
                {
                    if let Some(name) = self.client.output_name(output.id().protocol_id()) {
                        self.toplevel_on_outputs.insert(name.to_string());
                    }
                }
                ClientEvent::SurfaceLeave { surface, output }
                    if surface.id().protocol_id() == toplevel_id =>
                {
                    if let Some(name) = self.client.output_name(output.id().protocol_id()) {
                        self.toplevel_on_outputs.remove(name);
                    }
                }
                _ => {}
            }
        }
    }

    fn toplevel_in_some_strip(&self) -> bool {
        self.fix
            .ewm_ref()
            .output_strips
            .values()
            .any(|s| s.surfaces().any(|e| e.surface_id == self.sid))
    }

    /// Protocol ids of surfaces this client owns (toplevel + popups).
    fn our_surface_ids(&self) -> HashSet<u32> {
        let mut s = HashSet::new();
        s.insert(self.toplevel.surface.id().protocol_id());
        for p in &self.popups {
            s.insert(p.surface.id().protocol_id());
        }
        s
    }
}

fn apply_op(h: &mut Harness, op: &Op) {
    match *op {
        Op::AddOutput(i) => {
            let (name, w, height) = OUTPUTS[i];
            if !h.fix.has_output(name) {
                h.fix.add_output(name, w, height);
            }
        }
        Op::RemoveOutput(i) => {
            let name = OUTPUTS[i].0;
            if h.fix.has_output(name) {
                h.fix.remove_output(name);
            }
        }
        Op::ApplyLayout { output, n_views } => {
            let name = OUTPUTS[output].0;
            if !h.fix.has_output(name) {
                return;
            }
            let frames: Vec<Frame> = (0..n_views)
                .map(|i| Frame {
                    name: String::new(),
                    surface_id: 100 + i as u64,
                    view_id: nz(100 + i as u64),
                    width: VIEW_W as f64,
                    surfaces: vec![LayoutEntry {
                        view_id: nz(1 + i as u64),
                        surface_id: h.sid,
                        x: 0,
                        y: 0,
                        w: VIEW_W as u32,
                        h: VIEW_H as u32,
                        primary: true,
                        fullscreen: false,
                    }],
                    selected_view_id: Some(nz(1 + i as u64)),
                    move_anim: None,
                })
                .collect();
            h.fix.ewm().apply_output_layout(name, frames);
        }
        Op::WarpPointer { x, y } => {
            h.fix.warp_pointer(x as f64, y as f64);
        }
        Op::PointerButton { pressed } => {
            h.fix.pointer_button(BTN_LEFT, pressed);
        }
        Op::CreatePopup { ax, ay } => {
            // Skip when the parent isn't placed; unconstrain would zero-clamp.
            if !h.toplevel_in_some_strip() {
                return;
            }
            let anchor = Rectangle::new(Point::from((ax, ay)), Size::from((1, 1)));
            let popup =
                h.fix
                    .open_popup(&mut h.client, &h.toplevel, anchor, Size::from((100, 100)));
            h.popups.push(popup);
        }
    }
}

fn server_id(
    s: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
) -> Option<u32> {
    s.map(|s| s.id().protocol_id())
}

fn check_l1(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let mut from_strips: HashMap<u64, HashSet<String>> = HashMap::new();
    for (out_name, strip) in &ewm.output_strips {
        for entry in strip.surfaces() {
            from_strips
                .entry(entry.surface_id)
                .or_default()
                .insert(out_name.clone());
        }
    }
    prop_assert_eq!(
        from_strips,
        ewm.surface_outputs.clone(),
        "L1: surface_outputs out of sync with output_strips",
    );
    Ok(())
}

fn check_c1(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let primary = ewm
        .output_strips
        .values()
        .flat_map(|s| s.active_surfaces())
        .find(|e| e.surface_id == h.sid && e.primary);
    if let Some(entry) = primary {
        let expected = (entry.w as i32, entry.h as i32);
        prop_assert_eq!(
            h.last_configure,
            Some(expected),
            "C1: last toplevel configure was {:?}, expected {:?} from primary entry",
            h.last_configure,
            expected,
        );
    }
    Ok(())
}

fn check_c2(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let server: HashSet<String> = ewm
        .surface_outputs
        .get(&h.sid)
        .cloned()
        .unwrap_or_default();
    prop_assert_eq!(
        h.toplevel_on_outputs.clone(),
        server.clone(),
        "C2: client output set {:?} != surface_outputs {:?}",
        h.toplevel_on_outputs,
        server,
    );
    Ok(())
}

fn check_l2(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let sid = h.sid;
    let mut expected: Vec<Point<i32, Logical>> = Vec::new();
    for (out_name, strip) in &ewm.output_strips {
        let Some(output) = ewm.space.outputs().find(|o| o.name() == *out_name) else {
            continue;
        };
        let Some(output_geo) = ewm.space.output_geometry(output) else {
            continue;
        };
        let working_area = ewm.get_working_area(output);
        for (frame_idx, entry) in strip.surfaces_with_frame() {
            if entry.surface_id != sid || entry.fullscreen {
                continue;
            }
            let frame_dx = strip.screen_x_of_frame(frame_idx) as i32;
            expected.push(Point::from((
                output_geo.loc.x + working_area.loc.x + frame_dx + entry.x,
                output_geo.loc.y + working_area.loc.y + entry.y,
            )));
        }
    }
    let actual = ewm.surface_view_origins(sid);
    prop_assert_eq!(
        actual.clone(),
        expected.clone(),
        "L2 surface_view_origins mismatch for sid {}: expected {:?}, got {:?}",
        sid,
        expected,
        actual,
    );
    Ok(())
}

fn check_l3(h: &Harness, p: Point<f64, Logical>) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let hit = ewm.surface_under_point(p);
    let focus = ewm.pointer_focus.as_ref();
    let hit_id = server_id(hit.as_ref().map(|(s, _)| s));
    let focus_id = server_id(focus.map(|(s, _)| s));
    prop_assert_eq!(
        focus_id,
        hit_id,
        "L3 pointer_focus mismatch at {:?}: focus={:?}, hit={:?}",
        p,
        focus_id,
        hit_id,
    );
    Ok(())
}

fn check_p1(
    h: &Harness,
    events: &[ClientEvent],
    p: Point<f64, Logical>,
) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let expected = server_id(ewm.surface_under_point(p).as_ref().map(|(s, _)| s));
    let last_enter = events.iter().rev().find_map(|e| match e {
        ClientEvent::PointerEnter { surface, .. } => Some(surface.id().protocol_id()),
        _ => None,
    });
    // The client only sees an Enter when focus actually changed (smithay
    // suppresses re-enter on the same surface). If no Enter is in the drain
    // window, we have nothing to compare; only assert when one was delivered.
    if let Some(actual) = last_enter {
        prop_assert_eq!(
            Some(actual),
            expected,
            "P1 enter target mismatch at {:?}: client got {:?}, hit-test {:?}",
            p,
            actual,
            expected,
        );
    }
    Ok(())
}

fn check_pp2(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let placements = collect_popup_placements(ewm, world_rect());
    let popup_ids: HashSet<u32> = h
        .popups
        .iter()
        .map(|p| p.surface.id().protocol_id())
        .collect();
    for p in &placements {
        let centre = Point::from((
            (p.location.x + p.size.w / 2) as f64,
            (p.location.y + p.size.h / 2) as f64,
        ));
        let hit = ewm.surface_under_point(centre);
        let hit_id = hit.as_ref().map(|(s, _)| s.id().protocol_id());
        prop_assert!(
            hit_id.is_some_and(|id| popup_ids.contains(&id)),
            "PP2: popup centre {:?} hit {:?}, expected a popup (one of {:?})",
            centre,
            hit_id,
            popup_ids,
        );
    }
    Ok(())
}

fn check_p3(
    h: &Harness,
    events: &[ClientEvent],
    pressed: bool,
) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let focus_id = ewm
        .pointer_focus
        .as_ref()
        .map(|(s, _)| s.id().protocol_id());
    let ours = h.our_surface_ids();
    let focused_on_ours = focus_id.is_some_and(|id| ours.contains(&id));
    let saw_button = events
        .iter()
        .any(|e| matches!(e, ClientEvent::PointerButton { button: BTN_LEFT, pressed: p } if *p == pressed));
    if focused_on_ours {
        prop_assert!(
            saw_button,
            "P3: focused on our surface ({:?}) but client received no PointerButton(pressed={})",
            focus_id,
            pressed,
        );
    } else {
        prop_assert!(
            !saw_button,
            "P3: focus is {:?} (not our surface) but client received PointerButton(pressed={})",
            focus_id,
            pressed,
        );
    }
    Ok(())
}

fn check_pp1(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    for (sid, window) in &ewm.id_windows {
        let Some(parent_surface) = window.wl_surface() else {
            continue;
        };
        let popups: Vec<_> = PopupManager::popups_for_surface(&parent_surface).collect();
        if popups.is_empty() {
            continue;
        }
        let views = ewm
            .output_strips
            .values()
            .flat_map(|s| s.surfaces())
            .filter(|e| e.surface_id == *sid)
            .count();
        let placements = collect_popup_placements(ewm, world_rect());
        for (popup, _) in &popups {
            let popup_id = popup.wl_surface().id().protocol_id();
            let matching = placements
                .iter()
                .filter(|p| p.surface.id().protocol_id() == popup_id)
                .count();
            prop_assert_eq!(
                matching,
                views,
                "PP1 popup {} (parent sid={}): expected {} placements, got {}",
                popup_id,
                sid,
                views,
                matching,
            );
        }
    }
    Ok(())
}

fn assert_invariants(h: &mut Harness, last_op: &Op) -> Result<(), TestCaseError> {
    let events = h.client.drain_events();
    h.fold_events(&events);
    check_l1(h)?;
    check_l2(h)?;
    check_pp1(h)?;
    check_pp2(h)?;
    check_c1(h)?;
    check_c2(h)?;
    match *last_op {
        Op::WarpPointer { x, y } => {
            let p = Point::from((x as f64, y as f64));
            check_l3(h, p)?;
            check_p1(h, &events, p)?;
        }
        Op::PointerButton { pressed } => check_p3(h, &events, pressed)?,
        _ => {}
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 16,
        ..ProptestConfig::default()
    })]

    #[test]
    fn pointer_invariants_hold(ops in prop::collection::vec(op_strategy(), 1..10)) {
        let mut h = Harness::new();
        for op in &ops {
            apply_op(&mut h, op);
            h.fix.roundtrip(&mut h.client);
            assert_invariants(&mut h, op)?;
        }
    }
}

/// Smoke test: build a fixture, apply a 2-view layout, attach a popup,
/// check invariants. Catches infra regressions before the proptest does.
#[test]
fn fixture_smoke() {
    let mut h = Harness::new();
    apply_op(&mut h, &Op::AddOutput(0));
    apply_op(&mut h, &Op::ApplyLayout { output: 0, n_views: 2 });
    let popup_op = Op::CreatePopup { ax: 100, ay: 100 };
    apply_op(&mut h, &popup_op);
    h.fix.roundtrip(&mut h.client);
    assert_invariants(&mut h, &popup_op).expect("smoke invariants hold");
}
