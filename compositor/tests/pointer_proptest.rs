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
//!       origin per layout entry of `S`, computed per-view from each entry's
//!       own `fullscreen` flag: effective-fullscreen primary entries land at
//!       the output origin offset by `fullscreen_center_offset`, effective-
//!       fullscreen non-primaries pin to `output_geo.loc`, and non-fullscreen
//!       entries use `output_geo.loc + working_area.loc + frame_dx + entry.{x,y}`.
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
//! PP1 - Single popup placement per popup. For every popup parented to a
//!       toplevel S currently in `id_windows`, `collect_popup_placements`
//!       emits exactly one placement, anchored to S's focused-or-primary
//!       view origin.
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
//!       carries `(width, height)` equal to that entry's output size when
//!       the entry is `fullscreen=true`, else `(entry.w, entry.h)`.
//!
//! C2  - Output-membership parity. The set of output names the client
//!       believes the toplevel is on (running `wl_surface.enter` minus
//!       `leave`) equals `ewm.surface_outputs[sid]`.
//!
//! N1  - No hit-test masking. When `surface_under_point` returns the
//!       toplevel, the point lies inside some entry's visible rect:
//!       active-frame fullscreen primary covers the whole output,
//!       active-frame non-primary fullscreen pins to output.loc at its
//!       own size, and ordinary entries use their working-area rect at
//!       the frame's screen position. Inactive-frame fullscreens render
//!       off-screen so they don't capture hits.
//!
//! N4  - One fullscreen per strip. After every layout, no strip has more
//!       than one entry with `fullscreen=true`; the compositor must
//!       normalize multi-fullscreen layouts down.

use std::collections::{HashMap, HashSet};

use ewm_core::render::collect_popup_placements;
use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, Popup, TestClient, Toplevel};
use ewm_core::{fullscreen_center_offset, LayoutEntry, ViewId};
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
    /// Each bit `i` of `fullscreen_mask` corresponds to view `i`; the strategy
    /// can flag any subset (including multiple) so the compositor's
    /// one-fullscreen-per-strip rule can be exercised.
    ApplyLayout {
        output: usize,
        n_views: usize,
        fullscreen_mask: u32,
    },
    WarpPointer { x: i32, y: i32 },
    PointerButton { pressed: bool },
    CreatePopup { ax: i32, ay: i32 },
    /// Drive the client side of fullscreen: calls `set_fullscreen` /
    /// `unset_fullscreen` on the toplevel.
    SetClientFullscreen { on: bool },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..OUTPUTS.len()).prop_map(Op::AddOutput),
        (0..OUTPUTS.len()).prop_map(Op::RemoveOutput),
        ((0..OUTPUTS.len()), 0usize..=4, any::<u32>()).prop_map(
            |(output, n_views, fullscreen_mask)| Op::ApplyLayout {
                output,
                n_views,
                fullscreen_mask,
            },
        ),
        (-200i32..2200, -200i32..1300).prop_map(|(x, y)| Op::WarpPointer { x, y }),
        any::<bool>().prop_map(|pressed| Op::PointerButton { pressed }),
        (50i32..350, 50i32..350).prop_map(|(ax, ay)| Op::CreatePopup { ax, ay }),
        any::<bool>().prop_map(|on| Op::SetClientFullscreen { on }),
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
        Op::ApplyLayout {
            output,
            n_views,
            fullscreen_mask,
        } => {
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
                        fullscreen: (fullscreen_mask >> i) & 1 == 1,
                    }],
                    selected_view_id: Some(nz(1 + i as u64)),
                    move_anim: None,
                })
                .collect();
            h.fix.ewm().apply_output_layout(name, frames);
        }
        Op::SetClientFullscreen { on } => {
            if on {
                h.toplevel.xdg_toplevel.set_fullscreen(None);
            } else {
                h.toplevel.xdg_toplevel.unset_fullscreen();
            }
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
        .iter()
        .flat_map(|(name, strip)| strip.active_surfaces().map(move |e| (name.clone(), e)))
        .find(|(_, e)| e.surface_id == h.sid && e.primary);
    let Some((out_name, entry)) = primary else {
        return Ok(());
    };
    let expected = if entry.fullscreen {
        ewm.space
            .outputs()
            .find(|o| o.name() == out_name)
            .and_then(|o| ewm.space.output_geometry(o))
            .map(|g| (g.size.w, g.size.h))
    } else {
        Some((entry.w as i32, entry.h as i32))
    };
    if let Some(expected) = expected {
        prop_assert_eq!(
            h.last_configure,
            Some(expected),
            "C1: last toplevel configure was {:?}, expected {:?} (entry fullscreen={})",
            h.last_configure,
            expected,
            entry.fullscreen,
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
            if entry.surface_id != sid {
                continue;
            }
            // Geometry follows entry.fullscreen alone, with no AND against
            // the client's committed XDG state. Each view computes its origin
            // from its own flag, independent of sibling views.
            let origin = if entry.fullscreen && entry.primary {
                let window_size = ewm
                    .id_windows
                    .get(&entry.surface_id)
                    .map(|w| w.geometry().size)
                    .unwrap_or_default();
                let (ox, oy) = fullscreen_center_offset(window_size, output_geo.size);
                Point::from((output_geo.loc.x + ox, output_geo.loc.y + oy))
            } else if entry.fullscreen {
                Point::from((output_geo.loc.x, output_geo.loc.y))
            } else {
                let frame_dx = strip.screen_x_of_frame(frame_idx) as i32;
                Point::from((
                    output_geo.loc.x + working_area.loc.x + frame_dx + entry.x,
                    output_geo.loc.y + working_area.loc.y + entry.y,
                ))
            };
            expected.push(origin);
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

/// Visible rect for one entry. Returns `None` for inactive-frame fullscreen
/// entries, which render off-screen and can't be hit. Fullscreen primary
/// centers the window inside the output, so its hit rect is the centered
/// window rect — clicks in the letterbox fall through to other entries.
fn visible_entry_rect(
    ewm: &ewm_core::Ewm,
    output_geo: Rectangle<i32, Logical>,
    working_area: Rectangle<i32, Logical>,
    frame_dx: i32,
    in_active: bool,
    entry: &LayoutEntry,
) -> Option<Rectangle<i32, Logical>> {
    if entry.fullscreen {
        if !in_active {
            return None;
        }
        if entry.primary {
            let window_size = ewm
                .id_windows
                .get(&entry.surface_id)
                .map(|w| w.geometry().size)
                .unwrap_or_default();
            let (cx, cy) = fullscreen_center_offset(window_size, output_geo.size);
            Some(Rectangle::new(
                Point::from((output_geo.loc.x + cx, output_geo.loc.y + cy)),
                window_size,
            ))
        } else {
            Some(Rectangle::new(
                output_geo.loc,
                Size::from((entry.w as i32, entry.h as i32)),
            ))
        }
    } else {
        Some(Rectangle::new(
            Point::from((
                output_geo.loc.x + working_area.loc.x + frame_dx + entry.x,
                output_geo.loc.y + working_area.loc.y + entry.y,
            )),
            Size::from((entry.w as i32, entry.h as i32)),
        ))
    }
}

fn check_n1(h: &Harness, p: Point<f64, Logical>) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let Some((surf, _)) = ewm.surface_under_point(p) else {
        return Ok(());
    };
    let our_id = h.toplevel.surface.id().protocol_id();
    if surf.id().protocol_id() != our_id {
        return Ok(());
    }
    let p_int = Point::from((p.x as i32, p.y as i32));
    for (out_name, strip) in &ewm.output_strips {
        let Some(output) = ewm.space.outputs().find(|o| o.name() == *out_name) else {
            continue;
        };
        let Some(output_geo) = ewm.space.output_geometry(output) else {
            continue;
        };
        let working_area = ewm.get_working_area(output);
        let active_idx = strip.active_idx();
        for (frame_idx, entry) in strip.surfaces_with_frame() {
            if entry.surface_id != h.sid {
                continue;
            }
            let frame_dx = strip.screen_x_of_frame(frame_idx) as i32;
            let in_active = frame_idx == active_idx;
            let Some(rect) =
                visible_entry_rect(ewm, output_geo, working_area, frame_dx, in_active, entry)
            else {
                continue;
            };
            if rect.contains(p_int) {
                return Ok(());
            }
        }
    }
    Err(TestCaseError::fail(format!(
        "N1: surface_under_point({p:?}) returned our toplevel but no visible entry covers p",
    )))
}

fn check_n4(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    for (name, strip) in &ewm.output_strips {
        let count = strip.surfaces().filter(|e| e.fullscreen).count();
        prop_assert!(
            count <= 1,
            "N4: strip {} has {} fullscreen entries, expected at most 1",
            name,
            count,
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
        let has_anchor = ewm.focused_or_primary_view_origin(*sid).is_some();
        let placements = collect_popup_placements(ewm, world_rect());
        for (popup, _) in &popups {
            let popup_id = popup.wl_surface().id().protocol_id();
            let matching = placements
                .iter()
                .filter(|p| p.surface.id().protocol_id() == popup_id)
                .count();
            let expected = if has_anchor { 1 } else { 0 };
            prop_assert_eq!(
                matching,
                expected,
                "PP1 popup {} (parent sid={}): expected {} placement(s), got {}",
                popup_id,
                sid,
                expected,
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
    check_n4(h)?;
    check_pp1(h)?;
    check_pp2(h)?;
    check_c1(h)?;
    check_c2(h)?;
    match *last_op {
        Op::WarpPointer { x, y } => {
            let p = Point::from((x as f64, y as f64));
            check_l3(h, p)?;
            check_p1(h, &events, p)?;
            check_n1(h, p)?;
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
    apply_op(
        &mut h,
        &Op::ApplyLayout {
            output: 0,
            n_views: 2,
            fullscreen_mask: 0,
        },
    );
    let popup_op = Op::CreatePopup { ax: 100, ay: 100 };
    apply_op(&mut h, &popup_op);
    h.fix.roundtrip(&mut h.client);
    assert_invariants(&mut h, &popup_op).expect("smoke invariants hold");
}
