//! wlr-output-management-unstable-v1 protocol implementation
//!
//! Allows tools like wlr-randr and kanshi to query and configure outputs.
//! Ported from niri's implementation, adapted to ewm's types.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::mem;

use smithay::reexports::wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1, zwlr_output_configuration_v1, zwlr_output_head_v1,
    zwlr_output_manager_v1, zwlr_output_mode_v1,
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_output::Transform as WlTransform;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};
use smithay::utils::Transform;
use smithay::wayland::{Dispatch2, GlobalDispatch2};
use zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1;
use zwlr_output_configuration_v1::ZwlrOutputConfigurationV1;
use zwlr_output_head_v1::{AdaptiveSyncState, ZwlrOutputHeadV1};
use zwlr_output_manager_v1::ZwlrOutputManagerV1;
use zwlr_output_mode_v1::ZwlrOutputModeV1;

use crate::OutputConfig;
use crate::output_mode::{ConfiguredMode, Mode};
use crate::protocols::EmptyData;

const VERSION: u32 = 4;

/// Snapshot of an output's state for the protocol.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputHeadState {
    pub name: String,
    pub make: String,
    pub model: String,
    pub serial_number: Option<String>,
    pub physical_size: Option<(i32, i32)>,
    pub enabled: bool,
    pub modes: Vec<OutputModeState>,
    pub current_mode: Option<usize>,
    pub position: Option<(i32, i32)>,
    pub scale: Option<f64>,
    pub transform: Option<Transform>,
}

/// A single output mode.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputModeState {
    pub width: i32,
    pub height: i32,
    pub refresh: i32, // mHz
    pub preferred: bool,
}

/// Per-client tracking data.
#[derive(Debug)]
struct ClientData {
    /// Output head objects and their mode objects, keyed by output name.
    heads: HashMap<String, (ZwlrOutputHeadV1, Vec<ZwlrOutputModeV1>)>,
    /// Active configuration objects.
    confs: HashMap<ZwlrOutputConfigurationV1, OutputConfigurationState>,
    /// The manager object for this client.
    manager: ZwlrOutputManagerV1,
}

/// Global state for the output management protocol.
pub struct OutputManagementState {
    display: DisplayHandle,
    serial: u32,
    clients: HashMap<ClientId, ClientData>,
    current_state: HashMap<String, OutputHeadState>,
    /// Set by the backend when output topology or config changes.
    /// Cleared by `refresh()` which sends protocol updates to clients.
    pub output_heads_changed: bool,
}

pub struct OutputManagementGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

pub trait OutputManagementHandler {
    fn output_management_state(&mut self) -> &mut OutputManagementState;
    fn apply_output_config(&mut self, configs: HashMap<String, OutputConfig>);
}

#[derive(Debug)]
enum OutputConfigurationState {
    Ongoing(HashMap<String, OutputConfig>),
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinishConfigError {
    Missing,
    AlreadyUsed,
    AllDisabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigureHeadError {
    AlreadyConfigured,
}

pub enum OutputConfigurationHeadState {
    Cancelled,
    Ok(String, ZwlrOutputConfigurationV1),
}

#[derive(Debug, Clone, PartialEq)]
struct OutputChangePlan {
    added: Vec<String>,
    removed: Vec<String>,
    updated: Vec<HeadChange>,
}

impl OutputChangePlan {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.updated.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
struct HeadChange {
    output_name: String,
    modes_changed: bool,
    current_mode: CurrentModeChange,
    properties: Option<HeadPropertyChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurrentModeChange {
    None,
    Set(usize),
    Disabled,
    Enabled(usize),
}

struct OutputHeadName(String);

struct OutputConfigurationData {
    serial: u32,
}

#[derive(Debug, Clone, PartialEq)]
struct HeadPropertyChange {
    position: Option<(i32, i32)>,
    scale: Option<f64>,
    transform: Option<Transform>,
}

impl OutputManagementState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = OutputManagementGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ZwlrOutputManagerV1, _>(VERSION, global_data);

        Self {
            display: display.clone(),
            clients: HashMap::new(),
            serial: 0,
            current_state: HashMap::new(),
            output_heads_changed: false,
        }
    }

    /// Update protocol state when outputs change.
    /// Compares new state against current, sends incremental updates to clients.
    pub fn notify_changes(&mut self, new_state: HashMap<String, OutputHeadState>) {
        let plan = output_change_plan(&self.current_state, &new_state);
        if !plan.is_empty() {
            self.apply_change_plan(&plan, &new_state);
            self.current_state = new_state;
            self.serial += 1;
            for data in self.clients.values() {
                data.manager.done(self.serial);
                for conf in data.confs.keys() {
                    conf.cancelled();
                }
            }
        }
    }

    fn apply_change_plan(
        &mut self,
        plan: &OutputChangePlan,
        new_state: &HashMap<String, OutputHeadState>,
    ) {
        for output_name in &plan.added {
            let new_head = new_state
                .get(output_name)
                .expect("added head must exist in new state");
            notify_new_head(self, output_name, new_head);
        }

        for change in &plan.updated {
            let new_head = new_state
                .get(&change.output_name)
                .expect("updated head must exist in new state");
            self.apply_head_change(change, new_head);
        }

        for output_name in &plan.removed {
            notify_removed_head(&mut self.clients, output_name);
        }
    }

    fn apply_head_change(&mut self, change: &HeadChange, new_head: &OutputHeadState) {
        if change.modes_changed {
            self.apply_mode_list_change(change, new_head);
        }

        match change.current_mode {
            CurrentModeChange::None => {}
            CurrentModeChange::Set(new_index) => {
                for client in self.clients.values() {
                    if let Some((head, modes)) = client.heads.get(&change.output_name)
                        && let Some(new_mode) = modes.get(new_index)
                    {
                        head.current_mode(new_mode);
                    }
                }
            }
            CurrentModeChange::Disabled => {
                for client in self.clients.values() {
                    if let Some((head, _)) = client.heads.get(&change.output_name) {
                        head.enabled(0);
                    }
                }
            }
            CurrentModeChange::Enabled(new_index) => {
                for client in self.clients.values() {
                    if let Some((head, modes)) = client.heads.get(&change.output_name) {
                        head.enabled(1);
                        if let Some(mode) = modes.get(new_index) {
                            head.current_mode(mode);
                        }
                    }
                }
            }
        }

        if let Some(properties) = &change.properties {
            for client in self.clients.values() {
                if let Some((head, _)) = client.heads.get(&change.output_name) {
                    if let Some((x, y)) = properties.position {
                        head.position(x, y);
                    }
                    if let Some(scale) = properties.scale {
                        head.scale(scale);
                    }
                    if let Some(transform) = properties.transform {
                        head.transform(transform.into());
                    }
                }
            }
        }
    }

    fn apply_mode_list_change(&mut self, change: &HeadChange, new_head: &OutputHeadState) {
        for client in self.clients.values_mut() {
            if let Some((head, modes)) = client.heads.get_mut(&change.output_name) {
                let common_len = modes.len().min(new_head.modes.len());
                for (wl_mode, mode) in modes.iter().zip(&new_head.modes).take(common_len) {
                    wl_mode.size(mode.width, mode.height);
                    wl_mode.refresh(mode.refresh);
                }

                if let Some(client_obj) = client.manager.client() {
                    if new_head.modes.len() > common_len {
                        for mode in &new_head.modes[common_len..] {
                            let new_mode = client_obj
                                .create_resource::<ZwlrOutputModeV1, _, crate::State>(
                                    &self.display,
                                    head.version(),
                                    EmptyData,
                                )
                                .unwrap();
                            head.mode(&new_mode);
                            new_mode.size(mode.width, mode.height);
                            new_mode.refresh(mode.refresh);
                            if mode.preferred {
                                new_mode.preferred();
                            }
                            modes.push(new_mode);
                        }
                    } else if modes.len() > common_len {
                        for mode in modes.drain(common_len..) {
                            mode.finished();
                        }
                    }
                }
            }
        }
    }
}

fn output_change_plan(
    current_state: &HashMap<String, OutputHeadState>,
    new_state: &HashMap<String, OutputHeadState>,
) -> OutputChangePlan {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut updated = Vec::new();

    for (output_name, new_head) in new_state {
        let Some(old_head) = current_state.get(output_name) else {
            added.push(output_name.clone());
            continue;
        };

        let modes_changed = old_head.modes != new_head.modes;
        let current_mode = current_mode_change(old_head, new_head, modes_changed);
        let properties = head_property_change(old_head, new_head);

        if modes_changed || current_mode != CurrentModeChange::None || properties.is_some() {
            updated.push(HeadChange {
                output_name: output_name.clone(),
                modes_changed,
                current_mode,
                properties,
            });
        }
    }

    for old_name in current_state.keys() {
        if !new_state.contains_key(old_name) {
            removed.push(old_name.clone());
        }
    }

    added.sort();
    removed.sort();
    updated.sort_by(|left, right| left.output_name.cmp(&right.output_name));

    OutputChangePlan {
        added,
        removed,
        updated,
    }
}

fn current_mode_change(
    old_head: &OutputHeadState,
    new_head: &OutputHeadState,
    modes_changed: bool,
) -> CurrentModeChange {
    match (old_head.current_mode, new_head.current_mode) {
        (Some(old_index), Some(new_index)) => {
            if old_head.modes.len() == new_head.modes.len()
                && (modes_changed || old_index != new_index)
            {
                CurrentModeChange::Set(new_index)
            } else {
                CurrentModeChange::None
            }
        }
        (Some(_), None) => CurrentModeChange::Disabled,
        (None, Some(new_index)) => {
            if old_head.modes.len() == new_head.modes.len() {
                CurrentModeChange::Enabled(new_index)
            } else {
                CurrentModeChange::None
            }
        }
        (None, None) => CurrentModeChange::None,
    }
}

fn head_property_change(
    old_head: &OutputHeadState,
    new_head: &OutputHeadState,
) -> Option<HeadPropertyChange> {
    if !new_head.enabled
        || (old_head.position == new_head.position
            && old_head.scale == new_head.scale
            && old_head.transform == new_head.transform)
    {
        return None;
    }

    Some(HeadPropertyChange {
        position: (old_head.position != new_head.position)
            .then_some(new_head.position)
            .flatten(),
        scale: (old_head.scale != new_head.scale)
            .then_some(new_head.scale)
            .flatten(),
        transform: (old_head.transform != new_head.transform)
            .then_some(new_head.transform)
            .flatten(),
    })
}

fn ongoing_output_config_mut(
    state: Option<&mut OutputConfigurationState>,
) -> Result<&mut HashMap<String, OutputConfig>, FinishConfigError> {
    let Some(state) = state else {
        return Err(FinishConfigError::Missing);
    };

    let OutputConfigurationState::Ongoing(config) = state else {
        return Err(FinishConfigError::AlreadyUsed);
    };

    Ok(config)
}

fn insert_output_config(
    configs: &mut HashMap<String, OutputConfig>,
    output_name: String,
    config: OutputConfig,
) -> Result<(), ConfigureHeadError> {
    match configs.entry(output_name) {
        Entry::Occupied(_) => Err(ConfigureHeadError::AlreadyConfigured),
        Entry::Vacant(entry) => {
            entry.insert(config);
            Ok(())
        }
    }
}

fn take_ongoing_output_config(
    state: Option<&mut OutputConfigurationState>,
) -> Result<HashMap<String, OutputConfig>, FinishConfigError> {
    let Some(state) = state else {
        return Err(FinishConfigError::Missing);
    };

    let OutputConfigurationState::Ongoing(config) =
        mem::replace(state, OutputConfigurationState::Finished)
    else {
        return Err(FinishConfigError::AlreadyUsed);
    };

    if config.values().any(|config| config.enabled) {
        Ok(config)
    } else {
        Err(FinishConfigError::AllDisabled)
    }
}

fn report_output_config_error(conf: &ZwlrOutputConfigurationV1, err: FinishConfigError) {
    match err {
        FinishConfigError::Missing => {}
        FinishConfigError::AlreadyUsed => {
            conf.post_error(
                zwlr_output_configuration_v1::Error::AlreadyUsed,
                "configuration had already been used",
            );
        }
        FinishConfigError::AllDisabled => {
            conf.failed();
        }
    }
}

fn finish_output_configuration_request(
    conf: &ZwlrOutputConfigurationV1,
    state: Option<&mut OutputConfigurationState>,
) -> Option<HashMap<String, OutputConfig>> {
    match take_ongoing_output_config(state) {
        Ok(config) => Some(config),
        Err(err) => {
            report_output_config_error(conf, err);
            None
        }
    }
}

// GlobalDispatch2 for ZwlrOutputManagerV1

impl<D> GlobalDispatch2<ZwlrOutputManagerV1, D> for OutputManagementGlobalData
where
    D: Dispatch<ZwlrOutputManagerV1, EmptyData>,
    D: Dispatch<ZwlrOutputHeadV1, OutputHeadName>,
    D: Dispatch<ZwlrOutputModeV1, EmptyData>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn bind(
        &self,
        state: &mut D,
        display: &DisplayHandle,
        client: &Client,
        manager: New<ZwlrOutputManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(manager, EmptyData);
        let g_state = state.output_management_state();
        let mut client_data = ClientData {
            heads: HashMap::new(),
            confs: HashMap::new(),
            manager: manager.clone(),
        };
        for (output_name, head_state) in &g_state.current_state {
            send_new_head::<D>(display, client, &mut client_data, output_name, head_state);
        }
        g_state.clients.insert(client.id(), client_data);
        manager.done(g_state.serial);
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

// Dispatch2 for ZwlrOutputManagerV1

impl<D> Dispatch2<ZwlrOutputManagerV1, D> for EmptyData
where
    D: Dispatch<ZwlrOutputConfigurationV1, OutputConfigurationData>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        client: &Client,
        _manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                let g_state = state.output_management_state();
                let conf = data_init.init(id, OutputConfigurationData { serial });
                if let Some(client_data) = g_state.clients.get_mut(&client.id()) {
                    if serial != g_state.serial {
                        conf.cancelled();
                    }
                    let state = OutputConfigurationState::Ongoing(HashMap::new());
                    client_data.confs.insert(conf, state);
                } else {
                    tracing::error!("CreateConfiguration: missing client data");
                }
            }
            zwlr_output_manager_v1::Request::Stop => {
                if let Some(c) = state.output_management_state().clients.remove(&client.id()) {
                    c.manager.finished()
                }
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, client: ClientId, _resource: &ZwlrOutputManagerV1) {
        state.output_management_state().clients.remove(&client);
    }
}

// Dispatch2 for ZwlrOutputConfigurationV1

impl<D> Dispatch2<ZwlrOutputConfigurationV1, D> for OutputConfigurationData
where
    D: Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadState>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        client: &Client,
        conf: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let g_state = state.output_management_state();
        let outdated = self.serial != g_state.serial;

        let new_config = g_state
            .clients
            .get_mut(&client.id())
            .and_then(|data| data.confs.get_mut(conf));

        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                let Some(output_name) = head.data::<OutputHeadName>().map(|d| &d.0) else {
                    tracing::error!("EnableHead: missing attached output");
                    let _fail = data_init.init(id, OutputConfigurationHeadState::Cancelled);
                    return;
                };
                if outdated {
                    let _fail = data_init.init(id, OutputConfigurationHeadState::Cancelled);
                    return;
                }

                let new_config = match ongoing_output_config_mut(new_config) {
                    Ok(new_config) => new_config,
                    Err(err) => {
                        report_output_config_error(conf, err);
                        let _fail = data_init.init(id, OutputConfigurationHeadState::Cancelled);
                        return;
                    }
                };

                let Some(current_head) = g_state.current_state.get(output_name) else {
                    tracing::error!("EnableHead: output missing from current state");
                    let _fail = data_init.init(id, OutputConfigurationHeadState::Cancelled);
                    return;
                };

                if insert_output_config(
                    new_config,
                    output_name.clone(),
                    output_config_from_head(current_head),
                )
                .is_err()
                {
                    let _fail = data_init.init(id, OutputConfigurationHeadState::Cancelled);
                    conf.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                        "head has been already configured",
                    );
                    return;
                }

                data_init.init(
                    id,
                    OutputConfigurationHeadState::Ok(output_name.clone(), conf.clone()),
                );
            }
            zwlr_output_configuration_v1::Request::DisableHead { head } => {
                if outdated {
                    return;
                }
                let Some(output_name) = head.data::<OutputHeadName>().map(|d| &d.0) else {
                    tracing::error!("DisableHead: missing attached output");
                    return;
                };

                let new_config = match ongoing_output_config_mut(new_config) {
                    Ok(new_config) => new_config,
                    Err(err) => {
                        report_output_config_error(conf, err);
                        return;
                    }
                };

                if insert_output_config(
                    new_config,
                    output_name.clone(),
                    OutputConfig {
                        enabled: false,
                        ..Default::default()
                    },
                )
                .is_err()
                {
                    conf.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                        "head has been already configured",
                    );
                }
            }
            zwlr_output_configuration_v1::Request::Apply => {
                if outdated {
                    conf.cancelled();
                    return;
                }

                let Some(new_config) = finish_output_configuration_request(conf, new_config) else {
                    return;
                };

                state.apply_output_config(new_config);
                // FIXME: assumes apply_output_config always succeeds.
                conf.succeeded();
            }
            zwlr_output_configuration_v1::Request::Test => {
                if outdated {
                    conf.cancelled();
                    return;
                }

                if finish_output_configuration_request(conf, new_config).is_some() {
                    conf.succeeded();
                }
            }
            zwlr_output_configuration_v1::Request::Destroy => {
                g_state
                    .clients
                    .get_mut(&client.id())
                    .map(|d| d.confs.remove(conf));
            }
            _ => unreachable!(),
        }
    }
}

// Dispatch2 for ZwlrOutputConfigurationHeadV1

impl<D> Dispatch2<ZwlrOutputConfigurationHeadV1, D> for OutputConfigurationHeadState
where
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        client: &Client,
        conf_head: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let g_state = state.output_management_state();
        let Some(client_data) = g_state.clients.get_mut(&client.id()) else {
            tracing::error!("ConfigurationHead: missing client data");
            return;
        };
        let OutputConfigurationHeadState::Ok(output_name, conf) = self else {
            tracing::warn!("ConfigurationHead: request sent to a cancelled head");
            return;
        };
        let Some(serial) = conf.data::<OutputConfigurationData>().map(|d| &d.serial) else {
            tracing::error!("ConfigurationHead: missing serial");
            return;
        };
        if *serial != g_state.serial {
            tracing::warn!("ConfigurationHead: request sent to an outdated configuration");
            return;
        }
        let Some(new_config) = client_data.confs.get_mut(conf) else {
            tracing::error!("ConfigurationHead: unknown configuration");
            return;
        };
        let OutputConfigurationState::Ongoing(new_config) = new_config else {
            conf.post_error(
                zwlr_output_configuration_v1::Error::AlreadyUsed,
                "configuration had already been used",
            );
            return;
        };
        let Some(new_config) = new_config.get_mut(output_name) else {
            tracing::error!("ConfigurationHead: config missing from enabled heads");
            return;
        };

        match request {
            zwlr_output_configuration_head_v1::Request::SetMode { mode } => {
                let index = match client_data
                    .heads
                    .get(output_name)
                    .map(|(_, mods)| mods.iter().position(|m| m.id() == mode.id()))
                {
                    Some(Some(index)) => index,
                    _ => {
                        tracing::warn!("SetMode: failed to find requested mode");
                        conf_head.post_error(
                            zwlr_output_configuration_head_v1::Error::InvalidMode,
                            "failed to find requested mode",
                        );
                        return;
                    }
                };

                let Some(current_head) = g_state.current_state.get(output_name) else {
                    tracing::warn!("SetMode: output missing from current state");
                    return;
                };

                let Some(mode) = current_head.modes.get(index) else {
                    tracing::error!("SetMode: requested mode is out of range");
                    return;
                };

                new_config.mode = Some(Mode {
                    custom: false,
                    mode: ConfiguredMode {
                        width: mode.width as u16,
                        height: mode.height as u16,
                        refresh: refresh_hz(mode.refresh),
                    },
                });
            }
            zwlr_output_configuration_head_v1::Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                let Some(mode) = requested_custom_mode(width, height, refresh) else {
                    tracing::warn!("SetCustomMode: invalid requested custom mode");
                    conf_head.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidCustomMode,
                        "custom mode dimensions or refresh are invalid",
                    );
                    return;
                };
                new_config.mode = Some(mode);
            }
            zwlr_output_configuration_head_v1::Request::SetPosition { x, y } => {
                new_config.position = Some((x, y));
            }
            zwlr_output_configuration_head_v1::Request::SetTransform { transform } => {
                let Some(transform) = requested_transform(transform) else {
                    tracing::warn!("SetTransform: unknown requested transform");
                    conf_head.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidTransform,
                        "unknown transform value",
                    );
                    return;
                };
                new_config.transform = Some(transform);
            }
            zwlr_output_configuration_head_v1::Request::SetScale { scale } => {
                let Some(scale) = requested_scale(scale) else {
                    conf_head.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidScale,
                        "scale is negative, zero, or not finite",
                    );
                    return;
                };
                new_config.scale = Some(scale);
            }
            zwlr_output_configuration_head_v1::Request::SetAdaptiveSync { state } => {
                if !is_known_adaptive_sync_state(state) {
                    tracing::warn!("SetAdaptiveSync: unknown requested adaptive sync state");
                    conf_head.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidAdaptiveSyncState,
                        "unknown adaptive sync value",
                    );
                }
                // ewm doesn't support VRR; valid requests are accepted as no-ops.
            }
            _ => unreachable!(),
        }
    }
}

// Dispatch2 for ZwlrOutputHeadV1

impl<D> Dispatch2<ZwlrOutputHeadV1, D> for OutputHeadName
where
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _output_head: &ZwlrOutputHeadV1,
        request: zwlr_output_head_v1::Request,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_head_v1::Request::Release => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, client: ClientId, _resource: &ZwlrOutputHeadV1) {
        if let Some(c) = state.output_management_state().clients.get_mut(&client) {
            c.heads.remove(&self.0);
        }
    }
}

// Dispatch2 for ZwlrOutputModeV1

impl<D> Dispatch2<ZwlrOutputModeV1, D> for EmptyData
where
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _mode: &ZwlrOutputModeV1,
        request: zwlr_output_mode_v1::Request,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_mode_v1::Request::Release => {}
            _ => unreachable!(),
        }
    }
}

// Helper functions

fn notify_removed_head(clients: &mut HashMap<ClientId, ClientData>, output_name: &str) {
    for data in clients.values_mut() {
        if let Some((head, modes)) = data.heads.remove(output_name) {
            modes.iter().for_each(|m| m.finished());
            head.finished();
        }
    }
}

fn notify_new_head(
    state: &mut OutputManagementState,
    output_name: &str,
    head_state: &OutputHeadState,
) {
    let display = &state.display;
    let clients = &mut state.clients;
    for data in clients.values_mut() {
        if let Some(client) = data.manager.client() {
            send_new_head::<crate::State>(display, &client, data, output_name, head_state);
        }
    }
}

fn send_new_head<D>(
    display: &DisplayHandle,
    client: &Client,
    client_data: &mut ClientData,
    output_name: &str,
    head_state: &OutputHeadState,
) where
    D: Dispatch<ZwlrOutputHeadV1, OutputHeadName>,
    D: Dispatch<ZwlrOutputModeV1, EmptyData>,
    D: 'static,
{
    let new_head = client
        .create_resource::<ZwlrOutputHeadV1, _, D>(
            display,
            client_data.manager.version(),
            OutputHeadName(output_name.to_string()),
        )
        .unwrap();
    client_data.manager.head(&new_head);

    new_head.name(head_state.name.clone());
    new_head.description(format!(
        "{} - {} - {}",
        head_state.make, head_state.model, head_state.name
    ));

    if let Some((w, h)) = head_state.physical_size {
        new_head.physical_size(w, h);
    }

    // Send modes
    let mut new_modes = Vec::with_capacity(head_state.modes.len());
    for (index, mode) in head_state.modes.iter().enumerate() {
        let new_mode = client
            .create_resource::<ZwlrOutputModeV1, _, D>(display, new_head.version(), EmptyData)
            .unwrap();
        new_head.mode(&new_mode);
        new_mode.size(mode.width, mode.height);
        new_mode.refresh(mode.refresh);
        if mode.preferred {
            new_mode.preferred();
        }
        if Some(index) == head_state.current_mode {
            new_head.current_mode(&new_mode);
        }
        new_modes.push(new_mode);
    }

    // Send position/transform/scale for enabled outputs
    if head_state.enabled {
        if let Some((x, y)) = head_state.position {
            new_head.position(x, y);
        }
        if let Some(transform) = head_state.transform {
            new_head.transform(transform.into());
        }
        if let Some(scale) = head_state.scale {
            new_head.scale(scale);
        }
    }

    new_head.enabled(head_state.enabled as i32);

    if new_head.version() >= zwlr_output_head_v1::EVT_MAKE_SINCE {
        new_head.make(head_state.make.clone());
    }
    if new_head.version() >= zwlr_output_head_v1::EVT_MODEL_SINCE {
        new_head.model(head_state.model.clone());
    }
    if new_head.version() >= zwlr_output_head_v1::EVT_SERIAL_NUMBER_SINCE
        && let Some(serial) = &head_state.serial_number
    {
        new_head.serial_number(serial.clone());
    }
    if new_head.version() >= zwlr_output_head_v1::EVT_ADAPTIVE_SYNC_SINCE {
        new_head.adaptive_sync(zwlr_output_head_v1::AdaptiveSyncState::Disabled);
    }

    client_data
        .heads
        .insert(output_name.to_string(), (new_head, new_modes));
}

fn requested_transform(transform: WEnum<WlTransform>) -> Option<Transform> {
    match transform {
        WEnum::Value(WlTransform::Normal) => Some(Transform::Normal),
        WEnum::Value(WlTransform::_90) => Some(Transform::_90),
        WEnum::Value(WlTransform::_180) => Some(Transform::_180),
        WEnum::Value(WlTransform::_270) => Some(Transform::_270),
        WEnum::Value(WlTransform::Flipped) => Some(Transform::Flipped),
        WEnum::Value(WlTransform::Flipped90) => Some(Transform::Flipped90),
        WEnum::Value(WlTransform::Flipped180) => Some(Transform::Flipped180),
        WEnum::Value(WlTransform::Flipped270) => Some(Transform::Flipped270),
        WEnum::Value(_) | WEnum::Unknown(_) => None,
    }
}

fn requested_scale(scale: f64) -> Option<f64> {
    if scale.is_finite() && scale > 0.0 {
        Some(scale)
    } else {
        None
    }
}

/// Convert a wl_output refresh rate in mHz to Hz, treating 0 as unspecified.
fn refresh_hz(millihertz: i32) -> Option<f64> {
    (millihertz > 0).then_some(millihertz as f64 / 1000.0)
}

fn requested_custom_mode(width: i32, height: i32, refresh: i32) -> Option<Mode> {
    if width <= 0 || height <= 0 || refresh < 0 {
        None
    } else {
        Some(Mode {
            custom: true,
            mode: ConfiguredMode {
                width: width as u16,
                height: height as u16,
                refresh: refresh_hz(refresh),
            },
        })
    }
}

fn is_known_adaptive_sync_state(state: WEnum<AdaptiveSyncState>) -> bool {
    match state {
        WEnum::Value(AdaptiveSyncState::Enabled | AdaptiveSyncState::Disabled) => true,
        WEnum::Value(_) | WEnum::Unknown(_) => false,
    }
}

/// Build an OutputConfig from the current head state (used when enabling a head).
fn output_config_from_head(head: &OutputHeadState) -> OutputConfig {
    let mode = head.current_mode.and_then(|idx| {
        head.modes.get(idx).map(|m| Mode {
            custom: false,
            mode: ConfiguredMode {
                width: m.width as u16,
                height: m.height as u16,
                refresh: refresh_hz(m.refresh),
            },
        })
    });

    OutputConfig {
        mode,
        modeline: None,
        position: head.position,
        scale: head.scale,
        transform: head.transform,
        enabled: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: i32, height: i32, refresh: i32) -> OutputModeState {
        OutputModeState {
            width,
            height,
            refresh,
            preferred: false,
        }
    }

    fn head(name: &str) -> OutputHeadState {
        OutputHeadState {
            name: name.to_string(),
            make: "make".to_string(),
            model: "model".to_string(),
            serial_number: None,
            physical_size: Some((600, 340)),
            enabled: true,
            modes: vec![mode(1920, 1080, 60_000)],
            current_mode: Some(0),
            position: Some((0, 0)),
            scale: Some(1.0),
            transform: Some(Transform::Normal),
        }
    }

    fn state(heads: impl IntoIterator<Item = OutputHeadState>) -> HashMap<String, OutputHeadState> {
        heads
            .into_iter()
            .map(|head| (head.name.clone(), head))
            .collect()
    }

    fn config(enabled: bool) -> OutputConfig {
        OutputConfig {
            enabled,
            ..Default::default()
        }
    }

    fn config_state(
        configs: impl IntoIterator<Item = (&'static str, OutputConfig)>,
    ) -> OutputConfigurationState {
        OutputConfigurationState::Ongoing(
            configs
                .into_iter()
                .map(|(name, config)| (name.to_string(), config))
                .collect(),
        )
    }

    #[test]
    fn change_plan_is_empty_for_equal_states() {
        let current = state([head("HEAD-A")]);
        let new = current.clone();

        let plan = output_change_plan(&current, &new);

        assert!(plan.is_empty());
    }

    #[test]
    fn change_plan_detects_added_and_removed_outputs() {
        let current = state([head("HEAD-A")]);
        let new = state([head("HEAD-B")]);

        let plan = output_change_plan(&current, &new);

        assert_eq!(plan.added, ["HEAD-B"]);
        assert_eq!(plan.removed, ["HEAD-A"]);
        assert!(plan.updated.is_empty());
        assert!(!plan.is_empty());
    }

    #[test]
    fn change_plan_tracks_mode_current_mode_and_properties() {
        let current_head = head("HEAD-A");
        let mut new_head = current_head.clone();
        new_head.modes[0] = mode(2560, 1440, 144_000);
        new_head.scale = Some(1.5);
        new_head.transform = Some(Transform::_90);

        let plan = output_change_plan(&state([current_head]), &state([new_head]));

        assert_eq!(plan.added, Vec::<String>::new());
        assert_eq!(plan.removed, Vec::<String>::new());
        assert_eq!(plan.updated.len(), 1);

        let change = &plan.updated[0];
        assert_eq!(change.output_name, "HEAD-A");
        assert!(change.modes_changed);
        assert_eq!(change.current_mode, CurrentModeChange::Set(0));
        assert_eq!(
            change.properties,
            Some(HeadPropertyChange {
                position: None,
                scale: Some(1.5),
                transform: Some(Transform::_90),
            }),
        );
    }

    #[test]
    fn change_plan_tracks_enable_and_disable_transitions() {
        let enabled = head("HEAD-A");
        let mut disabled = enabled.clone();
        disabled.enabled = false;
        disabled.current_mode = None;

        let disable_plan =
            output_change_plan(&state([enabled.clone()]), &state([disabled.clone()]));
        assert_eq!(
            disable_plan.updated[0].current_mode,
            CurrentModeChange::Disabled,
        );

        let enable_plan = output_change_plan(&state([disabled]), &state([enabled]));
        assert_eq!(
            enable_plan.updated[0].current_mode,
            CurrentModeChange::Enabled(0),
        );
    }

    #[test]
    fn disabled_outputs_do_not_emit_property_changes() {
        let mut old_head = head("HEAD-A");
        old_head.enabled = false;
        old_head.current_mode = None;

        let mut new_head = old_head.clone();
        new_head.position = Some((100, 200));
        new_head.scale = Some(2.0);
        new_head.transform = Some(Transform::_180);

        let plan = output_change_plan(&state([old_head]), &state([new_head]));

        assert!(plan.is_empty());
    }

    #[test]
    fn taking_output_config_requires_one_enabled_head_and_finishes_state() {
        let mut state = config_state([("HEAD-A", config(false)), ("HEAD-B", config(true))]);

        let config = take_ongoing_output_config(Some(&mut state)).unwrap();

        assert!(!config["HEAD-A"].enabled);
        assert!(config["HEAD-B"].enabled);
        assert!(matches!(state, OutputConfigurationState::Finished));
    }

    #[test]
    fn taking_all_disabled_output_config_fails_and_finishes_state() {
        let mut state = config_state([("HEAD-A", config(false))]);

        let err = take_ongoing_output_config(Some(&mut state)).unwrap_err();

        assert_eq!(err, FinishConfigError::AllDisabled);
        assert!(matches!(state, OutputConfigurationState::Finished));
    }

    #[test]
    fn taking_finished_or_missing_output_config_reports_protocol_errors() {
        let mut state = OutputConfigurationState::Finished;

        let already_used = take_ongoing_output_config(Some(&mut state)).unwrap_err();
        let missing = take_ongoing_output_config(None).unwrap_err();

        assert_eq!(already_used, FinishConfigError::AlreadyUsed);
        assert_eq!(missing, FinishConfigError::Missing);
        assert!(matches!(state, OutputConfigurationState::Finished));
    }

    #[test]
    fn ongoing_output_config_mut_exposes_only_unfinished_configs() {
        let mut ongoing = config_state([]);
        let mut finished = OutputConfigurationState::Finished;

        assert!(ongoing_output_config_mut(Some(&mut ongoing)).is_ok());
        assert_eq!(
            ongoing_output_config_mut(Some(&mut finished)).unwrap_err(),
            FinishConfigError::AlreadyUsed,
        );
        assert_eq!(
            ongoing_output_config_mut(None).unwrap_err(),
            FinishConfigError::Missing,
        );
    }

    #[test]
    fn insert_output_config_rejects_duplicate_heads() {
        let mut configs = HashMap::new();
        let source = head("HEAD-A");
        let enabled = output_config_from_head(&source);
        let disabled = config(false);

        assert_eq!(
            insert_output_config(&mut configs, "HEAD-A".to_string(), enabled),
            Ok(()),
        );
        assert_eq!(
            insert_output_config(&mut configs, "HEAD-A".to_string(), disabled),
            Err(ConfigureHeadError::AlreadyConfigured),
        );

        let stored = &configs["HEAD-A"];
        assert!(stored.enabled);
        assert_eq!(
            stored.mode,
            Some(Mode {
                custom: false,
                mode: ConfiguredMode {
                    width: 1920,
                    height: 1080,
                    refresh: Some(60.0),
                },
            })
        );
        assert_eq!(stored.position, Some((0, 0)));
        assert_eq!(stored.scale, Some(1.0));
        assert_eq!(stored.transform, Some(Transform::Normal));
    }

    #[test]
    fn requested_transform_rejects_unknown_protocol_values() {
        assert_eq!(
            requested_transform(WEnum::Value(WlTransform::_270)),
            Some(Transform::_270),
        );
        assert_eq!(
            requested_transform(WEnum::Value(WlTransform::Flipped90)),
            Some(Transform::Flipped90),
        );
        assert_eq!(requested_transform(WEnum::Unknown(99)), None);
    }

    #[test]
    fn requested_scale_rejects_non_positive_and_non_finite_values() {
        assert_eq!(requested_scale(1.25), Some(1.25));
        assert_eq!(requested_scale(0.0), None);
        assert_eq!(requested_scale(-1.0), None);
        assert_eq!(requested_scale(f64::NAN), None);
        assert_eq!(requested_scale(f64::INFINITY), None);
    }

    #[test]
    fn requested_custom_mode_accepts_unspecified_refresh_only() {
        assert_eq!(
            requested_custom_mode(1920, 1080, 60_000),
            Some(Mode {
                custom: true,
                mode: ConfiguredMode {
                    width: 1920,
                    height: 1080,
                    refresh: Some(60.0),
                },
            })
        );
        assert_eq!(
            requested_custom_mode(1920, 1080, 0),
            Some(Mode {
                custom: true,
                mode: ConfiguredMode {
                    width: 1920,
                    height: 1080,
                    refresh: None,
                },
            })
        );
        assert_eq!(requested_custom_mode(0, 1080, 60_000), None);
        assert_eq!(requested_custom_mode(1920, 0, 60_000), None);
        assert_eq!(requested_custom_mode(1920, 1080, -1), None);
    }

    #[test]
    fn adaptive_sync_validation_rejects_unknown_protocol_values() {
        assert!(is_known_adaptive_sync_state(WEnum::Value(
            AdaptiveSyncState::Disabled,
        )));
        assert!(is_known_adaptive_sync_state(WEnum::Value(
            AdaptiveSyncState::Enabled,
        )));
        assert!(!is_known_adaptive_sync_state(WEnum::Unknown(99)));
    }
}
