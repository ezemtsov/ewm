//! Properties for input method popup placement.

use ewm_core::im::popup;
use proptest::prelude::*;
use smithay::utils::{Logical, Point, Rectangle, Size};

fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(Point::from((x, y)), Size::from((w, h)))
}

proptest! {
    #[test]
    fn popup_follows_cursor_inside_target(
        (tx, ty, tw, th) in (-500..500, -500..500, 1..2000, 1..2000),
        (cx, cy, cw, ch) in (-600..2600, -600..2600, 0..100, 1..100),
        (ox, oy) in (-50..50, -50..50),
        (pw, ph) in (1..1000, 1..1000),
    ) {
        let target = rect(tx, ty, tw, th);
        let cursor = rect(cx, cy, cw, ch);
        let bbox = rect(cx + ox, cy + oy, pw, ph);
        let loc = popup::location(target, cursor, bbox);

        if pw <= tw {
            prop_assert!(loc.x >= tx);
            prop_assert!(loc.x + pw <= tx + tw);
        }

        let below = bbox.loc.y + ch;
        if ty + th >= below + ph {
            prop_assert_eq!(loc.y, below);
        } else {
            prop_assert_eq!(loc.y, bbox.loc.y - ph);
        }
    }
}
