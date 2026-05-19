//! Property tests for xdg popup positioning.
//!
//! Drives real xdg-shell clients via `TestClient` so popups land in the
//! actual `PopupManager`; `collect_popup_placements` is the same function the
//! render pipeline consumes, so a failing assertion here is a failing render.

use std::collections::HashSet;

use ewm_core::render::collect_popup_placements;
use ewm_core::strip::Frame;
use ewm_core::testing::{Fixture, TestClient, Toplevel};
use ewm_core::{LayoutEntry, ViewId};
use proptest::prelude::*;
use smithay::utils::{Logical, Point, Rectangle, Size};

fn nz(n: u64) -> ViewId {
    std::num::NonZeroU64::new(n).unwrap()
}

/// Toplevel with a popup attached, paired with the server-side surface_id.
struct ParentedPopup {
    surface_id: u64,
    _toplevel: Toplevel,
}

/// Bring up `HEAD-A` (1920x1080), spawn a toplevel, attach a popup at
/// (200, 150) with size 100x100. Returns the server-side surface_id assigned
/// to the toplevel.
fn spawn_toplevel_with_popup(fix: &mut Fixture, client: &mut TestClient) -> ParentedPopup {
    fix.add_output("HEAD-A", 1920, 1080);
    fix.roundtrip(client);

    let toplevel = fix.open_toplevel(client, "ewm-test-toplevel", Size::from((800, 600)));
    let surface_id = fix
        .find_surface_id(&toplevel.surface)
        .expect("server did not assign a surface_id to the test toplevel");
    let anchor = Rectangle::new(Point::from((200, 150)), Size::from((1, 1)));
    let _popup = fix.open_popup(client, &toplevel, anchor, Size::from((100, 100)));

    ParentedPopup {
        surface_id,
        _toplevel: toplevel,
    }
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
    let parented = spawn_toplevel_with_popup(&mut fix, &mut client);

    apply_n_view_layout(&mut fix, parented.surface_id, 2);

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
    let parented = spawn_toplevel_with_popup(&mut fix, &mut client);

    let frames = vec![Frame {
        name: String::new(),
        surface_id: 100,
        view_id: nz(100),
        width: 1920.0,
        surfaces: vec![LayoutEntry {
            view_id: nz(1),
            surface_id: parented.surface_id,
            x: 0,
            y: 0,
            w: 800,
            h: 600,
            primary: true,
            fullscreen: false,
        }],
        selected_view_id: Some(nz(1)),
        move_anim: None,
    }];
    fix.ewm().apply_output_layout("HEAD-A", frames);

    let placements = collect_popup_placements(fix.ewm_ref(), head_a_rect());
    assert_eq!(
        placements.len(),
        1,
        "expected exactly one popup placement for a single-view layout",
    );
}

/// Lay `surface_id` out as N views, one per frame on HEAD-A. Frames are
/// 480px wide so a 4-view grid still fits inside the 1920px output.
fn apply_n_view_layout(fix: &mut Fixture, surface_id: u64, n: usize) {
    let frames: Vec<Frame> = (0..n)
        .map(|i| Frame {
            name: String::new(),
            surface_id: 100 + i as u64,
            view_id: nz(100 + i as u64),
            width: 480.0,
            surfaces: vec![LayoutEntry {
                view_id: nz(1 + i as u64),
                surface_id,
                x: 0,
                y: 0,
                w: 400,
                h: 400,
                primary: i == 0,
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

    /// For any view count N >= 1, `collect_popup_placements` returns exactly
    /// N placements at N distinct locations. Currently fails for N > 1.
    #[test]
    fn popup_placements_track_view_count(n in 1usize..=4) {
        let mut fix = Fixture::new().unwrap();
        let mut client = fix.new_client().unwrap();
        let parented = spawn_toplevel_with_popup(&mut fix, &mut client);

        apply_n_view_layout(&mut fix, parented.surface_id, n);

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
    }
}
