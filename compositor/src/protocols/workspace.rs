//! ext-workspace-v1 protocol implementation
//!
//! Exposes Emacs frames as workspaces to external tools (waybar, quickshell, etc.)
//! Each output maps to a workspace group, each frame maps to a workspace.
//!
//! Uses a pull/refresh model: `refresh()` runs every event-loop iteration,
//! reads the full layout and urgent state from `Ewm`, diffs against
//! protocol mirrors, and sends only changes. Clients always get current state
//! because `refresh()` runs after bind.

use std::collections::{HashMap, HashSet};

use crate::protocols::EmptyData;
use crate::strip::{Frame, Strip};
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
use smithay::wayland::{Dispatch2, GlobalDispatch2};

const VERSION: u32 = 1;

/// Workspace identity is the Emacs frame's xdg_toplevel surface id -- stable
/// across reorders and cross-output moves, so external clients can track a
/// workspace through its lifetime regardless of position. The protocol id is
/// separate: named workspaces use their name as id, unnamed workspaces do not
/// send an id.
type WorkspaceKey = u64;

pub trait WorkspaceHandler {
    fn workspace_manager_state(&mut self) -> &mut WorkspaceManagerState;
    fn activate_workspace(&mut self, output: String, frame_index: usize);
}

/// Queued client request, executed on Commit.
enum Action {
    Activate {
        output_name: String,
        frame_index: usize,
    },
}

/// Per-workspace-group protocol mirror.
struct GroupData {
    /// The output this group represents.
    output: Output,
    /// One handle per manager binding (per client).
    instances: Vec<ExtWorkspaceGroupHandleV1>,
}

/// Manager binding a handle belongs to.
#[derive(PartialEq, Eq)]
pub struct WrappedManager(ExtWorkspaceManagerV1);

impl GroupData {
    /// Create a new group handle for a manager binding and send initial properties.
    fn add_instance<D>(
        &mut self,
        display: &DisplayHandle,
        client: &Client,
        manager: &ExtWorkspaceManagerV1,
    ) -> Option<ExtWorkspaceGroupHandleV1>
    where
        D: Dispatch<ExtWorkspaceGroupHandleV1, WrappedManager> + 'static,
    {
        let Ok(handle) = client.create_resource::<ExtWorkspaceGroupHandleV1, _, D>(
            display,
            manager.version(),
            WrappedManager(manager.clone()),
        ) else {
            return None;
        };
        manager.workspace_group(&handle);
        handle.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());

        for wl_output in self.output.client_outputs(client) {
            handle.output_enter(&wl_output);
        }

        self.instances.push(handle.clone());
        Some(handle)
    }
}

/// Per-workspace protocol mirror, stores current state for diffing.
struct WorkspaceData {
    /// Output this workspace currently belongs to.  Tracked here so a
    /// cross-output move can be detected as a leave-from-old/enter-new on
    /// the same workspace handle.
    output: String,
    /// Protocol `id`: workspace name when named, unset for unnamed workspaces.
    id: Option<String>,
    name: String,
    coordinates: [u32; 2],
    state: ext_workspace_handle_v1::State,
    /// One handle per manager binding (per client).
    instances: Vec<ExtWorkspaceHandleV1>,
}

impl WorkspaceData {
    fn new(
        output: String,
        id: Option<String>,
        name: String,
        coordinates: [u32; 2],
        active: bool,
        urgent: bool,
    ) -> Self {
        Self {
            output,
            id,
            name,
            coordinates,
            state: workspace_handle_state(active, urgent),
            instances: Vec::new(),
        }
    }

    fn from_desired(desired: &DesiredWorkspace) -> Self {
        Self::new(
            desired.output.clone(),
            desired.id.clone(),
            desired.name.clone(),
            desired.coordinates,
            desired.active,
            desired.urgent,
        )
    }

    fn apply_desired_properties(&mut self, desired: &DesiredWorkspace) -> bool {
        let mut changed = false;

        if self.name != desired.name {
            self.name = desired.name.clone();
            for inst in &self.instances {
                inst.name(self.name.clone());
            }
            changed = true;
        }

        if self.coordinates != desired.coordinates {
            self.coordinates = desired.coordinates;
            let bytes = coordinates_bytes(self.coordinates);
            for inst in &self.instances {
                inst.coordinates(bytes.clone());
            }
            changed = true;
        }

        let desired_state = workspace_handle_state(desired.active, desired.urgent);
        if self.state != desired_state {
            self.state = desired_state;
            for inst in &self.instances {
                inst.state(self.state);
            }
            changed = true;
        }

        changed
    }

    /// Create a new workspace handle for a manager binding and send initial properties.
    fn add_instance<D>(
        &mut self,
        display: &DisplayHandle,
        client: &Client,
        manager: &ExtWorkspaceManagerV1,
    ) -> Option<ExtWorkspaceHandleV1>
    where
        D: Dispatch<ExtWorkspaceHandleV1, WrappedManager> + 'static,
    {
        let Ok(handle) = client.create_resource::<ExtWorkspaceHandleV1, _, D>(
            display,
            manager.version(),
            WrappedManager(manager.clone()),
        ) else {
            return None;
        };
        manager.workspace(&handle);

        handle.name(self.name.clone());
        if let Some(id) = &self.id {
            handle.id(id.clone());
        }
        handle.coordinates(coordinates_bytes(self.coordinates));
        handle.state(self.state);
        handle.capabilities(ext_workspace_handle_v1::WorkspaceCapabilities::Activate);

        self.instances.push(handle.clone());
        Some(handle)
    }
}

pub struct WorkspaceManagerState {
    display: DisplayHandle,
    /// Per-manager-binding action queue, drained on Commit.
    instances: HashMap<ExtWorkspaceManagerV1, Vec<Action>>,
    /// output_name -> group
    groups: HashMap<String, GroupData>,
    /// (output_name, tab_index) -> workspace
    workspaces: HashMap<WorkspaceKey, WorkspaceData>,
}

pub struct WorkspaceGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct WorkspaceDebugState {
    groups: Vec<String>,
    workspaces: Vec<WorkspaceDebugWorkspace>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct WorkspaceDebugWorkspace {
    surface_id: u64,
    output: String,
    id: Option<String>,
    name: String,
    coordinates: [u32; 2],
    active: bool,
    urgent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesiredWorkspace {
    output: String,
    id: Option<String>,
    name: String,
    coordinates: [u32; 2],
    active: bool,
    urgent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesiredWorkspaceSnapshot {
    groups: Vec<String>,
    workspace_order: Vec<WorkspaceKey>,
    workspaces: HashMap<WorkspaceKey, DesiredWorkspace>,
}

impl WorkspaceManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ExtWorkspaceManagerV1, WorkspaceGlobalData>,
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

    pub(crate) fn debug_state(&self) -> WorkspaceDebugState {
        let mut groups: Vec<_> = self.groups.keys().cloned().collect();
        groups.sort();

        let mut workspaces: Vec<_> = self
            .workspaces
            .iter()
            .map(|(&surface_id, ws)| WorkspaceDebugWorkspace {
                surface_id,
                output: ws.output.clone(),
                id: ws.id.clone(),
                name: ws.name.clone(),
                coordinates: ws.coordinates,
                active: ws.state.contains(ext_workspace_handle_v1::State::Active),
                urgent: ws.state.contains(ext_workspace_handle_v1::State::Urgent),
            })
            .collect();
        workspaces.sort_by(|a, b| {
            (&a.output, a.coordinates, a.surface_id).cmp(&(&b.output, b.coordinates, b.surface_id))
        });

        WorkspaceDebugState { groups, workspaces }
    }

    fn remove_manager(&mut self, manager: &ExtWorkspaceManagerV1) {
        self.instances.retain(|m, _| m != manager);
        let manager = WrappedManager(manager.clone());
        for group_data in self.groups.values_mut() {
            group_data
                .instances
                .retain(|inst| inst.data() != Some(&manager));
        }
        for ws_data in self.workspaces.values_mut() {
            ws_data
                .instances
                .retain(|inst| inst.data() != Some(&manager));
        }
    }
}

/// Compute the display name for a frame (1-based index).
pub(crate) fn frame_display_name(index: usize) -> String {
    (index + 1).to_string()
}

pub(crate) fn frame_workspace_name(frame: &Frame, index: usize) -> String {
    frame
        .workspace_name
        .clone()
        .unwrap_or_else(|| frame_display_name(index))
}

fn workspace_handle_state(active: bool, urgent: bool) -> ext_workspace_handle_v1::State {
    let mut state = ext_workspace_handle_v1::State::empty();
    if active {
        state |= ext_workspace_handle_v1::State::Active;
    }
    if urgent {
        state |= ext_workspace_handle_v1::State::Urgent;
    }
    state
}

fn coordinates_bytes(coordinates: [u32; 2]) -> Vec<u8> {
    coordinates.iter().flat_map(|x| x.to_ne_bytes()).collect()
}

fn desired_workspace_snapshot(
    output_strips: &HashMap<String, Strip>,
    urgent_surfaces: &HashSet<u64>,
    urgent_workspaces: &HashSet<u64>,
    sorted_outputs: &[Output],
) -> DesiredWorkspaceSnapshot {
    let mut groups = Vec::new();
    let mut workspace_order = Vec::new();
    let mut workspaces = HashMap::new();
    for output in sorted_outputs {
        let output_name = output.name();
        let Some(strip) = output_strips.get(&output_name) else {
            continue;
        };
        groups.push(output_name.clone());

        for (idx, frame) in strip.frames.iter().enumerate() {
            let key = frame.surface_id;
            let name = frame_workspace_name(frame, idx);
            let id = frame.workspace_name.clone();
            if workspaces
                .insert(
                    key,
                    DesiredWorkspace {
                        output: output_name.clone(),
                        id,
                        name,
                        coordinates: [0, idx as u32],
                        active: idx == strip.active_idx(),
                        urgent: frame_is_urgent(frame, urgent_surfaces, urgent_workspaces),
                    },
                )
                .is_none()
            {
                workspace_order.push(key);
            }
        }
    }

    DesiredWorkspaceSnapshot {
        groups,
        workspace_order,
        workspaces,
    }
}

fn frame_is_urgent(
    frame: &Frame,
    urgent_surfaces: &HashSet<u64>,
    urgent_workspaces: &HashSet<u64>,
) -> bool {
    urgent_workspaces.contains(&frame.surface_id)
        || frame
            .entries
            .iter()
            .filter_map(|entry| entry.surface_id())
            .any(|surface_id| urgent_surfaces.contains(&surface_id))
}

fn remove_stale_workspaces(
    workspace_state: &mut WorkspaceManagerState,
    desired: &DesiredWorkspaceSnapshot,
) -> bool {
    let dead_ws_keys: Vec<WorkspaceKey> = workspace_state
        .workspaces
        .keys()
        .filter(|key| !desired.workspaces.contains_key(key))
        .cloned()
        .collect();

    let mut changed = false;
    for key in dead_ws_keys {
        changed |= remove_workspace(workspace_state, key);
    }
    changed
}

fn remove_workspace(workspace_state: &mut WorkspaceManagerState, key: WorkspaceKey) -> bool {
    let Some(ws) = workspace_state.workspaces.remove(&key) else {
        return false;
    };
    if let Some(group) = workspace_state.groups.get(&ws.output) {
        send_workspace_leave(&group.instances, &ws.instances);
    }
    for inst in &ws.instances {
        inst.removed();
    }
    true
}

fn remove_stale_groups(
    workspace_state: &mut WorkspaceManagerState,
    desired: &DesiredWorkspaceSnapshot,
) -> bool {
    let dead_group_keys: Vec<String> = workspace_state
        .groups
        .keys()
        .filter(|name| !desired.groups.contains(name))
        .cloned()
        .collect();

    let mut changed = false;
    for name in dead_group_keys {
        let Some(group) = workspace_state.groups.remove(&name) else {
            continue;
        };
        for inst in &group.instances {
            inst.removed();
        }
        changed = true;
    }

    changed
}

fn group_for_output<D>(workspace_state: &WorkspaceManagerState, output: Output) -> GroupData
where
    D: Dispatch<ExtWorkspaceGroupHandleV1, WrappedManager> + 'static,
{
    let mut group = GroupData {
        output,
        instances: Vec::new(),
    };

    for manager in workspace_state.instances.keys() {
        if let Some(client) = manager.client() {
            let _ = group.add_instance::<D>(&workspace_state.display, &client, manager);
        }
    }

    group
}

fn sync_groups<D>(
    workspace_state: &mut WorkspaceManagerState,
    sorted_outputs: &[Output],
    desired: &DesiredWorkspaceSnapshot,
) -> HashSet<String>
where
    D: Dispatch<ExtWorkspaceGroupHandleV1, WrappedManager> + 'static,
{
    let mut new_groups = HashSet::new();

    for output_name in &desired.groups {
        if workspace_state.groups.contains_key(output_name) {
            continue;
        }

        let output = sorted_outputs
            .iter()
            .find(|output| output.name() == output_name.as_str())
            .expect("desired groups only contain live outputs");
        let group = group_for_output::<D>(workspace_state, output.clone());

        workspace_state.groups.insert(output_name.clone(), group);
        new_groups.insert(output_name.clone());
    }

    new_groups
}

fn desired_workspaces_in_protocol_order(
    desired: &DesiredWorkspaceSnapshot,
) -> Vec<(WorkspaceKey, &DesiredWorkspace)> {
    desired
        .workspace_order
        .iter()
        .filter_map(|&key| {
            desired
                .workspaces
                .get(&key)
                .map(|workspace| (key, workspace))
        })
        .collect()
}

fn sync_workspace_group_membership(
    groups: &HashMap<String, GroupData>,
    workspace: &mut WorkspaceData,
    desired: &DesiredWorkspace,
    new_groups: &HashSet<String>,
) -> bool {
    if workspace.output != desired.output {
        if let Some(old_group) = groups.get(&workspace.output) {
            send_workspace_leave(&old_group.instances, &workspace.instances);
        }
        workspace.output = desired.output.clone();
        if let Some(new_group) = groups.get(&desired.output) {
            send_workspace_enter(&new_group.instances, &workspace.instances);
        }
        return true;
    }

    if !new_groups.contains(&desired.output) {
        return false;
    }

    if let Some(group) = groups.get(&desired.output) {
        send_workspace_enter(&group.instances, &workspace.instances);
    }
    true
}

fn insert_workspace<D>(
    workspace_state: &mut WorkspaceManagerState,
    key: WorkspaceKey,
    desired: &DesiredWorkspace,
) where
    D: Dispatch<ExtWorkspaceHandleV1, WrappedManager> + 'static,
{
    let mut workspace = WorkspaceData::from_desired(desired);
    for manager in workspace_state.instances.keys() {
        if let Some(client) = manager.client() {
            let _ = workspace.add_instance::<D>(&workspace_state.display, &client, manager);
        }
    }

    if let Some(group) = workspace_state.groups.get(&desired.output) {
        send_workspace_enter(&group.instances, &workspace.instances);
    }
    workspace_state.workspaces.insert(key, workspace);
}

fn sync_existing_workspace(
    groups: &HashMap<String, GroupData>,
    workspaces: &mut HashMap<WorkspaceKey, WorkspaceData>,
    key: WorkspaceKey,
    desired: &DesiredWorkspace,
    new_groups: &HashSet<String>,
) -> Option<bool> {
    let workspace = workspaces.get_mut(&key)?;

    let mut changed = workspace.apply_desired_properties(desired);
    changed |= sync_workspace_group_membership(groups, workspace, desired, new_groups);
    Some(changed)
}

fn sync_workspaces<D>(
    workspace_state: &mut WorkspaceManagerState,
    desired: &DesiredWorkspaceSnapshot,
    new_groups: &HashSet<String>,
) -> bool
where
    D: Dispatch<ExtWorkspaceHandleV1, WrappedManager> + 'static,
{
    let mut changed = false;

    for (key, desired_workspace) in desired_workspaces_in_protocol_order(desired) {
        if workspace_state
            .workspaces
            .get(&key)
            .is_some_and(|workspace| workspace.id.as_deref() != desired_workspace.id.as_deref())
        {
            remove_workspace(workspace_state, key);
            insert_workspace::<D>(workspace_state, key, desired_workspace);
            changed = true;
            continue;
        }

        if let Some(workspace_changed) = sync_existing_workspace(
            &workspace_state.groups,
            &mut workspace_state.workspaces,
            key,
            desired_workspace,
            new_groups,
        ) {
            changed |= workspace_changed;
            continue;
        }

        insert_workspace::<D>(workspace_state, key, desired_workspace);
        changed = true;
    }

    changed
}

fn notify_managers_done(workspace_state: &WorkspaceManagerState) {
    for manager in workspace_state.instances.keys() {
        manager.done();
    }
}

/// Refresh workspace protocol state to match `output_strips` (source of truth).
///
/// Runs every event-loop iteration. Diffs current mirrors against source of truth
/// and sends only changed events. Creates/removes groups and workspaces as needed.
pub fn refresh<D>(
    workspace_state: &mut WorkspaceManagerState,
    output_strips: &HashMap<String, Strip>,
    urgent_surfaces: &HashSet<u64>,
    urgent_workspaces: &HashSet<u64>,
    sorted_outputs: &[Output],
) where
    D: Dispatch<ExtWorkspaceGroupHandleV1, WrappedManager> + 'static,
    D: Dispatch<ExtWorkspaceHandleV1, WrappedManager> + 'static,
{
    let desired = desired_workspace_snapshot(
        output_strips,
        urgent_surfaces,
        urgent_workspaces,
        sorted_outputs,
    );

    let mut changed = remove_stale_workspaces(workspace_state, &desired);
    changed |= remove_stale_groups(workspace_state, &desired);

    let new_groups = sync_groups::<D>(workspace_state, sorted_outputs, &desired);
    changed |= !new_groups.is_empty();
    changed |= sync_workspaces::<D>(workspace_state, &desired, &new_groups);

    if changed {
        notify_managers_done(workspace_state);
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
        let group_mgr: &WrappedManager = group.data().unwrap();
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
        let group_mgr: &WrappedManager = group.data().unwrap();
        for ws in ws_instances {
            if ws.data() == Some(group_mgr) {
                group.workspace_leave(ws);
            }
        }
    }
}

// GlobalDispatch2: new client binds to the workspace manager global

impl<D> GlobalDispatch2<ExtWorkspaceManagerV1, D> for WorkspaceGlobalData
where
    D: Dispatch<ExtWorkspaceManagerV1, EmptyData>,
    D: Dispatch<ExtWorkspaceGroupHandleV1, WrappedManager>,
    D: Dispatch<ExtWorkspaceHandleV1, WrappedManager>,
    D: WorkspaceHandler,
{
    fn bind(
        &self,
        state: &mut D,
        _handle: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, EmptyData);
        let ps = state.workspace_manager_state();

        // Create workspace instances for the new client, grouped by their
        // current output so the freshly-created group can issue
        // workspace_enter for each.
        let mut new_ws_by_output: HashMap<String, Vec<ExtWorkspaceHandleV1>> = HashMap::new();
        for ws_data in ps.workspaces.values_mut() {
            let output_name = ws_data.output.clone();
            let Some(ws_handle) = ws_data.add_instance::<D>(&ps.display, client, &manager) else {
                continue;
            };
            new_ws_by_output
                .entry(output_name)
                .or_default()
                .push(ws_handle);
        }

        // Create group instances for all tracked outputs, with workspace_enter.
        for (output_name, group_data) in &mut ps.groups {
            let Some(group_handle) = group_data.add_instance::<D>(&ps.display, client, &manager)
            else {
                continue;
            };

            // Send workspace_enter for workspaces on this output.
            for ws_handle in new_ws_by_output.get(output_name).into_iter().flatten() {
                group_handle.workspace_enter(ws_handle);
            }
        }

        manager.done();
        ps.instances.insert(manager, Vec::new());
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

// Dispatch2 for manager requests

impl<D> Dispatch2<ExtWorkspaceManagerV1, D> for EmptyData
where
    D: WorkspaceHandler,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &ExtWorkspaceManagerV1,
        request: <ExtWorkspaceManagerV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                let ps = state.workspace_manager_state();
                let actions = ps
                    .instances
                    .get_mut(resource)
                    .map(std::mem::take)
                    .unwrap_or_default();
                for action in actions {
                    match action {
                        Action::Activate {
                            output_name,
                            frame_index,
                        } => {
                            state.activate_workspace(output_name, frame_index);
                        }
                    }
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                resource.finished();
                let ps = state.workspace_manager_state();
                ps.remove_manager(resource);
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &ExtWorkspaceManagerV1) {
        let ps = state.workspace_manager_state();
        ps.remove_manager(resource);
    }
}

// Dispatch2 for group handle requests

impl<D> Dispatch2<ExtWorkspaceGroupHandleV1, D> for WrappedManager
where
    D: WorkspaceHandler,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _resource: &ExtWorkspaceGroupHandleV1,
        request: <ExtWorkspaceGroupHandleV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_workspace_group_handle_v1::Request::CreateWorkspace { .. } => {}
            ext_workspace_group_handle_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &ExtWorkspaceGroupHandleV1) {
        let ps = state.workspace_manager_state();
        for group_data in ps.groups.values_mut() {
            group_data.instances.retain(|inst| inst != resource);
        }
    }
}

// Dispatch2 for workspace handle requests

impl<D> Dispatch2<ExtWorkspaceHandleV1, D> for WrappedManager
where
    D: WorkspaceHandler,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &ExtWorkspaceHandleV1,
        request: <ExtWorkspaceHandleV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_workspace_handle_v1::Request::Activate => {
                // Look up the workspace by handle, then resolve its current
                // strip position. The lisp side activates by 1-based
                // (output, frame_index); compute it from the workspace's
                // current `coordinates`.
                let ps = state.workspace_manager_state();
                let Some(ws) = ps
                    .workspaces
                    .values()
                    .find(|ws| ws.instances.contains(resource))
                else {
                    return;
                };
                let output_name = ws.output.clone();
                let frame_index = ws.coordinates[1] as usize + 1;

                // Queue for processing on Commit.
                if let Some(actions) = ps.instances.get_mut(&self.0) {
                    actions.push(Action::Activate {
                        output_name,
                        frame_index,
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

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &ExtWorkspaceHandleV1) {
        let ps = state.workspace_manager_state();
        for ws_data in ps.workspaces.values_mut() {
            ws_data.instances.retain(|inst| inst != resource);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;
    use smithay::output::{PhysicalProperties, Subpixel};

    use super::*;
    use crate::strip::{AnimationsClock, Frame};

    const HEAD_A: &str = "HEAD-A";
    const HEAD_B: &str = "HEAD-B";

    fn strip_with_ids(start: u64, len: usize, raw_active_idx: usize) -> Strip {
        let mut strip = Strip::new(AnimationsClock::default());
        strip.frames = (0..len).map(|idx| Frame::new(start + idx as u64)).collect();
        if len > 0 {
            strip.set_active(raw_active_idx % len);
        }
        strip
    }

    fn output(name: &str) -> Output {
        Output::new(
            name.to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: String::new(),
                model: String::new(),
                serial_number: String::new(),
            },
        )
    }

    #[test]
    fn desired_snapshot_marks_explicit_workspace_urgent() {
        let mut strips = HashMap::new();
        strips.insert(HEAD_A.to_string(), strip_with_ids(100, 3, 0));

        let outputs = vec![output(HEAD_A)];
        let urgent_surfaces = HashSet::new();
        let urgent_workspaces = HashSet::from([101]);
        let snapshot =
            desired_workspace_snapshot(&strips, &urgent_surfaces, &urgent_workspaces, &outputs);

        assert!(!snapshot.workspaces.get(&100).unwrap().urgent);
        assert!(snapshot.workspaces.get(&101).unwrap().urgent);
        assert!(!snapshot.workspaces.get(&102).unwrap().urgent);
    }

    proptest! {
        #[test]
        fn desired_snapshot_mirrors_sorted_live_outputs(
            a_len in 0usize..=5,
            b_len in 0usize..=5,
            a_active in 0usize..=5,
            b_active in 0usize..=5,
            a_live in any::<bool>(),
            b_live in any::<bool>(),
            a_first in any::<bool>(),
        ) {
            let mut strips = HashMap::new();
            strips.insert(HEAD_A.to_string(), strip_with_ids(100, a_len, a_active));
            strips.insert(HEAD_B.to_string(), strip_with_ids(200, b_len, b_active));

            let mut outputs = Vec::new();
            if a_live {
                outputs.push(output(HEAD_A));
            }
            if b_live {
                outputs.push(output(HEAD_B));
            }
            if !a_first {
                outputs.reverse();
            }

            let urgent_surfaces = HashSet::new();
            let urgent_workspaces = HashSet::new();
            let snapshot =
                desired_workspace_snapshot(&strips, &urgent_surfaces, &urgent_workspaces, &outputs);
            let expected_groups = outputs
                .iter()
                .map(Output::name)
                .collect::<Vec<_>>();
            prop_assert_eq!(&snapshot.groups, &expected_groups);
            prop_assert_eq!(
                snapshot.workspaces.len(),
                if a_live { a_len } else { 0 } + if b_live { b_len } else { 0 },
            );

            let protocol_order = desired_workspaces_in_protocol_order(&snapshot)
                .into_iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>();
            let expected_workspace_order = outputs
                .iter()
                .flat_map(|output| {
                    let (start, len) = match output.name().as_str() {
                        HEAD_A => (100, a_len),
                        HEAD_B => (200, b_len),
                        _ => unreachable!(),
                    };
                    (0..len).map(move |idx| start + idx as u64)
                })
                .collect::<Vec<_>>();
            prop_assert_eq!(protocol_order, expected_workspace_order);

            for (output, start, len, raw_active, live) in [
                (HEAD_A, 100, a_len, a_active, a_live),
                (HEAD_B, 200, b_len, b_active, b_live),
            ] {
                for idx in 0..len {
                    let key = start + idx as u64;
                    if !live {
                        prop_assert!(!snapshot.workspaces.contains_key(&key));
                        continue;
                    }
                    let ws = snapshot.workspaces.get(&key).expect("workspace exists");
                    prop_assert_eq!(ws.output.as_str(), output);
                    prop_assert_eq!(ws.id.as_deref(), None);
                    prop_assert_eq!(ws.name.as_str(), (idx + 1).to_string());
                    prop_assert_eq!(ws.coordinates, [0, idx as u32]);
                    prop_assert_eq!(ws.active, idx == raw_active % len);
                }
            }
        }
    }
}
