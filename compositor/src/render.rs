//! Shared render element collection
//!
//! This module provides functions for collecting render elements from
//! the compositor state, shared between Winit and DRM backends.
//!
//! # Design Invariants
//!
//! 1. **Per-output rendering**: Elements are collected per-output, not globally.
//!    Each output only receives elements that intersect with its geometry. This is
//!    critical for efficient rendering, accurate damage tracking, and screen sharing.
//!
//! 2. **Rendering order**: Elements are collected front-to-back:
//!    Overlay → Cursor → Top → Popups → Windows → Bottom → Background
//!    This order matches typical desktop compositor layering.
//!
//! 3. **View-based rendering**: Surfaces with Emacs-managed views are rendered at
//!    view positions. Surfaces without views use space positions (for Emacs frames).

use std::ptr;

use crate::tracy_span;

use anyhow::{ensure, Context};
use smithay::{
    backend::{
        allocator::{dmabuf::Dmabuf, Buffer, Fourcc},
        renderer::{
            element::{
                memory::MemoryRenderBufferRenderElement,
                solid::SolidColorRenderElement,
                surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
                Element, Id, Kind, RenderElement,
            },
            gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture},
            sync::SyncPoint,
            Bind, Color32F, ExportMem, Frame, Offscreen, Renderer, Unbind,
        },
    },
    desktop::{layer_map_for_output, LayerMap, PopupManager},
    output::Output,
    reexports::{
        calloop::LoopHandle,
        wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm::Format},
    },
    utils::{Physical, Point, Rectangle, Scale, Size, Transform},
    wayland::shell::wlr_layer::Layer,
    wayland::shm,
};
use tracing::warn;

use crate::protocols::screencopy::ScreencopyBuffer;
use crate::{cursor, Ewm, State};

/// Combined render element type for ewm
/// This allows rendering both wayland surfaces, cursor images, and solid colors
pub enum EwmRenderElement {
    Surface(WaylandSurfaceRenderElement<GlesRenderer>),
    Cursor(MemoryRenderBufferRenderElement<GlesRenderer>),
    SolidColor(SolidColorRenderElement),
}

impl Element for EwmRenderElement {
    fn id(&self) -> &Id {
        match self {
            EwmRenderElement::Surface(e) => e.id(),
            EwmRenderElement::Cursor(e) => e.id(),
            EwmRenderElement::SolidColor(e) => e.id(),
        }
    }

    fn current_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
        match self {
            EwmRenderElement::Surface(e) => e.current_commit(),
            EwmRenderElement::Cursor(e) => e.current_commit(),
            EwmRenderElement::SolidColor(e) => e.current_commit(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            EwmRenderElement::Surface(e) => e.geometry(scale),
            EwmRenderElement::Cursor(e) => e.geometry(scale),
            EwmRenderElement::SolidColor(e) => e.geometry(scale),
        }
    }

    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        match self {
            EwmRenderElement::Surface(e) => e.src(),
            EwmRenderElement::Cursor(e) => e.src(),
            EwmRenderElement::SolidColor(e) => e.src(),
        }
    }

    fn transform(&self) -> smithay::utils::Transform {
        match self {
            EwmRenderElement::Surface(e) => e.transform(),
            EwmRenderElement::Cursor(e) => e.transform(),
            EwmRenderElement::SolidColor(e) => e.transform(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            EwmRenderElement::Surface(e) => e.kind(),
            EwmRenderElement::Cursor(e) => e.kind(),
            EwmRenderElement::SolidColor(e) => e.kind(),
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
            EwmRenderElement::SolidColor(e) => {
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
            EwmRenderElement::SolidColor(e) => e.underlying_storage(renderer),
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
    use smithay::wayland::seat::WaylandFocus;

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

    // Render popups on top of windows
    for window in ewm.space.elements() {
        if let Some(surface) = window.wl_surface() {
            let window_loc = ewm.space.element_location(window).unwrap_or_default();
            let window_geo = window.geometry();

            for (popup, popup_offset) in PopupManager::popups_for_surface(&surface) {
                let popup_loc = window_loc + window_geo.loc + popup_offset - popup.geometry().loc;
                let render_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    render_elements_from_surface_tree(
                        renderer,
                        popup.wl_surface(),
                        popup_loc.to_physical_precise_round(scale),
                        scale,
                        1.0,
                        Kind::Unspecified,
                    );
                elements.extend(render_elements);
            }
        }
    }

    elements
}

/// Render layer surfaces on a specific layer to element list.
/// LayerMap returns layers in reverse stacking order, so we reverse to get correct order.
fn render_layer(
    layer_map: &LayerMap,
    layer: Layer,
    renderer: &mut GlesRenderer,
    scale: Scale<f64>,
    elements: &mut Vec<EwmRenderElement>,
) {
    for surface in layer_map.layers_on(layer).rev() {
        if let Some(geo) = layer_map.layer_geometry(surface) {
            let render_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                render_elements_from_surface_tree(
                    renderer,
                    surface.wl_surface(),
                    geo.loc.to_physical_precise_round(scale),
                    scale,
                    1.0,
                    Kind::Unspecified,
                );
            elements.extend(render_elements.into_iter().map(EwmRenderElement::Surface));
        }
    }
}

/// Collect render elements for a specific output.
///
/// This function collects only elements visible on the target output, filtering
/// during collection rather than after. This is important for:
/// 1. Efficient rendering - don't process elements that won't be visible
/// 2. Accurate damage tracking - elements from other outputs don't trigger false damage
///
/// Rendering order (front to back):
/// 1. Cursor (highest z-order, always visible)
/// 2. Overlay layer
/// 3. Top layer
/// 4. Popups
/// 5. Views and windows
/// 6. Bottom layer
/// 7. Background layer (lowest z-order)
///
/// Parameters:
/// - `output`: The output to render for (provides layer map)
/// - `output_pos`: The output's position in global logical space
/// - `output_size`: The output's size in logical coordinates
/// - `include_cursor`: Whether to include the cursor element
pub fn collect_render_elements_for_output(
    ewm: &Ewm,
    renderer: &mut GlesRenderer,
    scale: Scale<f64>,
    cursor_buffer: &cursor::CursorBuffer,
    output_pos: Point<i32, smithay::utils::Logical>,
    output_size: Size<i32, smithay::utils::Logical>,
    include_cursor: bool,
    output: &Output,
) -> Vec<EwmRenderElement> {
    tracy_span!("collect_render_elements");

    use smithay::backend::renderer::element::AsRenderElements;
    use smithay::utils::Logical;
    use smithay::wayland::seat::WaylandFocus;

    let mut elements: Vec<EwmRenderElement> = Vec::new();

    // If session is locked, render ONLY the lock surface for this output
    if ewm.is_locked() {
        if let Some(state) = ewm.output_state.get(output) {
            if let Some(ref lock_surface) = state.lock_surface {
                // Render lock surface at (0,0) covering full output
                let lock_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    render_elements_from_surface_tree(
                        renderer,
                        lock_surface.wl_surface(),
                        Point::<i32, Physical>::from((0, 0)),
                        scale,
                        1.0,
                        Kind::Unspecified,
                    );
                elements.extend(lock_elements.into_iter().map(EwmRenderElement::Surface));
            }

            // Add solid color background behind lock surface
            // (rendered last = behind everything else)
            let bg_element = SolidColorRenderElement::from_buffer(
                &state.lock_color_buffer,
                (0, 0),
                scale,
                1.0,
                Kind::Unspecified,
            );
            elements.push(EwmRenderElement::SolidColor(bg_element));
        }
        // Return early - don't render anything else when locked
        return elements;
    }

    // Output bounds in global logical coordinates
    let output_rect: Rectangle<i32, Logical> = Rectangle::new(output_pos, output_size);

    // Collect all layer elements in a tight scope to avoid holding the RefCell
    // borrow across the rest of the function. layer_map_for_output() returns
    // RefMut<LayerMap> — calling it again (e.g. via get_working_area) while
    // this borrow is alive would panic.
    let (mut overlay_elems, mut top_elems, mut bottom_elems, mut bg_elems) = {
        let layer_map = layer_map_for_output(output);
        let mut overlay = Vec::new();
        let mut top = Vec::new();
        let mut bottom = Vec::new();
        let mut bg = Vec::new();
        render_layer(&layer_map, Layer::Overlay, renderer, scale, &mut overlay);
        render_layer(&layer_map, Layer::Top, renderer, scale, &mut top);
        render_layer(&layer_map, Layer::Bottom, renderer, scale, &mut bottom);
        render_layer(&layer_map, Layer::Background, renderer, scale, &mut bg);
        (overlay, top, bottom, bg)
        // layer_map (RefMut) dropped here
    };

    // 1. Cursor (highest z-order, always visible above all layers)
    if include_cursor {
        let (pointer_x, pointer_y) = ewm.pointer_location;
        let pointer_pos = Point::from((pointer_x as i32, pointer_y as i32));

        if output_rect.contains(pointer_pos) {
            let cursor_logical = Point::from((
                pointer_x - cursor::CURSOR_HOTSPOT.0 as f64 - output_pos.x as f64,
                pointer_y - cursor::CURSOR_HOTSPOT.1 as f64 - output_pos.y as f64,
            ));
            let cursor_pos: Point<i32, Physical> = cursor_logical.to_physical_precise_round(scale);

            match cursor_buffer.render_element(renderer, cursor_pos) {
                Ok(cursor_element) => {
                    elements.push(EwmRenderElement::Cursor(cursor_element));
                }
                Err(e) => {
                    warn!("Failed to create cursor render element: {:?}", e);
                }
            }
        }
    }

    // 2. Overlay layer
    elements.append(&mut overlay_elems);

    // 3. Top layer
    elements.append(&mut top_elems);

    // Track position for popup insertion (after top layer)
    let popup_insert_pos = elements.len();

    // 4. Render declared surfaces from output_layouts (authoritative, no intersection test)
    let working_area = ewm.get_working_area(output);
    if let Some(entries) = ewm.output_layouts.get(&output.name()) {
        for entry in entries {
            if let Some(window) = ewm.id_windows.get(&entry.id) {
                // Frame-relative → output-local (working_area.loc is relative to output origin)
                let location = Point::from((
                    working_area.loc.x + entry.x,
                    working_area.loc.y + entry.y,
                ));
                let loc_physical = location.to_physical_precise_round(scale);
                let view_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    window.render_elements(renderer, loc_physical, scale, 1.0);
                elements.extend(view_elements.into_iter().map(EwmRenderElement::Surface));
            }
        }
    }

    // 5. Render surfaces from the space that are NOT in output_layouts (like Emacs frames)
    for window in ewm.space.elements() {
        let window_id = ewm.window_ids.get(window).copied().unwrap_or(0);

        // Skip surfaces managed by output_layouts
        if ewm.surface_outputs.contains_key(&window_id) {
            continue;
        }

        let loc = ewm.space.element_location(window).unwrap_or_default();
        let window_geo = window.geometry();

        let window_rect: Rectangle<i32, Logical> =
            Rectangle::new(loc, Size::from((window_geo.size.w, window_geo.size.h)));

        if !output_rect.overlaps(window_rect) {
            continue;
        }

        let loc_offset = Point::from((loc.x - output_pos.x, loc.y - output_pos.y));
        let loc_physical = loc_offset.to_physical_precise_round(scale);

        let window_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            window.render_elements(renderer, loc_physical, scale, 1.0);
        elements.extend(window_elements.into_iter().map(EwmRenderElement::Surface));
    }

    // 6. Bottom layer
    elements.append(&mut bottom_elems);

    // 7. Background layer
    elements.append(&mut bg_elems);

    // Collect popups and insert them after the top layer (before windows)
    let mut popup_elements: Vec<EwmRenderElement> = Vec::new();
    for window in ewm.space.elements() {
        if let Some(surface) = window.wl_surface() {
            let window_loc = ewm.space.element_location(window).unwrap_or_default();
            let window_geo = window.geometry();

            for (popup, popup_offset) in PopupManager::popups_for_surface(&surface) {
                let popup_loc = window_loc + window_geo.loc + popup_offset - popup.geometry().loc;

                let popup_rect: Rectangle<i32, Logical> =
                    Rectangle::new(popup_loc, popup.geometry().size);
                if !output_rect.overlaps(popup_rect) {
                    continue;
                }

                let render_loc =
                    Point::from((popup_loc.x - output_pos.x, popup_loc.y - output_pos.y));
                let render_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    render_elements_from_surface_tree(
                        renderer,
                        popup.wl_surface(),
                        render_loc.to_physical_precise_round(scale),
                        scale,
                        1.0,
                        Kind::Unspecified,
                    );
                popup_elements.extend(render_elements.into_iter().map(EwmRenderElement::Surface));
            }
        }
    }

    // Insert popups after top layer but before windows
    elements.splice(popup_insert_pos..popup_insert_pos, popup_elements);

    elements
}

/// Render elements to a dmabuf buffer for screencopy
pub fn render_to_dmabuf(
    renderer: &mut GlesRenderer,
    dmabuf: Dmabuf,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: impl Iterator<Item = impl RenderElement<GlesRenderer>>,
) -> anyhow::Result<SyncPoint> {
    ensure!(
        dmabuf.width() == size.w as u32 && dmabuf.height() == size.h as u32,
        "invalid buffer size"
    );
    renderer.bind(dmabuf).context("error binding dmabuf")?;
    render_elements_to_buffer(renderer, size, scale, transform, elements)
}

/// Render elements to an SHM buffer for screencopy
pub fn render_to_shm(
    renderer: &mut GlesRenderer,
    buffer: &WlBuffer,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: impl Iterator<Item = impl RenderElement<GlesRenderer>>,
) -> anyhow::Result<()> {
    shm::with_buffer_contents_mut(buffer, |shm_buffer, shm_len, buffer_data| {
        ensure!(
            buffer_data.format == Format::Xrgb8888
                && buffer_data.width == size.w
                && buffer_data.height == size.h
                && buffer_data.stride == size.w * 4
                && shm_len == buffer_data.stride as usize * buffer_data.height as usize,
            "invalid buffer format or size"
        );

        // Render to a texture first
        let buffer_size = size.to_logical(1).to_buffer(1, Transform::Normal);
        let texture: GlesTexture = renderer
            .create_buffer(Fourcc::Xrgb8888, buffer_size)
            .context("error creating texture")?;

        renderer
            .bind(texture.clone())
            .context("error binding texture")?;

        // Render elements (don't unbind yet - we need to copy the framebuffer)
        let _ = render_elements_no_unbind(renderer, size, scale, transform, elements)?;

        // Download the result
        let mapping = renderer
            .copy_framebuffer(Rectangle::from_size(buffer_size), Fourcc::Xrgb8888)
            .context("error copying framebuffer")?;

        let bytes = renderer
            .map_texture(&mapping)
            .context("error mapping texture")?;

        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), shm_buffer.cast(), shm_len);
        }

        // Now unbind
        if let Err(err) = renderer.unbind() {
            warn!("error unbinding after rendering: {:?}", err);
        }

        Ok(())
    })
    .context("expected shm buffer, but didn't get one")?
}

/// Shared rendering logic - renders elements but does NOT unbind
fn render_elements_no_unbind(
    renderer: &mut GlesRenderer,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: impl Iterator<Item = impl RenderElement<GlesRenderer>>,
) -> anyhow::Result<SyncPoint> {
    let transform = transform.invert();
    let output_rect = Rectangle::from_size(transform.transform_size(size));

    let mut frame = renderer
        .render(size, transform)
        .context("error starting frame")?;

    frame
        .clear(Color32F::TRANSPARENT, &[output_rect])
        .context("error clearing")?;

    for element in elements {
        let src = element.src();
        let dst = element.geometry(scale);

        if let Some(mut damage) = output_rect.intersection(dst) {
            damage.loc -= dst.loc;
            element
                .draw(&mut frame, src, dst, &[damage], &[])
                .context("error drawing element")?;
        }
    }

    frame.finish().context("error finishing frame")
}

/// Shared rendering logic for screencopy (renders and unbinds)
fn render_elements_to_buffer(
    renderer: &mut GlesRenderer,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: impl Iterator<Item = impl RenderElement<GlesRenderer>>,
) -> anyhow::Result<SyncPoint> {
    let sync = render_elements_no_unbind(renderer, size, scale, transform, elements)?;

    if let Err(err) = renderer.unbind() {
        warn!("error unbinding after rendering: {:?}", err);
    }

    Ok(sync)
}

/// Process pending screencopy requests for a specific output
///
/// This should be called after rendering the main frame for an output.
/// It renders the screen content to any pending screencopy buffers.
pub fn process_screencopies_for_output(
    ewm: &mut Ewm,
    renderer: &mut GlesRenderer,
    output: &smithay::output::Output,
    cursor_buffer: &cursor::CursorBuffer,
    event_loop: &LoopHandle<'static, State>,
) {
    use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
    use tracing::trace;

    let output_scale = Scale::from(output.current_scale().fractional_scale());
    let output_transform = output.current_transform();

    // Get output geometry
    let output_geo = ewm.space.output_geometry(output).unwrap_or_default();
    let output_pos = output_geo.loc;
    let output_size = output_geo.size;

    // Collect pending screencopies for this output
    let mut pending = Vec::new();
    for queue in ewm.screencopy_state.queues_mut() {
        let (_damage_tracker, maybe_screencopy) = queue.split();
        if let Some(screencopy) = maybe_screencopy {
            if screencopy.output() == output {
                pending.push(queue.pop());
            }
        }
    }

    if pending.is_empty() {
        return;
    }

    // Collect render elements for this specific output
    let elements = collect_render_elements_for_output(
        ewm,
        renderer,
        output_scale,
        cursor_buffer,
        output_pos,
        output_size,
        true, // include_cursor
        output,
    );

    for screencopy in pending {
        let size = screencopy.buffer_size();
        let region_loc = screencopy.region_loc();
        let with_damage = screencopy.with_damage();

        // Offset elements for region capture
        let relocated_elements: Vec<_> = elements
            .iter()
            .map(|element| {
                RelocateRenderElement::from_element(
                    element,
                    region_loc.upscale(-1),
                    Relocate::Relative,
                )
            })
            .collect();

        let render_result = match screencopy.buffer() {
            ScreencopyBuffer::Dmabuf(dmabuf) => render_to_dmabuf(
                renderer,
                dmabuf.clone(),
                size,
                output_scale,
                output_transform,
                relocated_elements.iter().rev(),
            )
            .map(Some),
            ScreencopyBuffer::Shm(buffer) => render_to_shm(
                renderer,
                buffer,
                size,
                output_scale,
                output_transform,
                relocated_elements.iter().rev(),
            )
            .map(|_| None),
        };

        match render_result {
            Ok(sync) => {
                // Send damage info if requested (with_damage=true)
                if with_damage {
                    // For now, report full damage since we always render
                    // A more sophisticated implementation would track actual damage
                    // Damage is in buffer coordinates (same as Physical for scale=1)
                    let full_damage: Rectangle<i32, smithay::utils::Buffer> =
                        Rectangle::from_size(Size::from((size.w, size.h)));
                    screencopy.damage(std::iter::once(full_damage));
                    trace!("screencopy with_damage: sent full damage");
                }
                screencopy.submit_after_sync(false, sync, event_loop);
            }
            Err(err) => {
                warn!("Error rendering for screencopy: {:?}", err);
                // screencopy will be dropped and client notified of failure
            }
        }
    }
}
