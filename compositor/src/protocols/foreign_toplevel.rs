//! wlr-foreign-toplevel-management-v1 protocol implementation
//!
//! Exposes toplevel windows to external tools (taskbars, window switchers, etc.)
//! Based on niri's implementation.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use smithay::output::Output;
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

const VERSION: u32 = 3;

pub struct ForeignToplevelManagerState {
    display: DisplayHandle,
    instances: Vec<ZwlrForeignToplevelManagerV1>,
    toplevels: HashMap<WlSurface, ToplevelData>,
}

pub trait ForeignToplevelHandler {
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState;
    fn activate(&mut self, wl_surface: WlSurface);
    fn close(&mut self, wl_surface: WlSurface);
    fn set_fullscreen(&mut self, wl_surface: WlSurface, wl_output: Option<WlOutput>);
    fn unset_fullscreen(&mut self, wl_surface: WlSurface);
}

struct ToplevelData {
    title: Option<String>,
    app_id: Option<String>,
    states: Vec<u32>,
    output: Option<Output>,
    instances: HashMap<ZwlrForeignToplevelHandleV1, Vec<WlOutput>>,
}

pub struct ForeignToplevelGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

/// Window info for foreign toplevel tracking
pub struct WindowInfo {
    pub surface: WlSurface,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub output: Option<Output>,
    pub is_focused: bool,
}

impl ForeignToplevelManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelGlobalData>,
        D: Dispatch<ZwlrForeignToplevelManagerV1, ()>,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = ForeignToplevelGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ZwlrForeignToplevelManagerV1, _>(VERSION, global_data);
        Self {
            display: display.clone(),
            instances: Vec::new(),
            toplevels: HashMap::new(),
        }
    }

    /// Refresh toplevel state. Call this each frame after focus updates.
    pub fn refresh<D>(&mut self, windows: Vec<WindowInfo>)
    where
        D: Dispatch<ZwlrForeignToplevelHandleV1, ()> + 'static,
    {
        let current_surfaces: std::collections::HashSet<_> =
            windows.iter().map(|w| w.surface.clone()).collect();

        // Handle closed windows
        self.toplevels.retain(|surface, data| {
            if current_surfaces.contains(surface) {
                return true;
            }
            // Send closed event to all clients
            for instance in data.instances.keys() {
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
        D: Dispatch<ZwlrForeignToplevelHandleV1, ()> + 'static,
    {
        let states = to_state_vec(window.is_focused);

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

                let something_changed =
                    new_title.is_some() || new_app_id.is_some() || states_changed || output_changed;

                if something_changed {
                    for (instance, outputs) in &mut data.instances {
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
                            if let Some(output) = &data.output {
                                if let Some(client) = instance.client() {
                                    for wl_output in output.client_outputs(&client) {
                                        instance.output_enter(&wl_output);
                                        outputs.push(wl_output);
                                    }
                                }
                            }
                        }
                        instance.done();
                    }
                }

                // Clean up dead wl_outputs
                for outputs in data.instances.values_mut() {
                    outputs.retain(|x| x.is_alive());
                }
            }
            Entry::Vacant(entry) => {
                // New window
                let mut data = ToplevelData {
                    title: window.title.clone(),
                    app_id: window.app_id.clone(),
                    states,
                    output: window.output.clone(),
                    instances: HashMap::new(),
                };

                for manager in &self.instances {
                    if let Some(client) = manager.client() {
                        data.add_instance::<D>(&self.display, &client, manager);
                    }
                }

                entry.insert(data);
            }
        }
    }
}

impl ToplevelData {
    fn add_instance<D>(
        &mut self,
        handle: &DisplayHandle,
        client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
    ) where
        D: Dispatch<ZwlrForeignToplevelHandleV1, ()> + 'static,
    {
        let Ok(toplevel) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, D>(
            handle,
            manager.version(),
            (),
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

        self.instances.insert(toplevel, outputs);
    }
}

impl<D> GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelGlobalData, D>
    for ForeignToplevelManagerState
where
    D: GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelGlobalData>,
    D: Dispatch<ZwlrForeignToplevelManagerV1, ()>,
    D: Dispatch<ZwlrForeignToplevelHandleV1, ()>,
    D: ForeignToplevelHandler,
{
    fn bind(
        state: &mut D,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &ForeignToplevelGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());

        let protocol_state = state.foreign_toplevel_manager_state();

        for data in protocol_state.toplevels.values_mut() {
            data.add_instance::<D>(handle, client, &manager);
        }

        protocol_state.instances.push(manager);
    }

    fn can_view(client: Client, global_data: &ForeignToplevelGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D> Dispatch<ZwlrForeignToplevelManagerV1, (), D> for ForeignToplevelManagerState
where
    D: Dispatch<ZwlrForeignToplevelManagerV1, ()>,
    D: ForeignToplevelHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: <ZwlrForeignToplevelManagerV1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_foreign_toplevel_manager_v1::Request::Stop => {
                resource.finished();
                let state = state.foreign_toplevel_manager_state();
                state.instances.retain(|x| x != resource);
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ZwlrForeignToplevelManagerV1,
        _data: &(),
    ) {
        let state = state.foreign_toplevel_manager_state();
        state.instances.retain(|x| x != resource);
    }
}

impl<D> Dispatch<ZwlrForeignToplevelHandleV1, (), D> for ForeignToplevelManagerState
where
    D: Dispatch<ZwlrForeignToplevelHandleV1, ()>,
    D: ForeignToplevelHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrForeignToplevelHandleV1,
        request: <ZwlrForeignToplevelHandleV1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let protocol_state = state.foreign_toplevel_manager_state();

        let Some((surface, _)) = protocol_state
            .toplevels
            .iter()
            .find(|(_, data)| data.instances.contains_key(resource))
        else {
            return;
        };
        let surface = surface.clone();

        match request {
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => (),
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => (),
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized => (),
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

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ZwlrForeignToplevelHandleV1,
        _data: &(),
    ) {
        let state = state.foreign_toplevel_manager_state();
        for data in state.toplevels.values_mut() {
            data.instances.retain(|instance, _| instance != resource);
        }
    }
}

fn to_state_vec(has_focus: bool) -> Vec<u32> {
    let mut rv = Vec::new();
    // EWM windows are always maximized (fill their Emacs window)
    rv.push(zwlr_foreign_toplevel_handle_v1::State::Maximized as u32);
    // Activated = keyboard focus
    if has_focus {
        rv.push(zwlr_foreign_toplevel_handle_v1::State::Activated as u32);
    }
    rv
}

#[macro_export]
macro_rules! delegate_foreign_toplevel {
    ($ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1: $crate::protocols::foreign_toplevel::ForeignToplevelGlobalData
        ] => $crate::protocols::foreign_toplevel::ForeignToplevelManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1: ()
        ] => $crate::protocols::foreign_toplevel::ForeignToplevelManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1: ()
        ] => $crate::protocols::foreign_toplevel::ForeignToplevelManagerState);
    };
}
