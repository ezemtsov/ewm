//! Ported from niri's `src/screencasting/`. Implements screen casting
//! via the org.gnome.Mutter.ScreenCast D-Bus interface.

pub mod pw_utils;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context as _;
pub use pw_utils::{Cast, CastSessionId, CastSizeChange, CastStreamId, PipeWire, PwToCompositor};
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size};
use smithay::wayland::seat::WaylandFocus;
use tracing::{debug, info, warn};

use crate::cursor::CursorTextureCache;
use crate::dbus::{self, CastTarget, ScreenCastToCompositor};
use crate::render::{EwmRenderElement, render_cursor_elements};
use crate::{Ewm, LayoutEntry, LayoutEntryId, State};

fn output_cast_cursor_location(
    pointer_location: (f64, f64),
    output_pos: Point<i32, Logical>,
    output_size: Size<i32, Logical>,
    output_scale: Scale<f64>,
    include_cursor: bool,
) -> Point<i32, Physical> {
    if !include_cursor {
        return Point::default();
    }

    let (px, py) = pointer_location;
    let pointer_pos = Point::from((px as i32, py as i32));
    if !Rectangle::new(output_pos, output_size).contains(pointer_pos) {
        return Point::default();
    }

    let output_local = Point::from((px - output_pos.x as f64, py - output_pos.y as f64));
    output_local.to_physical_precise_round(output_scale)
}

pub struct Screencasting {
    pub casts: Vec<Cast>,

    /// Persistent sender given to every `PipeWire` instance. Outlives
    /// any single connection so the calloop source on the receiver side
    /// stays registered across reinits after `FatalError`.
    pub pw_to_compositor: Sender<PwToCompositor>,

    /// Layout entry id -> output, rebuilt by `refresh_mapped_cast_outputs`.
    pub mapped_cast_output: HashMap<LayoutEntryId, Output>,

    // Drop PipeWire last, specifically after `casts`, to prevent a
    // double-free: each Cast's StreamRc holds a clone of the core, and
    // dropping core before the streams gets us free()-then-use.
    pub pipewire: Option<PipeWire>,
}

impl Screencasting {
    pub fn new(event_loop: &LoopHandle<'static, State>) -> Self {
        let pw_to_compositor = {
            let (tx, rx) = channel::channel();
            event_loop
                .insert_source(rx, move |event, _, state| match event {
                    channel::Event::Msg(msg) => state.on_pw_msg(msg),
                    channel::Event::Closed => (),
                })
                .expect("failed to register PipeWire event source");
            tx
        };
        Self {
            casts: Vec::new(),
            pw_to_compositor,
            mapped_cast_output: HashMap::new(),
            pipewire: None,
        }
    }
}

struct LayoutEntryCastGeometry<'a> {
    output: Output,
    frame_surface_id: u64,
    frame_origin: Point<i32, Logical>,
    entry: &'a LayoutEntry,
    entry_rect: Rectangle<i32, Logical>,
}

impl Ewm {
    fn layout_entry_cast_geometry(
        &self,
        entry_id: LayoutEntryId,
    ) -> Option<LayoutEntryCastGeometry<'_>> {
        let location = self.layout_entry_for_id(entry_id)?;
        let output = self.find_mapped_output(location.output_name)?;
        let output_geo = self.space.output_geometry(&output)?;
        let working_area = self.get_working_area(&output);
        let strip = self.frame_set.mapped_strip(location.output_name)?;
        let frame = strip.frames.get(location.frame_idx)?;
        let frame_dx = strip.screen_x_of_frame(location.frame_idx) as i32;
        let frame_origin = Point::from((
            output_geo.loc.x + working_area.loc.x + frame_dx,
            output_geo.loc.y + working_area.loc.y,
        ));
        let entry_rect = self.layout_entry_rect(
            output_geo,
            working_area,
            strip,
            location.frame_idx,
            location.entry,
        );
        Some(LayoutEntryCastGeometry {
            output,
            frame_surface_id: frame.surface_id,
            frame_origin,
            entry: location.entry,
            entry_rect,
        })
    }

    pub(crate) fn screen_cast_output_for_target(&self, target: &CastTarget) -> Option<Output> {
        match target {
            CastTarget::Output { name } => self.find_mapped_output(name),
            CastTarget::LayoutEntry { entry_id } => self
                .casting
                .mapped_cast_output
                .get(entry_id)
                .cloned()
                .or_else(|| self.layout_entry_cast_geometry(*entry_id).map(|g| g.output)),
        }
    }

    pub(crate) fn layout_entry_cast_params(
        &self,
        entry_id: LayoutEntryId,
    ) -> Option<(Size<i32, Physical>, u32)> {
        let geometry = self.layout_entry_cast_geometry(entry_id)?;
        let scale = Scale::from(geometry.output.current_scale().fractional_scale());
        let size = geometry.entry_rect.size.to_physical_precise_round(scale);
        if size.w <= 0 || size.h <= 0 {
            return None;
        }
        let refresh = geometry
            .output
            .current_mode()
            .map(|m| (m.refresh / 1000) as u32)
            .unwrap_or(60);
        Some((size, refresh))
    }
}

impl State {
    /// Lazily create the PipeWire connection on the first cast and
    /// after any `FatalError`-driven teardown. Returns the GBM device
    /// and modifier list used by the new cast.
    fn prepare_pw_cast(
        &mut self,
        alpha: bool,
    ) -> anyhow::Result<(GbmDevice<DrmDeviceFd>, Vec<i64>)> {
        let gbm = self
            .backend
            .gbm_device()
            .context("no GBM device available")?;

        if self.ewm.casting.pipewire.is_none() {
            let pw = PipeWire::new(
                self.ewm.loop_handle.clone(),
                self.ewm.casting.pw_to_compositor.clone(),
            )
            .context("error initializing PipeWire")?;
            self.ewm.casting.pipewire = Some(pw);
        }

        // Filter render formats to the fourcc we'll negotiate. Window
        // casts use ARGB (alpha for transparency); output casts use XRGB.
        let fourcc = if alpha {
            smithay::backend::allocator::Fourcc::Argb8888
        } else {
            smithay::backend::allocator::Fourcc::Xrgb8888
        };
        let render_formats = self.backend.render_formats_for_fourcc(fourcc);

        Ok((gbm, render_formats))
    }

    pub fn on_pw_msg(&mut self, msg: PwToCompositor) {
        match msg {
            PwToCompositor::FatalError => {
                tracing::error!("PipeWire fatal error, tearing down");
                // Stop active casts before dropping pw so streams can
                // disconnect against a live core. Going through stop_cast
                // makes D-Bus emit Closed to consumers (xdp-gnome, ...).
                let session_ids: Vec<CastSessionId> = self
                    .ewm
                    .casting
                    .casts
                    .iter()
                    .map(|c| c.session_id)
                    .collect();
                for id in session_ids {
                    self.ewm.stop_cast(id);
                }
                if let Some(pw) = self.ewm.casting.pipewire.take() {
                    self.ewm.loop_handle.remove(pw.token);
                }
            }
            PwToCompositor::StopCast { session_id } => {
                self.ewm.stop_cast(session_id);
            }
            PwToCompositor::Redraw { session_id } => {
                let Some(cast) = self
                    .ewm
                    .casting
                    .casts
                    .iter()
                    .find(|c| c.session_id == session_id)
                else {
                    return;
                };
                let output = self.ewm.screen_cast_output_for_target(&cast.target);
                let Some(output) = output else { return };
                self.ewm.queue_redraw(&output);
            }
        }
    }

    pub fn on_screen_cast_msg(&mut self, msg: ScreenCastToCompositor) {
        match msg {
            ScreenCastToCompositor::StartCast {
                session_id,
                target,
                signal_ctx,
                cursor_mode,
            } => {
                info!(
                    "StartCast: session={}, target={:?}, cursor_mode={}",
                    session_id, target, cursor_mode,
                );

                let alpha = matches!(target, CastTarget::LayoutEntry { .. });

                let (gbm, render_formats) = match self.prepare_pw_cast(alpha) {
                    Ok(x) => x,
                    Err(err) => {
                        warn!("error starting screencast: {err:?}");
                        self.ewm.stop_cast(session_id);
                        return;
                    }
                };

                let cast_info: Option<(Size<i32, Physical>, u32)> = match &target {
                    CastTarget::Output { name } => {
                        self.backend.output_info(name).and_then(|info| {
                            dbus::current_output_mode(&info).map(|mode| {
                                (
                                    Size::from((mode.width, mode.height)),
                                    (mode.refresh / 1000) as u32,
                                )
                            })
                        })
                    }
                    CastTarget::LayoutEntry { entry_id } => {
                        let params = self.ewm.layout_entry_cast_params(*entry_id);
                        if params.is_none() {
                            warn!("StartCast: layout entry {} not found", entry_id);
                        }
                        params
                    }
                };

                let Some((size, refresh)) = cast_info else {
                    self.ewm.stop_cast(session_id);
                    return;
                };

                let pw = self.ewm.casting.pipewire.as_ref().unwrap();
                match pw.start_cast(
                    session_id,
                    gbm,
                    size,
                    refresh,
                    target.clone(),
                    alpha,
                    signal_ctx,
                    render_formats,
                    cursor_mode,
                ) {
                    Ok(cast) => {
                        info!(
                            "PipeWire stream created for {:?}, waiting for state change",
                            target,
                        );
                        self.ewm.casting.casts.push(cast);
                    }
                    Err(err) => {
                        warn!("Failed to create PipeWire stream: {err:?}");
                        self.ewm.stop_cast(session_id);
                    }
                }
            }
            ScreenCastToCompositor::StopCast { session_id } => {
                info!("StopCast: session={}", session_id);
                self.ewm.stop_cast(session_id);
            }
        }
    }
}

impl Ewm {
    /// Stop a screen cast session: remove from active list (Drop disconnects
    /// the PipeWire stream) and emit the D-Bus Closed signal so the consumer
    /// knows.
    pub fn stop_cast(&mut self, session_id: CastSessionId) {
        debug!(%session_id, "stop_cast");

        let mut idx = 0;
        while idx < self.casting.casts.len() {
            if self.casting.casts[idx].session_id == session_id {
                let _ = self.casting.casts.swap_remove(idx);
            } else {
                idx += 1;
            }
        }

        if let Some(ref dbus) = self.dbus_servers
            && let Some(ref conn) = dbus.conn_screen_cast
        {
            let server = conn.object_server();
            let path = format!("/org/gnome/Mutter/ScreenCast/Session/u{}", session_id);
            if let Ok(iface) = server.interface::<_, dbus::screen_cast::Session>(path.as_str()) {
                async_io::block_on(async {
                    let signal_emitter = iface.signal_emitter().clone();
                    iface.get().stop(server.inner(), signal_emitter).await
                });
            }
        }
    }

    fn request_stop_cast(&self, session_id: CastSessionId) {
        let _ = self
            .casting
            .pw_to_compositor
            .send(PwToCompositor::StopCast { session_id });
    }

    fn render_plain_entry_for_screen_cast(
        &self,
        renderer: &mut GlesRenderer,
        geometry: &LayoutEntryCastGeometry<'_>,
        output_scale: Scale<f64>,
        entry_size: Size<i32, Physical>,
    ) -> Vec<EwmRenderElement> {
        let Some(frame_window) = self.id_windows.get(&geometry.frame_surface_id) else {
            return Vec::new();
        };
        let Some(surface) = frame_window.wl_surface() else {
            return Vec::new();
        };

        let render_offset = Point::from((
            geometry.frame_origin.x - geometry.entry_rect.loc.x,
            geometry.frame_origin.y - geometry.entry_rect.loc.y,
        ))
        .to_physical_precise_round(output_scale);
        let view_elements = render_elements_from_surface_tree(
            renderer,
            &surface,
            render_offset,
            output_scale,
            1.0,
            Kind::Unspecified,
        );
        let mut elements = Vec::new();
        crate::render::push_constrained(
            &mut elements,
            view_elements,
            Point::default(),
            Scale::from(1.0),
            output_scale,
            Rectangle::from_size(entry_size),
        );
        elements
    }

    fn render_layout_entry_content_for_screen_cast(
        &self,
        renderer: &mut GlesRenderer,
        geometry: &LayoutEntryCastGeometry<'_>,
        output_scale: Scale<f64>,
        entry_size: Size<i32, Physical>,
    ) -> Vec<EwmRenderElement> {
        if let Some(window) = geometry
            .entry
            .surface_id()
            .and_then(|surface_id| self.id_windows.get(&surface_id))
            && let Some(surface) = window.wl_surface()
        {
            let view_elements = render_elements_from_surface_tree(
                renderer,
                &surface,
                Point::default(),
                output_scale,
                1.0,
                Kind::Unspecified,
            );
            let source_size = window
                .geometry()
                .size
                .to_physical_precise_round(output_scale);
            let mut elements = Vec::new();
            crate::render::push_view_elements_fit(
                &mut elements,
                window,
                view_elements,
                Point::default(),
                source_size,
                output_scale,
                Rectangle::from_size(entry_size),
                !geometry.entry.primary(),
                None,
                self.blur_config,
            );
            return elements;
        }

        self.render_plain_entry_for_screen_cast(renderer, geometry, output_scale, entry_size)
    }

    fn layout_entry_cursor_for_screen_cast(
        &self,
        renderer: &mut GlesRenderer,
        cursor_texture_cache: &CursorTextureCache,
        geometry: &LayoutEntryCastGeometry<'_>,
        output_scale: Scale<f64>,
        include_cursor: bool,
    ) -> (Vec<EwmRenderElement>, Point<i32, Physical>) {
        if !include_cursor {
            return (Vec::new(), Point::default());
        }

        let mut cursor_elements: Vec<EwmRenderElement> = Vec::new();
        let mut cursor_location = Point::<i32, Physical>::from((0, 0));

        let (px, py) = self.cursor_location();
        let rect = geometry.entry_rect;
        if px >= rect.loc.x as f64
            && py >= rect.loc.y as f64
            && px < (rect.loc.x + rect.size.w) as f64
            && py < (rect.loc.y + rect.size.h) as f64
        {
            let pointer_pos = Point::from((px - rect.loc.x as f64, py - rect.loc.y as f64));
            cursor_location = pointer_pos.to_physical_precise_round(output_scale);
            cursor_elements.extend(render_cursor_elements(
                self,
                renderer,
                cursor_texture_cache,
                &geometry.output,
                output_scale,
                pointer_pos,
            ));
        }

        (cursor_elements, cursor_location)
    }

    /// Render one layout entry into a screencast buffer.
    ///
    /// Returns true if a frame was rendered. The `cast` is passed
    /// separately because `casting.casts` is detached from `self`
    /// during the render loop.
    #[allow(clippy::too_many_arguments)]
    pub fn render_layout_entry_for_screen_cast(
        &self,
        renderer: &mut GlesRenderer,
        cast: &mut Cast,
        entry_id: LayoutEntryId,
        output: &Output,
        cursor_texture_cache: &CursorTextureCache,
        output_scale: Scale<f64>,
        target_frame_time: Duration,
    ) -> bool {
        let Some(geometry) = self.layout_entry_cast_geometry(entry_id) else {
            self.request_stop_cast(cast.session_id);
            return false;
        };
        if geometry.output.name() != output.name() {
            return false;
        }

        let entry_size = geometry
            .entry_rect
            .size
            .to_physical_precise_round(output_scale);
        if entry_size.w <= 0 || entry_size.h <= 0 {
            warn!(session_id = %cast.session_id, "layout-entry cast has empty size");
            self.request_stop_cast(cast.session_id);
            return false;
        }

        let refresh = output
            .current_mode()
            .map(|m| (m.refresh / 1000) as u32)
            .unwrap_or(60);

        if let Err(err) = cast.set_refresh(refresh) {
            warn!(session_id = %cast.session_id, "set_refresh failed: {err:?}");
            self.request_stop_cast(cast.session_id);
            return false;
        }
        match cast.ensure_size(entry_size) {
            Ok(CastSizeChange::Ready) => (),
            Ok(CastSizeChange::Pending) => return false,
            Err(err) => {
                warn!(session_id = %cast.session_id, "ensure_size failed: {err:?}");
                self.request_stop_cast(cast.session_id);
                return false;
            }
        }

        if cast.check_time_and_schedule(output, target_frame_time) {
            return false;
        }

        let content_elements = self.render_layout_entry_content_for_screen_cast(
            renderer,
            &geometry,
            output_scale,
            entry_size,
        );
        let (cursor_elements, cursor_location) = self.layout_entry_cursor_for_screen_cast(
            renderer,
            cursor_texture_cache,
            &geometry,
            output_scale,
            cast.cursor_mode.includes_cursor(),
        );

        if cast.dequeue_buffer_and_render(
            renderer,
            &content_elements,
            &cursor_elements,
            cursor_location,
            entry_size,
            output_scale,
        ) {
            cast.last_frame_time = target_frame_time;
            true
        } else {
            false
        }
    }

    /// Render every output cast targeting `output`. Element collection
    /// (`collect_render_elements_for_output`) is lazy — only when at
    /// least one cast is actually about to render — so outputs without
    /// any cast attention pay nothing. Casts that hit an unrecoverable
    /// error self-stop via `PwToCompositor::StopCast`.
    #[allow(clippy::too_many_arguments)]
    pub fn render_for_screen_cast(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        cursor_texture_cache: &CursorTextureCache,
        output_pos: Point<i32, smithay::utils::Logical>,
        output_size: Size<i32, smithay::utils::Logical>,
        output_size_physical: Size<i32, Physical>,
        output_scale: Scale<f64>,
        target_frame_time: Duration,
    ) {
        let mut casts = std::mem::take(&mut self.casting.casts);
        let mut elements_with_cursor: Option<(Vec<EwmRenderElement>, Vec<EwmRenderElement>)> = None;
        let mut elements_without_cursor: Option<(Vec<EwmRenderElement>, Vec<EwmRenderElement>)> =
            None;

        for cast in casts.iter_mut() {
            if !cast.is_active() {
                continue;
            }
            let CastTarget::Output { name } = &cast.target else {
                continue;
            };
            if *name != output.name() {
                continue;
            }

            match cast.ensure_size(output_size_physical) {
                Ok(CastSizeChange::Ready) => (),
                Ok(CastSizeChange::Pending) => {
                    tracing::trace!("cast is resize pending, skipping");
                    continue;
                }
                Err(err) => {
                    warn!(session_id = %cast.session_id, "ensure_size failed: {err:?}");
                    let _ = self
                        .casting
                        .pw_to_compositor
                        .send(PwToCompositor::StopCast {
                            session_id: cast.session_id,
                        });
                    continue;
                }
            }

            if cast.check_time_and_schedule(output, target_frame_time) {
                continue;
            }

            let include_cursor = cast.cursor_mode.includes_cursor();
            let elements = if include_cursor {
                &mut elements_with_cursor
            } else {
                &mut elements_without_cursor
            };
            let (content, cursor) = elements.get_or_insert_with(|| {
                crate::render::collect_render_elements_for_output(
                    self,
                    renderer,
                    output_scale,
                    cursor_texture_cache,
                    output_pos,
                    output_size,
                    include_cursor,
                    true,
                    output,
                )
            });

            let cursor_location = output_cast_cursor_location(
                self.cursor_location(),
                output_pos,
                output_size,
                output_scale,
                include_cursor,
            );

            if cast.dequeue_buffer_and_render(
                renderer,
                content,
                cursor,
                cursor_location,
                output_size_physical,
                output_scale,
            ) {
                cast.last_frame_time = target_frame_time;
            }
        }

        self.casting.casts = casts;
    }

    /// Render every layout-entry cast whose target entry currently lives on
    /// `output`. Casts that hit an unrecoverable error self-stop via
    /// `PwToCompositor::StopCast`.
    pub fn render_layout_entries_for_screen_cast(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        cursor_texture_cache: &CursorTextureCache,
        output_scale: Scale<f64>,
        target_frame_time: Duration,
    ) {
        let mut casts = std::mem::take(&mut self.casting.casts);

        for cast in casts.iter_mut() {
            if !cast.is_active() {
                continue;
            }
            let CastTarget::LayoutEntry { entry_id } = cast.target else {
                continue;
            };
            if self.casting.mapped_cast_output.get(&entry_id) != Some(output) {
                continue;
            }

            self.render_layout_entry_for_screen_cast(
                renderer,
                cast,
                entry_id,
                output,
                cursor_texture_cache,
                output_scale,
                target_frame_time,
            );
        }

        self.casting.casts = casts;
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn output_cast_cursor_location_matches_visibility_and_output_geometry(
            output_x in -2_000i32..=2_000,
            output_y in -2_000i32..=2_000,
            output_w in 1i32..=4_000,
            output_h in 1i32..=4_000,
            local_x in -1_000i32..=5_000,
            local_y in -1_000i32..=5_000,
            scale_milli in 250i32..=4_000,
            include_cursor in any::<bool>(),
        ) {
            let output_pos = Point::from((output_x, output_y));
            let output_size = Size::from((output_w, output_h));
            let output_scale = Scale::from(f64::from(scale_milli) / 1_000.0);
            let pointer_location = (
                f64::from(output_x + local_x),
                f64::from(output_y + local_y),
            );

            let inside_output =
                (0..output_w).contains(&local_x) && (0..output_h).contains(&local_y);
            let expected = if include_cursor && inside_output {
                Point::from((f64::from(local_x), f64::from(local_y)))
                    .to_physical_precise_round(output_scale)
            } else {
                Point::default()
            };

            prop_assert_eq!(
                output_cast_cursor_location(
                    pointer_location,
                    output_pos,
                    output_size,
                    output_scale,
                    include_cursor,
                ),
                expected,
            );
        }
    }
}
