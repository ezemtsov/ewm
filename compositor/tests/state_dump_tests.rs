//! Tests for the JSON shown by `ewm-show-state`.

use std::collections::HashSet;

use ewm_core::strip::Frame;
use ewm_core::testing::Fixture;
use serde_json::{json, Value};

const HEAD_A: &str = "HEAD-A";
const HEAD_B: &str = "HEAD-B";

fn frame(surface_id: u64) -> Frame {
    let mut frame = Frame::new(surface_id);
    frame.width = 800.0;
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

fn assert_workspace_protocol_matches_strips(state: &Value) {
    let strips = state["output_strips"]
        .as_object()
        .expect("output_strips must be an object");
    let protocol = workspace_protocol(state);
    let workspaces = protocol["workspaces"]
        .as_array()
        .expect("workspace_protocol.workspaces must be an array");

    let mut output_names: Vec<_> = strips.keys().cloned().collect();
    output_names.sort();
    assert_eq!(protocol["groups"], json!(output_names));

    let expected_workspace_count = strips
        .values()
        .map(|strip| strip["frames"].as_array().unwrap().len())
        .sum::<usize>();
    assert_eq!(workspaces.len(), expected_workspace_count);

    let mut seen_surface_ids = HashSet::new();
    for (output, strip) in strips {
        let active_idx = strip["active_idx"]
            .as_u64()
            .expect("strip active_idx must be numeric") as usize;
        let frames = strip["frames"]
            .as_array()
            .expect("strip frames must be an array");

        for (idx, frame) in frames.iter().enumerate() {
            let surface_id = frame["surface_id"]
                .as_u64()
                .expect("frame surface_id must be numeric");
            assert!(
                seen_surface_ids.insert(surface_id),
                "duplicate frame surface_id {surface_id} in output_strips"
            );

            let workspace = workspace_for_surface(state, surface_id);
            assert_eq!(workspace["output"], output.as_str());
            assert_eq!(workspace["id"], surface_id.to_string());
            assert_eq!(workspace["name"], (idx + 1).to_string());
            assert_eq!(workspace["coordinates"], json!([0, idx as u32]));
            assert_eq!(workspace["active"], idx == active_idx);
        }
    }
}

#[test]
fn ewm_state_reports_workspace_protocol_from_frame_strips() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1600, 900);

    fix.ewm()
        .apply_output_layout(HEAD_A, vec![frame(100), frame(101), frame(102)]);
    fix.ewm().set_focus_frame(101, "test", false);
    fix.dispatch();

    let state = fix.ewm_ref().debug_state();
    assert_workspace_protocol_matches_strips(&state);
    let focused = workspace_for_surface(&state, 101);
    assert_eq!(focused["output"], HEAD_A);
    assert_eq!(focused["active"], true);

    fix.ewm().apply_output_layout(HEAD_B, vec![frame(101)]);
    fix.dispatch();

    let state = fix.ewm_ref().debug_state();
    assert_workspace_protocol_matches_strips(&state);
    let moved = workspace_for_surface(&state, 101);
    assert_eq!(moved["output"], HEAD_B);
    assert_eq!(moved["coordinates"], json!([0, 0]));
    assert_eq!(moved["active"], true);
}
