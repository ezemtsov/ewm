//! Integration test for xdg_popup grabs (menu behaviour).
//!
//! Verifies the four things approach A relies on: an installed popup grab
//! routes hover/click to the popup, survives a click inside it, and is
//! dismissed by a click outside (on empty space / another client).

use ewm_core::render::collect_popup_placements;
use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture};
use ewm_core::{LayoutEntry, LayoutEntryId};
use smithay::utils::{Logical, Point, Rectangle, Size};
use wayland_client::Proxy as _;

const HEAD: &str = "HEAD-A";
const BTN_LEFT: u32 = 0x110;

fn nz(n: u64) -> LayoutEntryId {
    std::num::NonZeroU64::new(n).unwrap()
}

/// Wide enough to cover the output for `collect_popup_placements`.
fn world_rect() -> Rectangle<i32, Logical> {
    Rectangle::new(Point::from((-100, -100)), Size::from((5000, 5000)))
}

#[test]
fn popup_grab_routes_input_and_dismisses_on_outside_click() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output(HEAD, 1600, 900);

    // An app toplevel placed in a small (400x400) frame, leaving empty space
    // elsewhere on the output to click "outside" the menu.
    let app = fix.open_toplevel(&mut client, "app", Size::from((400, 400)));
    let app_sid = fix.find_surface_id(&client, &app.surface).unwrap();
    let entry = nz(1);
    let mut frame = Frame::new(100);
    frame.width = 400.0;
    frame.selected_entry_id = Some(entry);
    frame.entries.push(LayoutEntry::surface(
        entry,
        "app".to_string(),
        app_sid,
        0,
        0,
        400,
        400,
    ));
    fix.ewm().apply_output_layout(HEAD, vec![frame]);
    fix.roundtrip(&mut client);

    // A menu popup on the app toplevel, grabbed before mapping.
    let popup = fix.open_popup_grabbed(
        &mut client,
        &app,
        Rectangle::new(Point::from((10, 10)), Size::from((1, 1))),
        Size::from((100, 150)),
        1,
    );
    fix.roundtrip(&mut client);

    // The grab is installed and tracked.
    assert!(
        fix.ewm_ref().popup_grab.is_some(),
        "popup grab should be tracked after xdg_popup.grab",
    );
    assert!(
        fix.ewm_ref().pointer.is_grabbed(),
        "the seat pointer should be grabbed",
    );

    // Locate the popup on screen.
    let center = {
        let placements = collect_popup_placements(fix.ewm_ref(), world_rect(), None);
        let p = placements.first().expect("the popup should be placed");
        Point::from((
            (p.location.x + p.geometry.size.w / 2) as f64,
            (p.location.y + p.geometry.size.h / 2) as f64,
        ))
    };
    // Sanity: that point really is the popup surface.
    assert!(
        fix.ewm_ref().surface_under_point(center).is_some(),
        "popup centre should hit a surface",
    );

    // Hover the popup -> the client receives pointer input for it (fix #1:
    // motion is not pinned during a popup grab).
    let popup_id = popup.surface.id().protocol_id();
    client.drain_events();
    fix.pointer_motion_to(center.x, center.y);
    fix.roundtrip(&mut client);
    let events = client.drain_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            ClientEvent::PointerEnter { surface, .. } if surface.id().protocol_id() == popup_id
        )),
        "popup should receive PointerEnter on hover, got {events:?}",
    );

    // Click inside the popup -> delivered, and the grab survives.
    fix.pointer_button(BTN_LEFT, true);
    fix.roundtrip(&mut client);
    let events = client.drain_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            ClientEvent::PointerButton {
                button: BTN_LEFT,
                pressed: true
            }
        )),
        "click inside the popup should be delivered, got {events:?}",
    );
    assert!(
        fix.ewm_ref().pointer.is_grabbed(),
        "a click inside the popup must not dismiss it",
    );
    fix.pointer_button(BTN_LEFT, false);
    fix.roundtrip(&mut client);
    client.drain_events();

    // Hovering off the popup -- onto empty space, and onto ANOTHER client's
    // surface -- must NOT dismiss it (only a click does).
    fix.pointer_motion_to(1000.0, 600.0);
    fix.roundtrip(&mut client);
    assert!(
        fix.ewm_ref().pointer.is_grabbed(),
        "hovering empty space must not dismiss the popup",
    );
    // A second client's toplevel placed elsewhere (like an embedded app).
    let mut other = fix.new_client().unwrap();
    fix.roundtrip(&mut other);
    let other_top = fix.open_toplevel(&mut other, "other", Size::from((300, 300)));
    let other_sid = fix.find_surface_id(&other, &other_top.surface).unwrap();
    let mut other_frame = Frame::new(101);
    other_frame.width = 300.0;
    other_frame.selected_entry_id = Some(nz(2));
    other_frame.entries.push(LayoutEntry::surface(
        nz(2),
        "other".to_string(),
        other_sid,
        0,
        0,
        300,
        300,
    ));
    // Two frames on the strip: the app (400) then the other (300).
    let mut app_frame = Frame::new(100);
    app_frame.width = 400.0;
    app_frame.selected_entry_id = Some(entry);
    app_frame.entries.push(LayoutEntry::surface(
        entry,
        "app".to_string(),
        app_sid,
        0,
        0,
        400,
        400,
    ));
    fix.ewm()
        .apply_output_layout(HEAD, vec![app_frame, other_frame]);
    fix.roundtrip(&mut client);
    fix.roundtrip(&mut other);
    let hit_other = fix
        .ewm_ref()
        .surface_under_point(Point::from((550.0, 150.0)))
        .and_then(|(s, _)| fix.ewm_ref().surface_id(&s))
        == Some(other_sid);
    if hit_other {
        // The embedded-app case: hover another client's surface, no click.
        fix.pointer_motion_to(550.0, 150.0);
        fix.roundtrip(&mut client);
        assert!(
            fix.ewm_ref().pointer.is_grabbed(),
            "hovering another client's surface must not dismiss the popup",
        );
    }
    // Move back over the popup; still grabbed.
    fix.pointer_motion_to(center.x, center.y);
    fix.roundtrip(&mut client);
    assert!(
        fix.ewm_ref().pointer.is_grabbed(),
        "the popup must survive hovering back over it",
    );

    // Click on empty space outside the popup -> the grab is dismissed.
    let outside = Point::from((1000.0, 600.0));
    assert!(
        fix.ewm_ref().surface_under_point(outside).is_none(),
        "the outside point must be empty for a genuine outside click",
    );
    fix.pointer_motion_to(outside.x, outside.y);
    fix.pointer_button(BTN_LEFT, true);
    fix.roundtrip(&mut client);
    assert!(
        !fix.ewm_ref().pointer.is_grabbed(),
        "a click outside the popup must dismiss the grab",
    );
}

#[test]
fn nested_submenu_popup_does_not_dismiss_the_chain() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output(HEAD, 1600, 900);

    let app = fix.open_toplevel(&mut client, "app", Size::from((400, 400)));
    let app_sid = fix.find_surface_id(&client, &app.surface).unwrap();
    let entry = nz(1);
    let mut frame = Frame::new(100);
    frame.width = 400.0;
    frame.selected_entry_id = Some(entry);
    frame.entries.push(LayoutEntry::surface(
        entry,
        "app".to_string(),
        app_sid,
        0,
        0,
        400,
        400,
    ));
    fix.ewm().apply_output_layout(HEAD, vec![frame]);
    fix.roundtrip(&mut client);

    // Parent menu, grabbed.
    let menu = fix.open_popup_grabbed(
        &mut client,
        &app,
        Rectangle::new(Point::from((10, 10)), Size::from((1, 1))),
        Size::from((100, 150)),
        1,
    );
    fix.roundtrip(&mut client);
    assert!(fix.ewm_ref().popup_grab.is_some());

    // A submenu opened over an item -- must extend the grab, not dismiss it.
    let _sub = fix.open_nested_popup_grabbed(
        &mut client,
        &menu,
        Rectangle::new(Point::from((90, 10)), Size::from((1, 1))),
        Size::from((80, 120)),
        2,
    );
    fix.roundtrip(&mut client);

    assert!(
        fix.ewm_ref().popup_grab.is_some(),
        "a nested submenu must not dismiss the grab chain",
    );
    assert!(
        fix.ewm_ref().pointer.is_grabbed(),
        "the nested grab keeps the pointer grabbed",
    );
    let placements = collect_popup_placements(fix.ewm_ref(), world_rect(), None);
    assert!(
        placements.len() >= 2,
        "both parent menu and submenu should be placed, got {}",
        placements.len(),
    );
}
