//! Shared render element collection
//!
//! This module provides functions for collecting render elements from
//! the compositor state, shared between Winit and DRM backends.

use smithay::{
    backend::renderer::element::surface::WaylandSurfaceRenderElement,
    backend::renderer::gles::GlesRenderer,
    utils::Scale,
};

use crate::Ewm;

/// Collect all render elements from the compositor state
///
/// This gathers render elements from:
/// 1. All views for surfaces with view data (from Emacs) - using window from id_windows
/// 2. Surfaces without view data (like Emacs itself) - from the space
///
/// The `scale` parameter should be the output's scale factor.
pub fn collect_render_elements(
    ewm: &Ewm,
    renderer: &mut GlesRenderer,
    scale: Scale<f64>,
) -> Vec<WaylandSurfaceRenderElement<GlesRenderer>> {
    use smithay::backend::renderer::element::AsRenderElements;

    let mut elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();

    // Render ALL views for surfaces that have view data (from Emacs)
    // We use the window from id_windows which renders correctly,
    // rather than from space.elements() which has rendering issues.
    for (&id, views) in &ewm.surface_views {
        if let Some(window) = ewm.id_windows.get(&id) {
            for view in views.iter() {
                let location = smithay::utils::Point::from((view.x, view.y));
                let loc_physical = location.to_physical_precise_round(scale);
                let view_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    window.render_elements(renderer, loc_physical, scale, 1.0);
                elements.extend(view_elements);
            }
        }
    }

    // Render surfaces from the space that DON'T have view data (like Emacs itself)
    for window in ewm.space.elements() {
        let window_id = ewm.window_ids.get(window).copied().unwrap_or(0);

        // Skip surfaces that have views - they're already rendered above
        if ewm.surface_views.contains_key(&window_id) {
            continue;
        }

        let loc = ewm.space.element_location(window).unwrap_or_default();
        let loc_physical = loc.to_physical_precise_round(scale);

        let window_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            window.render_elements(renderer, loc_physical, scale, 1.0);
        elements.extend(window_elements);
    }

    elements
}
