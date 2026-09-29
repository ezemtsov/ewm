//! Auto-float: portal windows, dialogs (parented) and fixed-height windows
//! open floating; ordinary windows tile. The decision is taken at map, once
//! the client has committed its xdg min/max size hints.

use ewm_core::event::Event;
use ewm_core::testing::Fixture;
use smithay::utils::{Logical, Size};

const OUTPUT: &str = "HEAD-A";

/// The (open_floating, width, height) of the surface's `Mapped` event, or None
/// if it never mapped (the auto-float decision rides on `Mapped`, emitted at map).
fn mapped(fix: &mut Fixture, sid: u64) -> Option<(bool, Option<f64>, Option<f64>)> {
    fix.drain_events().iter().find_map(|event| match event {
        Event::Mapped {
            id,
            open_floating,
            width,
            height,
            ..
        } if *id == sid => Some((*open_floating, *width, *height)),
        _ => None,
    })
}

fn mapped_open_floating(fix: &mut Fixture, sid: u64) -> Option<bool> {
    mapped(fix, sid).map(|m| m.0)
}

#[test]
fn new_precedes_title_and_mapped() {
    // `New` is the "buffer exists" signal; per-surface events like `title` and
    // the placement `mapped` must follow it, or Emacs drops them (no buffer yet).
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    fix.add_output(OUTPUT, 1920, 1080);
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    // create_toplevel sets a title in its initial (pre-map) commit.
    let top = fix.open_toplevel(
        &mut client,
        "with-title",
        Size::<i32, Logical>::from((400, 300)),
    );
    let sid = fix.find_surface_id(&client, &top.surface).unwrap();

    let order: Vec<&str> = fix
        .drain_events()
        .iter()
        .filter_map(|e| match e {
            Event::New { id, .. } if *id == sid => Some("new"),
            Event::Title { id, .. } if *id == sid => Some("title"),
            Event::Mapped { id, .. } if *id == sid => Some("mapped"),
            _ => None,
        })
        .collect();
    let pos = |k| order.iter().position(|x| *x == k);
    assert_eq!(
        pos("new"),
        Some(0),
        "New must be first for a surface; got {order:?}"
    );
    if let (Some(n), Some(t)) = (pos("new"), pos("title")) {
        assert!(n < t, "New must precede Title; got {order:?}");
    }
    if let (Some(n), Some(m)) = (pos("new"), pos("mapped")) {
        assert!(n < m, "New must precede Mapped; got {order:?}");
    }
}

#[test]
fn plain_toplevel_does_not_auto_float() {
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    fix.add_output(OUTPUT, 1920, 1080);

    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let toplevel = fix.open_toplevel(&mut client, "plain", Size::<i32, Logical>::from((400, 300)));
    let sid = fix
        .find_surface_id(&client, &toplevel.surface)
        .expect("server did not assign a surface id");

    assert_eq!(
        mapped_open_floating(&mut fix, sid),
        Some(false),
        "a resizable, parentless window must tile, not float",
    );
}

#[test]
fn fixed_height_toplevel_auto_floats() {
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    fix.add_output(OUTPUT, 1920, 1080);

    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let toplevel =
        fix.open_toplevel_fixed_size(&mut client, "fixed", Size::<i32, Logical>::from((400, 300)));
    let sid = fix
        .find_surface_id(&client, &toplevel.surface)
        .expect("server did not assign a surface id");

    assert_eq!(
        mapped_open_floating(&mut fix, sid),
        Some(true),
        "a fixed-height window (min.h == max.h) must open floating",
    );
}

#[test]
fn dialog_with_parent_auto_floats() {
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    fix.add_output(OUTPUT, 1920, 1080);

    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let parent = fix.open_toplevel(
        &mut client,
        "parent",
        Size::<i32, Logical>::from((800, 600)),
    );
    let dialog = fix.open_toplevel_with_parent(
        &mut client,
        "dialog",
        &parent,
        Size::<i32, Logical>::from((400, 300)),
    );
    let sid = fix
        .find_surface_id(&client, &dialog.surface)
        .expect("server did not assign a surface id");

    assert_eq!(
        mapped_open_floating(&mut fix, sid),
        Some(true),
        "a toplevel with a parent (dialog) must open floating",
    );
}

#[test]
fn portal_toplevel_auto_floats() {
    // Socketpair clients carry no identity; only the portal tag makes this float.
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    fix.add_output(OUTPUT, 1920, 1080);
    fix.ewm().set_emacs_pid(std::process::id());

    let mut client = fix.new_portal_client().unwrap();
    fix.roundtrip(&mut client);
    let toplevel = fix.open_toplevel(
        &mut client,
        "portal",
        Size::<i32, Logical>::from((400, 300)),
    );
    let sid = fix
        .find_surface_id(&client, &toplevel.surface)
        .expect("server did not assign a surface id");

    assert_eq!(
        mapped_open_floating(&mut fix, sid),
        Some(true),
        "a portal window must open floating even without a parent or size hints",
    );
}

#[test]
fn title_set_on_mapping_commit_precedes_mapped() {
    // A placement rule keyed on the title must see a title the client sets
    // together with its first buffer, even one replacing an earlier title.
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    fix.add_output(OUTPUT, 1920, 1080);
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let top = fix.open_toplevel_retitled(
        &mut client,
        "early",
        "late",
        Size::<i32, Logical>::from((400, 300)),
    );
    let sid = fix.find_surface_id(&client, &top.surface).unwrap();

    let order: Vec<&str> = fix
        .drain_events()
        .iter()
        .filter_map(|e| match e {
            Event::Title { id, title, .. } if *id == sid && title == "late" => Some("title"),
            Event::Mapped { id, .. } if *id == sid => Some("mapped"),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        ["title", "mapped"],
        "a same-commit title must precede Mapped"
    );
}

#[test]
fn mapped_carries_size_of_tiled_toplevel_keeping_its_own() {
    // A Lisp rule may float a window the compositor would tile. The size is
    // reported when the client kept its own instead of filling the maximize configure.
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    fix.add_output(OUTPUT, 1920, 1080);
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let top = fix.open_toplevel(&mut client, "plain", Size::<i32, Logical>::from((400, 300)));
    let sid = fix.find_surface_id(&client, &top.surface).unwrap();

    assert_eq!(
        mapped(&mut fix, sid),
        Some((false, Some(400.0), Some(300.0)))
    );
}
