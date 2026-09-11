//! Integration tests for real xdg-shell lifecycle cleanup.

use std::collections::HashSet;

use ewm_core::event::Event;
use ewm_core::strip::Frame;
use ewm_core::testing::Fixture;
use ewm_core::{LayoutEntry, LayoutEntryId};
use smithay::utils::{Logical, Rectangle, Size};

const OUTPUT: &str = "HEAD-A";

fn nz(n: u64) -> LayoutEntryId {
    std::num::NonZeroU64::new(n).unwrap()
}

fn layout_surface_ids(fix: &Fixture) -> HashSet<u64> {
    fix.ewm_ref()
        .frame_set
        .mapped_strips()
        .values()
        .flat_map(|strip| strip.surface_entries().filter_map(LayoutEntry::surface_id))
        .collect()
}

fn frame_with_entry(frame_id: u64, entry_id: LayoutEntryId, surface_id: u64) -> Frame {
    Frame {
        name: String::new(),
        workspace_name: None,
        surface_id: frame_id,
        focus_id: nz(200),
        width: 1920.0,
        height: 0.0,
        entries: vec![LayoutEntry::surface(
            entry_id,
            "entry".to_string(),
            surface_id,
            0,
            0,
            400,
            300,
        )],
        fullscreen: None,
        selected_entry_id: Some(entry_id),
        move_anim: None,
        floating_pos: None,
    }
}

fn output_redraw_state(fix: &Fixture, output: &str) -> String {
    fix.debug_state()["redraw_states"]
        .as_array()
        .expect("redraw_states array")
        .iter()
        .find(|state| state["output"].as_str() == Some(output))
        .expect("output redraw state")["state"]
        .as_str()
        .expect("redraw state string")
        .to_string()
}

#[test]
fn destroying_real_toplevel_cleans_layout_focus_indexes_and_emits_close() {
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    fix.add_output(OUTPUT, 1920, 1080);

    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let toplevel = fix.open_toplevel(
        &mut client,
        "ewm-lifecycle-test",
        Size::<i32, Logical>::from((400, 300)),
    );
    let sid = fix
        .find_surface_id(&client, &toplevel.surface)
        .expect("server did not assign a surface id");

    let frame_id = 100;
    let entry_id = nz(1);
    fix.ewm()
        .apply_output_layout(OUTPUT, vec![frame_with_entry(frame_id, entry_id, sid)]);
    fix.ewm().set_focus_entry(entry_id, "test", false);
    let _ = fix.drain_events();

    assert!(fix.has_surface(sid));
    assert!(layout_surface_ids(&fix).contains(&sid));
    assert!(fix.ewm_ref().surface_outputs.contains_key(&sid));
    assert_eq!(fix.ewm_ref().focused_surface_id(), sid);

    toplevel.xdg_toplevel.destroy();
    toplevel.xdg_surface.destroy();
    toplevel.surface.destroy();
    fix.roundtrip(&mut client);

    assert!(!fix.has_surface(sid));
    assert!(!layout_surface_ids(&fix).contains(&sid));
    assert!(!fix.ewm_ref().surface_outputs.contains_key(&sid));
    assert_eq!(fix.ewm_ref().focused_frame_id, frame_id);
    assert_eq!(fix.ewm_ref().focused_entry_id, Some(entry_id));
    assert_eq!(fix.ewm_ref().focused_surface_id(), frame_id);

    let events = fix.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Close { id } if *id == sid)),
        "destroying toplevel {sid} did not emit Close; events: {events:?}",
    );
}

#[test]
fn popup_commit_queues_redraw_for_owning_output() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(OUTPUT, 1920, 1080);

    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let toplevel = fix.open_toplevel(
        &mut client,
        "ewm-popup-redraw-test",
        Size::<i32, Logical>::from((400, 300)),
    );
    let sid = fix
        .find_surface_id(&client, &toplevel.surface)
        .expect("server did not assign a surface id");

    let frame_id = 100;
    let entry_id = nz(1);
    fix.ewm()
        .apply_output_layout(OUTPUT, vec![frame_with_entry(frame_id, entry_id, sid)]);
    fix.dispatch_roundtrip(2);

    let popup = fix.open_popup(
        &mut client,
        &toplevel,
        Rectangle::new((20, 20).into(), Size::from((20, 20))),
        Size::from((80, 80)),
    );

    assert_eq!(output_redraw_state(&fix, OUTPUT), "Idle");
    fix.ewm().monitors_active = false;

    let buffer = client.create_shm_buffer(Size::<i32, Logical>::from((80, 80)));
    popup.surface.attach(Some(&buffer), 0, 0);
    popup.surface.damage_buffer(0, 0, 80, 80);
    popup.xdg_surface.set_window_geometry(0, 0, 80, 80);
    popup.surface.commit();
    fix.roundtrip(&mut client);

    assert_eq!(output_redraw_state(&fix, OUTPUT), "Queued");
}

#[test]
fn disconnecting_focused_toplevel_cleans_layout_focus_indexes_and_emits_close() {
    let mut fix = Fixture::new().unwrap();
    fix.enable_event_capture();
    fix.add_output(OUTPUT, 1920, 1080);

    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let toplevel = fix.open_toplevel(
        &mut client,
        "ewm-disconnect-test",
        Size::<i32, Logical>::from((400, 300)),
    );
    let sid = fix
        .find_surface_id(&client, &toplevel.surface)
        .expect("server did not assign a surface id");

    let frame_id = 100;
    let entry_id = nz(1);
    fix.ewm()
        .apply_output_layout(OUTPUT, vec![frame_with_entry(frame_id, entry_id, sid)]);
    fix.ewm().set_focus_entry(entry_id, "test", false);
    let _ = fix.drain_events();

    assert_eq!(fix.ewm_ref().focused_surface_id(), sid);

    drop(toplevel);
    drop(client);
    fix.dispatch_roundtrip(8);

    assert!(!fix.has_surface(sid));
    assert!(!layout_surface_ids(&fix).contains(&sid));
    assert!(!fix.ewm_ref().surface_outputs.contains_key(&sid));
    assert_eq!(fix.ewm_ref().focused_frame_id, frame_id);
    assert_eq!(fix.ewm_ref().focused_entry_id, Some(entry_id));
    assert_eq!(fix.ewm_ref().focused_surface_id(), frame_id);

    let events = fix.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Close { id } if *id == sid)),
        "disconnecting toplevel {sid} did not emit Close; events: {events:?}",
    );
}
