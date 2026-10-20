//! Property tests for pointer routing and layout invariants.
//!
//! Each numbered invariant is checked by `assert_invariants` after every op
//! in a random `Op` sequence. New invariants are appended; old numbers are
//! never reused. Mirrors `focus_proptest`'s shape. The oracles are what the
//! two clients see on the wire, the layout the harness applied and where the
//! strip draws a frame. L1, L2, L3, N1, N4, P1, P5 and PP1 are retired.
//!
//! E1  - Pointer focus is what the clients see. After every op, the surface a
//!       client holds pointer focus on, per its enter/leave stream, is the
//!       hit-test target under the pointer, and at most one client holds it.
//!       A click grab pins it where it was. A DnD grab speaks data_device,
//!       not wl_pointer, so E1 waits for the drop.
//!
//! E2  - Coordinates come from the drawn view. When the app toplevel holds
//!       pointer focus, the surface-local position it was last told puts the
//!       pointer where the drawn view says: `p - origin` for a primary view,
//!       which shows its buffer at native size, and inside the committed
//!       buffer for a scaled one.
//!
//! E3  - No hit-test masking. When exactly one drawn view contains `p` on its
//!       own output and no popup covers it, the hit is that view's surface.
//!       A frame draws at the strip's screen x and a view at its entry offset
//!       within it; a fullscreen view hides its frame's other views, and an
//!       inactive frame's fullscreen view is not drawn at all.
//!
//! D1  - No focus change mid-drag. A `WarpPointer` during a DnD grab leaves
//!       `focused_entry_id` untouched, whatever focus-follows-mouse says.
//!
//! PP2 - Popup placement/render-order parity. The centre of every visible
//!       popup placement, whether parented to an embedded app surface or an
//!       Emacs frame, hits *some* owned popup surface, never the parent or an
//!       underlying layout surface. Strict "hits its own surface" doesn't hold
//!       when popups overlap; the topmost rendered popup wins.
//!
//!
//! P3  - Button delivery. After a `PointerButton(pressed)` op + roundtrip,
//!       the client receives a `PointerButton` event with the same
//!       `pressed` value iff `ewm.pointer_focus` at button time was on a
//!       surface owned by this client. A release ending a grab may retarget
//!       focus afterwards; delivery follows the focus at button time.
//!
//!
//! P6  - Drag-source coordinates. After a `WarpPointer(p)` op while a click
//!       grab pins the frame's toplevel, the frame client receives motion at
//!       `p - origin`, unless drag-source is on and the surface under `p`
//!       belongs to another client: then it receives `-(p - origin) - 1`,
//!       both coordinates negative.
//!
//!
//! C1  - Configure tracks primary. If the toplevel has a primary entry,
//!       the most recent `xdg_toplevel.configure` the client received
//!       carries `(width, height)` equal to that entry's output size when
//!       the surface is requested fullscreen, else `(entry.w, entry.h)`.
//!
//!
//! C2  - Output-membership parity. The set of output names the client
//!       believes the toplevel is on (running `wl_surface.enter` minus
//!       `leave`) equals the outputs whose strips hold a frame or view of it.
//!
//! F1  - FFM sees plain entries. With focus-follows-mouse on and no grab, a
//!       warp that puts the frame client's pointer at frame-local `(fx, fy)`
//!       while the frame holds a plain layout lands on the entry whose rect
//!       spans it. When that entry or the focused surface changes, the entry
//!       and its frame take focus and keyboard routing stays on the frame
//!       surface.
//!
//! T1  - A press focuses what is under it. After a `Tap` or a button press at
//!       `p`, the layout entry under `p` when it landed becomes
//!       `focused_entry_id`. A press on nothing leaves focus untouched.

use std::collections::HashSet;

use ewm_core::layout::strip::Frame;
use ewm_core::render::collect_popup_placements;
use ewm_core::testing::{ClientEvent, Fixture, Popup, TestClient, Toplevel, nz};
use ewm_core::{LayoutEntry, LayoutEntryId};
use proptest::prelude::*;
use smithay::reexports::wayland_server::Resource as _;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface as ServerWlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use wayland_client::Proxy as _;

/// `BTN_LEFT` from linux/input-event-codes.h.
const BTN_LEFT: u32 = 0x110;

const OUTPUTS: [(&str, i32, i32); 2] = [("HEAD-A", 1920, 1080), ("HEAD-B", 1920, 1080)];
const VIEW_W: i32 = 400;
const VIEW_H: i32 = 400;

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
    /// Each set bit requests compositor-owned fullscreen for the corresponding view.
    ApplyLayout {
        output: usize,
        n_views: usize,
        fullscreen_mask: u32,
    },
    ApplyPlainLayout {
        output: usize,
        n_entries: usize,
    },
    ApplySplitClientLayout {
        output: usize,
    },
    WarpPointer {
        x: i32,
        y: i32,
    },
    ProgrammaticWarpPointer {
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
    SetDragSource {
        on: bool,
    },
    /// Start a DnD from the app toplevel; a no-op unless a button is held.
    StartDnd,
    /// Finger down and up at fractions of the touchscreen, which maps onto the first output.
    Tap {
        fx: f64,
        fy: f64,
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
        (0..OUTPUTS.len()).prop_map(|output| Op::ApplySplitClientLayout { output }),
        (-200i32..2200, -200i32..1300).prop_map(|(x, y)| Op::WarpPointer { x, y }),
        (-200i32..2200, -200i32..1300).prop_map(|(x, y)| Op::ProgrammaticWarpPointer { x, y }),
        any::<bool>().prop_map(|pressed| Op::PointerButton { pressed }),
        (50i32..350, 50i32..350).prop_map(|(ax, ay)| Op::CreatePopup { ax, ay }),
        (50i32..350, 50i32..350).prop_map(|(ax, ay)| Op::CreateFramePopup { ax, ay }),
        any::<bool>().prop_map(|on| Op::SetClientFullscreen { on }),
        any::<bool>().prop_map(|on| Op::SetFocusFollowsMouse { on }),
        any::<bool>().prop_map(|on| Op::SetDragSource { on }),
        Just(Op::StartDnd),
        (0.0f64..1.0, 0.0f64..1.0).prop_map(|(fx, fy)| Op::Tap { fx, fy }),
    ]
}

/// What one client has been told about the pointer.
#[derive(Default)]
struct ClientPointer {
    focus: Option<SurfaceKey>,
    local: (f64, f64),
    /// Whether an enter or motion arrived in the last drain window.
    fresh: bool,
}

impl ClientPointer {
    fn fold(&mut self, client: &TestClient, events: &[ClientEvent]) {
        self.fresh = false;
        for event in events {
            match event {
                ClientEvent::PointerEnter { surface, x, y } => {
                    self.focus = Some(client_surface_key(client, surface));
                    self.local = (*x, *y);
                    self.fresh = true;
                }
                ClientEvent::PointerLeave { .. } => self.focus = None,
                ClientEvent::PointerMotion { x, y } => {
                    self.local = (*x, *y);
                    self.fresh = true;
                }
                _ => {}
            }
        }
    }
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
    /// Frame client events from the last `roundtrip_all`.
    frame_events: Vec<ClientEvent>,
    app: ClientPointer,
    frame: ClientPointer,
    /// Latest `xdg_toplevel.configure` size the client has been told, accumulated
    /// across drain windows. `None` until the first configure arrives.
    last_configure: Option<(i32, i32)>,
    /// The configure the client last answered with a buffer.
    answered_configure: Option<(i32, i32)>,
    /// Output names the client believes the toplevel is on, accumulated from
    /// `wl_surface.enter` / `leave` events.
    toplevel_on_outputs: HashSet<String>,
    /// Pointer focus as the clients saw it before the last op.
    focus_before_op: Option<SurfaceKey>,
    dnd_active: bool,
    /// `focused_entry_id` before the last op.
    focused_entry_before_op: Option<LayoutEntryId>,
    /// Layout entry under the last press, sampled before it landed.
    entry_under_press: Option<LayoutEntryId>,
    /// Whether any surface was under the last press.
    hit_under_press: bool,
    /// The plain entry under the frame's pointer after the last op.
    plain_entry_under_pointer: Option<LayoutEntryId>,
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
            frame_events: Vec::new(),
            app: ClientPointer::default(),
            frame: ClientPointer::default(),
            last_configure: None,
            answered_configure: None,
            toplevel_on_outputs: HashSet::new(),
            focus_before_op: None,
            dnd_active: false,
            focused_entry_before_op: None,
            entry_under_press: None,
            hit_under_press: false,
            plain_entry_under_pointer: None,
        };
        // Animations land at once.
        h.fix.ewm().animations_clock.set_complete_instantly(true);
        // Drain setup events into the harness so the first op starts clean.
        let setup_events = h.client.drain_events();
        h.fold_events(&setup_events);
        h.frame_client.drain_events();
        h
    }

    fn roundtrip_all(&mut self) {
        settle_strip_animations(self);
        self.fix.roundtrip(&mut self.client);
        self.fix.roundtrip(&mut self.frame_client);
        self.frame_events = self.frame_client.drain_events();
        self.frame.fold(&self.frame_client, &self.frame_events);
    }

    /// The surface holding pointer focus, as the clients see it.
    fn union_focus(&self) -> Option<SurfaceKey> {
        self.app.focus.clone().or_else(|| self.frame.focus.clone())
    }

    fn pointer(&self) -> Point<f64, Logical> {
        Point::from(self.fix.ewm_ref().pointer_location())
    }

    fn toplevel_key(&self) -> SurfaceKey {
        client_surface_key(&self.client, &self.toplevel.surface)
    }

    fn frame_key(&self) -> SurfaceKey {
        client_surface_key(&self.frame_client, &self.frame_toplevel.surface)
    }

    /// Apply enter/leave/configure events from a drain window to harness state.
    fn fold_events(&mut self, events: &[ClientEvent]) {
        let toplevel_id = self.toplevel.surface.id().protocol_id();
        for ev in events {
            match ev {
                ClientEvent::ToplevelConfigure { width, height, .. } => {
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
            .layout
            .strips()
            .map(|(_, strip)| strip)
            .any(|strip| {
                strip
                    .surface_entries()
                    .any(|entry| entry.surface_id() == Some(self.sid))
            })
    }

    fn frame_in_some_strip(&self) -> bool {
        self.fix
            .ewm_ref()
            .layout
            .strips()
            .map(|(_, strip)| strip)
            .any(|strip| {
                strip
                    .frames()
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
    // The client answers its last configure first, so what its views show can follow.
    if h.last_configure != h.answered_configure
        && let Some((w, height)) = h.last_configure.filter(|&(w, height)| w > 0 && height > 0)
    {
        h.fix
            .map_toplevel(&mut h.client, &h.toplevel, Size::from((w, height)));
        h.answered_configure = h.last_configure;
        // The answer can move the pointer focus, and the clients learn of it before the op.
        h.roundtrip_all();
        let events = h.client.drain_events();
        h.fold_events(&events);
        h.app.fold(&h.client, &events);
    }
    h.focused_entry_before_op = h.fix.ewm_ref().focused_entry_id();
    h.focus_before_op = h.union_focus();
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
                    )],
                    selected_entry_id: Some(nz(1 + i as u64)),
                    ..Frame::new(if i == 0 { h.frame_sid } else { 100 + i as u64 })
                })
                .collect();
            h.fix.tile(name, frames);
            for i in 0..n_views {
                if fullscreen_mask & (1 << i) != 0 {
                    h.fix.set_fullscreen(nz(1 + i as u64), true);
                }
            }
        }
        Op::ApplyPlainLayout { output, n_entries } => {
            apply_plain_entry_layout(h, output, n_entries);
        }
        Op::ApplySplitClientLayout { output } => {
            apply_split_client_layout(h, output);
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
        Op::SetDragSource { on } => {
            h.fix.ewm().emacs_drag_source = on;
        }
        Op::WarpPointer { x, y } => {
            h.fix.pointer_motion_to(x as f64, y as f64);
        }
        Op::ProgrammaticWarpPointer { x, y } => {
            h.fix.warp_pointer(x as f64, y as f64);
        }
        Op::PointerButton { pressed } => {
            if pressed {
                note_press(h, h.pointer());
            }
            h.fix.pointer_button(BTN_LEFT, pressed);
            h.dnd_active &= h.fix.ewm_ref().pointer.is_grabbed();
        }
        Op::Tap { fx, fy } => {
            if let Some(p) = tap_point(h, fx, fy) {
                note_press(h, p);
            }
            h.fix.touch_down(fx, fy);
            h.fix.touch_up();
        }
        Op::StartDnd => {
            // Needs a held button; a second start supersedes the first grab.
            if h.fix.ewm_ref().pointer.is_grabbed() {
                h.fix.start_pointer_dnd(h.sid, None);
                h.dnd_active = true;
            }
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
    h.fix.tile(
        name,
        vec![Frame {
            focus_id: nz(40_000),
            width: VIEW_W as f64,
            entries,
            selected_entry_id: None,
            ..Frame::new(h.frame_sid)
        }],
    );
    settle_strip_animations(h);
}

fn apply_split_client_layout(h: &mut Harness, output: usize) {
    let name = OUTPUTS[output].0;
    if !h.fix.has_output(name) {
        return;
    }
    h.fix.tile(
        name,
        vec![Frame {
            focus_id: nz(50_000),
            width: (VIEW_W * 2) as f64,
            entries: vec![
                LayoutEntry::surface(nz(50_001), "first".to_string(), h.sid, 0, 0, 400, 400),
                LayoutEntry::surface(
                    nz(50_002),
                    "second".to_string(),
                    h.frame_sid,
                    500,
                    0,
                    400,
                    400,
                ),
            ],
            selected_entry_id: Some(nz(50_001)),
            ..Frame::new(h.frame_sid)
        }],
    );
}

fn settle_strip_animations(h: &mut Harness) {
    h.fix.ewm().animations_clock.set_complete_instantly(true);
    for strip in h.fix.ewm().layout.strips_mut() {
        strip.advance();
    }
}
/// A view where the strip draws it, or the compositor's rect for an active frame's fullscreen.
struct DrawnView {
    surface: u64,
    rect: Rectangle<i32, Logical>,
    output_geo: Rectangle<i32, Logical>,
    /// Shows its buffer at native size, so pointer coordinates are `p - rect.loc`.
    native: bool,
}

fn drawn_views(h: &Harness) -> Vec<DrawnView> {
    let ewm = h.fix.ewm_ref();
    let fullscreen_views = ewm.view_rects();
    let mut views = Vec::new();
    for (out_name, strip) in ewm.layout.strips() {
        let Some(output) = ewm.space.outputs().find(|o| o.name() == *out_name) else {
            continue;
        };
        let Some(output_geo) = ewm.space.output_geometry(output) else {
            continue;
        };
        let working_area = ewm.get_working_area(output);
        for (frame_idx, frame) in strip.frames().iter().enumerate() {
            let origin = Point::from((
                output_geo.loc.x
                    + working_area.loc.x
                    + strip.screen_x_of_frame(frame_idx).round() as i32,
                output_geo.loc.y + working_area.loc.y,
            ));
            match frame.fullscreen_entry() {
                Some(fullscreen) => {
                    if strip.active_idx() != frame_idx {
                        continue;
                    }
                    let Some(view) = fullscreen_views
                        .iter()
                        .find(|view| view.frame == frame.surface_id && view.entry == fullscreen.id)
                    else {
                        continue;
                    };
                    let mut rect = view.rect;
                    rect.loc += output_geo.loc;
                    views.push(DrawnView {
                        surface: view.surface,
                        rect,
                        output_geo,
                        native: fullscreen.primary(),
                    });
                }
                None => {
                    for entry in &frame.entries {
                        let Some(surface) = entry.surface_id() else {
                            continue;
                        };
                        views.push(DrawnView {
                            surface,
                            rect: Rectangle::new(
                                origin + Point::from((entry.x, entry.y)),
                                Size::from((entry.w as i32, entry.h as i32)),
                            ),
                            output_geo,
                            native: entry.primary(),
                        });
                    }
                }
            }
        }
    }
    views
}

/// The one drawn view under `p` on its own output, if exactly one is.
fn drawn_view_under(views: &[DrawnView], p: Point<f64, Logical>) -> Option<&DrawnView> {
    let mut under = views
        .iter()
        .filter(|view| rect_contains_f64(view.output_geo, p) && rect_contains_f64(view.rect, p));
    match (under.next(), under.next()) {
        (Some(view), None) => Some(view),
        _ => None,
    }
}

fn popup_covers(h: &Harness, p: Point<f64, Logical>) -> bool {
    collect_popup_placements(h.fix.ewm_ref(), world_rect(), None)
        .iter()
        .any(|placement| {
            rect_contains_f64(
                Rectangle::new(placement.location, placement.geometry.size),
                p,
            )
        })
}

fn check_e1(h: &Harness) -> Result<(), TestCaseError> {
    if h.dnd_active {
        return Ok(());
    }
    prop_assert!(
        h.app.focus.is_none() || h.frame.focus.is_none(),
        "E1: both clients hold pointer focus: app {:?}, frame {:?}",
        h.app.focus,
        h.frame.focus,
    );
    let focus = h.union_focus();
    let ewm = h.fix.ewm_ref();
    if ewm.pointer.is_grabbed() {
        prop_assert_eq!(
            focus.as_ref(),
            h.focus_before_op.as_ref(),
            "E1: a click grab must keep pointer focus where it was",
        );
        return Ok(());
    }
    let p = h.pointer();
    let hit = server_hit_key(ewm, ewm.surface_under_point(p).as_ref().map(|(s, _)| s));
    prop_assert_eq!(
        focus.as_ref(),
        hit.as_ref(),
        "E1: clients see pointer focus {:?} at {:?}, hit-test says {:?}",
        focus,
        p,
        hit,
    );
    Ok(())
}

fn check_e2(h: &Harness) -> Result<(), TestCaseError> {
    if !h.app.fresh || h.app.focus.as_ref() != Some(&h.toplevel_key()) {
        return Ok(());
    }
    let p = h.pointer();
    let views = drawn_views(h);
    let Some(view) = drawn_view_under(&views, p).filter(|view| view.surface == h.sid) else {
        return Ok(());
    };
    let (lx, ly) = h.app.local;
    if view.native {
        let expected = (p.x - view.rect.loc.x as f64, p.y - view.rect.loc.y as f64);
        prop_assert!(
            (lx - expected.0).abs() < 0.01 && (ly - expected.1).abs() < 0.01,
            "E2: pointer at {:?} over the view drawn at {:?} reached the client at ({}, {}), expected {:?}",
            p,
            view.rect,
            lx,
            ly,
            expected,
        );
    } else {
        let (w, height) = h.answered_configure.unwrap_or((VIEW_W, VIEW_H));
        prop_assert!(
            lx >= 0.0 && ly >= 0.0 && lx < w as f64 && ly < height as f64,
            "E2: pointer at {:?} over a scaled view reached the client at ({}, {}), outside its {}x{} buffer",
            p,
            lx,
            ly,
            w,
            height,
        );
    }
    Ok(())
}

fn check_e3(h: &Harness) -> Result<(), TestCaseError> {
    let p = h.pointer();
    let views = drawn_views(h);
    let Some(view) = drawn_view_under(&views, p) else {
        return Ok(());
    };
    if popup_covers(h, p) {
        return Ok(());
    }
    let expected = if view.surface == h.sid {
        h.toplevel_key()
    } else if view.surface == h.frame_sid {
        h.frame_key()
    } else {
        return Ok(());
    };
    let ewm = h.fix.ewm_ref();
    let hit = server_hit_key(ewm, ewm.surface_under_point(p).as_ref().map(|(s, _)| s));
    prop_assert_eq!(
        hit.as_ref(),
        Some(&expected),
        "E3: {:?} lies in the view of {} drawn at {:?}, but the hit-test found {:?}",
        p,
        view.surface,
        view.rect,
        hit,
    );
    Ok(())
}

/// The plain entry under the frame client's pointer, from the layout the harness applied.
fn plain_entry_under_frame_pointer(h: &Harness) -> Option<LayoutEntryId> {
    if h.frame.focus.as_ref() != Some(&h.frame_key()) {
        return None;
    }
    let (fx, fy) = h.frame.local;
    let ewm = h.fix.ewm_ref();
    let frame = ewm
        .layout
        .monitors()
        .iter()
        .flat_map(|monitor| monitor.frames())
        .find(|frame| frame.surface_id == h.frame_sid)?;
    if frame
        .entries
        .iter()
        .any(|entry| entry.surface_id().is_some())
    {
        return None;
    }
    frame
        .entries
        .iter()
        .find(|entry| {
            fx >= entry.x as f64
                && fy >= entry.y as f64
                && fx < (entry.x + entry.w as i32) as f64
                && fy < (entry.y + entry.h as i32) as f64
        })
        .map(|entry| entry.id)
}

fn check_f1(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    if !ewm.focus_follows_mouse_mode || ewm.pointer.is_grabbed() {
        return Ok(());
    }
    let Some(entry_id) = plain_entry_under_frame_pointer(h) else {
        return Ok(());
    };
    // FFM acts on a change under the pointer, so the same entry proves nothing.
    if h.plain_entry_under_pointer == Some(entry_id) && h.focus_before_op == Some(h.frame_key()) {
        return Ok(());
    }
    prop_assert_eq!(
        ewm.focused_entry_id(),
        Some(entry_id),
        "F1: FFM should focus the plain entry under the frame pointer at {:?}",
        h.frame.local,
    );
    prop_assert_eq!(
        ewm.focused_frame_id(),
        h.frame_sid,
        "F1: focused frame should own the plain entry",
    );
    prop_assert_eq!(
        ewm.focused_surface_id(),
        h.frame_sid,
        "F1: keyboard routing should stay on the Emacs frame surface",
    );
    Ok(())
}

fn check_c1(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let primary = ewm
        .layout
        .strips()
        .flat_map(|(name, strip)| {
            let active_idx = strip.active_idx();
            strip
                .frames()
                .get(active_idx)
                .into_iter()
                .flat_map(move |frame| {
                    frame
                        .entries
                        .iter()
                        .filter(|entry| entry.surface.is_some())
                        .map(move |entry| (name, frame, entry))
                })
        })
        .find(|(_, _, e)| e.surface_id() == Some(h.sid) && e.primary());
    let Some((out_name, _, entry)) = primary else {
        return Ok(());
    };
    let requested = ewm.pending_fullscreen(h.sid);
    let expected = if requested {
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
            "C1: last toplevel configure was {:?}, expected {:?} (fullscreen requested={})",
            h.last_configure,
            expected,
            requested,
        );
    }
    Ok(())
}

fn check_c2(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let placed: HashSet<String> = ewm
        .layout
        .monitors()
        .iter()
        .filter(|monitor| {
            monitor.frames().any(|frame| {
                frame.surface_id == h.sid
                    || frame
                        .entries
                        .iter()
                        .any(|entry| entry.surface_id() == Some(h.sid))
            })
        })
        .map(|monitor| monitor.name().to_string())
        .collect();
    prop_assert_eq!(
        h.toplevel_on_outputs.clone(),
        placed.clone(),
        "C2: client output set {:?} != outputs holding the toplevel {:?}",
        h.toplevel_on_outputs,
        placed,
    );
    Ok(())
}
fn check_d1(h: &Harness) -> Result<(), TestCaseError> {
    if !h.dnd_active {
        return Ok(());
    }
    prop_assert_eq!(
        h.fix.ewm_ref().focused_entry_id(),
        h.focused_entry_before_op,
        "D1: a drag must not move focus",
    );
    Ok(())
}

fn rect_contains_f64(rect: Rectangle<i32, Logical>, p: Point<f64, Logical>) -> bool {
    p.x >= rect.loc.x as f64
        && p.y >= rect.loc.y as f64
        && p.x < (rect.loc.x + rect.size.w) as f64
        && p.y < (rect.loc.y + rect.size.h) as f64
}
fn check_pp2(h: &Harness) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    let placements = collect_popup_placements(ewm, world_rect(), None);
    let popup_ids = h.popup_surface_keys();
    for p in &placements {
        let centre = Point::from((
            (p.location.x + p.geometry.size.w / 2) as f64,
            (p.location.y + p.geometry.size.h / 2) as f64,
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

fn check_p6(h: &Harness, p: Point<f64, Logical>) -> Result<(), TestCaseError> {
    let ewm = h.fix.ewm_ref();
    if !ewm.pointer.is_grabbed() || h.dnd_active {
        return Ok(());
    }
    let Some((focus, origin)) = ewm.pointer_focus.as_ref() else {
        return Ok(());
    };
    let frame_key = client_surface_key(&h.frame_client, &h.frame_toplevel.surface);
    if server_surface_key(ewm, focus) != Some(frame_key) {
        return Ok(());
    }
    let hit_client = ewm
        .surface_under_point(p)
        .and_then(|(s, _)| ewm.display_handle.get_client(s.id()).ok())
        .map(|c| c.id());
    let foreign =
        ewm.emacs_drag_source && hit_client.is_some_and(|c| c != h.frame_client.server_client_id());
    let local = p - *origin;
    let expected = if foreign {
        Point::from((-local.x - 1.0, -local.y - 1.0))
    } else {
        local
    };
    let motions: Vec<(f64, f64)> = h
        .frame_events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::PointerMotion { x, y } => Some((*x, *y)),
            _ => None,
        })
        .collect();
    prop_assert!(
        !motions.is_empty(),
        "P6: pinned frame received no motion for warp to {:?}",
        p
    );
    for (x, y) in motions {
        prop_assert!(
            (x - expected.x).abs() < 0.01 && (y - expected.y).abs() < 0.01,
            "P6: warp to {:?} (foreign={}, drag_source={}) reached the frame at ({}, {}), expected {:?}",
            p,
            foreign,
            ewm.emacs_drag_source,
            x,
            y,
            expected
        );
    }
    Ok(())
}

fn check_p3(h: &Harness, events: &[ClientEvent], pressed: bool) -> Result<(), TestCaseError> {
    let focus_id = h.focus_before_op.clone();
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
/// Where a tap at fractions of the touchscreen lands: the first output, untransformed.
fn tap_point(h: &Harness, fx: f64, fy: f64) -> Option<Point<f64, Logical>> {
    let ewm = h.fix.ewm_ref();
    let geo = ewm.space.output_geometry(ewm.sorted_outputs.first()?)?;
    Some(geo.loc.to_f64() + Point::from((fx * geo.size.w as f64, fy * geo.size.h as f64)))
}

/// Sample what a press at `p` will land on, before it lands.
fn note_press(h: &mut Harness, p: Point<f64, Logical>) {
    let (entry, hit) = {
        let ewm = h.fix.ewm_ref();
        (
            ewm.layout_entry_id_under(p),
            ewm.surface_under_point(p).is_some(),
        )
    };
    h.entry_under_press = entry;
    h.hit_under_press = hit;
}

/// T1 after a tap or a button press at `p`.
fn check_t1(h: &Harness, p: Point<f64, Logical>) -> Result<(), TestCaseError> {
    let focused = h.fix.ewm_ref().focused_entry_id();
    match h.entry_under_press {
        Some(entry_id) => prop_assert_eq!(
            focused,
            Some(entry_id),
            "T1: a press at {:?} must focus the entry under it",
            p,
        ),
        None if !h.hit_under_press => prop_assert_eq!(
            focused,
            h.focused_entry_before_op,
            "T1: a press on nothing at {:?} must leave focus alone",
            p,
        ),
        None => {}
    }
    Ok(())
}

fn assert_invariants(h: &mut Harness, last_op: &Op) -> Result<(), TestCaseError> {
    let events = h.client.drain_events();
    h.fold_events(&events);
    h.app.fold(&h.client, &events);
    check_pp2(h)?;
    check_c1(h)?;
    check_c2(h)?;
    check_e1(h)?;
    check_e2(h)?;
    check_e3(h)?;
    match *last_op {
        Op::WarpPointer { x, y } => {
            let p = Point::from((x as f64, y as f64));
            check_p6(h, p)?;
            check_f1(h)?;
            check_d1(h)?;
        }
        Op::PointerButton { pressed } => {
            check_p3(h, &events, pressed)?;
            if pressed {
                check_t1(h, h.pointer())?;
            }
        }
        Op::Tap { fx, fy } => {
            if let Some(p) = tap_point(h, fx, fy) {
                check_t1(h, p)?;
            }
        }
        _ => {}
    }
    h.plain_entry_under_pointer = plain_entry_under_frame_pointer(h);
    Ok(())
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
            (placement.location.x + placement.geometry.size.w / 2) as f64,
            (placement.location.y + placement.geometry.size.h / 2) as f64,
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
}
