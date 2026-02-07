//! Shared render element collection
//!
//! This module provides functions for collecting render elements from
//! the compositor state, shared between Winit and DRM backends.

use smithay::{
    backend::renderer::{
        element::{
            memory::MemoryRenderBufferRenderElement, surface::WaylandSurfaceRenderElement, Element,
            Id, Kind, RenderElement,
        },
        gles::{GlesError, GlesFrame, GlesRenderer},
    },
    utils::{Physical, Point, Rectangle, Scale},
};
use tracing::warn;

use crate::{cursor, Ewm};

/// Combined render element type for ewm
/// This allows rendering both wayland surfaces and cursor images
pub enum EwmRenderElement {
    Surface(WaylandSurfaceRenderElement<GlesRenderer>),
    Cursor(MemoryRenderBufferRenderElement<GlesRenderer>),
}

impl Element for EwmRenderElement {
    fn id(&self) -> &Id {
        match self {
            EwmRenderElement::Surface(e) => e.id(),
            EwmRenderElement::Cursor(e) => e.id(),
        }
    }

    fn current_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
        match self {
            EwmRenderElement::Surface(e) => e.current_commit(),
            EwmRenderElement::Cursor(e) => e.current_commit(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            EwmRenderElement::Surface(e) => e.geometry(scale),
            EwmRenderElement::Cursor(e) => e.geometry(scale),
        }
    }

    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        match self {
            EwmRenderElement::Surface(e) => e.src(),
            EwmRenderElement::Cursor(e) => e.src(),
        }
    }

    fn transform(&self) -> smithay::utils::Transform {
        match self {
            EwmRenderElement::Surface(e) => e.transform(),
            EwmRenderElement::Cursor(e) => e.transform(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            EwmRenderElement::Surface(e) => e.kind(),
            EwmRenderElement::Cursor(e) => e.kind(),
        }
    }
}

impl RenderElement<GlesRenderer> for EwmRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        match self {
            EwmRenderElement::Surface(e) => {
                RenderElement::<GlesRenderer>::draw(e, frame, src, dst, damage, opaque_regions)
            }
            EwmRenderElement::Cursor(e) => {
                RenderElement::<GlesRenderer>::draw(e, frame, src, dst, damage, opaque_regions)
            }
        }
    }

    fn underlying_storage(
        &self,
        renderer: &mut GlesRenderer,
    ) -> Option<smithay::backend::renderer::element::UnderlyingStorage<'_>> {
        match self {
            EwmRenderElement::Surface(e) => e.underlying_storage(renderer),
            EwmRenderElement::Cursor(e) => e.underlying_storage(renderer),
        }
    }
}

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

/// Collect render elements including cursor for DRM backend
///
/// This version includes the cursor render element at the pointer position.
pub fn collect_render_elements_with_cursor(
    ewm: &Ewm,
    renderer: &mut GlesRenderer,
    scale: Scale<f64>,
    cursor_buffer: &cursor::CursorBuffer,
) -> Vec<EwmRenderElement> {
    use smithay::backend::renderer::element::AsRenderElements;

    // Cursor goes on top - add it first (elements at start render on top)
    let mut elements: Vec<EwmRenderElement> = Vec::new();

    let (pointer_x, pointer_y) = ewm.pointer_location;
    let cursor_pos: Point<i32, Physical> = Point::from((
        (pointer_x - cursor::CURSOR_HOTSPOT.0 as f64) as i32,
        (pointer_y - cursor::CURSOR_HOTSPOT.1 as f64) as i32,
    ));

    match cursor_buffer.render_element(renderer, cursor_pos) {
        Ok(cursor_element) => {
            elements.push(EwmRenderElement::Cursor(cursor_element));
        }
        Err(e) => {
            warn!("Failed to create cursor render element: {:?}", e);
        }
    }

    // Render ALL views for surfaces that have view data (from Emacs)
    for (&id, views) in &ewm.surface_views {
        if let Some(window) = ewm.id_windows.get(&id) {
            for view in views.iter() {
                let location = smithay::utils::Point::from((view.x, view.y));
                let loc_physical = location.to_physical_precise_round(scale);
                let view_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    window.render_elements(renderer, loc_physical, scale, 1.0);
                elements.extend(view_elements.into_iter().map(EwmRenderElement::Surface));
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
        elements.extend(window_elements.into_iter().map(EwmRenderElement::Surface));
    }

    elements
}
