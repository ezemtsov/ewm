//! Property tests for pointer routing and layout invariants.
//!
//! Each numbered invariant is checked by `assert_invariants` after every op
//! in a random `Op` sequence. New invariants are appended; old numbers are
//! never reused. Mirrors `focus_proptest`'s shape.
//!
//! L1  - Surface-index parity. `ewm.surface_outputs` equals the set of
//!       `(surface_id, output_name)` pairs derived from
//!       `output_strips[output].surface_entries()`.
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
//! PP2 - Popup placement/render-order parity. The centre of every visible
//!       popup placement, whether parented to an embedded app surface or an
//!       Emacs frame, hits *some* owned popup surface, never the parent or an
//!       underlying layout surface. Strict "hits its own surface" doesn't hold
//!       when popups overlap; the topmost rendered popup wins.
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
//! N4  - One fullscreen per frame. After every layout, no frame within a
//!       strip has more than one entry with `fullscreen=true`; the
//!       compositor normalizes multi-fullscreen frames down. Different
//!       frames in the same strip can each be fullscreen independently.
//!
//! F1  - FFM sees plain entries. A pointer move inside an Emacs frame whose
//!       layout entries have no attached Wayland surfaces still resolves the
//!       entry entry id and focuses that entry, while keyboard routing remains
//!       on the frame surface.

use std::collections::{HashMap, HashSet};

use ewm_core::render::collect_popup_placements;
use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, Popup, TestClient, Toplevel};
use ewm_core::{fullscreen_center_offset, LayoutEntry, LayoutEntryId};
use proptest::prelude::*;
use smithay::desktop::PopupManager;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface as ServerWlSurface;
use smithay::reexports::wayland_server::Resource as _;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::seat::WaylandFocus;
use wayland_client::Proxy as _;

/// `BTN_LEFT` from linux/input-event-codes.h.
const BTN_LEFT: u32 = 0x110;

const OUTPUTS: [(&str, i32, i32); 2] = [("HEAD-A", 1920, 1080), ("HEAD-B", 1920, 1080)];
const VIEW_W: i32 = 400;
const VIEW_H: i32 = 400;

fn nz(n: u64) -> LayoutEntryId {
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SurfaceKey {
    client: ClientId,
    protocol_id: u32,
}

fn client_surface_key(
    client: &TestClient,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) -> SurfaceKey {
    SurfaceKey {
        client: client.server_client_id(),
        protocol_id: surface.id().protocol_id(),
    }
}

fn server_surface_key(ewm: &ewm_core::Ewm, surface: &ServerWlSurface) -> Option<SurfaceKey> {
    Some(SurfaceKey {
        client: ewm.display_handle.get_client(surface.id()).ok()?.id(),
        protocol_id: surface.id().protocol_id(),
    })
}

fn server_hit_key(ewm: &ewm_core::Ewm, hit: Option<&ServerWlSurface>) -> Option<SurfaceKey> {
    hit.and_then(|surface| server_surface_key(ewm, surface))
}

#[derive(Debug, Clone)]
enum Op {
    AddOutput(usize),
    RemoveOutput(usize),
    /// Each bit `i` of `fullscreen_mask` corresponds to view `i`; the strategy
    /// can flag any subset across frames.
    ApplyLayout {
        output: usize,
        n_views: usize,
        fullscreen_mask: u32,
    },
    ApplyPlainLayout {
        output: usize,
        n_entries: usize,
    },
    WarpPointer {
        x: i32,
        y: i32,
    },
    PointerButton {
        pressed: bool,
    },
    CreatePopup {
        ax: i32,
        ay: i32,
    },
    CreateFramePopup {
        ax: i32,
        ay: i32,
    },
    /// Drive the client side of fullscreen: calls `set_fullscreen` /
    /// `unset_fullscreen` on the toplevel.
    SetClientFullscreen {
        on: bool,
    },
    SetFocusFollowsMouse {
        on: bool,
    },
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
        ((0..OUTPUTS.len()), 1usize..=4)
            .prop_map(|(output, n_entries)| { Op::ApplyPlainLayout { output, n_entries } }),
        (-200i32..2200, -200i32..1300).prop_map(|(x, y)| Op::WarpPointer { x, y }),
        any::<bool>().prop_map(|pressed| Op::PointerButton { pressed }),
        (50i32..350, 50i32..350).prop_map(|(ax, ay)| Op::CreatePopup { ax, ay }),
        (50i32..350, 50i32..350).prop_map(|(ax, ay)| Op::CreateFramePopup { ax, ay }),
        any::<bool>().prop_map(|on| Op::SetClientFullscreen { on }),
        any::<bool>().prop_map(|on| Op::SetFocusFollowsMouse { on }),
    ]
}

struct Harness {
    fix: Fixture,
    client: TestClient,
    toplevel: Toplevel,
    sid: u64,
    app_popups: Vec<Popup>,
    frame_client: TestClient,
    frame_toplevel: Toplevel,
    frame_sid: u64,
    frame_popups: Vec<Popup>,
    /// Latest `xdg_toplevel.configure` size the client has been told, accumulated
    /// across drain windows. `None` until the first configure arrives.
    last_configure: Option<(i32, i32)>,
    /// Output names the client believes the toplevel is on, accumulated from
    /// `wl_surface.enter` / `leave` events.
    toplevel_on_outputs: HashSet<String>,
    pointer_entry_id_before_warp: Option<LayoutEntryId>,
    pointer_surface_id_before_warp: Option<u64>,
}

impl Harness {
    fn new() -> Self {
        let mut fix = Fixture::new().expect("fixture");
        let mut client = fix.new_client().expect("client");
        fix.roundtrip(&mut client);
        let toplevel = fix.open_toplevel(&mut client, "ewm-test-toplevel", view_size());
        let sid = fix
            .find_surface_id(&client, &toplevel.surface)
            .expect("server did not assign a surface_id");

        let mut frame_client = fix.new_client().expect("frame client");
        fix.roundtrip(&mut frame_client);
        let frame_toplevel = fix.open_toplevel(&mut frame_client, "ewm-test-frame", view_size());
        let frame_sid = fix
            .find_surface_id(&frame_client, &frame_toplevel.surface)
            .expect("server did not assign a frame surface_id");

        let mut h = Harness {
            fix,
            client,
            toplevel,
            sid,
            app_popups: Vec::new(),
            frame_client,
            frame_toplevel,
            frame_sid,
            frame_popups: Vec::new(),
            last_configure: None,
            toplevel_on_outputs: HashSet::new(),
            pointer_entry_id_before_warp: None,
            pointer_surface_id_before_warp: None,
        };
        // Drain setup events into the harness so the first op starts clean.
        let setup_events = h.client.drain_events();
        h.fold_events(&setup_events);
        h.frame_client.drain_events();
        h
    }

    fn roundtrip_all(&mut self) {
        self.fix.roundtrip(&mut self.client);
        self.fix.roundtrip(&mut self.frame_client);
        self.frame_client.drain_events();
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
        self.fix.ewm_ref().output_strips.values().any(|strip| {
            strip
                .surface_entries()
                .any(|entry| entry.surface_id() == Some(self.sid))
        })
    }

    fn frame_in_some_strip(&self) -> bool {
        self.fix.ewm_ref().output_strips.values().any(|strip| {
            strip
                .frames
                .iter()
                .any(|frame| frame.surface_id == self.frame_sid)
        })
    }

    /// Surfaces the app client owns (toplevel + app popups).
    fn our_surface_keys(&self) -> HashSet<SurfaceKey> {
        let mut s = HashSet::new();
        s.insert(client_surface_key(&self.client, &self.toplevel.surface));
        for p in &self.app_popups {
            s.insert(client_surface_key(&self.client, &p.surface));
        }
        s
    }

    fn popup_surface_keys(&self) -> HashSet<SurfaceKey> {
        self.app_popups
            .iter()
            .map(|p| client_surface_key(&self.client, &p.surface))
            .chain(
                self.frame_popups
                    .iter()
                    .map(|p| client_surface_key(&self.frame_client, &p.surface)),
            )
            .collect()
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
                    surface_id: if i == 0 { h.frame_sid } else { 100 + i as u64 },
                    focus_id: nz(100 + i as u64),
                    width: VIEW_W as f64,
                    entries: vec![LayoutEntry::surface(
                        nz(1 + i as u64),
                        "entry".to_string(),
                        h.sid,
                        0,
                        0,
                        VIEW_W as u32,
                        VIEW_H as u32,
                        (fullscreen_mask >> i) & 1 == 1,
                    )],
                    selected_entry_id: Some(nz(1 + i as u64)),
                    move_anim: None,
                })
                .collect();
            h.fix.ewm().apply_output_layout(name, frames);
        }
        Op::ApplyPlainLayout { output, n_entries } => {
            apply_plain_entry_layout(h, output, n_entries);
        }
        Op::SetClientFullscreen { on } => {
            if on {
                h.toplevel.xdg_toplevel.set_fullscreen(None);
            } else {
                h.toplevel.xdg_toplevel.unset_fullscreen();
            }
        }
        Op::SetFocusFollowsMouse { on } => {
            h.fix.ewm().focus_follows_mouse_mode = on;
        }
        Op::WarpPointer { x, y } => {
            let (entry_id, surface_id) = {
                let ewm = h.fix.ewm_ref();
                let surface_id = ewm
                    .pointer_focus
                    .as_ref()
                    .and_then(|(surface, _)| ewm.surface_id(surface));
                (ewm.pointer_entry_id, surface_id)
            };
            h.pointer_entry_id_before_warp = entry_id;
            h.pointer_surface_id_before_warp = surface_id;
            h.fix.pointer_motion_to(x as f64, y as f64);
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
            h.app_popups.push(popup);
        }
        Op::CreateFramePopup { ax, ay } => {
            // Skip when the frame isn't placed; unconstrain would zero-clamp.
            if !h.frame_in_some_strip() {
                return;
            }
            let anchor = Rectangle::new(Point::from((ax, ay)), Size::from((1, 1)));
            let popup = h.fix.open_popup(
                &mut h.frame_client,
                &h.frame_toplevel,
                anchor,
                Size::from((100, 100)),
            );
            h.frame_popups.push(popup);
        }
    }
}

fn apply_multi_entry_layout(
    h: &mut Harness,
    output: usize,
    n_entries: usize,
    fullscreen_mask: u32,
) {
    let name = OUTPUTS[output].0;
    if !h.fix.has_output(name) {
        return;
    }
    let entries = (0..n_entries)
        .map(|i| {
            LayoutEntry::surface(
                nz(10_000 + i as u64),
                "entry".to_string(),
                h.sid,
                (i as i32) * 10,
                (i as i32) * 10,
                VIEW_W as u32,
                VIEW_H as u32,
                (fullscreen_mask >> i) & 1 == 1,
            )
        })
        .collect::<Vec<_>>();
    let selected_entry_id = entries.first().map(|entry| entry.id);
    h.fix.ewm().apply_output_layout(
        name,
        vec![Frame {
            name: String::new(),
            surface_id: 10_000,
            focus_id: nz(20_000),
            width: VIEW_W as f64,
            entries,
            selected_entry_id,
            move_anim: None,
        }],
    );
}

fn plain_entry_entry_id(i: usize) -> LayoutEntryId {
    nz(30_000 + i as u64)
}

fn plain_entry_left(n_entries: usize, i: usize) -> i32 {
    (i as i32 * VIEW_W) / n_entries as i32
}

fn apply_plain_entry_layout(h: &mut Harness, output: usize, n_entries: usize) {
    let name = OUTPUTS[output].0;
    if !h.fix.has_output(name) {
        return;
    }
    let entries = (0..n_entries)
        .map(|i| {
            let x = plain_entry_left(n_entries, i);
            let next_x = plain_entry_left(n_entries, i + 1);
            let entry_id = plain_entry_entry_id(i);
            LayoutEntry::emacs_window(
                entry_id,
                "entry".to_string(),
                x,
                0,
                (next_x - x) as u32,
                VIEW_H as u32,
            )
        })
        .collect::<Vec<_>>();
    h.fix.ewm().apply_output_layout(
        name,
        vec![Frame {
            name: String::new(),
            surface_id: h.frame_sid,
            focus_id: nz(40_000),
            width: VIEW_W as f64,
            entries,
            selected_entry_id: None,
            move_anim: None,
        }],
    );
    settle_strip_animations(h);
}

fn settle_strip_animations(h: &mut Harness) {
    h.fix.ewm().animations_clock.set_complete_instantly(true);
    for strip in h.fix.ewm().output_strips.values_mut() {
        strip.advance();
    }
}

fn check_l1(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let mut from_strips: HashMap<u64, HashSet<String>> = HashMap::new();
    for (out_name, strip) in &ewm.output_strips {
        for surface_id in strip.surface_entries().filter_map(LayoutEntry::surface_id) {
            from_strips
                .entry(surface_id)
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
        .flat_map(|(name, strip)| {
            strip
                .active_surface_entries()
                .map(move |e| (name.clone(), e))
        })
        .find(|(_, e)| e.surface_id() == Some(h.sid) && e.primary());
    let Some((out_name, entry)) = primary else {
        return Ok(());
    };
    let expected = if entry.fullscreen() {
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
            entry.fullscreen(),
        );
    }
    Ok(())
}

fn check_c2(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let server: HashSet<String> = ewm.surface_outputs.get(&h.sid).cloned().unwrap_or_default();
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
        for (frame_idx, entry) in strip.surface_entries_with_frame() {
            if entry.surface_id() != Some(sid) {
                continue;
            }
            // Geometry follows entry.fullscreen alone, with no AND against
            // the client's committed XDG state. Each view computes its origin
            // from its own flag, independent of sibling views.
            let origin = if entry.fullscreen() && entry.primary() {
                let window_size = ewm
                    .id_windows
                    .get(&entry.surface_id().expect("surface-backed entry"))
                    .map(|w| w.geometry().size)
                    .unwrap_or_default();
                let (ox, oy) = fullscreen_center_offset(window_size, output_geo.size);
                Point::from((output_geo.loc.x + ox, output_geo.loc.y + oy))
            } else if entry.fullscreen() {
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
    let hit_id = server_hit_key(ewm, hit.as_ref().map(|(s, _)| s));
    let focus_id = server_hit_key(ewm, focus.map(|(s, _)| s));
    prop_assert_eq!(
        focus_id.as_ref(),
        hit_id.as_ref(),
        "L3 pointer_focus mismatch at {:?}: focus={:?}, hit={:?}",
        p,
        focus_id,
        hit_id,
    );
    Ok(())
}

fn rect_contains_f64(rect: Rectangle<i32, Logical>, p: Point<f64, Logical>) -> bool {
    p.x >= rect.loc.x as f64
        && p.y >= rect.loc.y as f64
        && p.x < (rect.loc.x + rect.size.w) as f64
        && p.y < (rect.loc.y + rect.size.h) as f64
}

fn plain_entry_under(h: &Harness, p: Point<f64, Logical>) -> Option<(u64, LayoutEntryId)> {
    let ewm = h.fix.ewm_ref();
    for output in ewm.space.outputs() {
        let Some(output_geo) = ewm.space.output_geometry(output) else {
            continue;
        };
        if !rect_contains_f64(output_geo, p) {
            continue;
        }

        let output_name = output.name();
        let Some(strip) = ewm.output_strips.get(&output_name) else {
            continue;
        };
        let working_area = ewm.get_working_area(output);
        for (frame_idx, frame) in strip.frames.iter().enumerate() {
            let frame_dx = strip.screen_x_of_frame(frame_idx) as i32;
            for entry in &frame.entries {
                if entry.surface_id().is_some() {
                    continue;
                }
                let rect = Rectangle::new(
                    Point::from((
                        output_geo.loc.x + working_area.loc.x + frame_dx + entry.x,
                        output_geo.loc.y + working_area.loc.y + entry.y,
                    )),
                    Size::from((entry.w as i32, entry.h as i32)),
                );
                if rect_contains_f64(rect, p) {
                    return Some((frame.surface_id, entry.id));
                }
            }
        }
    }
    None
}

fn check_f1(h: &Harness, p: Point<f64, Logical>) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    if !ewm.focus_follows_mouse_mode || ewm.is_strip_animating_at(p) {
        return Ok(());
    }

    let Some((frame_surface_id, entry_id)) = plain_entry_under(h, p) else {
        return Ok(());
    };
    let pointer_focus_id = ewm
        .pointer_focus
        .as_ref()
        .and_then(|(surface, _)| ewm.surface_id(surface));
    let entry_changed = h.pointer_entry_id_before_warp != Some(entry_id);
    let surface_changed = h.pointer_surface_id_before_warp != Some(frame_surface_id);

    prop_assert_eq!(
        ewm.layout_entry_id_under(p),
        Some(entry_id),
        "F1: plain entry under {:?} must resolve through layout_entry_id_under",
        p,
    );
    prop_assert_eq!(
        ewm.pointer_entry_id,
        Some(entry_id),
        "F1: pointer_entry_id should track the plain entry under {:?}",
        p,
    );
    prop_assert_eq!(
        pointer_focus_id,
        Some(frame_surface_id),
        "F1: pointer focus should remain on the Emacs frame surface",
    );
    if !entry_changed && !surface_changed {
        return Ok(());
    }

    prop_assert_eq!(
        ewm.focused_frame_id,
        frame_surface_id,
        "F1: focused frame should own the plain entry",
    );
    prop_assert_eq!(
        ewm.focused_entry_id,
        Some(entry_id),
        "F1: FFM should focus the plain entry entry id",
    );
    prop_assert_eq!(
        ewm.focused_surface_id(),
        frame_surface_id,
        "F1: keyboard routing should stay on the Emacs frame surface",
    );
    Ok(())
}

fn check_p1(
    h: &Harness,
    events: &[ClientEvent],
    p: Point<f64, Logical>,
) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    if ewm.is_strip_animating_at(p) {
        return Ok(());
    }

    let expected = server_hit_key(ewm, ewm.surface_under_point(p).as_ref().map(|(s, _)| s));
    let last_enter = events.iter().rev().find_map(|e| match e {
        ClientEvent::PointerEnter { surface, .. } => Some(client_surface_key(&h.client, surface)),
        _ => None,
    });
    // The client only sees an Enter when focus actually changed (smithay
    // suppresses re-enter on the same surface). If no Enter is in the drain
    // window, we have nothing to compare; only assert when one was delivered.
    if let Some(actual) = last_enter {
        prop_assert_eq!(
            Some(&actual),
            expected.as_ref(),
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
    let placements = collect_popup_placements(ewm, world_rect(), None);
    let popup_ids = h.popup_surface_keys();
    for p in &placements {
        let centre = Point::from((
            (p.location.x + p.size.w / 2) as f64,
            (p.location.y + p.size.h / 2) as f64,
        ));
        let hit = ewm.surface_under_point(centre);
        let hit_id = server_hit_key(ewm, hit.as_ref().map(|(s, _)| s));
        prop_assert!(
            hit_id.as_ref().is_some_and(|id| popup_ids.contains(id)),
            "PP2: popup centre {:?} hit {:?}, expected a popup (one of {:?})",
            centre,
            hit_id,
            popup_ids,
        );
    }
    Ok(())
}

fn check_p3(h: &Harness, events: &[ClientEvent], pressed: bool) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let focus_id = ewm
        .pointer_focus
        .as_ref()
        .and_then(|(s, _)| server_surface_key(ewm, s));
    let ours = h.our_surface_keys();
    let focused_on_ours = focus_id.as_ref().is_some_and(|id| ours.contains(id));
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
    if entry.fullscreen() {
        if !in_active {
            return None;
        }
        if entry.primary() {
            let window_size = ewm
                .id_windows
                .get(&entry.surface_id()?)
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
    let our_key = client_surface_key(&h.client, &h.toplevel.surface);
    if server_surface_key(ewm, &surf).as_ref() != Some(&our_key) {
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
        for (frame_idx, entry) in strip.surface_entries_with_frame() {
            if entry.surface_id() != Some(h.sid) {
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
        for (frame_idx, frame) in strip.frames.iter().enumerate() {
            let count = frame
                .entries
                .iter()
                .filter(|entry| entry.fullscreen())
                .count();
            prop_assert!(
                count <= 1,
                "N4: strip {} frame {} has {} fullscreen entries, expected at most 1",
                name,
                frame_idx,
                count,
            );
        }
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
        let has_anchor = ewm.window_global_position(window).is_some();
        let placements = collect_popup_placements(ewm, world_rect(), None);
        for (popup, _) in &popups {
            let popup_key = server_surface_key(ewm, popup.wl_surface());
            let matching = placements
                .iter()
                .filter(|p| server_surface_key(ewm, &p.surface) == popup_key)
                .count();
            let expected = if has_anchor { 1 } else { 0 };
            prop_assert_eq!(
                matching,
                expected,
                "PP1 popup {} (parent sid={}): expected {} placement(s), got {}",
                popup.wl_surface().id().protocol_id(),
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
            check_f1(h, p)?;
            check_n1(h, p)?;
        }
        Op::PointerButton { pressed } => check_p3(h, &events, pressed)?,
        _ => {}
    }
    Ok(())
}

#[test]
fn emacs_frame_popup_receives_hits_above_embedded_surface() {
    let mut h = Harness::new();
    apply_op(&mut h, &Op::AddOutput(0));
    h.roundtrip_all();
    apply_op(
        &mut h,
        &Op::ApplyLayout {
            output: 0,
            n_views: 1,
            fullscreen_mask: 0,
        },
    );
    h.roundtrip_all();
    apply_op(&mut h, &Op::CreateFramePopup { ax: 120, ay: 120 });
    h.roundtrip_all();

    let popup = h.frame_popups.last().expect("frame popup was created");
    let popup_key = client_surface_key(&h.frame_client, &popup.surface);
    let (centre, hit_key) = {
        let ewm = h.fix.ewm_ref();
        let placement = collect_popup_placements(ewm, world_rect(), None)
            .into_iter()
            .find(|p| server_surface_key(ewm, &p.surface).as_ref() == Some(&popup_key))
            .expect("frame popup placement");
        let centre = Point::from((
            (placement.location.x + placement.size.w / 2) as f64,
            (placement.location.y + placement.size.h / 2) as f64,
        ));
        let hit = ewm.surface_under_point(centre);
        (centre, server_hit_key(ewm, hit.as_ref().map(|(s, _)| s)))
    };

    assert_eq!(
        hit_key,
        Some(popup_key),
        "frame popup centre {centre:?} should hit the popup, not the embedded surface",
    );
}

#[cfg(feature = "testing-layer-shell")]
#[test]
fn layer_shell_popup_receives_placement_and_hit_testing() {
    let mut fix = Fixture::new().expect("fixture");
    fix.add_output(OUTPUTS[0].0, OUTPUTS[0].1, OUTPUTS[0].2);
    fix.dispatch();

    let mut client = fix.new_client().expect("client");
    fix.roundtrip(&mut client);
    let layer = fix.open_layer_surface(&mut client, "ewm-test-layer", Size::from((300, 30)));
    let popup = fix.open_layer_popup(
        &mut client,
        &layer,
        Rectangle::new(Point::from((20, 20)), Size::from((1, 1))),
        Size::from((100, 80)),
    );

    let popup_key = client_surface_key(&client, &popup.surface);
    let (centre, hit_key, without_layer_popup) = {
        let ewm = fix.ewm_ref();
        let output = ewm.sorted_outputs.first().expect("mapped output");
        let output_geo = ewm.space.output_geometry(output).expect("output geometry");

        let without_layer_popup = collect_popup_placements(ewm, output_geo, None)
            .iter()
            .any(|p| server_surface_key(ewm, &p.surface).as_ref() == Some(&popup_key));

        let placement = collect_popup_placements(ewm, output_geo, Some(output))
            .into_iter()
            .find(|p| server_surface_key(ewm, &p.surface).as_ref() == Some(&popup_key))
            .expect("layer popup placement");
        let centre = Point::from((
            (placement.location.x + placement.size.w / 2) as f64,
            (placement.location.y + placement.size.h / 2) as f64,
        ));
        let hit = ewm.surface_under_point(centre);
        let hit_key = server_hit_key(ewm, hit.as_ref().map(|(s, _)| s));
        (centre, hit_key, without_layer_popup)
    };

    assert!(
        !without_layer_popup,
        "layer popup should be excluded when no layer output is requested",
    );
    assert_eq!(
        hit_key.as_ref(),
        Some(&popup_key),
        "layer popup centre {centre:?} should hit the popup surface",
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 16,
        ..ProptestConfig::default()
    })]

    #[test]
    fn pointer_invariants_hold(ops in prop::collection::vec(op_strategy(), 1..10)) {
        let mut h = Harness::new();
        let prefix = [
            Op::AddOutput(0),
            Op::SetFocusFollowsMouse { on: true },
            Op::WarpPointer { x: -100, y: -100 },
            Op::ApplyPlainLayout {
                output: 0,
                n_entries: 2,
            },
            Op::WarpPointer {
                x: VIEW_W / 4,
                y: VIEW_H / 2,
            },
            Op::WarpPointer {
                x: VIEW_W * 3 / 4,
                y: VIEW_H / 2,
            },
        ];

        for op in prefix.iter().chain(ops.iter()) {
            apply_op(&mut h, op);
            h.roundtrip_all();
            assert_invariants(&mut h, op)?;
        }
    }

    #[test]
    fn multi_fullscreen_entries_are_normalized(
        output in 0usize..OUTPUTS.len(),
        n_entries in 2usize..=4,
    ) {
        let mut h = Harness::new();
        let add = Op::AddOutput(output);
        apply_op(&mut h, &add);
        h.roundtrip_all();

        apply_multi_entry_layout(&mut h, output, n_entries, (1 << n_entries) - 1);
        h.roundtrip_all();

        check_n4(&h)?;
    }
}
