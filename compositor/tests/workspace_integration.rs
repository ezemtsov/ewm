//! What an ext-workspace-v1 client sees of the strips: groups follow the output object, names
//! and urgency follow the frames.

use ewm_core::layout::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient, frame_with_entry, nz};
use ewm_core::{FocusSource, Notify};
use smithay::utils::Size;
use wayland_client::{Proxy, WEnum};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1;
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::{
    ExtWorkspaceHandleV1, State,
};

/// A bar bound to the manager, with the burst of events that follows.
fn bar(fix: &mut Fixture) -> (TestClient, Vec<ClientEvent>) {
    let mut bar = fix.new_client().unwrap();
    fix.roundtrip(&mut bar);
    bar.bind_workspace_manager();
    fix.roundtrip(&mut bar);
    let events = bar.drain_events();
    (bar, events)
}

/// Every `output_enter` a group received, as (group, output name).
fn group_entries(
    client: &TestClient,
    events: &[ClientEvent],
) -> Vec<(ExtWorkspaceGroupHandleV1, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::WorkspaceGroupOutputEnter { group, output } => {
                let name = client.output_name(output.id().protocol_id())?.to_owned();
                Some((group.clone(), name))
            }
            _ => None,
        })
        .collect()
}

/// The handle last named `name`.
fn named(events: &[ClientEvent], name: &str) -> ExtWorkspaceHandleV1 {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            ClientEvent::WorkspaceName { workspace, name: n } if n == name => {
                Some(workspace.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("a workspace named {name:?} in {events:?}"))
}

/// The urgent bit of the last state `workspace` was sent, if any.
fn urgent(events: &[ClientEvent], workspace: &ExtWorkspaceHandleV1) -> Option<bool> {
    events.iter().rev().find_map(|e| match e {
        ClientEvent::WorkspaceState {
            workspace: w,
            state: WEnum::Value(state),
        } if w == workspace => Some(state.contains(State::Urgent)),
        _ => None,
    })
}

fn removed(events: &[ClientEvent], workspace: &ExtWorkspaceHandleV1) -> bool {
    events.contains(&ClientEvent::WorkspaceRemoved {
        workspace: workspace.clone(),
    })
}

/// Remove `name` and connect it again before the refresh runs, as a lid
/// policy pass does with a laptop panel.
fn recreate_output(fix: &mut Fixture, name: &str) {
    fix.remove_output(name);
    fix.add_output(name, 1600, 900);
    fix.tile(name, vec![Frame::new(1)]);
}

/// A bar started after the re-creation binds wl_output first and the manager
/// second, and must find the group on its screen.
#[test]
fn new_client_finds_recreated_output_in_group() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output("HEAD-A", 1600, 900);
    fix.tile("HEAD-A", vec![Frame::new(1)]);
    fix.dispatch();
    recreate_output(&mut fix, "HEAD-A");
    fix.dispatch();

    let (bar, events) = bar(&mut fix);
    let entered: Vec<_> = group_entries(&bar, &events)
        .into_iter()
        .map(|(_, o)| o)
        .collect();
    assert_eq!(entered, ["HEAD-A"]);
}

/// A bar already holding the manager sees the old group leave its workspace
/// and go away, then a fresh group enter the new wl_output with the same
/// workspace handle.
#[test]
fn bound_client_sees_group_replaced_with_output() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output("HEAD-A", 1600, 900);
    fix.tile("HEAD-A", vec![Frame::new(1)]);
    let (mut bar, events) = bar(&mut fix);
    let (old_group, _) = group_entries(&bar, &events).remove(0);

    recreate_output(&mut fix, "HEAD-A");
    fix.roundtrip(&mut bar);

    let events = bar.drain_events();
    let left = events.iter().position(
        |e| matches!(e, ClientEvent::WorkspaceLeave { group, .. } if *group == old_group),
    );
    let removed = events.iter().position(
        |e| matches!(e, ClientEvent::WorkspaceGroupRemoved { group } if *group == old_group),
    );
    let (Some(left), Some(removed)) = (left, removed) else {
        panic!("the stale group leaves its workspace and is removed, got {events:?}");
    };
    assert!(
        left < removed,
        "I12: workspace_leave precedes removed, got {events:?}"
    );

    let (new_group, name) = group_entries(&bar, &events)
        .into_iter()
        .find(|(group, _)| *group != old_group)
        .expect("a fresh group enters the new wl_output");
    assert_eq!(name, "HEAD-A");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ClientEvent::WorkspaceEnter { group, .. } if *group == new_group)),
        "the workspace enters the fresh group, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ClientEvent::WorkspaceRemoved { .. })),
        "the workspace handle survives, got {events:?}"
    );
}

/// A rename reaches the bar as a fresh handle with the name for its id, the name follows the
/// frame to another output, and clearing it hands the slot back to the index.
#[test]
fn a_renamed_workspace_keeps_its_name_across_outputs() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output("HEAD-A", 1600, 900);
    fix.add_output("HEAD-B", 1600, 900);
    fix.tile("HEAD-A", vec![Frame::new(100), Frame::new(101)]);
    let (mut bar, events) = bar(&mut fix);
    let second = named(&events, "2");
    let (group_b, _) = group_entries(&bar, &events)
        .into_iter()
        .find(|(_, output)| output == "HEAD-B")
        .unwrap();

    assert!(fix.ewm().rename_workspace(Some(101), "code".to_string()));
    assert!(!fix.ewm().rename_workspace(Some(100), "CODE".to_string()));
    fix.roundtrip(&mut bar);
    let events = bar.drain_events();
    assert!(removed(&events, &second), "{events:?}");
    let code = named(&events, "code");
    assert!(events.contains(&ClientEvent::WorkspaceId {
        workspace: code.clone(),
        id: "code".to_string(),
    }));

    fix.remove_output("HEAD-A");
    fix.roundtrip(&mut bar);
    let events = bar.drain_events();
    assert!(
        events.contains(&ClientEvent::WorkspaceEnter {
            group: group_b,
            workspace: code.clone(),
        }),
        "{events:?}"
    );
    assert!(!removed(&events, &code));

    assert!(fix.ewm().rename_workspace(Some(101), "   ".to_string()));
    fix.roundtrip(&mut bar);
    let events = bar.drain_events();
    assert!(removed(&events, &code), "{events:?}");
    let cleared = named(&events, "2");
    assert!(
        !events.iter().any(
            |e| matches!(e, ClientEvent::WorkspaceId { workspace, .. } if *workspace == cleared)
        )
    );
}

/// Urgency reaches the bar from a surface asking for attention and from an explicit mark, and
/// focusing the workspace clears it.
#[test]
fn an_urgent_workspace_is_flagged_until_focused() {
    let mut fix = Fixture::new().unwrap();
    let mut app = fix.new_client().unwrap();
    fix.roundtrip(&mut app);
    fix.add_output("HEAD-A", 1600, 900);
    let noisy = fix.open_toplevel_id(&mut app, "noisy", Size::from((400, 400)));
    let quiet = fix.open_toplevel_id(&mut app, "quiet", Size::from((400, 400)));
    let marked = fix.open_toplevel_id(&mut app, "marked", Size::from((400, 400)));
    fix.tile(
        "HEAD-A",
        vec![
            frame_with_entry(100, nz(1), noisy),
            frame_with_entry(101, nz(2), quiet),
            Frame::new(marked),
        ],
    );
    fix.ewm()
        .set_focus_entry(nz(2), FocusSource::Test, Notify::None);
    let (mut bar, events) = bar(&mut fix);
    let (first, second, third) = (
        named(&events, "1"),
        named(&events, "2"),
        named(&events, "3"),
    );

    fix.ewm().set_surface_urgent(noisy, true);
    assert!(fix.ewm().mark_workspace_urgent(Some(marked)));
    fix.ewm().mark_workspace_urgent(Some(101));
    fix.roundtrip(&mut bar);
    let events = bar.drain_events();
    assert_eq!(urgent(&events, &first), Some(true));
    assert_eq!(urgent(&events, &second), None);
    assert_eq!(urgent(&events, &third), Some(true));

    fix.ewm()
        .set_focus_entry(nz(1), FocusSource::Test, Notify::None);
    fix.roundtrip(&mut bar);
    assert_eq!(urgent(&bar.drain_events(), &first), Some(false));
    fix.ewm()
        .set_focus_frame(marked, FocusSource::Test, Notify::None);
    fix.roundtrip(&mut bar);
    assert_eq!(urgent(&bar.drain_events(), &third), Some(false));
}
