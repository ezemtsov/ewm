//! Shared render element collection
//!
//! This module provides functions for collecting render elements from
//! the compositor state, shared between DRM and headless backends.
//!
//! # Design Invariants
//!
//! 1. **Per-output rendering**: Elements are collected per-output, not globally. Each output only
//!    receives elements that intersect with its geometry. This is critical for efficient rendering,
//!    accurate damage tracking, and screen sharing.
//!
//! 2. **Rendering order**: Elements are collected front-to-back, matching typical desktop
//!    compositor layering: Cursor -> Overlay -> Top -> Popups -> Windows -> Bottom -> Background.
//!
//! 3. **Layout-based rendering**: Views draw at the window rects their layout entries declare,
//!    Emacs frames at the origin their strip or floating space gives them.

use std::collections::HashMap;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::element::utils::{
    CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::{Element, Kind, RenderElement, render_elements};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::desktop::{LayerMap, PopupManager, layer_map_for_output};
use smithay::output::{Output, OutputModeSource};
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::wlr_layer::Layer;
use tracing::warn;

use crate::cursor::{CursorTextureCache, RenderCursor, XCursor};
use crate::layout::closing_frame::Closing;
use crate::layout::overview::Geometry;
use crate::layout::resize_fade::{Fit, ResizeFade, contents_rect};
use crate::layout::snapshot::Snapshot;
use crate::layout::strip;
use crate::layout::{FrameLocation, LayoutEntryLocation};
use crate::protocols::screencopy::{Screencopy, ScreencopyBuffer};
use crate::render_helpers::background_effect;
use crate::render_helpers::framebuffer_effect::FramebufferEffectElement;
use crate::render_helpers::shadow_style::{BackgroundEffect, Blur, CornerRadius};
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::zoom::ZoomRenderElement;
use crate::render_helpers::{render_to_dmabuf, render_to_shm};
use crate::surface::SurfaceState;
use crate::{Ewm, LayoutEntry, LayoutEntryId, State, tracy_span};

/// Render a window's surface tree without its popups. Popups are emitted once
/// per surface via `window_popup_placements`. Smithay's `Window::render_elements`
/// would otherwise inline them at every view's location, duplicating across outputs.
fn render_window_without_popups(
    window: &smithay::desktop::Window,
    renderer: &mut GlesRenderer,
    location: Point<i32, Physical>,
    scale: Scale<f64>,
    alpha: f32,
) -> Vec<WaylandSurfaceRenderElement<GlesRenderer>> {
    let Some(surface) = window.wl_surface() else {
        return Vec::new();
    };
    render_elements_from_surface_tree(
        renderer,
        &surface,
        location,
        scale,
        alpha,
        Kind::Unspecified,
    )
}

render_elements! {
    pub ViewContentElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Texture=TextureRenderElement<GlesTexture>,
}

type ConstrainedRenderElement = CropRenderElement<RescaleRenderElement<ViewContentElement>>;

// Output content the overview zooms: frames, their views, floating frames,
// window popups, the bottom/background layers and the output fill.
render_elements! {
    pub ContentRenderElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Constrained=ConstrainedRenderElement,
    SolidColor=SolidColorRenderElement,
    Shadow=crate::render_helpers::shadow::ShadowRenderElement,
    BackgroundEffect=FramebufferEffectElement,
    Texture=TextureRenderElement<GlesTexture>,
}

/// Content scaled about the output origin, then moved into place.
pub type ZoomedRenderElement = ZoomRenderElement<ContentRenderElement>;

// Combined render element type for ewm.
// Generated via Smithay's `render_elements!` macro, which auto-derives
// `Element`, `RenderElement<GlesRenderer>`, and `From` impls for each variant.
render_elements! {
    pub EwmRenderElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Constrained=ConstrainedRenderElement,
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
    SolidColor=SolidColorRenderElement,
    Texture=TextureRenderElement<GlesTexture>,
    Zoomed=ZoomedRenderElement,
    BackgroundEffect=FramebufferEffectElement,
}

/// A solid colour buffer drawn at `loc`.
fn solid(
    buffer: &SolidColorBuffer,
    loc: impl Into<Point<i32, Physical>>,
    scale: Scale<f64>,
) -> SolidColorRenderElement {
    SolidColorRenderElement::from_buffer(buffer, loc, scale, 1., Kind::Unspecified)
}

/// Render layer surfaces on a specific layer to element list.
/// LayerMap returns layers in reverse stacking order, so we reverse to get correct order.
fn render_layer<
    E: From<WaylandSurfaceRenderElement<GlesRenderer>> + From<FramebufferEffectElement>,
>(
    layer_map: &LayerMap,
    layer: Layer,
    renderer: &mut GlesRenderer,
    scale: Scale<f64>,
    blur_config: Blur,
    elements: &mut Vec<E>,
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
            elements.extend(render_elements.into_iter().map(E::from));

            push_background_effect(
                elements,
                None,
                surface.wl_surface(),
                geo.to_f64(),
                false,
                Point::from((0., 0.)),
                Scale::from(1.),
                scale.x,
                blur_config,
            );
        }
    }
}

/// Blur behind `surface` if it asked for one, in output-local logical coordinates.
#[allow(clippy::too_many_arguments)]
fn push_background_effect<E: From<FramebufferEffectElement>>(
    elements: &mut Vec<E>,
    ns: Option<usize>,
    surface: &WlSurface,
    geometry: Rectangle<f64, Logical>,
    clip_to_geometry: bool,
    surface_off: Point<f64, Logical>,
    surface_scale: Scale<f64>,
    scale: f64,
    blur_config: Blur,
) {
    if blur_config.off {
        return;
    }
    background_effect::render_for_tile(
        ns,
        geometry,
        scale,
        clip_to_geometry,
        surface,
        surface_off,
        surface_scale,
        blur_config,
        CornerRadius::default(),
        BackgroundEffect::default(),
        &mut |elem| elements.push(elem.into()),
    );
}

/// Where a view's contents sit on the output, at `element_scale`, cropped to `clip`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ViewBox {
    pub loc: Point<i32, Physical>,
    pub element_scale: Scale<f64>,
    pub output_scale: Scale<f64>,
    pub clip: Rectangle<i32, Physical>,
}

/// Blur behind one view of `window`, clipped like the view itself.
fn push_view_background_effect<E: From<FramebufferEffectElement>>(
    elements: &mut Vec<E>,
    ns: Option<usize>,
    window: &smithay::desktop::Window,
    view: ViewBox,
    blur_config: Blur,
) {
    let Some(surface) = window.wl_surface() else {
        return;
    };
    let geometry = view.clip.to_f64().to_logical(view.output_scale);
    let surface_off = view.loc.to_f64().to_logical(view.output_scale) - geometry.loc;
    push_background_effect(
        elements,
        ns,
        &surface,
        geometry,
        true,
        surface_off,
        view.element_scale,
        view.output_scale.x,
        blur_config,
    );
}

pub fn render_cursor_elements(
    ewm: &Ewm,
    renderer: &mut GlesRenderer,
    cursor_texture_cache: &CursorTextureCache,
    output: &Output,
    scale: Scale<f64>,
    pointer_pos: Point<f64, smithay::utils::Logical>,
) -> Vec<EwmRenderElement> {
    let cursor_scale = output.current_scale().integer_scale();
    match ewm.cursor_manager.get_render_cursor(cursor_scale) {
        RenderCursor::Hidden => Vec::new(),
        RenderCursor::Surface { hotspot, surface } => {
            let cursor_pos: Point<i32, Physical> =
                (pointer_pos - hotspot.to_f64()).to_physical_precise_round(scale);
            render_elements_from_surface_tree(
                renderer,
                &surface,
                cursor_pos,
                scale,
                1.0,
                Kind::Cursor,
            )
            .into_iter()
            .map(EwmRenderElement::Surface)
            .collect()
        }
        RenderCursor::Named {
            icon,
            scale: cursor_scale,
            cursor,
        } => {
            let (idx, frame) = cursor.frame(crate::utils::get_monotonic_time().as_millis() as u32);
            let hotspot = XCursor::hotspot(frame).to_logical(cursor_scale);
            let cursor_pos: Point<i32, Physical> =
                (pointer_pos - hotspot.to_f64()).to_physical_precise_round(scale);
            let texture = cursor_texture_cache.get(icon, cursor_scale, &cursor, idx);

            match MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                cursor_pos.to_f64(),
                &texture,
                None,
                None,
                None,
                Kind::Cursor,
            ) {
                Ok(cursor_element) => vec![EwmRenderElement::Cursor(cursor_element)],
                Err(err) => {
                    warn!("error importing a cursor texture: {err:?}");
                    Vec::new()
                }
            }
        }
    }
}

/// Where a frame's views are drawn: its origin, their bounds and the output-sized slot.
#[derive(Clone, Copy)]
struct FrameSpace {
    origin: Point<f64, Logical>,
    bounds: Rectangle<i32, Physical>,
    slot: Rectangle<i32, Physical>,
}

/// A view's place on the output: its contents at `loc` for a `window` that size, cropped to `clip`.
struct ViewPlace<'a> {
    surface: &'a SurfaceState,
    entry: &'a LayoutEntry,
    alpha: f32,
    loc: Point<f64, Logical>,
    window: Size<i32, Logical>,
    clip: Rectangle<i32, Physical>,
    /// The output-sized slot its backdrop fills.
    slot: Rectangle<i32, Physical>,
}

impl<'a> ViewPlace<'a> {
    /// `entry` fullscreen in the frame's slot, else at its rect, if any of it shows.
    fn new(
        surface: &'a SurfaceState,
        entry: &'a LayoutEntry,
        alpha: f32,
        frame: FrameSpace,
        output_size: Size<i32, Logical>,
        scale: Scale<f64>,
    ) -> Option<Self> {
        if entry.fullscreen() {
            Some(Self::fullscreen(
                surface,
                entry,
                alpha,
                frame.slot,
                output_size,
                scale,
            ))
        } else {
            Self::in_frame(surface, entry, alpha, frame, scale)
        }
    }

    /// Fullscreen in `slot`: centered by its own size when primary, else from the slot's corner.
    fn fullscreen(
        surface: &'a SurfaceState,
        entry: &'a LayoutEntry,
        alpha: f32,
        slot: Rectangle<i32, Physical>,
        output_size: Size<i32, Logical>,
        scale: Scale<f64>,
    ) -> Self {
        let fit = Fit {
            window: output_size,
            native: entry.primary(),
        };
        let geometry = surface.window.geometry().size;
        let offset = surface
            .fit_of(entry.id, fit)
            .fullscreen_offset(geometry, output_size);
        let loc = slot.loc.to_f64().to_logical(scale) + offset.to_f64();
        Self {
            surface,
            entry,
            alpha,
            loc,
            window: output_size,
            clip: slot,
            slot,
        }
    }

    /// At its rect in the frame, cropped to the frame's buffer.
    fn in_frame(
        surface: &'a SurfaceState,
        entry: &'a LayoutEntry,
        alpha: f32,
        frame: FrameSpace,
        scale: Scale<f64>,
    ) -> Option<Self> {
        let loc = frame.origin + Point::from((f64::from(entry.x), f64::from(entry.y)));
        let window = entry.rect().size;
        let rect = Rectangle::new(
            loc.to_physical_precise_round(scale),
            window.to_physical_precise_round(scale),
        );
        Some(Self {
            surface,
            entry,
            alpha,
            loc,
            window,
            clip: rect.intersection(frame.bounds)?,
            slot: frame.slot,
        })
    }
}

/// A view's contents at their scale, the old contents of a fade under them where they were, then
/// its backdrop.
fn push_view(
    content: &mut Vec<ContentRenderElement>,
    renderer: &mut GlesRenderer,
    place: &ViewPlace<'_>,
    backdrop: Option<&SolidColorBuffer>,
    scale: Scale<f64>,
    blur_config: Blur,
) {
    let ViewPlace {
        surface,
        entry,
        alpha,
        loc,
        window,
        clip,
        slot,
    } = *place;
    let geometry = surface.window.geometry().size;
    let fit = Fit {
        window,
        native: entry.primary(),
    };
    let fit = surface.fit_of(entry.id, fit);
    if let Some((at, content_scale)) = contents_rect(fit, geometry, loc) {
        let at = at.to_physical(scale).to_i32_round();
        if let Some(crop) = at.intersection(clip) {
            let content_scale = Scale::from(content_scale);
            let fade = surface.resize_fade_for(entry);
            let progress = fade.map_or(1., ResizeFade::progress);
            let view_elements = render_window_without_popups(
                &surface.window,
                renderer,
                at.loc,
                scale,
                alpha * progress,
            );
            // The old contents fade where they were, over the whole slot through a fullscreen flip.
            let under = fade.map_or_else(Vec::new, |fade| {
                let old = ViewBox {
                    loc: fade
                        .at(entry)
                        .map_or(at.loc, |was| was.loc.to_physical_precise_round(scale)),
                    element_scale: Scale::from(fade.scale(entry)),
                    output_scale: scale,
                    clip: crop,
                };
                let reach = if fade.flips() { slot } else { clip };
                fade_elements(fade, progress, old, reach, alpha)
            });
            let view = ViewBox {
                loc: at.loc,
                element_scale: content_scale,
                output_scale: scale,
                clip: crop,
            };
            push_view_elements(
                content,
                &surface.window,
                view_elements,
                view,
                Some(entry.id.get() as usize),
                under,
                blur_config,
            );
        }
    }
    if let Some(backdrop) = backdrop {
        let alpha = surface.backdrop_alpha(entry);
        if alpha > 0. {
            content.push(ContentRenderElement::SolidColor(
                SolidColorRenderElement::from_buffer(
                    backdrop,
                    slot.loc,
                    scale,
                    alpha,
                    Kind::Unspecified,
                ),
            ));
        }
    }
}

/// Push constrained (rescale + crop) render elements into the element list.
///
/// Shared pipeline for all view rendering: fullscreen primary/secondary
/// and normal primary/secondary.
pub(crate) fn push_constrained<I, E>(
    elements: &mut Vec<E>,
    view_elements: impl IntoIterator<Item = I>,
    view: ViewBox,
) where
    I: Into<ViewContentElement>,
    E: From<ConstrainedRenderElement>,
{
    elements.extend(
        view_elements
            .into_iter()
            .map(|e| RescaleRenderElement::from_element(e.into(), view.loc, view.element_scale))
            .filter_map(|e| CropRenderElement::from_element(e, view.output_scale, view.clip))
            .map(E::from),
    );
}

/// `fade`'s old contents in `old`, over `reach`: under the live view in `old.clip` they sum with
/// it to `alpha`, beyond it they fade out on their own.
fn fade_elements(
    fade: &ResizeFade,
    progress: f32,
    old: ViewBox,
    reach: Rectangle<i32, Physical>,
    alpha: f32,
) -> Vec<ConstrainedRenderElement> {
    // Under a live view at alpha * progress, the old contents sum to alpha * (1 - progress).
    let shown = alpha * progress;
    let under = if shown >= 1. {
        0.
    } else {
        alpha * (1. - progress) / (1. - shown)
    };
    let mut elements = Vec::new();
    let mut push = |constrain: Rectangle<i32, Physical>, alpha: f32| {
        let elem = fade.render(old.loc.to_f64().to_logical(old.output_scale), alpha);
        let elem = RescaleRenderElement::from_element(elem.into(), old.loc, old.element_scale);
        elements.extend(CropRenderElement::from_element(
            elem,
            old.output_scale,
            constrain,
        ));
    };
    if let Some(covered) = reach.intersection(old.clip) {
        push(covered, under);
    }
    for fringe in reach.subtract_rect(old.clip) {
        push(fringe, alpha * (1. - progress));
    }
    elements
}

/// The frame's buffer rect in its `slot`, or the slot before it has one. It bounds the views.
fn frame_bounds(
    surface: Option<&SurfaceState>,
    slot: Rectangle<i32, Physical>,
    scale: Scale<f64>,
) -> Rectangle<i32, Physical> {
    let size = surface
        .filter(|surface| surface.has_buffer())
        .map_or(slot.size, |surface| {
            surface
                .window
                .geometry()
                .size
                .to_physical_precise_round(scale)
        });
    Rectangle::new(slot.loc, size)
}

/// A view's elements, scaled and cropped, over the old contents `under`, over the blur.
pub(crate) fn push_view_elements<
    E: From<ConstrainedRenderElement> + From<FramebufferEffectElement>,
>(
    elements: &mut Vec<E>,
    window: &smithay::desktop::Window,
    view_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    view: ViewBox,
    ns: Option<usize>,
    under: Vec<ConstrainedRenderElement>,
    blur_config: Blur,
) {
    push_constrained(elements, view_elements, view);
    elements.extend(under.into_iter().map(E::from));
    push_view_background_effect(elements, ns, window, view, blur_config);
}

/// The layer-shell layers of one pass, the top two above the zoomed content, the rest within it.
#[derive(Default)]
struct Layers {
    overlay: Vec<EwmRenderElement>,
    top: Vec<EwmRenderElement>,
    bottom: Vec<ContentRenderElement>,
    background: Vec<ContentRenderElement>,
}

/// The working area, its origin and the output size in physical pixels, and the strip viewport.
struct FrameGeo {
    working_area: Rectangle<i32, Logical>,
    working_area_origin: Point<i32, Physical>,
    view_size: Size<f64, Logical>,
    strip_viewport: Rectangle<f64, Logical>,
    output_phys: Size<i32, Physical>,
}

/// What every view of a pass is drawn with, its frame's place aside.
struct ViewCommon<'a> {
    surfaces: &'a HashMap<u64, SurfaceState>,
    backdrop: Option<&'a SolidColorBuffer>,
    focused_entry_id: Option<LayoutEntryId>,
    unfocused_alpha: f32,
    output_size: Size<i32, Logical>,
    scale: Scale<f64>,
    blur: Blur,
}

impl ViewCommon<'_> {
    fn backdrop_alpha(&self, entry: &LayoutEntry) -> f32 {
        entry
            .surface_id()
            .and_then(|id| self.surfaces.get(&id))
            .map_or(0., |surface| surface.backdrop_alpha(entry))
    }

    /// Whether `frame`'s fullscreen view has an opaque backdrop, hiding whatever it covers.
    fn fullscreen_covers(&self, frame: &strip::Frame) -> bool {
        frame
            .fullscreen_entry()
            .is_some_and(|entry| self.backdrop_alpha(entry) >= 1.)
    }

    /// `frame`'s views in `space`, only the fullscreen one when `covered`.
    fn push_frame_views(
        &self,
        content: &mut Vec<ContentRenderElement>,
        renderer: &mut GlesRenderer,
        frame: &strip::Frame,
        space: FrameSpace,
        covered: bool,
    ) {
        let mut views: Vec<&LayoutEntry> = frame.entries.iter().collect();
        // Backdrop carriers first, so it covers their siblings, which hide only under an
        // opaque one.
        views.sort_by_cached_key(|entry| self.backdrop_alpha(entry) <= 0.);
        for entry in views {
            if covered && !entry.fullscreen() {
                continue;
            }
            let Some(surface) = entry.surface_id().and_then(|id| self.surfaces.get(&id)) else {
                continue;
            };
            // Fullscreen forced opaque so the backdrop doesn't bleed through.
            let focused = entry.fullscreen() || self.focused_entry_id == Some(entry.id);
            let alpha = if focused { 1.0 } else { self.unfocused_alpha } * surface.open_alpha();
            if let Some(place) =
                ViewPlace::new(surface, entry, alpha, space, self.output_size, self.scale)
            {
                push_view(
                    content,
                    renderer,
                    &place,
                    self.backdrop,
                    self.scale,
                    self.blur,
                );
            }
        }
    }
}

/// One output's render pass: its geometry and zoom, and the elements collected so far.
struct OutputRender<'a> {
    ewm: &'a mut Ewm,
    renderer: &'a mut GlesRenderer,
    output: &'a Output,
    include_cursor: bool,
    scale: Scale<f64>,
    rect: Rectangle<i32, Logical>,
    blur: Blur,
    geometry: Geometry,
    zoom_offset: Point<i32, Physical>,
    slide_offset: Point<i32, Physical>,
    /// Front to back, as rendering wants them.
    elements: Vec<EwmRenderElement>,
    cursor: Vec<EwmRenderElement>,
    /// The zoomed section under construction, in output-local coordinates.
    content: Vec<ContentRenderElement>,
}

impl<'a> OutputRender<'a> {
    fn new(
        ewm: &'a mut Ewm,
        renderer: &'a mut GlesRenderer,
        output: &'a Output,
        include_cursor: bool,
    ) -> Self {
        let scale = Scale::from(output.current_scale().fractional_scale());
        let rect = ewm.layout.output_rect(output).unwrap_or_default();
        let blur = ewm.blur_config;
        // Everything under the top layer scales with the overview, and slides up one output height
        // in content space while the frames hide.
        let geometry = ewm.overview_geometry(output);
        let zoom_offset = geometry.offset.to_physical_precise_round(scale);
        let slide = ewm.hide.current() * f64::from(rect.size.h) * geometry.zoom;
        let slide_offset = zoom_offset
            + Point::<f64, Logical>::from((0., -slide)).to_physical_precise_round(scale);
        Self {
            ewm,
            renderer,
            output,
            include_cursor,
            scale,
            rect,
            blur,
            geometry,
            zoom_offset,
            slide_offset,
            elements: Vec::new(),
            cursor: Vec::new(),
            content: Vec::new(),
        }
    }

    /// The cursor goes on top, kept apart from the content.
    fn cursor(&mut self, cache: &CursorTextureCache) {
        if !self.include_cursor || self.ewm.cursor_hide.is_hidden {
            return;
        }
        let (pointer_x, pointer_y) = self.ewm.cursor_location();
        let pointer_pos = Point::from((pointer_x as i32, pointer_y as i32));
        if self.rect.contains(pointer_pos) {
            let pointer_pos = Point::from((
                pointer_x - self.rect.loc.x as f64,
                pointer_y - self.rect.loc.y as f64,
            ));
            self.cursor.extend(render_cursor_elements(
                self.ewm,
                self.renderer,
                cache,
                self.output,
                self.scale,
                pointer_pos,
            ));
        }
    }

    /// The DnD icon rides under the cursor, above everything else.
    fn dnd_icon(&mut self) {
        let Some(dnd_icon) = self.ewm.dnd_icon.as_ref() else {
            return;
        };
        let (pointer_x, pointer_y) = self.ewm.cursor_location();
        let icon_pos = Point::from((
            pointer_x + dnd_icon.offset.x as f64 - self.rect.loc.x as f64,
            pointer_y + dnd_icon.offset.y as f64 - self.rect.loc.y as f64,
        ));
        let icon_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            render_elements_from_surface_tree(
                self.renderer,
                &dnd_icon.surface,
                icon_pos.to_physical_precise_round(self.scale),
                self.scale,
                1.,
                Kind::ScanoutCandidate,
            );
        self.elements
            .extend(icon_elements.into_iter().map(EwmRenderElement::Surface));
    }

    /// Under a session lock only the lock surface and its backdrop show. True when locked.
    fn lock(&mut self) -> bool {
        if !self.ewm.is_locked() {
            return false;
        }
        let state = self.ewm.output_state.get(self.output).unwrap();
        if let Some(surface) = state.lock_surface.as_ref() {
            let lock_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                render_elements_from_surface_tree(
                    self.renderer,
                    surface.wl_surface(),
                    Point::from((0, 0)),
                    self.scale,
                    1.,
                    Kind::ScanoutCandidate,
                );
            self.elements
                .extend(lock_elements.into_iter().map(EwmRenderElement::Surface));
        }
        self.elements.push(EwmRenderElement::SolidColor(solid(
            &state.lock_color_buffer,
            (0, 0),
            self.scale,
        )));
        true
    }

    /// The four layer-shell layers, each in stacking order.
    fn layers(&mut self) -> Layers {
        let mut layers = Layers::default();
        // The RefMut lives for this scope only. Taking the layer map again meanwhile, as
        // get_working_area does, would panic.
        let layer_map = layer_map_for_output(self.output);
        for (layer, out) in [
            (Layer::Overlay, &mut layers.overlay),
            (Layer::Top, &mut layers.top),
        ] {
            render_layer(&layer_map, layer, self.renderer, self.scale, self.blur, out);
        }
        for (layer, out) in [
            (Layer::Bottom, &mut layers.bottom),
            (Layer::Background, &mut layers.background),
        ] {
            render_layer(&layer_map, layer, self.renderer, self.scale, self.blur, out);
        }
        layers
    }

    /// Popups of `layers` at unit scale, straight into the elements.
    fn layer_popups(&mut self, layers: &[Layer]) {
        for placement in layer_popup_placements(self.output, self.rect, layers) {
            push_popup(
                &mut self.elements,
                self.renderer,
                self.scale,
                self.rect.loc,
                &placement,
                self.blur,
            );
        }
    }

    /// Popups of the zoomed `layers`, zooming with them.
    fn zoomed_layer_popups(&mut self, layers: &[Layer]) {
        for placement in layer_popup_placements(self.output, self.rect, layers) {
            push_popup(
                &mut self.content,
                self.renderer,
                self.scale,
                self.rect.loc,
                &placement,
                self.blur,
            );
        }
        self.flush_zoomed();
    }

    /// Window popups above their frames, culled against the content the zoom shows.
    fn window_popups(&mut self) {
        let visible = self.geometry.visible_content(self.rect);
        for placement in window_popup_placements(self.ewm, visible) {
            push_popup(
                &mut self.content,
                self.renderer,
                self.scale,
                self.rect.loc,
                &placement,
                self.blur,
            );
        }
        self.flush_slid();
    }

    fn frame_geo(&self) -> FrameGeo {
        let working_area = self.ewm.get_working_area(self.output);
        FrameGeo {
            working_area,
            working_area_origin: working_area.loc.to_physical_precise_round(self.scale),
            view_size: Size::<f64, Logical>::from((
                working_area.size.w as f64,
                working_area.size.h as f64,
            )),
            strip_viewport: self.geometry.strip_viewport(working_area),
            output_phys: self.rect.size.to_physical_precise_round(self.scale),
        }
    }

    /// The floats in view, closing ones on top: their views, then each frame's own surface and
    /// shadow unless a fullscreen view covers them.
    fn floats(&mut self, geo: &FrameGeo) {
        let scale = self.scale;
        // Floats in view with their slots, and how far their pin slid them.
        let float_viewport = self
            .geometry
            .to_content_rect(Rectangle::from_size(self.rect.size.to_f64()));
        let floats: Vec<(u64, Rectangle<f64, Logical>, f64)> = self
            .ewm
            .layout
            .monitor_of(self.output)
            .map_or(Vec::new(), |monitor| {
                self.ewm
                    .visible_floats(monitor)
                    .filter(|(_, slot)| float_viewport.overlaps(*slot))
                    .map(|(frame, slot)| (frame.surface_id, slot, monitor.pin_offset(frame)))
                    .collect()
            });
        let focused_frame_id = self.ewm.focused_frame_id();
        let views = ViewCommon {
            surfaces: &self.ewm.surfaces,
            backdrop: self
                .ewm
                .output_state
                .get(self.output)
                .map(|state| &state.fullscreen_backdrop),
            focused_entry_id: self.ewm.focused_entry_id(),
            unfocused_alpha: self.ewm.unfocused_alpha,
            output_size: self.rect.size,
            scale,
            blur: self.blur,
        };
        let Some(floating) = self.ewm.layout.floating_mut(self.output) else {
            return;
        };

        // Draw the closing frames on top of the other frames.
        for (pos, closing) in floating.closing_frames().iter().rev() {
            self.content
                .push(ContentRenderElement::Texture(closing.render(*pos)));
        }

        for (frame, data) in floating.frames_with_data_mut() {
            let Some(&(_, frame_rect, dx)) =
                floats.iter().find(|(id, _, _)| *id == frame.surface_id)
            else {
                continue;
            };
            push_closing_views(
                &mut self.content,
                &frame.closing_views,
                data.render_pos() + Point::from((dx, 0.)),
            );
            let frame_surface = views.surfaces.get(&frame.surface_id);
            let frame_box = frame_rect.to_physical_precise_round(scale);
            let space = FrameSpace {
                origin: frame_rect.loc,
                bounds: frame_bounds(frame_surface, frame_box, scale),
                slot: Rectangle::new(
                    Point::<f64, Logical>::from((dx, 0.)).to_physical_precise_round(scale),
                    geo.output_phys,
                ),
            };
            // A fullscreen view fills the slot. Its siblings, frame and shadow hide under it once
            // its backdrop is opaque.
            let covered = views.fullscreen_covers(frame);
            views.push_frame_views(&mut self.content, self.renderer, frame, space, covered);
            if covered {
                continue;
            }

            let Some(frame_surface) = frame_surface else {
                continue;
            };
            let window = &frame_surface.window;
            // A frame created hidden (sized, then shown) has no buffer yet. Skip it
            // so its shadow never draws at the placeholder size.
            if !frame_surface.has_buffer() {
                continue;
            }
            let frame_alpha = frame_surface.open_alpha();
            let frame_offset = frame_box.loc;
            let view_elements = render_window_without_popups(
                window,
                self.renderer,
                frame_offset,
                scale,
                frame_alpha,
            );
            self.content
                .extend(view_elements.into_iter().map(ContentRenderElement::Surface));
            push_view_background_effect(
                &mut self.content,
                None,
                window,
                ViewBox {
                    loc: frame_offset,
                    element_scale: Scale::from(1.),
                    output_scale: scale,
                    clip: frame_box,
                },
                self.blur,
            );

            let shadow = data.shadow_mut();
            shadow.update_render_elements(
                frame_rect.size,
                focused_frame_id == frame.surface_id,
                CornerRadius::default(),
                scale.x,
                frame_alpha,
            );
            shadow.render(self.renderer, frame_rect.loc, &mut |shadow| {
                self.content.push(ContentRenderElement::Shadow(shadow));
            });
        }
    }

    /// The views of the strip frames in view, closing views first. Frames slide by the strip's
    /// view offset, layer surfaces stay put.
    fn strip_views(&mut self, geo: &FrameGeo, above_top_layer: bool) {
        let scale = self.scale;
        let views = ViewCommon {
            surfaces: &self.ewm.surfaces,
            backdrop: self
                .ewm
                .output_state
                .get(self.output)
                .map(|state| &state.fullscreen_backdrop),
            focused_entry_id: self.ewm.focused_entry_id(),
            unfocused_alpha: self.ewm.unfocused_alpha,
            output_size: self.rect.size,
            scale,
            blur: self.blur,
        };
        let Some(strip) = self.ewm.layout.strip(self.output) else {
            return;
        };
        let active_idx = strip.active_idx();
        for (frame_idx, frame, frame_rect, screen_x) in
            strip.frames_with_render_geo(geo.view_size, geo.strip_viewport)
        {
            let frame_offset: Point<i32, Physical> =
                Point::<f64, Logical>::from((screen_x, 0.0)).to_physical_precise_round(scale);
            let space = FrameSpace {
                origin: Point::from((
                    f64::from(geo.working_area.loc.x) + screen_x,
                    f64::from(geo.working_area.loc.y),
                )),
                bounds: frame_bounds(
                    views.surfaces.get(&frame.surface_id),
                    Rectangle::new(
                        geo.working_area_origin + frame_offset,
                        frame_rect.size.to_physical_precise_round(scale),
                    ),
                    scale,
                ),
                slot: Rectangle::new(frame_offset, geo.output_phys),
            };
            push_closing_views(&mut self.content, &frame.closing_views, space.origin);
            // The top layer waits only for the active frame's fullscreen view.
            let covered =
                above_top_layer && frame_idx == active_idx && views.fullscreen_covers(frame);
            views.push_frame_views(&mut self.content, self.renderer, frame, space, covered);
        }
    }

    /// The strip frames' own surfaces, culled like their views, closing frames beneath them.
    fn strip_frames(&mut self, geo: &FrameGeo) {
        let scale = self.scale;
        let Some(strip) = self.ewm.layout.strip(self.output) else {
            return;
        };
        for (_, frame, frame_rect, screen_x) in
            strip.frames_with_render_geo(geo.view_size, geo.strip_viewport)
        {
            let Some(frame_surface) = self.ewm.surfaces.get(&frame.surface_id) else {
                continue;
            };
            let window = &frame_surface.window;
            let frame_offset_emacs: Point<i32, Physical> =
                Point::<f64, Logical>::from((screen_x, 0.0)).to_physical_precise_round(scale);
            let frame_alpha = frame_surface.open_alpha();
            let frame_loc = geo.working_area_origin + frame_offset_emacs;
            let view_elements =
                render_window_without_popups(window, self.renderer, frame_loc, scale, frame_alpha);
            self.content
                .extend(view_elements.into_iter().map(ContentRenderElement::Surface));
            let frame_size: Size<i32, Physical> = frame_rect.size.to_physical_precise_round(scale);
            push_view_background_effect(
                &mut self.content,
                None,
                window,
                ViewBox {
                    loc: frame_loc,
                    element_scale: Scale::from(1.),
                    output_scale: scale,
                    clip: Rectangle::new(frame_loc, frame_size),
                },
                self.blur,
            );
        }

        // Draw the closing frames beneath the other frames.
        for (closing, pos) in strip.closing_frames_with_render_pos() {
            let elem = closing.render(pos + geo.working_area.loc.to_f64());
            self.content.push(ContentRenderElement::Texture(elem));
        }
    }

    /// Panel exclusive zones in the fill colour, the wallpaper layers, the output fill, and in the
    /// overview its shadow and backdrop.
    fn background(&mut self, geo: &FrameGeo, mut layers: Layers) {
        let scale = self.scale;
        let output_size = self.rect.size;
        let mut backdrop = None;
        if let Some(state) = self.ewm.output_state.get_mut(self.output) {
            let background_color = self.ewm.overview.background_color;
            let zones = Rectangle::from_size(output_size).subtract_rect(geo.working_area);
            for (buffer, zone) in state.bands.iter_mut().zip(zones) {
                buffer.update(zone.size, background_color);
                let loc = zone.loc.to_physical_precise_round(scale);
                self.content
                    .push(ContentRenderElement::SolidColor(solid(buffer, loc, scale)));
            }
            self.content.append(&mut layers.bottom);
            self.content.append(&mut layers.background);
            state.background.set_color(background_color);
            self.content.push(ContentRenderElement::SolidColor(solid(
                &state.background,
                (0, 0),
                scale,
            )));

            if let Some(progress) = self.ewm.overview.progress() {
                state.overview_shadow.update_render_elements(
                    output_size.to_f64(),
                    true,
                    CornerRadius::default(),
                    scale.x,
                    progress.clamp(0., 1.) as f32,
                );
                state
                    .overview_shadow
                    .render(self.renderer, Point::from((0., 0.)), &mut |shadow| {
                        self.content.push(ContentRenderElement::Shadow(shadow));
                    });
                state
                    .overview_backdrop
                    .set_color(self.ewm.overview.backdrop_color);
                backdrop = Some(EwmRenderElement::SolidColor(solid(
                    &state.overview_backdrop,
                    (0, 0),
                    scale,
                )));
            }
        }
        self.flush_zoomed();
        self.elements.extend(backdrop);
    }

    /// Move the section into the elements, zoomed about the output.
    fn flush_zoomed(&mut self) {
        let (zoom, offset) = (self.geometry.zoom, self.zoom_offset);
        self.elements.extend(
            self.content
                .drain(..)
                .map(|elem| EwmRenderElement::Zoomed(ZoomRenderElement::new(elem, zoom, offset))),
        );
    }

    /// Move the section into the elements, zoomed and slid with the frames.
    fn flush_slid(&mut self) {
        let (zoom, offset) = (self.geometry.zoom, self.slide_offset);
        self.elements.extend(
            self.content
                .drain(..)
                .map(|elem| EwmRenderElement::Zoomed(ZoomRenderElement::new(elem, zoom, offset))),
        );
    }

    fn finish(self) -> (Vec<EwmRenderElement>, Vec<EwmRenderElement>) {
        (self.elements, self.cursor)
    }
}

/// Collect the render elements of `output`, filtering during collection so nothing from another
/// output damages this one.
///
/// Returns `(content_elements, cursor_elements)`, the cursor apart so a screencast skips a full
/// re-render when only the cursor moved.
///
/// Rendering order (front to back):
/// 1. Cursor
/// 2. Overlay layer
/// 3. Top layer and layer-shell popups
/// 4. Zoomed by the overview: window popups, floating frames, views, frames (these four also slide
///    off when hidden), panel exclusive zones in the fill colour, bottom and background layers, the
///    output fill and the overview shadow
/// 5. Overview backdrop
pub fn collect_render_elements_for_output(
    ewm: &mut Ewm,
    renderer: &mut GlesRenderer,
    cursor_texture_cache: &CursorTextureCache,
    output: &Output,
    include_cursor: bool,
) -> (Vec<EwmRenderElement>, Vec<EwmRenderElement>) {
    tracy_span!("collect_render_elements");
    crate::render_helpers::init(renderer);
    let mut pass = OutputRender::new(ewm, renderer, output, include_cursor);
    pass.cursor(cursor_texture_cache);
    pass.dnd_icon();
    if pass.lock() {
        return pass.finish();
    }
    let mut layers = pass.layers();
    pass.elements.append(&mut layers.overlay);
    // The top layer waits behind a fullscreen view, which covers it.
    let above_top_layer = pass.ewm.render_above_top_layer(output);
    if !above_top_layer {
        pass.elements.append(&mut layers.top);
    }
    // Popups of the unzoomed layers stay at unit scale above the zoomed content.
    pass.layer_popups(&[Layer::Overlay, Layer::Top]);
    pass.window_popups();
    pass.zoomed_layer_popups(&[Layer::Bottom, Layer::Background]);
    let geo = pass.frame_geo();
    pass.floats(&geo);
    pass.strip_views(&geo, above_top_layer);
    pass.flush_slid();
    if above_top_layer {
        pass.elements.append(&mut layers.top);
    }
    pass.strip_frames(&geo);
    pass.flush_slid();
    pass.background(&geo, layers);
    pass.finish()
}

/// `frame`'s views about its own origin, closing ones on top, the fullscreen one over the rest.
fn render_frame_views(
    content: &mut Vec<ContentRenderElement>,
    renderer: &mut GlesRenderer,
    ewm: &Ewm,
    frame: &crate::layout::strip::Frame,
    bake: &Bake<'_>,
) {
    push_closing_views(content, &frame.closing_views, Point::from((0., 0.)));
    let space = frame_own_space(ewm, frame, bake);
    let fullscreen = frame.entries.iter().filter(|entry| entry.fullscreen());
    let others = frame.entries.iter().filter(|entry| !entry.fullscreen());
    for entry in fullscreen.chain(others) {
        push_frame_view(content, renderer, ewm, entry, space, bake);
    }
}

/// `closing` views fading out about the frame origin at `origin`.
fn push_closing_views(
    content: &mut Vec<ContentRenderElement>,
    closing: &[Closing],
    origin: Point<f64, Logical>,
) {
    for closing in closing.iter().rev() {
        content.push(ContentRenderElement::Texture(closing.render(origin)));
    }
}

/// The output around a frame being baked: scale, size, its origin in frame space, the backdrop.
struct Bake<'a> {
    scale: Scale<f64>,
    output_size: Size<i32, Logical>,
    origin: Point<i32, Logical>,
    backdrop: Option<&'a SolidColorBuffer>,
}

fn bake_env<'a>(ewm: &'a Ewm, location: FrameLocation<'_>) -> Bake<'a> {
    let output = location.monitor.output();
    // A float's own place is its origin, a strip frame's is in the working area.
    let origin = if location.floating {
        let pos = location
            .monitor
            .floating
            .frame_with_data(location.frame.surface_id)
            .map_or(Point::default(), |(_, data)| data.render_pos());
        Point::from((-(pos.x.round() as i32), -(pos.y.round() as i32)))
    } else {
        let working_area = ewm.get_working_area(output);
        Point::from((-working_area.loc.x, -working_area.loc.y))
    };
    Bake {
        scale: Scale::from(output.current_scale().fractional_scale()),
        output_size: crate::utils::output_size(output).to_i32_round(),
        origin,
        backdrop: ewm
            .output_state
            .get(output)
            .map(|state| &state.fullscreen_backdrop),
    }
}

/// `frame`'s space about its own origin: its bounds and the output-sized slot around it.
fn frame_own_space(ewm: &Ewm, frame: &crate::layout::strip::Frame, bake: &Bake<'_>) -> FrameSpace {
    let frame_box = Rectangle::<f64, Logical>::from_size(Size::from((frame.width, frame.height)))
        .to_physical_precise_round(bake.scale);
    FrameSpace {
        origin: Point::from((0., 0.)),
        bounds: frame_bounds(ewm.surfaces.get(&frame.surface_id), frame_box, bake.scale),
        slot: Rectangle::new(
            bake.origin.to_physical_precise_round(bake.scale),
            bake.output_size.to_physical_precise_round(bake.scale),
        ),
    }
}

/// `entry` about its frame's origin, without blur.
fn push_frame_view(
    content: &mut Vec<ContentRenderElement>,
    renderer: &mut GlesRenderer,
    ewm: &Ewm,
    entry: &LayoutEntry,
    space: FrameSpace,
    bake: &Bake<'_>,
) {
    let Some(surface) = entry.surface_id().and_then(|id| ewm.surfaces.get(&id)) else {
        return;
    };
    let no_blur = Blur {
        off: true,
        ..Blur::default()
    };
    let focused = entry.fullscreen() || ewm.focused_entry_id() == Some(entry.id);
    let alpha = if focused { 1.0 } else { ewm.unfocused_alpha };
    if let Some(place) = ViewPlace::new(surface, entry, alpha, space, bake.output_size, bake.scale)
    {
        push_view(
            content,
            renderer,
            &place,
            bake.backdrop,
            bake.scale,
            no_blur,
        );
    }
}

/// The view at `location` baked for its close animation, about its frame's origin.
pub(crate) fn render_closing_view(
    ewm: &Ewm,
    renderer: &mut GlesRenderer,
    location: LayoutEntryLocation<'_>,
) -> Option<Snapshot> {
    tracy_span!("render_closing_view");

    let bake = bake_env(ewm, location.frame_location());
    let space = frame_own_space(ewm, location.frame, &bake);
    let mut content: Vec<ContentRenderElement> = Vec::new();
    push_frame_view(&mut content, renderer, ewm, location.entry, space, &bake);
    Snapshot::new(renderer, &content, bake.scale)
}

/// Frame `id` with its views baked to a texture for its close animation.
pub fn render_frame_snapshot(
    ewm: &mut Ewm,
    renderer: &mut GlesRenderer,
    id: u64,
) -> Option<Snapshot> {
    tracy_span!("render_frame_snapshot");

    let location = ewm.layout.frame_location(id)?;
    let output = location.monitor.output().clone();
    let bake = bake_env(ewm, location);
    let scale = bake.scale;

    let mut content: Vec<ContentRenderElement> = Vec::new();
    render_frame_views(&mut content, renderer, ewm, location.frame, &bake);
    // Like the live pass, no frame surface or shadow before the frame has shown a buffer.
    if let Some(surface) = ewm.surfaces.get(&id)
        && surface.has_buffer()
    {
        let elements =
            render_window_without_popups(&surface.window, renderer, Point::from((0, 0)), scale, 1.);
        content.extend(elements.into_iter().map(ContentRenderElement::Surface));
        let focused = ewm.focused_frame_id() == id;
        if let Some((_, data)) = ewm
            .layout
            .floating_mut(&output)
            .and_then(|space| space.frame_with_data_mut(id))
        {
            let size = data.size();
            let shadow = data.shadow_mut();
            shadow.update_render_elements(size, focused, CornerRadius::default(), scale.x, 1.);
            shadow.render(renderer, Point::from((0., 0.)), &mut |shadow| {
                content.push(ContentRenderElement::Shadow(shadow));
            });
        }
    }
    Snapshot::new(renderer, &content, scale)
}

/// `window` baked at its current size, before a resize commit replaces it.
pub(crate) fn render_view_snapshot(
    window: &smithay::desktop::Window,
    renderer: &mut GlesRenderer,
    scale: Scale<f64>,
) -> Option<Snapshot> {
    let elements = render_window_without_popups(window, renderer, Point::from((0, 0)), scale, 1.);
    Snapshot::new(renderer, &elements, scale)
}

/// Render one popup placement into `elements`.
fn push_popup<
    E: From<WaylandSurfaceRenderElement<GlesRenderer>> + From<FramebufferEffectElement>,
>(
    elements: &mut Vec<E>,
    renderer: &mut GlesRenderer,
    scale: Scale<f64>,
    output_pos: Point<i32, Logical>,
    placement: &PopupPlacement,
    blur_config: Blur,
) {
    let render_loc = Point::from((
        placement.location.x - output_pos.x,
        placement.location.y - output_pos.y,
    ));
    let render_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        render_elements_from_surface_tree(
            renderer,
            &placement.surface,
            render_loc.to_physical_precise_round(scale),
            scale,
            1.0,
            Kind::Unspecified,
        );
    elements.extend(render_elements.into_iter().map(E::from));

    let mut geometry = placement.geometry;
    geometry.loc += render_loc;
    push_background_effect(
        elements,
        None,
        &placement.surface,
        geometry.to_f64(),
        false,
        placement.geometry.loc.upscale(-1).to_f64(),
        Scale::from(1.),
        scale.x,
        blur_config,
    );
}

/// One on-screen placement of a popup.
#[derive(Clone)]
pub struct PopupPlacement {
    pub surface: WlSurface,
    pub location: Point<i32, Logical>,
    /// Popup geometry within its surface.
    pub geometry: Rectangle<i32, Logical>,
}

/// Popup placements on an output, front to back: layer-shell popups, then
/// window popups unless the overview or the hide has moved them away with their windows.
pub fn collect_popup_placements(
    ewm: &Ewm,
    output_rect: Rectangle<i32, smithay::utils::Logical>,
    layer_popup_output: Option<&Output>,
) -> Vec<PopupPlacement> {
    // Bottom and background popups zoom away with their layers.
    let layers: &[Layer] = if ewm.overview.is_active() {
        &[Layer::Overlay, Layer::Top]
    } else {
        &[Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background]
    };
    let mut placements = layer_popup_output.map_or_else(Vec::new, |output| {
        layer_popup_placements(output, output_rect, layers)
    });
    if ewm.frames_in_place() {
        placements.extend(window_popup_placements(ewm, output_rect));
    }
    placements
}

/// Popups of toplevel windows overlapping `output_rect`. One placement per
/// popup, anchored to the parent surface's render position: focused-or-primary
/// view for layout entries, the frame's own origin for Emacs frames.
pub fn window_popup_placements(
    ewm: &Ewm,
    output_rect: Rectangle<i32, smithay::utils::Logical>,
) -> Vec<PopupPlacement> {
    use smithay::wayland::seat::WaylandFocus;
    let mut placements = Vec::new();
    for (_, window) in ewm.windows() {
        let Some(surface) = window.wl_surface() else {
            continue;
        };
        let popups: Vec<_> = PopupManager::popups_for_surface(&surface).collect();
        if popups.is_empty() {
            continue;
        }
        let Some(window_loc) = ewm.window_global_position(window) else {
            continue;
        };
        // A float hidden under a fullscreen view hides its popups with it.
        let hidden = ewm
            .surface_id(&surface)
            .and_then(|id| ewm.preferred_entry_for_surface(id))
            .is_some_and(|location| {
                location.floating && !ewm.float_visible(location.monitor, location.frame)
            });
        if hidden {
            continue;
        }
        let window_geo = window.geometry();
        for (popup, popup_offset) in popups {
            let popup_geo = popup.geometry();
            let popup_loc = window_loc + window_geo.loc + popup_offset - popup_geo.loc;
            let popup_rect: Rectangle<i32, smithay::utils::Logical> =
                Rectangle::new(popup_loc, popup_geo.size);
            if !output_rect.overlaps(popup_rect) {
                continue;
            }
            placements.push(PopupPlacement {
                surface: popup.wl_surface().clone(),
                location: popup_loc,
                geometry: popup_geo,
            });
        }
    }
    placements
}

/// Popups of the layer-shell surfaces on `layers`, visible on an output.
pub fn layer_popup_placements(
    output: &Output,
    output_rect: Rectangle<i32, smithay::utils::Logical>,
    layers: &[Layer],
) -> Vec<PopupPlacement> {
    let mut placements = Vec::new();
    let layer_map = layer_map_for_output(output);
    for &layer in layers {
        for surface in layer_map.layers_on(layer).rev() {
            let Some(geo) = layer_map.layer_geometry(surface) else {
                continue;
            };

            for (popup, popup_offset) in PopupManager::popups_for_surface(surface.wl_surface()) {
                let popup_geo = popup.geometry();
                let popup_loc = output_rect.loc + geo.loc + popup_offset - popup_geo.loc;
                let popup_rect: Rectangle<i32, smithay::utils::Logical> =
                    Rectangle::new(popup_loc, popup_geo.size);
                if !output_rect.overlaps(popup_rect) {
                    continue;
                }
                placements.push(PopupPlacement {
                    surface: popup.wl_surface().clone(),
                    location: popup_loc,
                    geometry: popup_geo,
                });
            }
        }
    }
    placements
}

fn collect_screencopy_elements_for_output(
    ewm: &mut Ewm,
    renderer: &mut GlesRenderer,
    output: &Output,
    cursor_texture_cache: &CursorTextureCache,
    include_cursor: bool,
) -> Vec<EwmRenderElement> {
    let (mut content, cursor) = collect_render_elements_for_output(
        ewm,
        renderer,
        cursor_texture_cache,
        output,
        include_cursor,
    );

    content.splice(0..0, cursor);
    content
}

fn relocate_screencopy_elements(
    elements: &[EwmRenderElement],
    region_loc: Point<i32, Physical>,
) -> Vec<RelocateRenderElement<&EwmRenderElement>> {
    elements
        .iter()
        .map(|element| {
            RelocateRenderElement::from_element(element, region_loc.upscale(-1), Relocate::Relative)
        })
        .collect()
}

fn sync_screencopy_damage_tracker(
    damage_tracker: &mut OutputDamageTracker,
    size: Size<i32, Physical>,
    output_scale: Scale<f64>,
    output_transform: Transform,
) {
    let OutputModeSource::Static {
        size: last_size,
        scale: last_scale,
        transform: last_transform,
    } = damage_tracker.mode().clone()
    else {
        unreachable!("screencopy damage tracker must have static mode");
    };

    if size != last_size || output_scale != last_scale || output_transform != last_transform {
        *damage_tracker = OutputDamageTracker::new(size, output_scale, output_transform);
    }
}

fn damage_screencopy<'a>(
    damage_tracker: &'a mut OutputDamageTracker,
    elements: &[impl Element],
    size: Size<i32, Physical>,
    output_scale: Scale<f64>,
    output_transform: Transform,
) -> Option<&'a Vec<Rectangle<i32, Physical>>> {
    sync_screencopy_damage_tracker(damage_tracker, size, output_scale, output_transform);
    damage_tracker.damage_output(1, elements).unwrap().0
}

fn send_screencopy_damage(
    screencopy: &Screencopy,
    damages: &[Rectangle<i32, Physical>],
    output_transform: Transform,
) {
    let physical_size = output_transform.transform_size(screencopy.buffer_size());
    let buffer_damages = damages.iter().map(|damage| {
        damage
            .to_logical(1)
            .to_buffer(1, output_transform.invert(), &physical_size.to_logical(1))
    });
    screencopy.damage(buffer_damages);
}

fn render_screencopy_elements(
    renderer: &mut GlesRenderer,
    screencopy: &Screencopy,
    size: Size<i32, Physical>,
    output_scale: Scale<f64>,
    output_transform: Transform,
    elements: &[impl RenderElement<GlesRenderer>],
) -> anyhow::Result<Option<SyncPoint>> {
    match screencopy.buffer() {
        ScreencopyBuffer::Dmabuf(dmabuf) => render_to_dmabuf(
            renderer,
            dmabuf.clone(),
            size,
            output_scale,
            output_transform,
            elements.iter().rev(),
        )
        .map(Some),
        ScreencopyBuffer::Shm(buffer) => render_to_shm(
            renderer,
            buffer,
            size,
            output_scale,
            output_transform,
            elements.iter().rev(),
        )
        .map(|_| None),
    }
}

fn reset_screencopy_damage_tracker(damage_tracker: &mut OutputDamageTracker) {
    *damage_tracker = OutputDamageTracker::new((0, 0), 1.0, Transform::Normal);
}

/// Process pending screencopy requests for a specific output
///
/// This should be called after rendering the main frame for an output.
/// Uses per-queue damage tracking: each screencopy client gets its own
/// damage tracker, so only actual changes since that client's last capture
/// are reported. When `with_damage` is set and there's no damage, the
/// request stays in the queue until the next redraw.
pub fn process_screencopies_for_output(
    ewm: &mut Ewm,
    renderer: &mut GlesRenderer,
    output: &smithay::output::Output,
    cursor_texture_cache: &CursorTextureCache,
    event_loop: &LoopHandle<'static, State>,
) {
    use std::cell::OnceCell;

    use tracing::trace;

    let output_scale = Scale::from(output.current_scale().fractional_scale());
    let output_transform = output.current_transform();

    // Take screencopy state to avoid borrow conflict with element collection
    let mut screencopy_state = std::mem::take(&mut ewm.screencopy_state);
    let elements_with_cursor = OnceCell::new();
    let elements_without_cursor = OnceCell::new();

    screencopy_state.with_queues_mut(|queue| {
        let (damage_tracker, maybe_screencopy) = queue.split();
        let Some(screencopy) = maybe_screencopy else {
            return;
        };
        if screencopy.output() != output {
            return;
        }

        // Lazily collect render elements (shared across all queues for this output)
        let elements = if screencopy.overlay_cursor() {
            elements_with_cursor.get_or_init(|| {
                collect_screencopy_elements_for_output(
                    ewm,
                    renderer,
                    output,
                    cursor_texture_cache,
                    true, // include_cursor
                )
            })
        } else {
            elements_without_cursor.get_or_init(|| {
                collect_screencopy_elements_for_output(
                    ewm,
                    renderer,
                    output,
                    cursor_texture_cache,
                    false, // include_cursor
                )
            })
        };

        let size = screencopy.buffer_size();
        let with_damage = screencopy.with_damage();

        let relocated_elements = relocate_screencopy_elements(elements, screencopy.region_loc());
        let damages = damage_screencopy(
            damage_tracker,
            &relocated_elements,
            size,
            output_scale,
            output_transform,
        );

        if with_damage && damages.is_none() {
            trace!("screencopy: no damage, waiting for next redraw");
            return;
        }

        let render_result = render_screencopy_elements(
            renderer,
            screencopy,
            size,
            output_scale,
            output_transform,
            &relocated_elements,
        );

        match render_result {
            Ok(sync) => {
                if let (true, Some(damages)) = (with_damage, damages) {
                    send_screencopy_damage(screencopy, damages, output_transform);
                }
                queue.pop().submit_after_sync(false, sync, event_loop);
            }
            Err(err) => {
                // Reset damage tracker so next attempt reports full damage
                reset_screencopy_damage_tracker(damage_tracker);
                queue.pop();
                warn!("Error rendering for screencopy: {:?}", err);
            }
        }
    });

    ewm.screencopy_state = screencopy_state;
}

/// Render a screencopy request immediately (without damage tracking).
///
/// Used for `Copy` requests (not `CopyWithDamage`) which should be served
/// as soon as possible without waiting for the next output redraw cycle.
/// Still updates the per-queue damage tracker so subsequent `CopyWithDamage`
/// calls correctly track damage relative to this rendered frame.
pub fn render_screencopy_immediate(
    ewm: &mut Ewm,
    renderer: &mut GlesRenderer,
    manager: &smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    screencopy: crate::protocols::screencopy::Screencopy,
    cursor_texture_cache: &CursorTextureCache,
    event_loop: &LoopHandle<'static, State>,
) {
    let output = screencopy.output().clone();
    let output_scale = Scale::from(output.current_scale().fractional_scale());
    let output_transform = output.current_transform();

    let elements = collect_screencopy_elements_for_output(
        ewm,
        renderer,
        &output,
        cursor_texture_cache,
        screencopy.overlay_cursor(),
    );
    let size = screencopy.buffer_size();
    let relocated_elements = relocate_screencopy_elements(&elements, screencopy.region_loc());

    // Update the per-queue damage tracker so subsequent CopyWithDamage calls
    // correctly track damage relative to this rendered frame.
    if let Some(queue) = ewm.screencopy_state.get_queue_mut(manager) {
        let (damage_tracker, _) = queue.split();
        let _ = damage_screencopy(
            damage_tracker,
            &relocated_elements,
            size,
            output_scale,
            output_transform,
        );
    }

    let render_result = render_screencopy_elements(
        renderer,
        &screencopy,
        size,
        output_scale,
        output_transform,
        &relocated_elements,
    );

    match render_result {
        Ok(sync) => {
            screencopy.submit_after_sync(false, sync, event_loop);
        }
        Err(err) => {
            if let Some(queue) = ewm.screencopy_state.get_queue_mut(manager) {
                let (damage_tracker, _) = queue.split();
                reset_screencopy_damage_tracker(damage_tracker);
            }
            warn!("Error rendering for screencopy: {:?}", err);
        }
    }
}
