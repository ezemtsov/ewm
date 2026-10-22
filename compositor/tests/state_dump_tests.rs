//! Tests for the JSON shown by `ewm-show-state`.

use ewm_core::layout::strip::Frame;
use ewm_core::testing::{Fixture, nz};
use ewm_core::{FocusSource, LayoutEntry, Notify};
use serde_json::{Value, json};
use smithay::utils::Size;

const HEAD_A: &str = "HEAD-A";
const HEAD_B: &str = "HEAD-B";

fn frame(surface_id: u64) -> Frame {
    let mut frame = Frame::new(surface_id);
    frame.width = 800.0;
    frame
}

fn frame_with_surface_entry(frame_id: u64, entry_id: u64, surface_id: u64) -> Frame {
    let mut frame = frame(frame_id);
    let entry_id = nz(entry_id);
    frame.entries.push(LayoutEntry::surface(
        entry_id,
        format!("surface-{surface_id}"),
        surface_id,
        0,
        0,
        800,
        600,
    ));
    frame.selected_entry_id = Some(entry_id);
    frame
}

fn workspace_protocol(state: &Value) -> &Value {
    state
        .get("workspace_protocol")
        .expect("state dump missing workspace_protocol")
}

fn workspace_for_surface(state: &Value, surface_id: u64) -> &Value {
    workspace_protocol(state)["workspaces"]
        .as_array()
        .expect("workspace_protocol.workspaces must be an array")
        .iter()
        .find(|ws| ws["surface_id"] == surface_id)
        .unwrap_or_else(|| panic!("workspace for surface {surface_id} not found"))
}

#[test]
fn workspace_protocol_reports_renamed_workspace_and_preserves_name_across_layouts() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1600, 900);

    fix.tile(HEAD_A, vec![frame(100), frame(101)]);
    assert!(fix.ewm().rename_workspace(Some(101), "code".to_string()));
    fix.dispatch();

    let state = fix.debug_state();
    let unnamed = workspace_for_surface(&state, 100);
    let named = workspace_for_surface(&state, 101);
    assert_eq!(unnamed["id"], Value::Null);
    assert_eq!(unnamed["name"], "1");
    assert_eq!(named["id"], "code");
    assert_eq!(named["name"], "code");

    assert!(!fix.ewm().rename_workspace(Some(100), "CODE".to_string()));

    fix.tile(HEAD_A, vec![frame(100), frame(101)]);
    fix.dispatch();

    let state = fix.debug_state();
    let named = workspace_for_surface(&state, 101);
    assert_eq!(named["id"], "code");
    assert_eq!(named["name"], "code");

    fix.remove_output(HEAD_A);
    fix.dispatch();

    let state = fix.debug_state();
    let moved = workspace_for_surface(&state, 101);
    assert_eq!(moved["output"], HEAD_B);
    assert_eq!(moved["id"], "code");
    assert_eq!(moved["name"], "code");

    assert!(fix.ewm().rename_workspace(Some(101), "   ".to_string()));
    fix.dispatch();

    let state = fix.debug_state();
    let cleared = workspace_for_surface(&state, 101);
    assert_eq!(cleared["id"], Value::Null);
    assert_eq!(cleared["name"], "2");
}

#[test]
fn workspace_protocol_reports_urgent_workspace_from_surface_attention() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output(HEAD_A, 1600, 900);
    let urgent_id = fix.open_toplevel_id(&mut client, "urgent", Size::from((400, 400)));
    let focused_id = fix.open_toplevel_id(&mut client, "focused", Size::from((400, 400)));

    fix.tile(
        HEAD_A,
        vec![
            frame_with_surface_entry(100, 1001, urgent_id),
            frame_with_surface_entry(101, 1002, focused_id),
        ],
    );
    fix.ewm()
        .set_focus_entry(nz(1002), FocusSource::Test, Notify::None);
    fix.ewm().set_surface_urgent(urgent_id, true);
    fix.dispatch();

    let state = fix.debug_state();
    let urgent = workspace_for_surface(&state, 100);
    let inactive = workspace_for_surface(&state, 101);
    assert_eq!(state["urgent_surfaces"], json!([urgent_id]));
    assert_eq!(urgent["urgent"], true);
    assert_eq!(inactive["urgent"], false);

    fix.ewm()
        .set_focus_entry(nz(1001), FocusSource::Test, Notify::None);
    fix.dispatch();

    let state = fix.debug_state();
    let urgent = workspace_for_surface(&state, 100);
    assert_eq!(state["urgent_surfaces"], json!([]));
    assert_eq!(urgent["urgent"], false);
}

#[test]
fn workspace_protocol_reports_urgent_workspace_from_explicit_mark() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output(HEAD_A, 1600, 900);
    let focused_id = fix.open_toplevel_id(&mut client, "focused", Size::from((400, 400)));
    let urgent_id = fix.open_toplevel_id(&mut client, "urgent", Size::from((400, 400)));

    fix.tile(HEAD_A, vec![frame(focused_id), frame(urgent_id)]);
    fix.ewm()
        .set_focus_frame(focused_id, FocusSource::Test, Notify::None);
    assert!(fix.ewm().mark_workspace_urgent(Some(urgent_id)));
    assert!(!fix.ewm().mark_workspace_urgent(Some(focused_id)));
    fix.dispatch();

    let state = fix.debug_state();
    let focused = workspace_for_surface(&state, focused_id);
    let urgent = workspace_for_surface(&state, urgent_id);
    assert_eq!(state["urgent_workspaces"], json!([urgent_id]));
    assert_eq!(focused["urgent"], false);
    assert_eq!(urgent["urgent"], true);

    fix.ewm()
        .set_focus_frame(urgent_id, FocusSource::Test, Notify::None);
    fix.dispatch();

    let state = fix.debug_state();
    let urgent = workspace_for_surface(&state, urgent_id);
    assert_eq!(state["urgent_workspaces"], json!([]));
    assert_eq!(urgent["urgent"], false);
}
