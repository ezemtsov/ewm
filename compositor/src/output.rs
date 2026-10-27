//! Outputs: their arrival, geometry, order and working areas.

use std::collections::HashMap;
use std::mem;
use std::time::Duration;

use smithay::desktop::layer_map_for_output;
use smithay::output::Output;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::utils::{Logical, Point, Rectangle, Size};
use tracing::{info, warn};

use crate::handlers::configure_lock_surface;
use crate::{
    Event, Ewm, FocusSource, Monitor, Notify, OutputInfo, RedrawState, State, backend, module,
    output_mode, protocols,
};
#[cfg(feature = "screencast")]
use crate::{dbus, screencasting};

impl Ewm {
    pub(crate) fn output_mapped(&self, output: &Output) -> bool {
        self.layout.monitor_of(output).is_some()
    }

    /// Convert a `WlOutput` to an `Output` that is mapped.
    pub(crate) fn output_from_resource(&self, wl_output: &WlOutput) -> Option<Output> {
        Output::from_resource(wl_output).filter(|output| self.output_mapped(output))
    }

    /// Find a connected output by name, whether or not it is currently mapped.
    pub(crate) fn connected_output(&self, name: &str) -> Option<Output> {
        self.output_state
            .keys()
            .find(|output| output.name() == name)
            .cloned()
    }

    /// Unmap a connected output while keeping its head available for
    /// output-management so it can be re-enabled later.
    pub(crate) fn disable_output(&mut self, output: &Output) {
        if !self.output_mapped(output) {
            return;
        }

        self.detach_output(output);
        self.queue_redraw_all();
    }

    /// Drop every record of a removed output once the backend has torn it down.
    pub fn remove_output(&mut self, output: &Output) {
        let output_name = output.name();

        // The output goes first, then the frames it took with it.
        self.queue_event(Event::OutputDisconnected {
            name: output_name.clone(),
        });
        self.detach_output(output);

        // Cancel pending estimated-VBlank timers
        if let Some(output_state) = self.output_state.remove(output)
            && let RedrawState::WaitingForEstimatedVBlank(token)
            | RedrawState::WaitingForEstimatedVBlankAndQueued(token) = output_state.redraw_state
        {
            self.loop_handle.remove(token);
        }

        self.check_lock_on_output_removed();

        info!("Output removed: {}", output_name);
    }

    /// Stop the screen casts of an output that left the space.
    #[cfg(feature = "screencast")]
    pub(crate) fn stop_output_casts(&mut self, output: &Output) {
        let output_name = output.name();
        let target = dbus::CastTarget::Output {
            name: output_name.clone(),
        };
        let sessions_to_stop: Vec<screencasting::CastSessionId> = self
            .casting
            .casts
            .iter()
            .filter(|cast| cast.target == target)
            .map(|cast| cast.session_id)
            .collect();
        for session_id in sessions_to_stop {
            info!(output = %output_name, %session_id, "stopping cast, its output left");
            self.stop_cast(session_id);
        }
    }

    /// Withdraw the wl_output global now and drop it once clients had time to notice.
    pub(crate) fn retire_output_global(&mut self, output: &Output) {
        let Some(global) = self.output_state.get_mut(output).unwrap().global.take() else {
            return;
        };
        self.display_handle.disable_global::<State>(global.clone());
        self.loop_handle
            .insert_source(
                Timer::from_duration(Duration::from_secs(10)),
                move |_, _, state| {
                    state
                        .ewm
                        .display_handle
                        .remove_global::<State>(global.clone());
                    TimeoutAction::Drop
                },
            )
            .unwrap();
    }

    /// Build a snapshot of all output states for the output management protocol.
    pub fn build_output_head_states(
        &self,
        output_infos: &[OutputInfo],
    ) -> HashMap<String, protocols::output_management::OutputHeadState> {
        use protocols::output_management::{OutputHeadState, OutputModeState};

        let mut states = HashMap::new();
        for info in output_infos {
            let output = self.layout.output(&info.name);
            let geo = output.and_then(|o| self.layout.output_rect(o));

            let enabled = self
                .output_config
                .get(&info.name)
                .map(|c| c.enabled)
                .unwrap_or(true);

            // Find current mode index
            let current_mode = if enabled {
                output.and_then(|o| {
                    let current = o.current_mode()?;
                    info.modes.iter().position(|m| {
                        m.width == current.size.w
                            && m.height == current.size.h
                            && m.refresh == current.refresh
                    })
                })
            } else {
                None
            };

            let head = OutputHeadState {
                name: info.name.clone(),
                make: info.make.clone(),
                model: info.model.clone(),
                serial_number: None,
                physical_size: if info.width_mm > 0 || info.height_mm > 0 {
                    Some((info.width_mm, info.height_mm))
                } else {
                    None
                },
                enabled,
                modes: info
                    .modes
                    .iter()
                    .map(|m| OutputModeState {
                        width: m.width,
                        height: m.height,
                        refresh: m.refresh,
                        preferred: m.preferred,
                    })
                    .collect(),
                current_mode,
                position: geo.map(|g| (g.loc.x, g.loc.y)),
                scale: Some(info.scale),
                transform: Some(backend::int_to_transform(info.transform)),
            };
            states.insert(info.name.clone(), head);
        }
        states
    }

    pub(crate) fn pointer_is_on_output(&self, output: &Output) -> bool {
        let pos: Point<f64, Logical> = self.cursor_location().into();
        self.space
            .output_under(pos)
            .any(|candidate| candidate == output)
    }

    /// Refresh the output management protocol once heads changed, from redraw so no Dispatch
    /// handler is still running.
    pub fn refresh_output_management(&mut self, output_infos: &[OutputInfo]) {
        if !self.output_management_state.output_heads_changed {
            return;
        }
        self.output_management_state.output_heads_changed = false;
        let new_state = self.build_output_head_states(output_infos);
        self.output_management_state.notify_changes(new_state);
    }

    /// The monitor showing surface `id`: its focused view's, else the first in output order
    /// among those the surface is on.
    pub(crate) fn surface_monitor(&self, id: u64) -> Option<&Monitor> {
        if let Some(location) = self.focused_entry_for_surface(id) {
            return Some(location.monitor);
        }
        let outputs = &self.surfaces.get(&id)?.outputs;
        self.mapped_monitors()
            .find(|monitor| outputs.contains(monitor.name()))
    }

    /// The output showing surface `id`, owned.
    pub(crate) fn find_surface_output(&self, id: u64) -> Option<Output> {
        self.surface_monitor(id)
            .map(|monitor| monitor.output().clone())
    }

    /// Find the Output at a global logical position.
    pub(crate) fn output_at(&self, pos: Point<f64, Logical>) -> Option<&Output> {
        self.space
            .output_under(pos)
            .next()
            .or_else(|| self.first_output())
    }

    pub(crate) fn first_output(&self) -> Option<&Output> {
        self.sorted_outputs.first()
    }

    /// Find the Output whose geometry strictly contains `pos`.
    ///
    /// Unlike [`Self::output_at`], returns `None` when `pos` lies in dead
    /// space between or outside outputs instead of falling back to an
    /// arbitrary output.
    pub(crate) fn output_containing(&self, pos: Point<f64, Logical>) -> Option<&Output> {
        self.space.output_under(pos).next()
    }

    /// Clamp a pointer target into the union of output geometries.
    ///
    /// If `target` already lies on an output, returns it unchanged.
    /// Otherwise clips the movement against the geometry of the output
    /// containing `previous` (the pointer's prior position), so the
    /// pointer can never wander into dead space between outputs.
    /// Falls back to the centre of the first output if neither point is
    /// on any output.
    pub(crate) fn clamp_pointer_to_outputs(
        &self,
        target: Point<f64, Logical>,
        previous: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        if self.output_containing(target).is_some() {
            return target;
        }
        if let Some(output) = self.output_containing(previous) {
            let geom = self.layout.output_rect(output).unwrap();
            let x = target
                .x
                .clamp(geom.loc.x as f64, (geom.loc.x + geom.size.w - 1) as f64);
            let y = target
                .y
                .clamp(geom.loc.y as f64, (geom.loc.y + geom.size.h - 1) as f64);
            return smithay::utils::Point::from((x, y));
        }
        let Some(output) = self.first_output() else {
            return target;
        };
        let Some(geom) = self.layout.output_rect(output) else {
            return target;
        };
        crate::utils::center(geom).to_f64()
    }

    /// The output showing the focused frame.
    pub(crate) fn focused_output(&self) -> Option<&Output> {
        self.layout.focused_output()
    }

    pub(crate) fn output_under_cursor(&self) -> Option<&Output> {
        self.output_containing(self.cursor_location().into())
    }

    /// Output for surface placement and keyboard nav: focused, else under the cursor, else first.
    pub fn active_output(&self) -> Option<&Output> {
        self.focused_output()
            .or_else(|| self.output_under_cursor())
            .or_else(|| self.first_output())
    }

    /// The mapped output called `name`, owned.
    pub(crate) fn find_mapped_output(&self, name: &str) -> Option<Output> {
        self.layout.output(name).cloned()
    }

    /// Output adjacent to `current` along one axis. Candidates must overlap
    /// the perpendicular band and the closest center in the requested
    /// direction wins.
    fn output_in_direction(&self, current: &Output, dx: i32, dy: i32) -> Option<Output> {
        let cur = self.layout.output_rect(current)?;
        let horizontal = match (dx.signum(), dy.signum()) {
            (-1 | 1, 0) => true,
            (0, -1 | 1) => false,
            _ => return None,
        };
        let sign = if horizontal { dx.signum() } else { dy.signum() };
        let band = if horizontal {
            Rectangle::new(
                smithay::utils::Point::from((i32::MIN / 2, cur.loc.y)),
                Size::from((i32::MAX, cur.size.h)),
            )
        } else {
            Rectangle::new(
                smithay::utils::Point::from((cur.loc.x, i32::MIN / 2)),
                Size::from((cur.size.w, i32::MAX)),
            )
        };
        let cur_center = crate::utils::center(cur);

        self.space
            .outputs()
            .filter_map(|o| self.layout.output_rect(o).map(|g| (o, g)))
            .filter_map(|(output, geo)| {
                if !geo.overlaps(band) {
                    return None;
                }
                let center = crate::utils::center(geo);
                let distance = if horizontal {
                    i64::from(sign) * (i64::from(center.x) - i64::from(cur_center.x))
                } else {
                    i64::from(sign) * (i64::from(center.y) - i64::from(cur_center.y))
                };
                (distance > 0).then_some((output, distance))
            })
            .min_by_key(|(_, distance)| *distance)
            .map(|(o, _)| o.clone())
    }

    /// Move the keyboard to the output spatially adjacent to the current one, onto the frame that
    /// last held it there. `dx` is -1 or 1 for left or right, `dy` for up or down, mixed axes and
    /// zero vectors a no-op.
    pub fn focus_output_direction(&mut self, dx: i32, dy: i32) {
        let Some(cur) = self.active_output().cloned() else {
            return;
        };
        let target = self.output_in_direction(&cur, dx, dy);
        let Some(target) = target else { return };
        let Some(id) = self.layout.focused_frame_on(&target) else {
            return;
        };
        self.set_focus_frame(id, FocusSource::FocusOutputDirection, Notify::Emacs);
    }

    /// The output after `current` in order, wrapping around.
    pub(crate) fn output_next_of(&self, current: &Output) -> Option<Output> {
        let outputs = &self.sorted_outputs;
        let idx = outputs.iter().position(|output| output == current)?;
        outputs.get((idx + 1) % outputs.len()).cloned()
    }

    /// Move the focused frame to the next output in order, wrapping around.
    pub fn move_frame_to_next_output(&mut self) {
        if let Some((id, next)) = self.focused_frame_id().zip(
            self.focused_output()
                .and_then(|cur| self.output_next_of(cur)),
        ) {
            self.move_frame_to_output(id, &next);
        }
    }

    /// Pack the mapped outputs: configured positions first, the rest left to right by identity.
    /// `reported` is the output whose new geometry the caller announces itself.
    pub(crate) fn reposition_outputs(&mut self, reported: Option<&Output>) {
        struct Data {
            output: Output,
            config: Option<Point<i32, Logical>>,
        }

        let mut outputs: Vec<Data> = self
            .space
            .outputs()
            .map(|output| Data {
                config: self
                    .output_config
                    .get(&output.name())
                    .and_then(|config| config.position)
                    .map(Point::from),
                output: output.clone(),
            })
            .collect();

        // Configured outputs first, then by identity so the order never depends on connector
        // enumeration, displays without an identity last.
        outputs.sort_by_cached_key(|data| {
            let fields = crate::utils::output_identity_fields(&data.output);
            (
                data.config.is_none(),
                fields.iter().all(Option::is_none),
                fields,
                data.output.name(),
            )
        });

        let mut placed: Vec<(Output, Rectangle<i32, Logical>)> = Vec::with_capacity(outputs.len());
        let (mut right, mut bottom) = (0, 0);
        for data in outputs {
            let size = self
                .layout
                .output_rect(&data.output)
                .unwrap_or_default()
                .size;
            let position = data
                .config
                .filter(|&loc| {
                    let Some((_, overlap)) = placed
                        .iter()
                        .find(|(_, geo)| geo.overlaps(Rectangle::new(loc, size)))
                    else {
                        return true;
                    };
                    warn!(
                        "output {} at x={} y={} sized {}x{} \
                         overlaps an existing output at x={} y={} sized {}x{}, \
                         falling back to automatic placement",
                        data.output.name(),
                        loc.x,
                        loc.y,
                        size.w,
                        size.h,
                        overlap.loc.x,
                        overlap.loc.y,
                        overlap.size.w,
                        overlap.size.h,
                    );
                    false
                })
                .unwrap_or_else(|| Point::from((right, 0)));
            right = right.max(position.x + size.w);
            bottom = bottom.max(position.y + size.h);
            if data.output.current_location() != position {
                self.space.map_output(&data.output, position);
                data.output
                    .change_current_state(None, None, None, Some(position));
                self.output_management_state.output_heads_changed = true;
                if reported != Some(&data.output) {
                    let event = self.output_config_event(&data.output);
                    self.queue_event(event);
                    self.queue_redraw(&data.output);
                }
            }
            placed.push((data.output, Rectangle::new(position, size)));
        }

        placed.sort_by_cached_key(|(output, geo)| (geo.loc.x, geo.loc.y, output.name()));
        self.sorted_outputs = placed.into_iter().map(|(output, _)| output).collect();
        self.layout.sort_monitors(&self.sorted_outputs);
        self.output_size = Size::from((right, bottom));
    }

    /// The geometry Emacs is told when `output` changes or moves.
    fn output_config_event(&self, output: &Output) -> Event {
        let mode = output.current_mode().unwrap_or(smithay::output::Mode {
            size: (0, 0).into(),
            refresh: 0,
        });
        let geo = self.layout.output_rect(output).unwrap_or_default();
        Event::OutputConfigChanged {
            name: output.name(),
            width: mode.size.w,
            height: mode.size.h,
            refresh: mode.refresh,
            x: geo.loc.x,
            y: geo.loc.y,
            scale: output.current_scale().fractional_scale(),
            transform: backend::transform_to_int(output.current_transform()),
        }
    }

    pub(crate) fn active_outputs_snapshot(&self) -> Vec<module::ActiveOutput> {
        self.mapped_monitors()
            .filter_map(|monitor| {
                let geo = monitor.rect()?;
                let working_area = monitor.working_area;
                Some(module::ActiveOutput {
                    name: monitor.name().to_string(),
                    origin: (
                        geo.loc.x + working_area.loc.x,
                        geo.loc.y + working_area.loc.y,
                    ),
                })
            })
            .collect()
    }

    /// Map `output`, pack the layout around it, give it a monitor and advertise it to clients.
    pub(crate) fn map_output(&mut self, output: &Output) {
        self.space.map_output(output, output.current_location());
        let working_area = self.get_working_area(output);
        self.layout.add_output(output.clone(), working_area);
        self.reposition_outputs(Some(output));
        self.output_state
            .get_mut(output)
            .unwrap()
            .global
            .get_or_insert_with(|| output.create_global::<State>(&self.display_handle));
    }

    /// Register an output the backend has set up and mapped.
    pub fn add_output(&mut self, output: &Output, info: OutputInfo) {
        self.refresh_cursor_outputs();

        self.queue_event(Event::OutputDetected(info));

        if self.output_mapped(output) {
            self.attach_output(output);
            self.introspect_windows_dirty = true;
        }
        self.output_management_state.output_heads_changed = true;
    }

    /// Follow a new mode, scale, transform or position the backend applied to `output`.
    pub fn output_config_changed(&mut self, output: &Output, became_mapped: bool) {
        let current_scale = output.current_scale().fractional_scale();
        self.reposition_outputs(Some(output));
        if let Some(strip) = self.layout.strip_mut(output) {
            strip.update_config(current_scale);
        }

        // Notify existing surfaces of scale/transform change
        self.send_scale_transform_to_output_surfaces(output);
        self.refresh_cursor_outputs();

        // Resize lock buffer and reconfigure lock surface for new output size
        let output_size = crate::utils::output_size(output);
        let is_locked = self.is_locked();
        if let Some(state) = self.output_state.get_mut(output) {
            state.resize_lock_buffer((output_size.w as i32, output_size.h as i32));
            state.resize_backdrops((output_size.w as i32, output_size.h as i32));
            if is_locked && let Some(lock_surface) = &state.lock_surface {
                configure_lock_surface(lock_surface, output);
            }
        }

        // Notify output screen casts of size change
        #[cfg(feature = "screencast")]
        if let Some(current_mode) = output.current_mode() {
            let physical_size = Size::from((current_mode.size.w, current_mode.size.h));
            let refresh = (current_mode.refresh / 1000) as u32;
            let target = dbus::CastTarget::Output {
                name: output.name(),
            };
            for cast in self.casting.casts.iter_mut() {
                if cast.target == target {
                    if let Err(err) = cast.set_refresh(refresh) {
                        warn!(session_id = %cast.session_id, "set_refresh failed: {err:?}");
                    }
                    if let Err(err) = cast.ensure_size(physical_size) {
                        warn!(session_id = %cast.session_id, "ensure_size failed: {err:?}");
                    }
                }
            }
        }

        let working_area_changed = self.check_working_area_change(output);
        if !working_area_changed {
            // Always refresh Emacs frames: the output position may have changed
            // even when the working area (relative to the output) didn't.
            self.update_frames_for_working_area(output);
        }
        self.refresh_primary_assignments();
        self.queue_redraw_all();
        if became_mapped {
            self.attach_output(output);
        }

        let event = self.output_config_event(output);
        self.queue_event(event);
        // Enabled by config, nothing else asks Emacs for the frames it lacks.
        if became_mapped {
            self.queue_event(Event::OutputsComplete);
        }

        self.output_management_state.output_heads_changed = true;
    }

    /// Send `output`'s scale and transform to every window and layer surface on it.
    pub fn send_scale_transform_to_output_surfaces(&self, output: &Output) {
        let scale = output.current_scale();
        let transform = output.current_transform();

        let name = output.name();
        for surface in self.surfaces.values() {
            if surface.outputs.contains(&name) {
                crate::utils::set_preferred_scale_transform(&surface.window, output);
            }
        }

        // Notify layer surfaces on this output
        let layer_map = layer_map_for_output(output);
        for layer in layer_map.layers() {
            layer.with_surfaces(|surface, data| {
                crate::utils::send_scale_transform(surface, data, scale, transform);
            });
        }
    }

    /// Get the working area for an output (non-exclusive zone from layer surfaces).
    /// This is the area available for Emacs frames after panels reserve their space.
    pub fn get_working_area(&self, output: &Output) -> Rectangle<i32, smithay::utils::Logical> {
        let map = layer_map_for_output(output);
        map.non_exclusive_zone()
    }

    /// Update Emacs frames to fit within the working area of an output.
    /// Called when layer surface exclusive zones change.
    pub fn update_frames_for_working_area(&mut self, output: &Output) {
        let working_area = self.get_working_area(output);

        for id in self.layout.strip_frame_ids(output) {
            let Some(window) = self.window(id) else {
                continue;
            };
            if let Some(toplevel) = window.toplevel() {
                let new_size = Some(working_area.size);
                let changed = toplevel.with_pending_state(|state| {
                    let changed = state.size != new_size;
                    state.size = new_size;
                    state.bounds = new_size;
                    changed
                });
                if changed {
                    toplevel.send_configure();
                }
            }
        }

        if let Some(space) = self.layout.floating_mut(output) {
            space.update_config(working_area.to_f64());
        }

        self.queue_redraw(output);
    }

    /// Check and update the working area of an output.
    ///
    /// Returns true when the working area changed and Emacs frames were resized.
    pub fn check_working_area_change(&mut self, output: &Output) -> bool {
        // Re-arrange layer map so it picks up any scale/mode/transform change
        layer_map_for_output(output).arrange();
        let working_area = self.get_working_area(output);

        let Some(monitor) = self.layout.monitor_of_mut(output) else {
            return false;
        };
        let changed = mem::replace(&mut monitor.working_area, working_area) != working_area;
        monitor.set_view(working_area);
        monitor.resized |= changed;

        if changed {
            info!(
                "Working area for {} changed: {}x{}+{}+{}",
                output.name(),
                working_area.size.w,
                working_area.size.h,
                working_area.loc.x,
                working_area.loc.y
            );

            // Update Emacs frames to fit new working area
            self.update_frames_for_working_area(output);
        }

        changed
    }

    /// Get working areas as serializable structs for state dump.
    pub fn get_working_areas_info(&self) -> Vec<crate::event::WorkingAreaInfo> {
        self.layout
            .monitors()
            .iter()
            .map(|monitor| {
                let rect = monitor.working_area;
                crate::event::WorkingAreaInfo {
                    output: monitor.name().to_string(),
                    x: rect.loc.x,
                    y: rect.loc.y,
                    width: rect.size.w,
                    height: rect.size.h,
                }
            })
            .collect()
    }
}

impl State {
    /// Merge `config` into the stored one for output `name` and apply it.
    pub(crate) fn configure_output(&mut self, name: String, config: module::OutputConfig) {
        let module::OutputConfig {
            x,
            y,
            width,
            height,
            refresh,
            custom,
            modeline,
            scale,
            transform,
            enabled,
        } = config;
        let current_output = self.ewm.find_mapped_output(&name);
        let current_mode = current_output
            .as_ref()
            .and_then(|output| output.current_mode());
        let current_pos = current_output
            .as_ref()
            .and_then(|output| self.ewm.layout.output_rect(output))
            .map(|geo| (geo.loc.x, geo.loc.y))
            .unwrap_or((0, 0));

        // Update stored config (merge with existing)
        let config = self.ewm.output_config.entry(name.clone()).or_default();
        if width.is_some() || height.is_some() || refresh.is_some() {
            // Fall back to current mode for unspecified parameters
            let w = width.unwrap_or_else(|| current_mode.map(|mode| mode.size.w).unwrap_or(1920));
            let h = height.unwrap_or_else(|| current_mode.map(|mode| mode.size.h).unwrap_or(1080));
            config.mode = Some(output_mode::Mode {
                custom,
                mode: output_mode::ConfiguredMode {
                    width: w as u16,
                    height: h as u16,
                    refresh,
                },
            });
        }
        if modeline.is_some() {
            config.modeline = modeline;
        }
        if x.is_some() || y.is_some() {
            config.position = Some((x.unwrap_or(current_pos.0), y.unwrap_or(current_pos.1)));
        }
        if let Some(s) = scale {
            config.scale = Some(s);
        }
        if let Some(t) = transform {
            config.transform = Some(backend::int_to_transform(t));
        }
        if let Some(e) = enabled {
            config.enabled = e;
        }

        // Apply the config (adjusts pointer if geometry changed)
        self.apply_output_config_for(&name);
    }
}
