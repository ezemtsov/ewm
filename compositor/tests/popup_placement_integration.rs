//! Popups on an Emacs frame's own surface unconstrain against the working
//! area at the frame's screen position, not a screen-sized box at its origin.

use ewm_core::render::collect_popup_placements;
use ewm_core::strip::Frame;
use ewm_core::testing::{Fixture, Popup, TestClient, Toplevel};
use smithay::reexports::wayland_server::Resource as _;
use smithay::utils::{Point, Rectangle, Size};
use wayland_client::Proxy as _;

const HEAD: &str = "HEAD-A";

/// Two 800px Emacs frames side by side on a 1600x900 output.
fn scene() -> (Fixture, TestClient, Toplevel) {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output(HEAD, 1600, 900);

    let left = fix.open_toplevel(&mut client, "left", Size::from((800, 900)));
    let right = fix.open_toplevel(&mut client, "right", Size::from((800, 900)));
    let left_sid = fix.find_surface_id(&client, &left.surface).unwrap();
    let right_sid = fix.find_surface_id(&client, &right.surface).unwrap();

    let mut frames = vec![Frame::new(left_sid), Frame::new(right_sid)];
    for frame in &mut frames {
        frame.width = 800.0;
    }
    fix.ewm().apply_output_layout(HEAD, frames);
    fix.ewm().animations_clock.set_complete_instantly(true);
    for strip in fix.ewm().frame_set.mapped_strips_mut() {
        strip.advance();
    }
    fix.roundtrip(&mut client);
    (fix, client, right)
}

fn placement(fix: &Fixture, popup: &Popup) -> Point<i32, smithay::utils::Logical> {
    let ewm = fix.ewm_ref();
    collect_popup_placements(ewm, Rectangle::from_size(Size::from((1600, 900))), None)
        .into_iter()
        .find(|p| p.surface.id().protocol_id() == popup.surface.id().protocol_id())
        .expect("popup placement")
        .location
}

#[test]
fn frame_popup_slides_back_onto_the_screen() {
    let (mut fix, mut client, right) = scene();
    let anchor = Rectangle::new(Point::from((700, 100)), Size::from((1, 1)));
    let popup = fix.open_sliding_popup(&mut client, &right, anchor, Size::from((200, 100)));
    // Right frame starts at x=800; the 8px padded working area ends at 1592.
    assert_eq!(placement(&fix, &popup), Point::from((1392, 101)));
}

#[test]
fn frame_popup_may_overhang_a_neighbouring_frame() {
    let (mut fix, mut client, right) = scene();
    let anchor = Rectangle::new(Point::from((-100, 100)), Size::from((1, 1)));
    let popup = fix.open_sliding_popup(&mut client, &right, anchor, Size::from((50, 50)));
    assert_eq!(placement(&fix, &popup), Point::from((700, 101)));
}
