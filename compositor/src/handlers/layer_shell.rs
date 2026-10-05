use smithay::desktop::{PopupKind, layer_map_for_output};
use smithay::wayland::shell::wlr_layer::{Layer, WlrLayerShellHandler, WlrLayerShellState};
use tracing::{info, warn};

use crate::State;

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
            let name = self.ewm.active_output();
            name.and_then(|n| self.ewm.find_mapped_output(&n))
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
        self.ewm.unconstrain_popup(&popup);
        if let Err(err) = self.ewm.popups.track_popup(PopupKind::Xdg(popup)) {
            warn!("error tracking popup: {err:?}");
        }
    }
}
