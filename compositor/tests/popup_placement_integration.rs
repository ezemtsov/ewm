//! Popups on an Emacs frame's own surface unconstrain against the working
//! area at the frame's screen position, not a screen-sized box at its origin.

use ewm_core::layout::strip::Frame;
use ewm_core::render::collect_popup_placements;
use ewm_core::testing::{Fixture, Popup, TestClient, Toplevel, proptest_cases};
use proptest::prelude::*;
use smithay::reexports::wayland_server::Resource as _;
use smithay::utils::{Logical, Point, Rectangle, Size};
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
    fix.tile(HEAD, frames);
    fix.ewm().animations_clock.set_complete_instantly(true);
    for strip in fix.ewm().layout.strips_mut() {
        strip.advance();
    }
    fix.roundtrip(&mut client);
    (fix, client, right)
}

fn placement(fix: &Fixture, popup: &Popup) -> Point<i32, Logical> {
    let ewm = fix.ewm_ref();
    collect_popup_placements(ewm, Rectangle::from_size(Size::from((1600, 900))), None)
        .into_iter()
        .find(|p| p.surface.id().protocol_id() == popup.surface.id().protocol_id())
        .expect("popup placement")
        .location
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases(24)))]

    /// The popup hangs below its anchor in the right frame and slides back only as far as the
    /// working area asks, over the neighbouring frame when that is where it lands.
    #[test]
    fn a_frame_popup_slides_back_onto_the_screen(
        anchor in (-400i32..=1200, 0i32..=900),
        size in (1i32..=300, 1i32..=300),
    ) {
        let (mut fix, mut client, right) = scene();
        let anchor = Rectangle::new(Point::from(anchor), Size::from((1, 1)));
        let popup = fix.open_sliding_popup(&mut client, &right, anchor, Size::from(size));
        // Popups keep 8px off the edges of the working area.
        let area = fix.ewm_ref().get_working_area(&fix.output(HEAD));
        let area = Rectangle::new(area.loc + Point::from((8, 8)), area.size - Size::from((16, 16)));
        let fits = |at: Point<i32, Logical>| area.contains_rect(Rectangle::new(at, Size::from(size)));
        let wanted = Point::from((800 + anchor.loc.x, anchor.loc.y + 1));
        let placed = placement(&fix, &popup);
        prop_assert!(fits(placed), "{:?} outside {:?}", placed, area);
        if fits(wanted) {
            prop_assert_eq!(placed, wanted);
        }
    }
}
