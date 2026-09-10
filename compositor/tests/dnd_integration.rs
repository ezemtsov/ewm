//! DnD grabs follow the pointer instead of pinning to their source, and the
//! drop focuses its target.

use ewm_core::strip::Frame;
use ewm_core::testing::Fixture;
use ewm_core::{Event, LayoutEntry, LayoutEntryId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Size;

const HEAD: &str = "HEAD-A";
const BTN_LEFT: u32 = 0x110;
const APP_ENTRY: LayoutEntryId = LayoutEntryId::new(1).unwrap();
const TEXT_ENTRY: LayoutEntryId = LayoutEntryId::new(2).unwrap();

/// An Emacs frame filling the output: an app view on the left, a plain
/// Emacs window on the right. Returns the fixture, the two surface ids and
/// a drag icon surface belonging to the app.
fn scene() -> (Fixture, u64, u64, WlSurface) {
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
    fix.ewm().apply_output_layout(HEAD, vec![frame]);
    fix.roundtrip(&mut frame_client);
    fix.roundtrip(&mut app_client);
    (fix, app_sid, frame_sid, icon)
}

#[test]
fn dnd_grab_follows_pointer_and_drop_focuses_target() {
    let (mut fix, app_sid, frame_sid, icon) = scene();

    fix.pointer_motion_to(400.0, 450.0);
    fix.pointer_button(BTN_LEFT, true);
    assert_eq!(fix.ewm_ref().focused_entry_id, Some(APP_ENTRY));
    fix.start_pointer_dnd(app_sid, Some(icon));
    assert!(fix.ewm_ref().pointer.is_grabbed());
    assert!(fix.ewm_ref().dnd_icon.is_some());
    // Focus-follows-mouse must stay quiet for the whole drag.
    fix.ewm().focus_follows_mouse_mode = true;
    fix.enable_event_capture();

    fix.pointer_motion_to(1200.0, 450.0);
    assert_eq!(fix.pointer_focus_surface_id(), Some(frame_sid));
    assert!(fix.ewm_ref().pointer.is_grabbed());
    assert_eq!(fix.ewm_ref().focused_entry_id, Some(APP_ENTRY));
    let events = fix.drain_events();
    assert!(
        !events.iter().any(|e| matches!(e, Event::Focus { .. })),
        "no focus change mid-drag: {events:?}"
    );

    fix.pointer_button(BTN_LEFT, false);
    assert!(!fix.ewm_ref().pointer.is_grabbed());
    assert!(fix.ewm_ref().pending_drop.is_none());
    assert!(fix.ewm_ref().dnd_icon.is_none());
    assert_eq!(fix.ewm_ref().focused_entry_id, Some(TEXT_ENTRY));
    let events = fix.drain_events();
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
    let (mut fix, app_sid, _, icon) = scene();

    fix.pointer_motion_to(400.0, 450.0);
    fix.pointer_button(BTN_LEFT, true);
    fix.start_pointer_dnd(app_sid, Some(icon.clone()));
    // The second grab cancels the first, which must not clear the new icon.
    fix.start_pointer_dnd(app_sid, Some(icon));
    assert!(fix.ewm_ref().pointer.is_grabbed());
    assert!(fix.ewm_ref().dnd_icon.is_some());

    fix.pointer_button(BTN_LEFT, false);
    assert!(fix.ewm_ref().dnd_icon.is_none());
}

#[test]
fn click_drag_still_pins_pointer_focus() {
    let (mut fix, app_sid, _, _) = scene();

    fix.pointer_motion_to(400.0, 450.0);
    fix.pointer_button(BTN_LEFT, true);
    fix.pointer_motion_to(1200.0, 450.0);
    assert_eq!(fix.pointer_focus_surface_id(), Some(app_sid));
    fix.pointer_button(BTN_LEFT, false);
    assert_eq!(fix.ewm_ref().focused_entry_id, Some(APP_ENTRY));
}
