//! wlr-output-management-unstable-v1 protocol implementation
//!
//! Allows tools like wlr-randr and kanshi to query and configure outputs.
//! Ported from niri's implementation, adapted to ewm's types.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
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
use zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1;
use zwlr_output_configuration_v1::ZwlrOutputConfigurationV1;
use zwlr_output_head_v1::ZwlrOutputHeadV1;
use zwlr_output_manager_v1::ZwlrOutputManagerV1;
use zwlr_output_mode_v1::ZwlrOutputModeV1;

use crate::OutputConfig;

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

pub enum OutputConfigurationHeadState {
    Cancelled,
    Ok(String, ZwlrOutputConfigurationV1),
}

impl OutputManagementState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
        D: Dispatch<ZwlrOutputManagerV1, ()>,
        D: Dispatch<ZwlrOutputHeadV1, String>,
        D: Dispatch<ZwlrOutputConfigurationV1, u32>,
        D: Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadState>,
        D: Dispatch<ZwlrOutputModeV1, ()>,
        D: OutputManagementHandler,
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
        let mut changed = false;

        for (output_name, new_head) in new_state.iter() {
            if let Some(old) = self.current_state.get(output_name) {
                // Check for mode list changes
                let modes_changed = old.modes != new_head.modes;
                if modes_changed {
                    changed = true;
                    for client in self.clients.values_mut() {
                        if let Some((head, modes)) = client.heads.get_mut(output_name) {
                            // Update existing modes (shortest iterator wins)
                            let common_len = modes.len().min(new_head.modes.len());
                            for (wl_mode, mode) in
                                modes.iter().zip(&new_head.modes).take(common_len)
                            {
                                wl_mode.size(mode.width, mode.height);
                                wl_mode.refresh(mode.refresh);
                            }

                            if let Some(client_obj) = client.manager.client() {
                                if new_head.modes.len() > common_len {
                                    // New modes added
                                    for mode in &new_head.modes[common_len..] {
                                        let new_mode = client_obj
                                            .create_resource::<ZwlrOutputModeV1, _, crate::State>(
                                                &self.display,
                                                head.version(),
                                                (),
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
                                    // Modes removed
                                    for mode in modes.drain(common_len..) {
                                        mode.finished();
                                    }
                                }
                            }
                        }
                    }
                }

                // Check current mode changes
                match (old.current_mode, new_head.current_mode) {
                    (Some(old_index), Some(new_index)) => {
                        if old.modes.len() == new_head.modes.len()
                            && (modes_changed || old_index != new_index)
                        {
                            changed = true;
                            for client in self.clients.values() {
                                if let Some((head, modes)) = client.heads.get(output_name) {
                                    if let Some(new_mode) = modes.get(new_index) {
                                        head.current_mode(new_mode);
                                    }
                                }
                            }
                        }
                    }
                    (Some(_), None) => {
                        changed = true;
                        for client in self.clients.values() {
                            if let Some((head, _)) = client.heads.get(output_name) {
                                head.enabled(0);
                            }
                        }
                    }
                    (None, Some(new_index)) => {
                        if old.modes.len() == new_head.modes.len() {
                            changed = true;
                            for client in self.clients.values() {
                                if let Some((head, modes)) = client.heads.get(output_name) {
                                    head.enabled(1);
                                    if let Some(mode) = modes.get(new_index) {
                                        head.current_mode(mode);
                                    }
                                }
                            }
                        }
                    }
                    (None, None) => {}
                }

                // Check position/scale/transform changes
                if old.position != new_head.position
                    || old.scale != new_head.scale
                    || old.transform != new_head.transform
                {
                    if new_head.enabled {
                        changed = true;
                        for client in self.clients.values() {
                            if let Some((head, _)) = client.heads.get(output_name) {
                                if let Some((x, y)) = new_head.position {
                                    if old.position != new_head.position {
                                        head.position(x, y);
                                    }
                                }
                                if old.scale != new_head.scale {
                                    if let Some(scale) = new_head.scale {
                                        head.scale(scale);
                                    }
                                }
                                if old.transform != new_head.transform {
                                    if let Some(transform) = new_head.transform {
                                        head.transform(transform.into());
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                // New output
                changed = true;
                notify_new_head(self, output_name, new_head);
            }
        }

        // Check for removed outputs
        for old_name in self.current_state.keys() {
            if !new_state.contains_key(old_name) {
                changed = true;
                notify_removed_head(&mut self.clients, old_name);
            }
        }

        if changed {
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
}

// --- GlobalDispatch for ZwlrOutputManagerV1 ---

impl<D> GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData, D> for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadState>,
    D: Dispatch<ZwlrOutputModeV1, ()>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn bind(
        state: &mut D,
        display: &DisplayHandle,
        client: &Client,
        manager: New<ZwlrOutputManagerV1>,
        _manager_state: &OutputManagementGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(manager, ());
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

    fn can_view(client: Client, global_data: &OutputManagementGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

// --- Dispatch for ZwlrOutputManagerV1 ---

impl<D> Dispatch<ZwlrOutputManagerV1, (), D> for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadState>,
    D: Dispatch<ZwlrOutputModeV1, ()>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        client: &Client,
        _manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                let g_state = state.output_management_state();
                let conf = data_init.init(id, serial);
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

    fn destroyed(state: &mut D, client: ClientId, _resource: &ZwlrOutputManagerV1, _data: &()) {
        state.output_management_state().clients.remove(&client);
    }
}

// --- Dispatch for ZwlrOutputConfigurationV1 ---

impl<D> Dispatch<ZwlrOutputConfigurationV1, u32, D> for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadState>,
    D: Dispatch<ZwlrOutputModeV1, ()>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        client: &Client,
        conf: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        serial: &u32,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let g_state = state.output_management_state();
        let outdated = *serial != g_state.serial;

        let new_config = g_state
            .clients
            .get_mut(&client.id())
            .and_then(|data| data.confs.get_mut(conf));

        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                let Some(output_name) = head.data::<String>() else {
                    tracing::error!("EnableHead: missing attached output");
                    let _fail = data_init.init(id, OutputConfigurationHeadState::Cancelled);
                    return;
                };
                if outdated {
                    let _fail = data_init.init(id, OutputConfigurationHeadState::Cancelled);
                    return;
                }

                let Some(new_config) = new_config else {
                    let _fail = data_init.init(id, OutputConfigurationHeadState::Cancelled);
                    return;
                };

                let OutputConfigurationState::Ongoing(new_config) = new_config else {
                    let _fail = data_init.init(id, OutputConfigurationHeadState::Cancelled);
                    conf.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyUsed,
                        "configuration had already been used",
                    );
                    return;
                };

                let Some(current_head) = g_state.current_state.get(output_name) else {
                    tracing::error!("EnableHead: output missing from current state");
                    let _fail = data_init.init(id, OutputConfigurationHeadState::Cancelled);
                    return;
                };

                match new_config.entry(output_name.clone()) {
                    Entry::Occupied(_) => {
                        let _fail = data_init.init(id, OutputConfigurationHeadState::Cancelled);
                        conf.post_error(
                            zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                            "head has been already configured",
                        );
                        return;
                    }
                    Entry::Vacant(entry) => {
                        // Build an OutputConfig from the current head state
                        let config = output_config_from_head(current_head);
                        entry.insert(config);
                    }
                };

                data_init.init(
                    id,
                    OutputConfigurationHeadState::Ok(output_name.clone(), conf.clone()),
                );
            }
            zwlr_output_configuration_v1::Request::DisableHead { head } => {
                if outdated {
                    return;
                }
                let Some(output_name) = head.data::<String>() else {
                    tracing::error!("DisableHead: missing attached output");
                    return;
                };

                let Some(new_config) = new_config else {
                    return;
                };

                let OutputConfigurationState::Ongoing(new_config) = new_config else {
                    conf.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyUsed,
                        "configuration had already been used",
                    );
                    return;
                };

                match new_config.entry(output_name.clone()) {
                    Entry::Occupied(_) => {
                        conf.post_error(
                            zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                            "head has been already configured",
                        );
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(OutputConfig {
                            enabled: false,
                            ..Default::default()
                        });
                    }
                };
            }
            zwlr_output_configuration_v1::Request::Apply => {
                if outdated {
                    conf.cancelled();
                    return;
                }

                let Some(new_config) = new_config else {
                    return;
                };

                let OutputConfigurationState::Ongoing(new_config) =
                    mem::replace(new_config, OutputConfigurationState::Finished)
                else {
                    conf.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyUsed,
                        "configuration had already been used",
                    );
                    return;
                };

                let any_enabled = new_config.values().any(|c| c.enabled);
                if !any_enabled {
                    conf.failed();
                    return;
                }

                state.apply_output_config(new_config);
                // Assume success (same as niri: FIXME for verification)
                conf.succeeded();
            }
            zwlr_output_configuration_v1::Request::Test => {
                if outdated {
                    conf.cancelled();
                    return;
                }

                let Some(new_config) = new_config else {
                    return;
                };

                let OutputConfigurationState::Ongoing(new_config) =
                    mem::replace(new_config, OutputConfigurationState::Finished)
                else {
                    conf.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyUsed,
                        "configuration had already been used",
                    );
                    return;
                };

                let any_enabled = new_config.values().any(|c| c.enabled);
                if !any_enabled {
                    conf.failed();
                    return;
                }

                conf.succeeded();
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

// --- Dispatch for ZwlrOutputConfigurationHeadV1 ---

impl<D> Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadState, D>
    for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadState>,
    D: Dispatch<ZwlrOutputModeV1, ()>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        client: &Client,
        _conf_head: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        data: &OutputConfigurationHeadState,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let g_state = state.output_management_state();
        let Some(client_data) = g_state.clients.get_mut(&client.id()) else {
            tracing::error!("ConfigurationHead: missing client data");
            return;
        };
        let OutputConfigurationHeadState::Ok(output_name, conf) = data else {
            tracing::warn!("ConfigurationHead: request sent to a cancelled head");
            return;
        };
        let Some(serial) = conf.data::<u32>() else {
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

                new_config.mode = Some((
                    mode.width,
                    mode.height,
                    if mode.refresh > 0 {
                        Some(mode.refresh)
                    } else {
                        None
                    },
                ));
            }
            zwlr_output_configuration_head_v1::Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                // ewm doesn't support custom modes, but we can accept it as a mode request
                if refresh == 0 {
                    tracing::warn!("SetCustomMode: refresh 0 requested, ignoring");
                    return;
                }
                new_config.mode = Some((width, height, Some(refresh)));
            }
            zwlr_output_configuration_head_v1::Request::SetPosition { x, y } => {
                new_config.position = Some((x, y));
            }
            zwlr_output_configuration_head_v1::Request::SetTransform { transform } => {
                let transform = match transform {
                    WEnum::Value(WlTransform::Normal) => Transform::Normal,
                    WEnum::Value(WlTransform::_90) => Transform::_90,
                    WEnum::Value(WlTransform::_180) => Transform::_180,
                    WEnum::Value(WlTransform::_270) => Transform::_270,
                    WEnum::Value(WlTransform::Flipped) => Transform::Flipped,
                    WEnum::Value(WlTransform::Flipped90) => Transform::Flipped90,
                    WEnum::Value(WlTransform::Flipped180) => Transform::Flipped180,
                    WEnum::Value(WlTransform::Flipped270) => Transform::Flipped270,
                    _ => {
                        tracing::warn!("SetTransform: unknown transform value");
                        return;
                    }
                };
                new_config.transform = Some(transform);
            }
            zwlr_output_configuration_head_v1::Request::SetScale { scale } => {
                if scale <= 0. {
                    return;
                }
                new_config.scale = Some(scale);
            }
            zwlr_output_configuration_head_v1::Request::SetAdaptiveSync { .. } => {
                // ewm doesn't support VRR, ignore
            }
            _ => unreachable!(),
        }
    }
}

// --- Dispatch for ZwlrOutputHeadV1 ---

impl<D> Dispatch<ZwlrOutputHeadV1, String, D> for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadState>,
    D: Dispatch<ZwlrOutputModeV1, ()>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _output_head: &ZwlrOutputHeadV1,
        request: zwlr_output_head_v1::Request,
        _data: &String,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_head_v1::Request::Release => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(state: &mut D, client: ClientId, _resource: &ZwlrOutputHeadV1, data: &String) {
        if let Some(c) = state.output_management_state().clients.get_mut(&client) {
            c.heads.remove(data);
        }
    }
}

// --- Dispatch for ZwlrOutputModeV1 ---

impl<D> Dispatch<ZwlrOutputModeV1, (), D> for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadState>,
    D: Dispatch<ZwlrOutputModeV1, ()>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _mode: &ZwlrOutputModeV1,
        request: zwlr_output_mode_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_mode_v1::Request::Release => {}
            _ => unreachable!(),
        }
    }
}

// --- Delegate macro ---

#[macro_export]
macro_rules! delegate_output_management {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1: $crate::protocols::output_management::OutputManagementGlobalData
        ] => $crate::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1: ()
        ] => $crate::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_configuration_v1::ZwlrOutputConfigurationV1: u32
        ] => $crate::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_head_v1::ZwlrOutputHeadV1: String
        ] => $crate::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_mode_v1::ZwlrOutputModeV1: ()
        ] => $crate::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1: $crate::protocols::output_management::OutputConfigurationHeadState
        ] => $crate::protocols::output_management::OutputManagementState);
    };
}

// --- Helper functions ---

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
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, OutputConfigurationHeadState>,
    D: Dispatch<ZwlrOutputModeV1, ()>,
    D: OutputManagementHandler,
    D: 'static,
{
    let new_head = client
        .create_resource::<ZwlrOutputHeadV1, _, D>(
            display,
            client_data.manager.version(),
            output_name.to_string(),
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
            .create_resource::<ZwlrOutputModeV1, _, D>(display, new_head.version(), ())
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
    if new_head.version() >= zwlr_output_head_v1::EVT_SERIAL_NUMBER_SINCE {
        if let Some(serial) = &head_state.serial_number {
            new_head.serial_number(serial.clone());
        }
    }
    if new_head.version() >= zwlr_output_head_v1::EVT_ADAPTIVE_SYNC_SINCE {
        new_head.adaptive_sync(zwlr_output_head_v1::AdaptiveSyncState::Disabled);
    }

    client_data
        .heads
        .insert(output_name.to_string(), (new_head, new_modes));
}

/// Build an OutputConfig from the current head state (used when enabling a head).
fn output_config_from_head(head: &OutputHeadState) -> OutputConfig {
    let mode = head.current_mode.and_then(|idx| {
        head.modes.get(idx).map(|m| {
            (
                m.width,
                m.height,
                if m.refresh > 0 { Some(m.refresh) } else { None },
            )
        })
    });

    OutputConfig {
        mode,
        position: head.position,
        scale: head.scale,
        transform: head.transform,
        enabled: true,
    }
}
