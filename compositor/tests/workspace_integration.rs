//! ext-workspace-v1 groups follow the output object, not its name.

use ewm_core::layout::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient};
use wayland_client::Proxy;
use wayland_protocols::ext::workspace::v1::client::ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1;

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

    let mut bar = fix.new_client().unwrap();
    fix.roundtrip(&mut bar);
    bar.bind_workspace_manager();
    fix.roundtrip(&mut bar);

    let events = bar.drain_events();
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
    let mut bar = fix.new_client().unwrap();
    fix.roundtrip(&mut bar);
    bar.bind_workspace_manager();
    fix.roundtrip(&mut bar);
    let events = bar.drain_events();
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
