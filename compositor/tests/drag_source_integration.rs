//! While Emacs tracks a drag source, a click grab on a frame's own surface
//! reports motion over another client's view as outside the frame, so
//! Emacs starts a cross-program drag that the DnD grab then routes.

use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient};
use ewm_core::{LayoutEntry, LayoutEntryId};
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
}

/// An Emacs frame filling the output: an app view on the left, a plain
/// Emacs window on the right.
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
    frame_client.drain_events();
    app_client.drain_events();
    Scene {
        fix,
        frame_client,
        app_client,
        frame_sid,
        app_sid,
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
fn drag_source_reports_foreign_views_outside_the_frame() {
    let mut s = scene();
    s.fix.pointer_motion_to(1200.0, 450.0);
    s.fix.pointer_button(BTN_LEFT, true);
    s.fix.roundtrip(&mut s.frame_client);
    s.frame_client.drain_events();

    // A plain click drag keeps frame coordinates over the app view.
    s.fix.pointer_motion_to(400.0, 450.0);
    assert_eq!(motions(&mut s.fix, &mut s.frame_client), [(400.0, 450.0)]);
    assert_eq!(s.fix.pointer_focus_surface_id(), Some(s.frame_sid));

    s.fix.ewm().emacs_drag_source = true;
    s.fix.pointer_motion_to(401.0, 450.0);
    assert_eq!(motions(&mut s.fix, &mut s.frame_client), [(-402.0, -451.0)]);
    assert_eq!(s.fix.pointer_focus_surface_id(), Some(s.frame_sid));

    // Over the frame's own window the real coordinates come back.
    s.fix.pointer_motion_to(1300.0, 450.0);
    assert_eq!(motions(&mut s.fix, &mut s.frame_client), [(1300.0, 450.0)]);

    s.fix.pointer_button(BTN_LEFT, false);
    assert!(!s.fix.ewm_ref().pointer.is_grabbed());
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
