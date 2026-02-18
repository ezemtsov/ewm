//! ext-workspace-v1 protocol implementation
//!
//! Exposes Emacs tabs as workspaces to external tools (waybar, quickshell, etc.)
//! Each output maps to a workspace group, each tab maps to a workspace.
//!
//! Uses a pull/refresh model: `refresh()` runs every event-loop iteration,
//! reads the full layout from `Ewm.output_workspaces`, diffs against protocol
//! mirrors, and sends only changes. Clients always get current state because
//! `refresh()` runs after bind.

use std::collections::HashMap;

use crate::module::TabInfo;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

const VERSION: u32 = 1;

type WorkspaceKey = (String, usize); // (output_name, tab_index)

pub trait WorkspaceHandler {
    fn workspace_manager_state(&mut self) -> &mut WorkspaceManagerState;
    fn activate_workspace(&mut self, output: String, tab_index: usize);
}

/// Queued client request, executed on Commit.
enum Action {
    Activate { output_name: String, tab_index: usize },
}

impl Action {
    fn order(&self) -> u8 {
        match self {
            Action::Activate { .. } => 1,
        }
    }
}

/// Per-workspace-group protocol mirror.
struct GroupData {
    /// The output this group represents.
    output: Output,
    /// One handle per manager binding (per client).
    instances: Vec<ExtWorkspaceGroupHandleV1>,
}

impl GroupData {
    /// Create a new group handle for a manager binding and send initial properties.
    fn add_instance<D>(
        &mut self,
        display: &DisplayHandle,
        client: &Client,
        manager: &ExtWorkspaceManagerV1,
        output: &Output,
    ) where
        D: Dispatch<ExtWorkspaceGroupHandleV1, ExtWorkspaceManagerV1> + 'static,
    {
        let Ok(handle) = client.create_resource::<ExtWorkspaceGroupHandleV1, _, D>(
            display,
            manager.version(),
            manager.clone(),
        ) else {
            return;
        };
        manager.workspace_group(&handle);
        handle.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());

        for wl_output in output.client_outputs(client) {
            handle.output_enter(&wl_output);
        }

        self.instances.push(handle);
    }
}

/// Per-workspace protocol mirror, stores current state for diffing.
struct WorkspaceData {
    name: String,
    coordinates: [u32; 2],
    state: ext_workspace_handle_v1::State,
    /// One handle per manager binding (per client).
    instances: Vec<ExtWorkspaceHandleV1>,
}

impl WorkspaceData {
    fn new(name: String, index: usize, active: bool) -> Self {
        Self {
            name,
            coordinates: [0, index as u32],
            state: if active {
                ext_workspace_handle_v1::State::Active
            } else {
                ext_workspace_handle_v1::State::empty()
            },
            instances: Vec::new(),
        }
    }

    /// Create a new workspace handle for a manager binding and send initial properties.
    fn add_instance<D>(
        &mut self,
        display: &DisplayHandle,
        client: &Client,
        manager: &ExtWorkspaceManagerV1,
    ) where
        D: Dispatch<ExtWorkspaceHandleV1, ExtWorkspaceManagerV1> + 'static,
    {
        let Ok(handle) = client.create_resource::<ExtWorkspaceHandleV1, _, D>(
            display,
            manager.version(),
            manager.clone(),
        ) else {
            return;
        };
        manager.workspace(&handle);

        handle.name(self.name.clone());
        handle.id(self.name.clone());
        handle.coordinates(
            self.coordinates
                .iter()
                .flat_map(|x| x.to_ne_bytes())
                .collect(),
        );
        handle.state(self.state);
        handle.capabilities(ext_workspace_handle_v1::WorkspaceCapabilities::Activate);

        self.instances.push(handle);
    }
}

pub struct WorkspaceManagerState {
    display: DisplayHandle,
    /// Per-manager-binding action queue, drained on Commit.
    instances: HashMap<ExtWorkspaceManagerV1, Vec<Action>>,
    /// output_name → group
    groups: HashMap<String, GroupData>,
    /// (output_name, tab_index) → workspace
    workspaces: HashMap<WorkspaceKey, WorkspaceData>,
}

pub struct WorkspaceGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

impl WorkspaceManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ExtWorkspaceManagerV1, WorkspaceGlobalData>,
        D: Dispatch<ExtWorkspaceManagerV1, ()>,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = WorkspaceGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ExtWorkspaceManagerV1, _>(VERSION, global_data);
        Self {
            display: display.clone(),
            instances: HashMap::new(),
            groups: HashMap::new(),
            workspaces: HashMap::new(),
        }
    }
}

/// Compute the display name for a tab (1-based index).
fn tab_display_name(index: usize) -> String {
    (index + 1).to_string()
}

/// Refresh workspace protocol state to match `output_workspaces` (source of truth).
///
/// Runs every event-loop iteration. Diffs current mirrors against source of truth
/// and sends only changed events. Creates/removes groups and workspaces as needed.
pub fn refresh<'a, D>(
    workspace_state: &mut WorkspaceManagerState,
    output_workspaces: &HashMap<String, Vec<TabInfo>>,
    outputs: impl Iterator<Item = &'a Output>,
) where
    D: Dispatch<ExtWorkspaceGroupHandleV1, ExtWorkspaceManagerV1> + 'static,
    D: Dispatch<ExtWorkspaceHandleV1, ExtWorkspaceManagerV1> + 'static,
{
    let mut changed = false;

    // Build map of live outputs by name.
    let live_outputs: HashMap<String, Output> =
        outputs.map(|o| (o.name(), o.clone())).collect();

    // Build "seen" set of (output_name, tab_index) from source of truth.
    let mut seen_workspaces: std::collections::HashSet<WorkspaceKey> =
        std::collections::HashSet::new();
    for (output_name, tabs) in output_workspaces {
        for (i, _) in tabs.iter().enumerate() {
            seen_workspaces.insert((output_name.clone(), i));
        }
    }

    // 1. Remove dead workspaces (not in source of truth or output gone).
    let dead_ws_keys: Vec<WorkspaceKey> = workspace_state
        .workspaces
        .keys()
        .filter(|k| !seen_workspaces.contains(k))
        .cloned()
        .collect();
    for key in dead_ws_keys {
        if let Some(ws) = workspace_state.workspaces.remove(&key) {
            // Send workspace_leave from group.
            if let Some(group) = workspace_state.groups.get(&key.0) {
                send_workspace_leave(&group.instances, &ws.instances);
            }
            for inst in &ws.instances {
                inst.removed();
            }
            changed = true;
        }
    }

    // 2. Remove dead groups (output no longer exists).
    let dead_group_keys: Vec<String> = workspace_state
        .groups
        .keys()
        .filter(|name| !live_outputs.contains_key(*name))
        .cloned()
        .collect();
    for name in dead_group_keys {
        if let Some(group) = workspace_state.groups.remove(&name) {
            for inst in &group.instances {
                inst.removed();
            }
            changed = true;
        }
    }

    // 3. Update/create workspaces and groups.
    for (output_name, tabs) in output_workspaces {
        let Some(output) = live_outputs.get(output_name) else {
            continue;
        };

        // Ensure group exists.
        let group_is_new = !workspace_state.groups.contains_key(output_name);
        if group_is_new {
            let mut group = GroupData {
                output: output.clone(),
                instances: Vec::new(),
            };
            for manager in workspace_state.instances.keys() {
                if let Some(client) = manager.client() {
                    group.add_instance::<D>(
                        &workspace_state.display,
                        &client,
                        manager,
                        output,
                    );
                }
            }
            workspace_state.groups.insert(output_name.clone(), group);
            changed = true;
        }

        for (i, tab) in tabs.iter().enumerate() {
            let key = (output_name.clone(), i);
            let display_name = tab_display_name(i);
            let new_state = if tab.active {
                ext_workspace_handle_v1::State::Active
            } else {
                ext_workspace_handle_v1::State::empty()
            };

            if let Some(ws) = workspace_state.workspaces.get_mut(&key) {
                // Existing workspace — diff properties.
                if ws.name != display_name {
                    ws.name = display_name.clone();
                    for inst in &ws.instances {
                        inst.name(display_name.clone());
                        inst.id(display_name.clone());
                    }
                    changed = true;
                }
                if ws.state != new_state {
                    ws.state = new_state;
                    for inst in &ws.instances {
                        inst.state(new_state);
                    }
                    changed = true;
                }
                // Re-associate with new group after output reconnect.
                if group_is_new {
                    if let Some(group) = workspace_state.groups.get(output_name) {
                        send_workspace_enter(&group.instances, &ws.instances);
                    }
                    changed = true;
                }
            } else {
                // New workspace.
                let mut ws = WorkspaceData::new(display_name, i, tab.active);
                for manager in workspace_state.instances.keys() {
                    if let Some(client) = manager.client() {
                        ws.add_instance::<D>(
                            &workspace_state.display,
                            &client,
                            manager,
                        );
                    }
                }
                // Send workspace_enter to group.
                if let Some(group) = workspace_state.groups.get(output_name) {
                    send_workspace_enter(&group.instances, &ws.instances);
                }
                workspace_state.workspaces.insert(key, ws);
                changed = true;
            }
        }
    }

    if changed {
        for manager in workspace_state.instances.keys() {
            manager.done();
        }
    }
}

/// Send `output_enter` for a late-binding wl_output.
///
/// When a client's wl_output is created after the group already exists,
/// the client needs to receive `output_enter` for the matching group instances.
pub fn on_output_bound(
    workspace_state: &mut WorkspaceManagerState,
    output: &Output,
    wl_output: &WlOutput,
) {
    let output_name = output.name();
    let Some(group) = workspace_state.groups.get(&output_name) else {
        return;
    };

    let wl_output_client = wl_output.client();
    let mut sent = false;

    for inst in &group.instances {
        let inst_client = inst.client();
        if inst_client.is_some() && inst_client == wl_output_client {
            inst.output_enter(wl_output);
            sent = true;
        }
    }

    if sent {
        for manager in workspace_state.instances.keys() {
            if manager.client() == wl_output_client {
                manager.done();
            }
        }
    }
}

/// Send workspace_enter from each group instance to matching workspace instance.
fn send_workspace_enter(
    group_instances: &[ExtWorkspaceGroupHandleV1],
    ws_instances: &[ExtWorkspaceHandleV1],
) {
    for group in group_instances {
        let group_mgr: &ExtWorkspaceManagerV1 = group.data().unwrap();
        for ws in ws_instances {
            if ws.data() == Some(group_mgr) {
                group.workspace_enter(ws);
            }
        }
    }
}

/// Send workspace_leave from each group instance to matching workspace instance.
fn send_workspace_leave(
    group_instances: &[ExtWorkspaceGroupHandleV1],
    ws_instances: &[ExtWorkspaceHandleV1],
) {
    for group in group_instances {
        let group_mgr: &ExtWorkspaceManagerV1 = group.data().unwrap();
        for ws in ws_instances {
            if ws.data() == Some(group_mgr) {
                group.workspace_leave(ws);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GlobalDispatch: new client binds to the workspace manager global
// ---------------------------------------------------------------------------

impl<D> GlobalDispatch<ExtWorkspaceManagerV1, WorkspaceGlobalData, D> for WorkspaceManagerState
where
    D: GlobalDispatch<ExtWorkspaceManagerV1, WorkspaceGlobalData>,
    D: Dispatch<ExtWorkspaceManagerV1, ()>,
    D: Dispatch<ExtWorkspaceGroupHandleV1, ExtWorkspaceManagerV1>,
    D: Dispatch<ExtWorkspaceHandleV1, ExtWorkspaceManagerV1>,
    D: WorkspaceHandler,
{
    fn bind(
        state: &mut D,
        _handle: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &WorkspaceGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        let ps = state.workspace_manager_state();

        // Create workspace instances for the new client (keyed by output for workspace_enter).
        let mut new_ws_by_output: HashMap<String, Vec<usize>> = HashMap::new();
        let ws_keys: Vec<WorkspaceKey> = ps.workspaces.keys().cloned().collect();
        for (output_name, index) in ws_keys {
            let ws_data = ps
                .workspaces
                .get_mut(&(output_name.clone(), index))
                .unwrap();
            ws_data.add_instance::<D>(&ps.display, client, &manager);
            new_ws_by_output
                .entry(output_name)
                .or_default()
                .push(index);
        }

        // Create group instances for all tracked outputs, with workspace_enter.
        let group_keys: Vec<String> = ps.groups.keys().cloned().collect();
        for output_name in group_keys {
            let group_data = ps.groups.get_mut(&output_name).unwrap();
            group_data.add_instance::<D>(
                &ps.display,
                client,
                &manager,
                &group_data.output.clone(),
            );

            // Send workspace_enter for workspaces on this output.
            let group_handle = group_data.instances.last().unwrap();
            for &idx in new_ws_by_output.get(&output_name).into_iter().flatten() {
                let key = (output_name.clone(), idx);
                if let Some(ws) = ps.workspaces.get(&key) {
                    if let Some(ws_inst) = ws.instances.last() {
                        group_handle.workspace_enter(ws_inst);
                    }
                }
            }
        }

        manager.done();
        ps.instances.insert(manager, Vec::new());
    }

    fn can_view(client: Client, global_data: &WorkspaceGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

// ---------------------------------------------------------------------------
// Dispatch for manager requests
// ---------------------------------------------------------------------------

impl<D> Dispatch<ExtWorkspaceManagerV1, (), D> for WorkspaceManagerState
where
    D: Dispatch<ExtWorkspaceManagerV1, ()>,
    D: WorkspaceHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ExtWorkspaceManagerV1,
        request: <ExtWorkspaceManagerV1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                let ps = state.workspace_manager_state();
                let mut actions = ps
                    .instances
                    .get_mut(resource)
                    .map(std::mem::take)
                    .unwrap_or_default();
                actions.sort_by_key(|a| a.order());
                for action in actions {
                    match action {
                        Action::Activate {
                            output_name,
                            tab_index,
                        } => {
                            state.activate_workspace(output_name, tab_index);
                        }
                    }
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                resource.finished();
                let ps = state.workspace_manager_state();
                ps.instances.retain(|m, _| m != resource);
                // Clean up instances belonging to this manager.
                for group_data in ps.groups.values_mut() {
                    group_data
                        .instances
                        .retain(|inst| inst.data() != Some(resource));
                }
                for ws_data in ps.workspaces.values_mut() {
                    ws_data
                        .instances
                        .retain(|inst| inst.data() != Some(resource));
                }
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ExtWorkspaceManagerV1,
        _data: &(),
    ) {
        let ps = state.workspace_manager_state();
        ps.instances.retain(|m, _| m != resource);
        for group_data in ps.groups.values_mut() {
            group_data
                .instances
                .retain(|inst| inst.data() != Some(resource));
        }
        for ws_data in ps.workspaces.values_mut() {
            ws_data
                .instances
                .retain(|inst| inst.data() != Some(resource));
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch for group handle requests
// ---------------------------------------------------------------------------

impl<D> Dispatch<ExtWorkspaceGroupHandleV1, ExtWorkspaceManagerV1, D> for WorkspaceManagerState
where
    D: Dispatch<ExtWorkspaceGroupHandleV1, ExtWorkspaceManagerV1>,
    D: WorkspaceHandler,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &ExtWorkspaceGroupHandleV1,
        request: <ExtWorkspaceGroupHandleV1 as Resource>::Request,
        _data: &ExtWorkspaceManagerV1,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_workspace_group_handle_v1::Request::CreateWorkspace { .. } => {}
            ext_workspace_group_handle_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ExtWorkspaceGroupHandleV1,
        _data: &ExtWorkspaceManagerV1,
    ) {
        let ps = state.workspace_manager_state();
        for group_data in ps.groups.values_mut() {
            group_data.instances.retain(|inst| inst != resource);
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch for workspace handle requests
// ---------------------------------------------------------------------------

impl<D> Dispatch<ExtWorkspaceHandleV1, ExtWorkspaceManagerV1, D> for WorkspaceManagerState
where
    D: Dispatch<ExtWorkspaceHandleV1, ExtWorkspaceManagerV1>,
    D: WorkspaceHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ExtWorkspaceHandleV1,
        request: <ExtWorkspaceHandleV1 as Resource>::Request,
        data: &ExtWorkspaceManagerV1,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_workspace_handle_v1::Request::Activate => {
                // Find which workspace this handle belongs to.
                let ps = state.workspace_manager_state();
                let Some(((output_name, tab_index), _)) = ps
                    .workspaces
                    .iter()
                    .find(|(_, ws)| ws.instances.contains(resource))
                else {
                    return;
                };
                let output_name = output_name.clone();
                let tab_index = *tab_index + 1; // 1-based for Emacs

                // Queue for processing on Commit.
                if let Some(actions) = ps.instances.get_mut(data) {
                    actions.push(Action::Activate {
                        output_name,
                        tab_index,
                    });
                }
            }
            ext_workspace_handle_v1::Request::Deactivate
            | ext_workspace_handle_v1::Request::Remove
            | ext_workspace_handle_v1::Request::Assign { .. } => {}
            ext_workspace_handle_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ExtWorkspaceHandleV1,
        _data: &ExtWorkspaceManagerV1,
    ) {
        let ps = state.workspace_manager_state();
        for ws_data in ps.workspaces.values_mut() {
            ws_data.instances.retain(|inst| inst != resource);
        }
    }
}

#[macro_export]
macro_rules! delegate_workspace {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_manager_v1::ExtWorkspaceManagerV1: $crate::protocols::workspace::WorkspaceGlobalData
        ] => $crate::protocols::workspace::WorkspaceManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_manager_v1::ExtWorkspaceManagerV1: ()
        ] => $crate::protocols::workspace::WorkspaceManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1: smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_manager_v1::ExtWorkspaceManagerV1
        ] => $crate::protocols::workspace::WorkspaceManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_handle_v1::ExtWorkspaceHandleV1: smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_manager_v1::ExtWorkspaceManagerV1
        ] => $crate::protocols::workspace::WorkspaceManagerState);
    };
}
