//! DnD grabs follow the pointer instead of pinning to their source, and the drop focuses its
//! target. While Emacs tracks a drag source, a click grab on a frame's own surface hands the
//! cross-program drag off to the DnD grab and leaves app-owned click grabs alone.

use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient};
use ewm_core::{Event, LayoutEntry, LayoutEntryId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Size;

const HEAD: &str = "HEAD-A";
const BTN_LEFT: u32 = 0x110;
const APP_ENTRY: LayoutEntryId = LayoutEntryId::new(1).unwrap();
const TEXT_ENTRY: LayoutEntryId = LayoutEntryId::new(2).unwrap();

struct Scene {
    fix: Fixture,
    frame_client: TestClient,
    app_client: TestClient,
    frame_sid: u64,
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

    let frame_tl = fix.open_toplevel(&mut frame_client, "frame", Size::from((1600, 900)));
    let frame_sid = fix
        .find_surface_id(&frame_client, &frame_tl.surface)
        .unwrap();
    let app = fix.open_toplevel(&mut app_client, "app", Size::from((800, 900)));
    let app_sid = fix.find_surface_id(&app_client, &app.surface).unwrap();
    let icon = app_client.create_surface();
    fix.roundtrip(&mut app_client);
    let icon = fix.server_surface(&app_client, &icon);

    let mut frame = Frame::new(frame_sid);
    frame.width = 1600.0;
    frame.selected_entry_id = Some(APP_ENTRY);
    frame.entries.push(LayoutEntry::surface(
        APP_ENTRY,
        "app".to_string(),
        app_sid,
        0,
        0,
        800,
        900,
    ));
    frame.entries.push(LayoutEntry::emacs_window(
        TEXT_ENTRY,
        "text".to_string(),
        800,
        0,
        800,
        900,
    ));
    fix.tile(HEAD, vec![frame]);
    fix.roundtrip(&mut frame_client);
    fix.roundtrip(&mut app_client);
    frame_client.drain_events();
    app_client.drain_events();
    Scene {
        fix,
        frame_client,
        app_client,
        frame_sid,
        app_sid,
        icon,
    }
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

    s.fix.pointer_motion_to(400.0, 450.0);
    s.fix.pointer_button(BTN_LEFT, true);
    assert_eq!(s.fix.ewm_ref().focused_entry_id, Some(APP_ENTRY));
    s.fix.start_pointer_dnd(s.app_sid, Some(s.icon.clone()));
    assert!(s.fix.ewm_ref().pointer.is_grabbed());
    assert!(s.fix.ewm_ref().dnd_icon.is_some());
    // Focus-follows-mouse must stay quiet for the whole drag.
    s.fix.ewm().focus_follows_mouse_mode = true;
    s.fix.enable_event_capture();

    s.fix.pointer_motion_to(1200.0, 450.0);
    assert_eq!(s.fix.pointer_focus_surface_id(), Some(s.frame_sid));
    assert!(s.fix.ewm_ref().pointer.is_grabbed());
    assert_eq!(s.fix.ewm_ref().focused_entry_id, Some(APP_ENTRY));
    let events = s.fix.drain_events();
    assert!(
        !events.iter().any(|e| matches!(e, Event::Focus { .. })),
        "no focus change mid-drag: {events:?}"
    );

    s.fix.pointer_button(BTN_LEFT, false);
    assert!(!s.fix.ewm_ref().pointer.is_grabbed());
    assert!(s.fix.ewm_ref().pending_drop.is_none());
    assert!(s.fix.ewm_ref().dnd_icon.is_none());
    assert_eq!(s.fix.ewm_ref().focused_entry_id, Some(TEXT_ENTRY));
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

    s.fix.pointer_motion_to(400.0, 450.0);
    s.fix.pointer_button(BTN_LEFT, true);
    s.fix.start_pointer_dnd(s.app_sid, Some(s.icon.clone()));
    // The second grab cancels the first, which must not clear the new icon.
    s.fix.start_pointer_dnd(s.app_sid, Some(s.icon.clone()));
    assert!(s.fix.ewm_ref().pointer.is_grabbed());
    assert!(s.fix.ewm_ref().dnd_icon.is_some());

    s.fix.pointer_button(BTN_LEFT, false);
    assert!(s.fix.ewm_ref().dnd_icon.is_none());
}

#[test]
fn drag_source_hands_off_to_the_dnd_grab() {
    let mut s = scene();
    s.fix.pointer_motion_to(1200.0, 450.0);
    s.fix.pointer_button(BTN_LEFT, true);
    s.fix.ewm().emacs_drag_source = true;
    s.fix.pointer_motion_to(400.0, 450.0);
    assert_eq!(motions(&mut s.fix, &mut s.frame_client).len(), 1);

    s.fix.start_pointer_dnd(s.frame_sid, None);
    s.fix.pointer_motion_to(410.0, 450.0);
    assert_eq!(s.fix.pointer_focus_surface_id(), Some(s.app_sid));

    s.fix.pointer_button(BTN_LEFT, false);
    assert!(!s.fix.ewm_ref().pointer.is_grabbed());
    assert_eq!(s.fix.ewm_ref().focused_entry_id, Some(APP_ENTRY));
}

#[test]
fn drag_source_leaves_app_click_grabs_alone() {
    let mut s = scene();
    s.fix.pointer_motion_to(400.0, 450.0);
    s.fix.pointer_button(BTN_LEFT, true);
    s.fix.roundtrip(&mut s.app_client);
    s.app_client.drain_events();
    s.fix.ewm().emacs_drag_source = true;

    s.fix.pointer_motion_to(1200.0, 450.0);
    assert_eq!(motions(&mut s.fix, &mut s.app_client), [(1200.0, 450.0)]);
    assert_eq!(s.fix.pointer_focus_surface_id(), Some(s.app_sid));
    s.fix.pointer_button(BTN_LEFT, false);
}
