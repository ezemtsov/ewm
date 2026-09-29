//! Tests for the JSON shown by `ewm-show-state`.

use std::collections::HashSet;
use std::num::NonZeroU64;

use ewm_core::LayoutEntry;
use ewm_core::cursor::CursorConfig;
use ewm_core::strip::Frame;
use ewm_core::testing::Fixture;
use serde_json::{Value, json};

const HEAD_A: &str = "HEAD-A";
const HEAD_B: &str = "HEAD-B";

fn frame(surface_id: u64) -> Frame {
    let mut frame = Frame::new(surface_id);
    frame.width = 800.0;
    frame
}

fn nz(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
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
            let expected_name = frame
                .get("workspace_name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| (idx + 1).to_string());
            let expected_id = frame
                .get("workspace_name")
                .and_then(Value::as_str)
                .map(Value::from)
                .unwrap_or(Value::Null);
            assert_eq!(workspace["id"], expected_id);
            assert_eq!(workspace["name"], json!(expected_name));
            assert_eq!(workspace["coordinates"], json!([0, idx as u32]));
            assert_eq!(workspace["active"], idx == active_idx);
            assert_eq!(workspace["urgent"], false);
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

    let state = fix.debug_state();
    assert_workspace_protocol_matches_strips(&state);
    let focused = workspace_for_surface(&state, 101);
    assert_eq!(focused["output"], HEAD_A);
    assert_eq!(focused["active"], true);

    fix.ewm().apply_output_layout(HEAD_B, vec![frame(101)]);
    fix.dispatch();

    let state = fix.debug_state();
    assert_workspace_protocol_matches_strips(&state);
    let moved = workspace_for_surface(&state, 101);
    assert_eq!(moved["output"], HEAD_B);
    assert_eq!(moved["coordinates"], json!([0, 0]));
    assert_eq!(moved["active"], true);
}

#[test]
fn workspace_protocol_reports_renamed_workspace_and_preserves_name_across_layouts() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1600, 900);

    fix.ewm()
        .apply_output_layout(HEAD_A, vec![frame(100), frame(101)]);
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

    fix.ewm()
        .apply_output_layout(HEAD_A, vec![frame(100), frame(101)]);
    fix.dispatch();

    let state = fix.debug_state();
    let named = workspace_for_surface(&state, 101);
    assert_eq!(named["id"], "code");
    assert_eq!(named["name"], "code");

    fix.ewm().apply_output_layout(HEAD_B, vec![frame(101)]);
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
    assert_eq!(cleared["name"], "1");
}

#[test]
fn workspace_protocol_reports_urgent_workspace_from_surface_attention() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);

    fix.ewm().apply_output_layout(
        HEAD_A,
        vec![
            frame_with_surface_entry(100, 1001, 200),
            frame_with_surface_entry(101, 1002, 201),
        ],
    );
    fix.ewm().set_focus_entry(nz(1002), "test", false);
    fix.ewm().set_surface_urgent(200, true);
    fix.dispatch();

    let state = fix.debug_state();
    let urgent = workspace_for_surface(&state, 100);
    let inactive = workspace_for_surface(&state, 101);
    assert_eq!(state["urgent_surfaces"], json!([200]));
    assert_eq!(urgent["urgent"], true);
    assert_eq!(inactive["urgent"], false);

    fix.ewm().set_focus_entry(nz(1001), "test", false);
    fix.dispatch();

    let state = fix.debug_state();
    let urgent = workspace_for_surface(&state, 100);
    assert_eq!(state["urgent_surfaces"], json!([]));
    assert_eq!(urgent["urgent"], false);
}

#[test]
fn workspace_protocol_reports_urgent_workspace_from_explicit_mark() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);

    fix.ewm()
        .apply_output_layout(HEAD_A, vec![frame(100), frame(101)]);
    fix.ewm().set_focus_frame(100, "test", false);
    assert!(fix.ewm().mark_workspace_urgent(Some(101)));
    assert!(!fix.ewm().mark_workspace_urgent(Some(100)));
    fix.dispatch();

    let state = fix.debug_state();
    let focused = workspace_for_surface(&state, 100);
    let urgent = workspace_for_surface(&state, 101);
    assert_eq!(state["urgent_workspaces"], json!([101]));
    assert_eq!(focused["urgent"], false);
    assert_eq!(urgent["urgent"], true);

    fix.ewm().set_focus_frame(101, "test", false);
    fix.dispatch();

    let state = fix.debug_state();
    let urgent = workspace_for_surface(&state, 101);
    assert_eq!(state["urgent_workspaces"], json!([]));
    assert_eq!(urgent["urgent"], false);
}

#[test]
fn ewm_state_reports_explicit_cursor_config() {
    let fix =
        Fixture::new_with_cursor_config(CursorConfig::new("ConfiguredCursorTheme", 37)).unwrap();
    let cursor = &fix.debug_state()["cursor"]["image"];

    assert_eq!(cursor["provider"], "named");
    assert_eq!(cursor["theme"], "ConfiguredCursorTheme");
    assert_eq!(cursor["configured_size"], 37);
}
