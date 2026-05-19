//! Property tests for xdg popup positioning.
//!
//! Drives real xdg-shell clients via `TestClient` so popups land in the
//! actual `PopupManager`; `collect_popup_placements` is the same function the
//! render pipeline consumes, so a failing assertion here is a failing render.

use std::collections::HashSet;

use ewm_core::render::collect_popup_placements;
use ewm_core::strip::Frame;
use ewm_core::testing::{Fixture, Popup, TestClient, Toplevel};
use ewm_core::{LayoutEntry, ViewId};
use proptest::prelude::*;
use smithay::utils::{Logical, Point, Rectangle, Size};

fn nz(n: u64) -> ViewId {
    std::num::NonZeroU64::new(n).unwrap()
}

/// Window dimensions match the per-view entry dimensions so non-primary
/// views don't trigger the `window_geo / entry` pointer scaling. With
/// matching dimensions `pointer_scale = 1.0` even after
/// `refresh_primary_assignments` demotes all but one entry per surface.
const VIEW_W: i32 = 400;
const VIEW_H: i32 = 400;
fn view_size() -> Size<i32, Logical> {
    Size::from((VIEW_W, VIEW_H))
}

/// Bring up `HEAD-A` (1920x1080) and a toplevel; return the toplevel and its
/// server-side surface_id. Callers apply a layout *before* attaching popups,
/// otherwise `unconstrain_popup` falls back to a zero rect.
fn open_test_toplevel(fix: &mut Fixture, client: &mut TestClient) -> (Toplevel, u64) {
    fix.add_output("HEAD-A", 1920, 1080);
    fix.roundtrip(client);
    let toplevel = fix.open_toplevel(client, "ewm-test-toplevel", view_size());
    let sid = fix
        .find_surface_id(&toplevel.surface)
        .expect("server did not assign a surface_id to the test toplevel");
    (toplevel, sid)
}

fn open_test_popup(fix: &mut Fixture, client: &mut TestClient, parent: &Toplevel) -> Popup {
    let anchor = Rectangle::new(Point::from((100, 100)), Size::from((1, 1)));
    fix.open_popup(client, parent, anchor, Size::from((100, 100)))
}

fn head_a_rect() -> Rectangle<i32, Logical> {
    Rectangle::new(Point::from((0, 0)), (1920, 1080).into())
}

/// Two views of the same surface must produce two popup placements, one
/// anchored to each view. Currently fails: `window_global_position` returns
/// one entry per surface, so `collect_popup_placements` emits only one.
#[test]
fn multi_view_popup_renders_per_view() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    let (toplevel, sid) = open_test_toplevel(&mut fix, &mut client);
    apply_n_view_layout(&mut fix, sid, 2);
    let _popup = open_test_popup(&mut fix, &mut client, &toplevel);

    let placements = collect_popup_placements(fix.ewm_ref(), head_a_rect());
    assert_eq!(
        placements.len(),
        2,
        "expected one popup placement per view of the parent surface, got {}",
        placements.len(),
    );
    assert_ne!(
        placements[0].location, placements[1].location,
        "expected popup placements at distinct positions, both at {:?}",
        placements[0].location,
    );
}

/// Sanity: the test client actually delivers a toplevel into `id_windows`
/// and a popup into the server's `PopupManager`, with non-zero geometries.
#[test]
fn fixture_delivers_toplevel_and_popup() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.add_output("HEAD-A", 1920, 1080);
    fix.roundtrip(&mut client);

    let toplevel = fix.open_toplevel(&mut client, "ewm-test-toplevel", Size::from((800, 600)));
    let sid = fix
        .find_surface_id(&toplevel.surface)
        .expect("server did not assign a surface_id to the test toplevel");
    let anchor = Rectangle::new(Point::from((200, 150)), Size::from((1, 1)));
    let _popup = fix.open_popup(&mut client, &toplevel, anchor, Size::from((100, 100)));

    use smithay::desktop::PopupManager;
    use smithay::wayland::seat::WaylandFocus;

    let ewm = fix.ewm_ref();
    let window = ewm
        .id_windows
        .get(&sid)
        .expect("server did not insert toplevel into id_windows");
    let parent = window
        .wl_surface()
        .expect("toplevel had no wl_surface")
        .into_owned();
    let popups: Vec<_> = PopupManager::popups_for_surface(&parent).collect();
    assert_eq!(popups.len(), 1, "expected exactly one popup tracked");
    let (popup, _offset) = &popups[0];
    assert!(
        popup.geometry().size.w > 0,
        "popup geometry not committed: {:?}",
        popup.geometry(),
    );
}

/// Sanity: a single-view layout still gets exactly one popup placement.
#[test]
fn single_view_popup_renders_once() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    let (toplevel, sid) = open_test_toplevel(&mut fix, &mut client);
    apply_n_view_layout(&mut fix, sid, 1);
    let _popup = open_test_popup(&mut fix, &mut client, &toplevel);

    let placements = collect_popup_placements(fix.ewm_ref(), head_a_rect());
    assert_eq!(
        placements.len(),
        1,
        "expected exactly one popup placement for a single-view layout",
    );
}

/// Lay `surface_id` out as N views, one per frame on HEAD-A. Entry size
/// matches `VIEW` so non-primary scaling is a no-op (see `VIEW`'s comment).
fn apply_n_view_layout(fix: &mut Fixture, surface_id: u64, n: usize) {
    let frames: Vec<Frame> = (0..n)
        .map(|i| Frame {
            name: String::new(),
            surface_id: 100 + i as u64,
            view_id: nz(100 + i as u64),
            width: VIEW_W as f64,
            surfaces: vec![LayoutEntry {
                view_id: nz(1 + i as u64),
                surface_id,
                x: 0,
                y: 0,
                w: VIEW_W as u32,
                h: VIEW_H as u32,
                primary: true,
                fullscreen: false,
            }],
            selected_view_id: Some(nz(1 + i as u64)),
            move_anim: None,
        })
        .collect();
    fix.ewm().apply_output_layout("HEAD-A", frames);
}

proptest! {
    #![proptest_config(ProptestConfig {
        // Real Wayland roundtrips per case are expensive; keep cases small.
        cases: 16,
        ..ProptestConfig::default()
    })]

    /// For any view count N >= 1, `collect_popup_placements` returns N
    /// placements at distinct locations, and clicking the centre of each
    /// placement resolves to the popup's `WlSurface` -- not the parent
    /// toplevel. Pins both render (placement count + positions) and input
    /// (per-view hit-test) paths.
    #[test]
    fn popup_placements_and_hit_test_per_view(n in 1usize..=4) {
        let mut fix = Fixture::new().unwrap();
        let mut client = fix.new_client().unwrap();
        let (toplevel, sid) = open_test_toplevel(&mut fix, &mut client);
        apply_n_view_layout(&mut fix, sid, n);
        let _popup = open_test_popup(&mut fix, &mut client, &toplevel);

        let placements = collect_popup_placements(fix.ewm_ref(), head_a_rect());
        prop_assert_eq!(
            placements.len(),
            n,
            "expected {} placements for {} views, got {}",
            n, n, placements.len(),
        );
        let unique: HashSet<(i32, i32)> = placements
            .iter()
            .map(|p| (p.location.x, p.location.y))
            .collect();
        prop_assert_eq!(
            unique.len(),
            n,
            "expected {} distinct placement origins, got {} (positions: {:?})",
            n,
            unique.len(),
            placements.iter().map(|p| p.location).collect::<Vec<_>>(),
        );

        for placement in &placements {
            let centre = Point::from((
                (placement.location.x + placement.size.w / 2) as f64,
                (placement.location.y + placement.size.h / 2) as f64,
            ));
            let hit = fix.ewm_ref().layout_surface_under(centre);
            let (hit_surface, _local) = hit.ok_or_else(|| {
                TestCaseError::fail(format!("no surface under popup centre {:?}", centre))
            })?;
            prop_assert_eq!(
                hit_surface,
                placement.surface.clone(),
                "click at {:?} routed to a non-popup surface",
                centre,
            );
        }
    }
}
