//! Property tests for pointer routing and layout invariants.
//!
//! Each numbered invariant is checked by `assert_invariants` after every op
//! in a random `Op` sequence. New invariants are appended; old numbers are
//! never reused. Mirrors `focus_proptest`'s shape.
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
    CreatePopup { ax: i32, ay: i32 },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..OUTPUTS.len()).prop_map(Op::AddOutput),
        (0..OUTPUTS.len()).prop_map(Op::RemoveOutput),
        ((0..OUTPUTS.len()), 0usize..=4)
            .prop_map(|(output, n_views)| Op::ApplyLayout { output, n_views }),
        (-200i32..2200, -200i32..1300).prop_map(|(x, y)| Op::WarpPointer { x, y }),
        (50i32..350, 50i32..350).prop_map(|(ax, ay)| Op::CreatePopup { ax, ay }),
    ]
}

struct Harness {
    fix: Fixture,
    client: TestClient,
    toplevel: Toplevel,
    sid: u64,
    popups: Vec<Popup>,
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
        Harness {
            fix,
            client,
            toplevel,
            sid,
            popups: Vec::new(),
        }
    }

    fn toplevel_in_some_strip(&self) -> bool {
        self.fix
            .ewm_ref()
            .output_strips
            .values()
            .any(|s| s.surfaces().any(|e| e.surface_id == self.sid))
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
        Op::CreatePopup { ax, ay } => {
            // unconstrain_popup falls back to output_size when the parent
            // isn't in any strip; skip those cases so the popup is sized.
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
    check_l2(h)?;
    check_pp1(h)?;
    let events = h.client.drain_events();
    if let Op::WarpPointer { x, y } = *last_op {
        let p = Point::from((x as f64, y as f64));
        check_l3(h, p)?;
        check_p1(h, &events, p)?;
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
