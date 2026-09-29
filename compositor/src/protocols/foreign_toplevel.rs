//! wlr-foreign-toplevel-management-v1 protocol implementation
//!
//! Exposes toplevel windows to external tools (taskbars, window switchers, etc.)
//! Based on niri's implementation.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::foreign_toplevel_list::v1::server::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};

use crate::protocols::EmptyData;

const EXT_LIST_VERSION: u32 = 1;
const WLR_MANAGEMENT_VERSION: u32 = 3;

pub struct ForeignToplevelManagerState {
    display: DisplayHandle,
    ext_list_instances: HashSet<ExtForeignToplevelListV1>,
    wlr_management_instances: HashSet<ZwlrForeignToplevelManagerV1>,
    toplevels: HashMap<WlSurface, ToplevelData>,
}

pub trait ForeignToplevelHandler {
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState;
    /// Whether external tools should be prevented from controlling this surface.
    fn is_read_only_surface(&self, wl_surface: &WlSurface) -> bool;
    fn activate(&mut self, wl_surface: WlSurface);
    fn close(&mut self, wl_surface: WlSurface);
    fn set_fullscreen(&mut self, wl_surface: WlSurface, wl_output: Option<WlOutput>);
    fn unset_fullscreen(&mut self, wl_surface: WlSurface);
    fn set_maximized(&mut self, wl_surface: WlSurface);
    fn unset_maximized(&mut self, wl_surface: WlSurface);
    fn minimize(&mut self, wl_surface: WlSurface);
}

struct ToplevelData {
    id: u64,
    title: Option<String>,
    app_id: Option<String>,
    states: Vec<u32>,
    output: Option<Output>,
    ext_list_instances: HashSet<ExtForeignToplevelHandleV1>,
    wlr_management_instances: HashMap<ZwlrForeignToplevelHandleV1, Vec<WlOutput>>,
}

#[derive(Clone)]
pub struct ForeignToplevelGlobalData {
    filter: Arc<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

/// Window info for foreign toplevel tracking
pub struct WindowInfo {
    pub id: u64,
    pub surface: WlSurface,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub output: Option<Output>,
    pub is_focused: bool,
    pub is_fullscreen: bool,
}

impl ForeignToplevelManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelGlobalData>,
        D: GlobalDispatch<ExtForeignToplevelListV1, ForeignToplevelGlobalData>,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = ForeignToplevelGlobalData {
            filter: Arc::new(filter),
        };
        display
            .create_global::<D, ExtForeignToplevelListV1, _>(EXT_LIST_VERSION, global_data.clone());
        display.create_global::<D, ZwlrForeignToplevelManagerV1, _>(
            WLR_MANAGEMENT_VERSION,
            global_data,
        );
        Self {
            display: display.clone(),
            ext_list_instances: HashSet::new(),
            wlr_management_instances: HashSet::new(),
            toplevels: HashMap::new(),
        }
    }

    /// Refresh toplevel state. Call this each frame after focus updates.
    pub fn refresh<D>(&mut self, windows: Vec<WindowInfo>)
    where
        D: Dispatch<ZwlrForeignToplevelHandleV1, EmptyData>,
        D: Dispatch<ExtForeignToplevelHandleV1, EmptyData>,
        D: 'static,
    {
        let current_surfaces: std::collections::HashSet<_> =
            windows.iter().map(|w| w.surface.clone()).collect();

        // Handle closed windows
        self.toplevels.retain(|surface, data| {
            if current_surfaces.contains(surface) {
                return true;
            }
            // Send closed event to all clients
            for instance in &data.ext_list_instances {
                instance.closed();
            }
            for instance in data.wlr_management_instances.keys() {
                instance.closed();
            }
            false
        });

        // Process non-focused windows first, then focused window last
        // This ensures deactivate happens before activate
        let mut focused_window = None;
        for window in &windows {
            if window.is_focused {
                focused_window = Some(window);
            } else {
                self.refresh_toplevel::<D>(window);
            }
        }

        // Process focused window last
        if let Some(window) = focused_window {
            self.refresh_toplevel::<D>(window);
        }
    }

    fn refresh_toplevel<D>(&mut self, window: &WindowInfo)
    where
        D: Dispatch<ZwlrForeignToplevelHandleV1, EmptyData>,
        D: Dispatch<ExtForeignToplevelHandleV1, EmptyData>,
        D: 'static,
    {
        let states = to_state_vec(window.is_focused, window.is_fullscreen);

        match self.toplevels.entry(window.surface.clone()) {
            Entry::Occupied(entry) => {
                let data = entry.into_mut();

                let mut new_title = None;
                if data.title != window.title {
                    data.title.clone_from(&window.title);
                    new_title = window.title.as_deref();
                }

                let mut new_app_id = None;
                if data.app_id != window.app_id {
                    data.app_id.clone_from(&window.app_id);
                    new_app_id = window.app_id.as_deref();
                }

                let mut states_changed = false;
                if data.states != states {
                    data.states = states.clone();
                    states_changed = true;
                }

                let mut output_changed = false;
                if data.output != window.output {
                    data.output.clone_from(&window.output);
                    output_changed = true;
                }

                let something_changed_for_ext = new_title.is_some() || new_app_id.is_some();

                let something_changed_for_wlr =
                    new_title.is_some() || new_app_id.is_some() || states_changed || output_changed;

                if something_changed_for_ext {
                    for instance in &data.ext_list_instances {
                        if let Some(new_title) = new_title {
                            instance.title(new_title.to_owned());
                        }
                        if let Some(new_app_id) = new_app_id {
                            instance.app_id(new_app_id.to_owned());
                        }
                        instance.done();
                    }
                }

                if something_changed_for_wlr {
                    for (instance, outputs) in &mut data.wlr_management_instances {
                        if let Some(new_title) = new_title {
                            instance.title(new_title.to_owned());
                        }
                        if let Some(new_app_id) = new_app_id {
                            instance.app_id(new_app_id.to_owned());
                        }
                        if states_changed {
                            instance
                                .state(data.states.iter().flat_map(|x| x.to_ne_bytes()).collect());
                        }
                        if output_changed {
                            for wl_output in outputs.drain(..) {
                                instance.output_leave(&wl_output);
                            }
                            if let Some(output) = &data.output
                                && let Some(client) = instance.client()
                            {
                                for wl_output in output.client_outputs(&client) {
                                    instance.output_enter(&wl_output);
                                    outputs.push(wl_output);
                                }
                            }
                        }
                        instance.done();
                    }
                }

                // Clean up dead wl_outputs
                for outputs in data.wlr_management_instances.values_mut() {
                    outputs.retain(|x| x.is_alive());
                }
            }
            Entry::Vacant(entry) => {
                // New window
                let mut data = ToplevelData {
                    id: window.id,
                    title: window.title.clone(),
                    app_id: window.app_id.clone(),
                    states,
                    output: window.output.clone(),
                    wlr_management_instances: HashMap::new(),
                    ext_list_instances: HashSet::new(),
                };

                for manager in &self.ext_list_instances {
                    if let Some(client) = manager.client() {
                        data.add_ext_instance::<D>(&self.display, &client, manager);
                    }
                }

                for manager in &self.wlr_management_instances {
                    if let Some(client) = manager.client() {
                        data.add_wlr_instance::<D>(&self.display, &client, manager);
                    }
                }

                entry.insert(data);
            }
        }
    }

    pub fn output_bound(&mut self, output: &Output, wl_output: &WlOutput) {
        let Some(client) = wl_output.client() else {
            return;
        };

        for data in self.toplevels.values_mut() {
            if data.output.as_ref() != Some(output) {
                continue;
            }

            for (instance, outputs) in &mut data.wlr_management_instances {
                if instance.client().as_ref() != Some(&client) {
                    continue;
                }

                instance.output_enter(wl_output);
                instance.done();
                outputs.push(wl_output.clone());
            }
        }
    }
}

impl ToplevelData {
    fn add_ext_instance<D>(
        &mut self,
        handle: &DisplayHandle,
        client: &Client,
        manager: &ExtForeignToplevelListV1,
    ) where
        D: Dispatch<ExtForeignToplevelHandleV1, EmptyData> + 'static,
    {
        let Ok(toplevel) = client.create_resource::<ExtForeignToplevelHandleV1, _, D>(
            handle,
            manager.version(),
            EmptyData,
        ) else {
            return;
        };
        manager.toplevel(&toplevel);

        toplevel.identifier(format!("{}", self.id));

        if let Some(title) = &self.title {
            toplevel.title(title.clone());
        }
        if let Some(app_id) = &self.app_id {
            toplevel.app_id(app_id.clone());
        }

        toplevel.done();

        self.ext_list_instances.insert(toplevel);
    }
    fn add_wlr_instance<D>(
        &mut self,
        handle: &DisplayHandle,
        client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
    ) where
        D: Dispatch<ZwlrForeignToplevelHandleV1, EmptyData> + 'static,
    {
        let Ok(toplevel) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, D>(
            handle,
            manager.version(),
            EmptyData,
        ) else {
            return;
        };
        manager.toplevel(&toplevel);

        if let Some(title) = &self.title {
            toplevel.title(title.clone());
        }
        if let Some(app_id) = &self.app_id {
            toplevel.app_id(app_id.clone());
        }

        toplevel.state(self.states.iter().flat_map(|x| x.to_ne_bytes()).collect());

        let mut outputs = Vec::new();
        if let Some(output) = &self.output {
            for wl_output in output.client_outputs(client) {
                toplevel.output_enter(&wl_output);
                outputs.push(wl_output);
            }
        }

        toplevel.done();

        self.wlr_management_instances.insert(toplevel, outputs);
    }
}

impl<D> GlobalDispatch2<ExtForeignToplevelListV1, D> for ForeignToplevelGlobalData
where
    D: Dispatch<ExtForeignToplevelListV1, EmptyData>,
    D: Dispatch<ExtForeignToplevelHandleV1, EmptyData>,
    D: ForeignToplevelHandler,
{
    fn bind(
        &self,
        state: &mut D,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ExtForeignToplevelListV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, EmptyData);

        let state = state.foreign_toplevel_manager_state();

        for data in state.toplevels.values_mut() {
            data.add_ext_instance::<D>(handle, client, &manager);
        }

        state.ext_list_instances.insert(manager);
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

impl<D> Dispatch2<ExtForeignToplevelListV1, D> for EmptyData
where
    D: ForeignToplevelHandler,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &ExtForeignToplevelListV1,
        request: <ExtForeignToplevelListV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_foreign_toplevel_list_v1::Request::Stop => {
                resource.finished();
                let state = state.foreign_toplevel_manager_state();
                state.ext_list_instances.remove(resource);
            }
            ext_foreign_toplevel_list_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &ExtForeignToplevelListV1) {
        let state = state.foreign_toplevel_manager_state();
        state.ext_list_instances.remove(resource);
    }
}

impl<D> Dispatch2<ExtForeignToplevelHandleV1, D> for EmptyData
where
    D: ForeignToplevelHandler,
{
    fn request(
        &self,
        _state: &mut D,
        _client: &Client,
        _resource: &ExtForeignToplevelHandleV1,
        request: <ExtForeignToplevelHandleV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_foreign_toplevel_handle_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &ExtForeignToplevelHandleV1) {
        let state = state.foreign_toplevel_manager_state();
        for data in state.toplevels.values_mut() {
            data.ext_list_instances.remove(resource);
        }
    }
}

impl<D> GlobalDispatch2<ZwlrForeignToplevelManagerV1, D> for ForeignToplevelGlobalData
where
    D: Dispatch<ZwlrForeignToplevelManagerV1, EmptyData>,
    D: Dispatch<ZwlrForeignToplevelHandleV1, EmptyData>,
    D: ForeignToplevelHandler,
{
    fn bind(
        &self,
        state: &mut D,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, EmptyData);

        let protocol_state = state.foreign_toplevel_manager_state();

        for data in protocol_state.toplevels.values_mut() {
            data.add_wlr_instance::<D>(handle, client, &manager);
        }

        protocol_state.wlr_management_instances.insert(manager);
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

impl<D> Dispatch2<ZwlrForeignToplevelManagerV1, D> for EmptyData
where
    D: ForeignToplevelHandler,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: <ZwlrForeignToplevelManagerV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_foreign_toplevel_manager_v1::Request::Stop => {
                resource.finished();
                let state = state.foreign_toplevel_manager_state();
                state.wlr_management_instances.remove(resource);
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &ZwlrForeignToplevelManagerV1) {
        let state = state.foreign_toplevel_manager_state();
        state.wlr_management_instances.remove(resource);
    }
}

impl<D> Dispatch2<ZwlrForeignToplevelHandleV1, D> for EmptyData
where
    D: ForeignToplevelHandler,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &ZwlrForeignToplevelHandleV1,
        request: <ZwlrForeignToplevelHandleV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let protocol_state = state.foreign_toplevel_manager_state();

        let Some((surface, _)) = protocol_state
            .toplevels
            .iter()
            .find(|(_, data)| data.wlr_management_instances.contains_key(resource))
        else {
            return;
        };
        let surface = surface.clone();

        if state.is_read_only_surface(&surface) {
            return;
        }

        match request {
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
                state.set_maximized(surface);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                state.unset_maximized(surface);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized => {
                state.minimize(surface);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized => (),
            zwlr_foreign_toplevel_handle_v1::Request::Activate { .. } => {
                state.activate(surface);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                state.close(surface);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. } => (),
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => (),
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { output } => {
                state.set_fullscreen(surface, output);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                state.unset_fullscreen(surface);
            }
            _ => (),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &ZwlrForeignToplevelHandleV1) {
        let state = state.foreign_toplevel_manager_state();
        for data in state.toplevels.values_mut() {
            data.wlr_management_instances.remove(resource);
        }
    }
}

fn to_state_vec(has_focus: bool, is_fullscreen: bool) -> Vec<u32> {
    let mut rv = Vec::new();
    if is_fullscreen {
        rv.push(zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32);
    } else {
        rv.push(zwlr_foreign_toplevel_handle_v1::State::Maximized as u32);
    }
    if has_focus {
        rv.push(zwlr_foreign_toplevel_handle_v1::State::Activated as u32);
    }
    rv
}
