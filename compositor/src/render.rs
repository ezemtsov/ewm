//! Shared render element collection
//!
//! This module provides functions for collecting render elements from
//! the compositor state, shared between Winit and DRM backends.

use std::ptr;

use anyhow::{ensure, Context};
use smithay::{
    backend::{
        allocator::{dmabuf::Dmabuf, Buffer, Fourcc},
        renderer::{
            element::{
                memory::MemoryRenderBufferRenderElement, surface::WaylandSurfaceRenderElement, Element,
                Id, Kind, RenderElement,
            },
            gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture},
            sync::SyncPoint,
            Bind, Color32F, ExportMem, Frame, Offscreen, Renderer, Unbind,
        },
    },
    reexports::{
        calloop::LoopHandle,
        wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm::Format},
    },
    utils::{Physical, Point, Rectangle, Scale, Size, Transform},
    wayland::shm,
};
use tracing::warn;

use crate::protocols::screencopy::ScreencopyBuffer;
use crate::{cursor, Ewm, LoopData};

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

/// Collect render elements for a specific output.
///
/// This function collects only elements visible on the target output, filtering
/// during collection rather than after. This is important for:
/// 1. Efficient rendering - don't process elements that won't be visible
/// 2. Accurate damage tracking - elements from other outputs don't trigger false damage
///
/// Parameters:
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
) -> Vec<EwmRenderElement> {
    use smithay::backend::renderer::element::AsRenderElements;
    use smithay::utils::Logical;

    let mut elements: Vec<EwmRenderElement> = Vec::new();

    // Output bounds in global logical coordinates
    let output_rect: Rectangle<i32, Logical> = Rectangle::new(output_pos, output_size);

    // Cursor goes on top - add it first (elements at start render on top)
    // Only include if cursor is within this output's bounds
    if include_cursor {
        let (pointer_x, pointer_y) = ewm.pointer_location;
        let pointer_pos = Point::from((pointer_x as i32, pointer_y as i32));

        if output_rect.contains(pointer_pos) {
            // Offset cursor position by output location
            let cursor_pos: Point<i32, Physical> = Point::from((
                (pointer_x - cursor::CURSOR_HOTSPOT.0 as f64 - output_pos.x as f64) as i32,
                (pointer_y - cursor::CURSOR_HOTSPOT.1 as f64 - output_pos.y as f64) as i32,
            ));

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

    // Render views for surfaces that have view data (from Emacs)
    // Only include views that intersect with this output
    for (&id, views) in &ewm.surface_views {
        if let Some(window) = ewm.id_windows.get(&id) {
            // Get window size for intersection test
            let window_geo = window.geometry();

            for view in views.iter() {
                // View bounds in global coordinates
                let view_rect: Rectangle<i32, Logical> = Rectangle::new(
                    Point::from((view.x, view.y)),
                    Size::from((window_geo.size.w, window_geo.size.h)),
                );

                // Skip views that don't intersect with this output
                if !output_rect.overlaps(view_rect) {
                    continue;
                }

                // Offset by output position for rendering
                let location = Point::from((view.x - output_pos.x, view.y - output_pos.y));
                let loc_physical = location.to_physical_precise_round(scale);
                let view_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    window.render_elements(renderer, loc_physical, scale, 1.0);
                elements.extend(view_elements.into_iter().map(EwmRenderElement::Surface));
            }
        }
    }

    // Render surfaces from the space that DON'T have view data (like Emacs frames)
    // Only include windows that intersect with this output
    for window in ewm.space.elements() {
        let window_id = ewm.window_ids.get(window).copied().unwrap_or(0);

        // Skip surfaces that have views - they're already rendered above
        if ewm.surface_views.contains_key(&window_id) {
            continue;
        }

        let loc = ewm.space.element_location(window).unwrap_or_default();
        let window_geo = window.geometry();

        // Window bounds in global coordinates
        let window_rect: Rectangle<i32, Logical> = Rectangle::new(
            loc,
            Size::from((window_geo.size.w, window_geo.size.h)),
        );

        // Skip windows that don't intersect with this output
        if !output_rect.overlaps(window_rect) {
            continue;
        }

        // Offset by output position for rendering
        let loc_offset = Point::from((loc.x - output_pos.x, loc.y - output_pos.y));
        let loc_physical = loc_offset.to_physical_precise_round(scale);

        let window_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            window.render_elements(renderer, loc_physical, scale, 1.0);
        elements.extend(window_elements.into_iter().map(EwmRenderElement::Surface));
    }

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
    event_loop: &LoopHandle<'static, LoopData>,
) {
    use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
    use tracing::trace;

    let output_scale = Scale::from(output.current_scale().fractional_scale());
    let output_transform = output.current_transform();

    // Get output geometry
    let output_geo = ewm
        .space
        .output_geometry(output)
        .unwrap_or_default();
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
            ScreencopyBuffer::Dmabuf(dmabuf) => {
                render_to_dmabuf(
                    renderer,
                    dmabuf.clone(),
                    size,
                    output_scale,
                    output_transform,
                    relocated_elements.iter().rev(),
                )
                .map(Some)
            }
            ScreencopyBuffer::Shm(buffer) => {
                render_to_shm(
                    renderer,
                    buffer,
                    size,
                    output_scale,
                    output_transform,
                    relocated_elements.iter().rev(),
                )
                .map(|_| None)
            }
        };

        match render_result {
            Ok(sync) => {
                // Send damage info if requested (with_damage=true)
                if with_damage {
                    // For now, report full damage since we always render
                    // A more sophisticated implementation would track actual damage
                    // Damage is in buffer coordinates (same as Physical for scale=1)
                    let full_damage: Rectangle<i32, smithay::utils::Buffer> = Rectangle::from_size(
                        Size::from((size.w, size.h))
                    );
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
