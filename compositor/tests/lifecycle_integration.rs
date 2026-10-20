//! Integration tests for real xdg-shell lifecycle cleanup.

use std::collections::HashSet;

use ewm_core::event::Event;
use ewm_core::strip::Frame;
use ewm_core::testing::{Fixture, nz};
use ewm_core::{LayoutEntry, LayoutEntryId};
use smithay::utils::{Logical, Rectangle, Size};

const OUTPUT: &str = "HEAD-A";

fn layout_surface_ids(fix: &Fixture) -> HashSet<u64> {
    fix.ewm_ref()
        .layout
        .strips()
        .map(|(_, strip)| strip)
        .flat_map(|strip| strip.surface_entries().filter_map(LayoutEntry::surface_id))
        .collect()
}

fn frame_with_entry(frame_id: u64, entry_id: LayoutEntryId, surface_id: u64) -> Frame {
    Frame {
        focus_id: nz(200),
        width: 1920.0,
        entries: vec![LayoutEntry::surface(
            entry_id,
            "entry".to_string(),
            surface_id,
            0,
            0,
            400,
            300,
        )],
        selected_entry_id: Some(entry_id),
        ..Frame::new(frame_id)
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
    fix.tile(OUTPUT, vec![frame_with_entry(frame_id, entry_id, sid)]);
    fix.dispatch_roundtrip(2);

    let popup = fix.open_popup(
        &mut client,
        &toplevel,
        Rectangle::new((20, 20).into(), Size::from((20, 20))),
        Size::from((80, 80)),
    );

    assert_eq!(output_redraw_state(&fix, OUTPUT), "Idle");
    let renders = fix.render_count(OUTPUT);

    let buffer = client.create_shm_buffer(Size::<i32, Logical>::from((80, 80)));
    popup.surface.attach(Some(&buffer), 0, 0);
    popup.surface.damage_buffer(0, 0, 80, 80);
    popup.xdg_surface.set_window_geometry(0, 0, 80, 80);
    popup.surface.commit();
    fix.roundtrip(&mut client);

    assert!(fix.render_count(OUTPUT) > renders);
}

#[test]
fn losing_a_focused_toplevel_cleans_layout_focus_indexes_and_emits_close() {
    // The client destroys the toplevel, or disconnects with it still mapped.
    for disconnect in [false, true] {
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
        fix.tile(OUTPUT, vec![frame_with_entry(frame_id, entry_id, sid)]);
        fix.ewm().set_focus_entry(entry_id, "test", false);
        let _ = fix.drain_events();
        assert!(fix.has_surface(sid));
        assert!(layout_surface_ids(&fix).contains(&sid));
        assert!(fix.ewm_ref().surface_in_layout(sid));
        assert_eq!(fix.ewm_ref().focused_surface_id(), sid);

        if disconnect {
            drop(toplevel);
            drop(client);
            fix.dispatch_roundtrip(8);
        } else {
            toplevel.xdg_toplevel.destroy();
            toplevel.xdg_surface.destroy();
            toplevel.surface.destroy();
            fix.roundtrip(&mut client);
        }

        assert!(!fix.has_surface(sid), "disconnect={disconnect}");
        assert!(
            !layout_surface_ids(&fix).contains(&sid),
            "disconnect={disconnect}"
        );
        assert!(
            !fix.ewm_ref().surface_in_layout(sid),
            "disconnect={disconnect}"
        );
        assert_eq!(fix.ewm_ref().focused_frame_id(), frame_id);
        assert_eq!(fix.ewm_ref().focused_entry_id(), Some(entry_id));
        assert_eq!(fix.ewm_ref().focused_surface_id(), frame_id);
        let events = fix.drain_events();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Close { id } if *id == sid)),
            "losing toplevel {sid} (disconnect={disconnect}) did not emit Close; events: {events:?}",
        );
    }
}
