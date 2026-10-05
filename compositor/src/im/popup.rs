//! Input method popup placement. Ported from niri's `unconstrain_popup`.

use smithay::desktop::utils::bbox_from_surface_tree;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::input_method::PopupSurface;

/// Place `popup` for its text cursor rectangle, inside `target`.
pub fn place(popup: &PopupSurface, target: Rectangle<i32, Logical>) {
    let rect = popup.text_input_rectangle();
    let bbox = bbox_from_surface_tree(popup.wl_surface(), rect.loc);
    popup.set_location(location(target, rect, bbox));
}

/// Where a popup of extent `bbox`, already at `rect.loc`, goes for the text cursor `rect`:
/// below it inside `target`, above it when below would overflow. Parent surface coordinates.
pub fn location(
    target: Rectangle<i32, Logical>,
    rect: Rectangle<i32, Logical>,
    mut bbox: Rectangle<i32, Logical>,
) -> Point<i32, Logical> {
    let overflow_x = (bbox.loc.x + bbox.size.w) - (target.loc.x + target.size.w);
    if overflow_x > 0 {
        bbox.loc.x -= overflow_x;
    }
    bbox.loc.x = bbox.loc.x.max(target.loc.x);

    let below = bbox.loc.y + rect.size.h;
    let y = if target.loc.y + target.size.h >= below + bbox.size.h {
        below
    } else {
        bbox.loc.y - bbox.size.h
    };
    Point::from((bbox.loc.x, y))
}
