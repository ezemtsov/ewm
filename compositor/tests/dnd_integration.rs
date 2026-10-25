//! Drags over the protocol: the grab follows the pointer instead of pinning to its source, the
//! drop focuses its target and hands the data over, a declined offer cancels the source. While
//! Emacs tracks a drag source, a click grab on a frame's own surface hands the cross-program drag
//! off to the DnD grab and leaves app-owned click grabs alone.

use ewm_core::layout::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient, Toplevel, frame_with_entries, received};
use ewm_core::{Event, LayoutEntry, LayoutEntryId};
use smithay::utils::Size;
use wayland_client::WEnum;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::wl_surface::WlSurface;

const HEAD: &str = "HEAD-A";
const BTN_LEFT: u32 = 0x110;
const MIME: &str = "text/uri-list";
const PAYLOAD: &[u8] = b"file:///tmp/floating.mp4\r\n";
const APP_ENTRY: LayoutEntryId = LayoutEntryId::new(1).unwrap();
const TEXT_ENTRY: LayoutEntryId = LayoutEntryId::new(2).unwrap();

struct Scene {
    fix: Fixture,
    frame_client: TestClient,
    frame: Toplevel,
    frame_sid: u64,
    app_client: TestClient,
    app: Toplevel,
    app_sid: u64,
    /// A drag icon surface belonging to the app.
    icon: WlSurface,
}

/// An Emacs frame filling the output: an app view on the left, a plain Emacs window on the right.
fn scene() -> Scene {
    let mut fix = Fixture::new().unwrap();
    let mut frame_client = fix.new_client().unwrap();
    let mut app_client = fix.new_client().unwrap();
    fix.roundtrip(&mut frame_client);
    fix.roundtrip(&mut app_client);
    fix.add_output(HEAD, 1600, 900);

    let (frame, frame_sid) =
        fix.open_toplevel_with_id(&mut frame_client, "frame", Size::from((1600, 900)));
    let (app, app_sid) = fix.open_toplevel_with_id(&mut app_client, "app", Size::from((800, 900)));
    let icon = app_client.create_surface();

    let mut layout = Frame::new(frame_sid);
    layout.width = 1600.0;
    layout.selected_entry_id = Some(APP_ENTRY);
    layout.entries.push(LayoutEntry::surface(
        APP_ENTRY,
        "app".to_string(),
        app_sid,
        0,
        0,
        800,
        900,
    ));
    layout.entries.push(LayoutEntry::emacs_window(
        TEXT_ENTRY,
        "text".to_string(),
        800,
        0,
        800,
        900,
    ));
    fix.tile(HEAD, vec![layout]);
    fix.roundtrip(&mut frame_client);
    fix.roundtrip(&mut app_client);
    frame_client.drain_events();
    app_client.drain_events();
    Scene {
        fix,
        frame_client,
        frame,
        frame_sid,
        app_client,
        app,
        app_sid,
        icon,
    }
}

/// Press at (X, Y) and let CLIENT see it, a drag needs the serial of the press it saw.
fn press(fix: &mut Fixture, client: &mut TestClient, x: f64, y: f64) {
    fix.pointer_motion_to(x, y);
    fix.pointer_button(BTN_LEFT, true);
    fix.roundtrip(client);
}

/// CLIENT drags from `origin` under the press it saw.
fn drag(fix: &mut Fixture, client: &mut TestClient, origin: &WlSurface, icon: Option<&WlSurface>) {
    let serial = client.press_serial().expect("the client saw the press");
    client.start_drag(origin, icon, MIME, PAYLOAD, serial);
    fix.roundtrip(client);
}

fn motions(fix: &mut Fixture, client: &mut TestClient) -> Vec<(f64, f64)> {
    fix.roundtrip(client);
    client
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
            ClientEvent::PointerMotion { x, y } => Some((x, y)),
            _ => None,
        })
        .collect()
}

#[test]
fn dnd_grab_follows_pointer_and_drop_focuses_target() {
    let mut s = scene();

    press(&mut s.fix, &mut s.app_client, 400.0, 450.0);
    assert_eq!(s.fix.ewm_ref().focused_entry_id(), Some(APP_ENTRY));
    drag(&mut s.fix, &mut s.app_client, &s.app.surface, Some(&s.icon));
    assert!(s.fix.ewm_ref().pointer.is_grabbed());
    assert!(s.fix.ewm_ref().dnd_icon.is_some());
    // Focus-follows-mouse must stay quiet for the whole drag.
    s.fix.ewm().config.focus_follows_mouse = true;
    s.fix.enable_event_capture();

    s.fix.pointer_motion_to(1200.0, 450.0);
    assert_eq!(s.fix.pointer_focus_surface_id(), Some(s.frame_sid));
    assert!(s.fix.ewm_ref().pointer.is_grabbed());
    assert_eq!(s.fix.ewm_ref().focused_entry_id(), Some(APP_ENTRY));
    let events = s.fix.drain_events();
    assert!(
        !events.iter().any(|e| matches!(e, Event::Focus { .. })),
        "no focus change mid-drag: {events:?}"
    );

    s.fix.pointer_button(BTN_LEFT, false);
    assert!(!s.fix.ewm_ref().pointer.is_grabbed());
    assert!(s.fix.ewm_ref().pending_drop.is_none());
    assert!(s.fix.ewm_ref().dnd_icon.is_none());
    assert_eq!(s.fix.ewm_ref().focused_entry_id(), Some(TEXT_ENTRY));
    let events = s.fix.drain_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Focus { focus_id, pointer: true, .. } if *focus_id == TEXT_ENTRY
        )),
        "drop must emit a pointer focus for the target window: {events:?}"
    );
}

#[test]
fn restarted_drag_keeps_its_icon() {
    let mut s = scene();

    press(&mut s.fix, &mut s.app_client, 400.0, 450.0);
    drag(&mut s.fix, &mut s.app_client, &s.app.surface, Some(&s.icon));
    // The second grab cancels the first, which must not clear the new icon.
    drag(&mut s.fix, &mut s.app_client, &s.app.surface, Some(&s.icon));
    assert!(s.fix.ewm_ref().pointer.is_grabbed());
    assert!(s.fix.ewm_ref().dnd_icon.is_some());

    s.fix.pointer_button(BTN_LEFT, false);
    assert!(s.fix.ewm_ref().dnd_icon.is_none());
}

#[test]
fn drag_source_hands_off_to_the_dnd_grab() {
    let mut s = scene();
    press(&mut s.fix, &mut s.frame_client, 1200.0, 450.0);
    s.fix.ewm().emacs_drag_source = true;
    s.fix.pointer_motion_to(400.0, 450.0);
    assert_eq!(motions(&mut s.fix, &mut s.frame_client).len(), 1);

    drag(&mut s.fix, &mut s.frame_client, &s.frame.surface, None);
    s.fix.pointer_motion_to(410.0, 450.0);
    assert_eq!(s.fix.pointer_focus_surface_id(), Some(s.app_sid));

    s.fix.pointer_button(BTN_LEFT, false);
    assert!(!s.fix.ewm_ref().pointer.is_grabbed());
    assert_eq!(s.fix.ewm_ref().focused_entry_id(), Some(APP_ENTRY));
}

#[test]
fn drag_source_leaves_app_click_grabs_alone() {
    let mut s = scene();
    press(&mut s.fix, &mut s.app_client, 400.0, 450.0);
    s.app_client.drain_events();
    s.fix.ewm().emacs_drag_source = true;

    s.fix.pointer_motion_to(1200.0, 450.0);
    assert_eq!(motions(&mut s.fix, &mut s.app_client), [(1200.0, 450.0)]);
    assert_eq!(s.fix.pointer_focus_surface_id(), Some(s.app_sid));
    s.fix.pointer_button(BTN_LEFT, false);
}

/// A drag from one app to another: negotiation, drop, data transfer and finish, or a cancel.
fn drag_between_apps(accept: bool) {
    let mut fix = Fixture::new().unwrap();
    let mut src = fix.new_client().unwrap();
    let mut dst = fix.new_client().unwrap();
    fix.roundtrip(&mut src);
    fix.roundtrip(&mut dst);
    fix.add_output(HEAD, 1600, 900);
    let (a, a_sid) = fix.open_toplevel_with_id(&mut src, "src", Size::from((800, 900)));
    let (b, b_sid) = fix.open_toplevel_with_id(&mut dst, "dst", Size::from((800, 900)));
    fix.tile(
        HEAD,
        vec![frame_with_entries(
            100,
            &[(APP_ENTRY, a_sid), (TEXT_ENTRY, b_sid)],
        )],
    );
    if accept {
        dst.accept_dnd(MIME);
    } else {
        dst.data_device();
    }
    fix.roundtrip(&mut src);
    fix.roundtrip(&mut dst);
    dst.drain_events();

    press(&mut fix, &mut src, 400.0, 450.0);
    drag(&mut fix, &mut src, &a.surface, None);
    assert!(fix.ewm_ref().pointer.is_grabbed());
    src.drain_events();

    fix.pointer_motion_to(1200.0, 450.0);
    fix.roundtrip(&mut dst);
    fix.roundtrip(&mut src);
    let events = dst.drain_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            ClientEvent::DndEnter { surface, mimes } if *surface == b.surface && mimes == &[MIME]
        )),
        "the target sees the offered mime on enter: {events:?}"
    );
    let events = src.drain_events();
    assert_eq!(
        events.iter().any(|e| matches!(
            e,
            ClientEvent::SourceAction {
                action: WEnum::Value(DndAction::Copy)
            }
        )),
        accept,
        "the source learns the copy action only from a taker: {events:?}"
    );

    fix.pointer_button(BTN_LEFT, false);
    fix.roundtrip(&mut dst);
    fix.roundtrip(&mut src);
    assert!(!fix.ewm_ref().pointer.is_grabbed());
    assert_eq!(fix.ewm_ref().focused_entry_id(), Some(TEXT_ENTRY));
    let dropped = dst.drain_events();
    let events = src.drain_events();
    assert_eq!(
        dropped.iter().any(|e| matches!(e, ClientEvent::DndDrop)),
        accept,
        "{dropped:?}"
    );
    assert_eq!(
        events
            .iter()
            .any(|e| matches!(e, ClientEvent::SourceDropPerformed)),
        accept,
        "{events:?}"
    );
    assert_eq!(
        events
            .iter()
            .any(|e| matches!(e, ClientEvent::SourceCancelled)),
        !accept,
        "{events:?}"
    );
    if !accept {
        return;
    }

    let data = dst.receive_dnd(MIME);
    fix.roundtrip(&mut dst);
    fix.roundtrip(&mut src);
    assert_eq!(received(data).as_deref(), Some(PAYLOAD));

    dst.finish_dnd();
    fix.roundtrip(&mut dst);
    fix.roundtrip(&mut src);
    let events = src.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ClientEvent::SourceFinished)),
        "{events:?}"
    );
}

#[test]
fn a_taken_drag_transfers_its_data() {
    drag_between_apps(true);
}

#[test]
fn a_declined_drag_cancels_its_source() {
    drag_between_apps(false);
}
