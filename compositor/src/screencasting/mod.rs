//! Ported from niri's `src/screencasting/`. Implements screen casting
//! via the org.gnome.Mutter.ScreenCast D-Bus interface.

pub mod pw_utils;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context as _;
pub use pw_utils::{Cast, CastSessionId, CastSizeChange, CastStreamId, PipeWire, PwToCompositor};
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use smithay::reexports::calloop::channel::{self, Sender};
use smithay::reexports::calloop::LoopHandle;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size};
use smithay::wayland::seat::WaylandFocus;
use tracing::{debug, info, warn};

use crate::cursor::CursorTextureCache;
use crate::dbus::{self, CastTarget, ScreenCastToCompositor};
use crate::render::{render_cursor_elements, EwmRenderElement};
use crate::{Ewm, State};

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

pub struct Screencasting {
    pub casts: Vec<Cast>,

    /// Persistent sender given to every `PipeWire` instance. Outlives
    /// any single connection so the calloop source on the receiver side
    /// stays registered across reinits after `FatalError`.
    pub pw_to_compositor: Sender<PwToCompositor>,

    /// Cached map: window id -> primary output. Populated in
    /// `apply_output_layout()`.
    pub mapped_cast_output: HashMap<u64, Output>,

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
                let output = match &cast.target {
                    CastTarget::Output { name } => self.ewm.find_mapped_output(name),
                    CastTarget::Window { id } => {
                        self.ewm.casting.mapped_cast_output.get(id).cloned()
                    }
                };
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

                let alpha = matches!(target, CastTarget::Window { .. });

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
                    CastTarget::Window { id } => {
                        let Some(window) = self.ewm.id_windows.get(id) else {
                            warn!("StartCast: window {} not found", id);
                            self.ewm.stop_cast(session_id);
                            return;
                        };
                        let output = self.ewm.casting.mapped_cast_output.get(id);
                        let scale = output
                            .map(|o| Scale::from(o.current_scale().fractional_scale()))
                            .unwrap_or(Scale::from(1.0));
                        let bbox = window.bbox_with_popups().to_physical_precise_up(scale);
                        let refresh = output
                            .and_then(|o| o.current_mode())
                            .map(|m| (m.refresh / 1000) as u32)
                            .unwrap_or(60);
                        Some((bbox.size, refresh))
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

        if let Some(ref dbus) = self.dbus_servers {
            if let Some(ref conn) = dbus.conn_screen_cast {
                let server = conn.object_server();
                let path = format!("/org/gnome/Mutter/ScreenCast/Session/u{}", session_id);
                if let Ok(iface) = server.interface::<_, dbus::screen_cast::Session>(path.as_str())
                {
                    async_io::block_on(async {
                        let signal_emitter = iface.signal_emitter().clone();
                        iface.get().stop(server.inner(), signal_emitter).await
                    });
                }
            }
        }
    }

    /// Stop all screen casts targeting a specific window (called when the
    /// window closes).
    pub fn stop_casts_for_window(&mut self, window_id: u64) {
        let target = CastTarget::Window { id: window_id };
        let sessions_to_stop: Vec<CastSessionId> = self
            .casting
            .casts
            .iter()
            .filter(|c| c.target == target)
            .map(|c| c.session_id)
            .collect();
        for session_id in sessions_to_stop {
            info!(%session_id, window_id, "stopping window cast (window closed)");
            self.stop_cast(session_id);
        }
    }

    /// Render a window into a screencast buffer.
    ///
    /// Returns true if a frame was rendered. The `cast` is passed
    /// separately because `casting.casts` is detached from `self`
    /// during the render loop.
    #[allow(clippy::too_many_arguments)]
    pub fn render_window_for_screen_cast(
        &self,
        renderer: &mut GlesRenderer,
        cast: &mut Cast,
        window_id: u64,
        output: &Output,
        cursor_texture_cache: &CursorTextureCache,
        output_scale: Scale<f64>,
        target_frame_time: Duration,
    ) -> bool {
        let Some(window) = self.id_windows.get(&window_id) else {
            return false;
        };

        // Use bbox_with_popups for accurate size including popups, scaled
        // to physical pixels via the output's fractional scale.
        let bbox = window
            .bbox_with_popups()
            .to_physical_precise_up(output_scale);
        let window_size = bbox.size;

        let refresh = output
            .current_mode()
            .map(|m| (m.refresh / 1000) as u32)
            .unwrap_or(60);

        if let Err(err) = cast.set_refresh(refresh) {
            warn!(session_id = %cast.session_id, "set_refresh failed: {err:?}");
            let _ = self
                .casting
                .pw_to_compositor
                .send(PwToCompositor::StopCast {
                    session_id: cast.session_id,
                });
            return false;
        }
        match cast.ensure_size(window_size) {
            Ok(CastSizeChange::Ready) => (),
            Ok(CastSizeChange::Pending) => return false,
            Err(err) => {
                warn!(session_id = %cast.session_id, "ensure_size failed: {err:?}");
                let _ = self
                    .casting
                    .pw_to_compositor
                    .send(PwToCompositor::StopCast {
                        session_id: cast.session_id,
                    });
                return false;
            }
        }

        if cast.check_time_and_schedule(output, target_frame_time) {
            return false;
        }

        // bbox.loc is the offset of the bounding box from the window origin;
        // negate it so popups extending beyond the window are visible.
        let render_offset = Point::from((-bbox.loc.x, -bbox.loc.y));

        let window_elements: Vec<EwmRenderElement> = if let Some(surface) = window.wl_surface() {
            let elems: Vec<_> = render_elements_from_surface_tree(
                renderer,
                &surface,
                render_offset,
                output_scale,
                1.0,
                Kind::Unspecified,
            );
            elems.into_iter().map(EwmRenderElement::Surface).collect()
        } else {
            Vec::new()
        };

        let mut cursor_elements: Vec<EwmRenderElement> = Vec::new();
        let mut cursor_location = Point::<i32, Physical>::from((0, 0));

        if cast.cursor_mode.includes_cursor() {
            let (px, py) = self.pointer_location();
            let output_geo = self.space.output_geometry(output);

            if let Some(output_geo) = output_geo {
                if let Some(strip) = self.output_strips.get(&output.name()) {
                    let working_area = self.get_working_area(output);
                    for entry in strip.surface_entries() {
                        if entry.surface_id() != Some(window_id) {
                            continue;
                        }
                        let entry_x = output_geo.loc.x + working_area.loc.x + entry.x;
                        let entry_y = output_geo.loc.y + working_area.loc.y + entry.y;

                        if px >= entry_x as f64
                            && py >= entry_y as f64
                            && px < (entry_x + entry.w as i32) as f64
                            && py < (entry_y + entry.h as i32) as f64
                        {
                            let bbox_logical = bbox.loc.to_f64().to_logical(output_scale);
                            // cursor_location for SPA_META_Cursor (without hotspot offset)
                            let loc_x = px - entry_x as f64 - bbox_logical.x;
                            let loc_y = py - entry_y as f64 - bbox_logical.y;
                            let pointer_pos = Point::from((loc_x, loc_y));
                            cursor_location = pointer_pos.to_physical_precise_round(output_scale);
                            cursor_elements.extend(render_cursor_elements(
                                self,
                                renderer,
                                cursor_texture_cache,
                                output,
                                output_scale,
                                pointer_pos,
                            ));
                        }
                        break;
                    }
                }
            }
        }

        if cast.dequeue_buffer_and_render(
            renderer,
            &window_elements,
            &cursor_elements,
            cursor_location,
            window_size,
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
                self.pointer_location(),
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

    /// Render every window cast whose target window currently lives on
    /// `output`. Casts that hit an unrecoverable error self-stop via
    /// `PwToCompositor::StopCast`.
    pub fn render_windows_for_screen_cast(
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
            let CastTarget::Window { id } = cast.target else {
                continue;
            };
            if self.casting.mapped_cast_output.get(&id) != Some(output) {
                continue;
            }

            self.render_window_for_screen_cast(
                renderer,
                cast,
                id,
                output,
                cursor_texture_cache,
                output_scale,
                target_frame_time,
            );
        }

        self.casting.casts = casts;
    }
}
