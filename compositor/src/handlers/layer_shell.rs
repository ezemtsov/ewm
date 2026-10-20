use smithay::desktop::{PopupKind, layer_map_for_output};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState,
};
use tracing::{debug, info, warn};

use crate::{Ewm, State, surface_root};

// Layer Shell
impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.ewm.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: smithay::wayland::shell::wlr_layer::LayerSurface,
        wl_output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        use smithay::desktop::LayerSurface;

        // Get the output for this layer surface
        let output = if let Some(wl_output) = &wl_output {
            self.ewm.output_from_resource(wl_output)
        } else {
            self.ewm.active_output().cloned()
        };

        let Some(output) = output else {
            warn!("No output for new layer surface, closing");
            surface.send_close();
            return;
        };

        let wl_surface = surface.wl_surface().clone();
        self.ewm.unmapped_layer_surfaces.insert(wl_surface.clone());

        let mut map = layer_map_for_output(&output);
        map.map_layer(&LayerSurface::new(surface, namespace.clone()))
            .unwrap();
        info!(
            "New layer surface: namespace={} on output {}",
            namespace,
            output.name()
        );
    }

    fn layer_destroyed(&mut self, surface: smithay::wayland::shell::wlr_layer::LayerSurface) {
        // Defer: event sends from resource destructors abort on a dying client.
        self.ewm.loop_handle.insert_idle(move |state| {
            let ewm = &mut state.ewm;
            let wl_surface = surface.wl_surface();
            ewm.unmapped_layer_surfaces.remove(wl_surface);

            let Some((output, layer)) = ewm.layer_for_root(wl_surface) else {
                return;
            };
            // Clear on-demand focus if it was this layer surface
            if ewm.layer_shell_on_demand_focus.as_ref() == Some(&layer) {
                ewm.layer_shell_on_demand_focus = None;
            }

            let mut map = layer_map_for_output(&output);
            map.unmap_layer(&layer);
            // Re-arrange after unmapping to recalculate exclusive zones
            map.arrange();
            drop(map);

            // Check for working area expansion (panel removed)
            ewm.check_working_area_change(&output);

            ewm.queue_redraw(&output);
            info!("Layer surface destroyed");
        });
    }

    fn new_popup(
        &mut self,
        _parent: smithay::wayland::shell::wlr_layer::LayerSurface,
        popup: smithay::wayland::shell::xdg::PopupSurface,
    ) {
        let popup = PopupKind::Xdg(popup);
        self.ewm.unconstrain_popup(&popup);
        if let Err(err) = self.ewm.popups.track_popup(popup) {
            warn!("error tracking popup: {err:?}");
        }
    }
}

impl Ewm {
    /// Handle a layer surface commit, returning whether SURFACE was one.
    pub(crate) fn handle_layer_surface_commit(&mut self, surface: &WlSurface) -> bool {
        let root_surface = surface_root(surface);
        let Some((output, layer)) = self.layer_for_root(&root_surface) else {
            return false;
        };

        if surface == &root_surface {
            let initial_configure_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<LayerSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .initial_configure_sent
            });

            let mut map = layer_map_for_output(&output);

            // Arrange the layers before sending the initial configure
            map.arrange();

            if initial_configure_sent {
                if crate::utils::is_mapped(surface) {
                    let was_unmapped = self.unmapped_layer_surfaces.remove(surface);
                    if was_unmapped {
                        debug!("Layer surface mapped");

                        // Auto-focus newly mapped OnDemand surfaces
                        if layer.cached_state().keyboard_interactivity
                            == KeyboardInteractivity::OnDemand
                        {
                            self.layer_shell_on_demand_focus = Some(layer.clone());
                        }
                    }
                } else {
                    self.unmapped_layer_surfaces.insert(surface.clone());
                }
            } else {
                let scale = output.current_scale();
                let transform = output.current_transform();
                with_states(surface, |data| {
                    crate::utils::send_scale_transform(surface, data, scale, transform);
                });
                layer.layer_surface().send_configure();
            }
            drop(map);

            // Check for working area changes (exclusive zones from panels)
            self.check_working_area_change(&output);

            self.queue_redraw(&output);
        } else {
            // This is a layer-shell subsurface
            self.queue_redraw(&output);
        }

        true
    }
}
