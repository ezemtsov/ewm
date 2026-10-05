use std::time::Duration;

use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
    add_pre_commit_hook, is_sync_subsurface, with_states,
};
use smithay::wayland::shm::{ShmHandler, ShmState};

use crate::{
    Event, OPEN_ANIM_DURATION, State, animation, client_state, compute_open_floating,
    natural_open_size, surface_root,
};

/// Bake an Emacs frame's close snapshot on the commit that unmaps it.
pub(crate) fn add_frame_unmap_pre_commit_hook(surface: &WlSurface) {
    add_pre_commit_hook::<State, _>(surface, |state, _dh, surface| {
        let unmapping = with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            matches!(guard.pending().buffer, Some(BufferAssignment::Removed))
        });
        if unmapping && let Some(id) = state.ewm.surface_id(surface) {
            state.store_close_snapshots(&[id]);
        }
    });
}

// Buffer handling
impl BufferHandler for State {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

// Compositor protocol
impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.ewm.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client_state(client).compositor
    }

    fn new_subsurface(&mut self, surface: &WlSurface, parent: &WlSurface) {
        let root = surface_root(parent);
        if let Some(output) = self.ewm.output_for_root(&root) {
            let scale = output.current_scale();
            let transform = output.current_transform();
            smithay::wayland::compositor::with_states(surface, |data| {
                crate::utils::send_scale_transform(surface, data, scale, transform);
            });
        }
    }

    fn commit(&mut self, surface: &WlSurface) {
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // Queue early import for DRM backend (processed in main loop)
        self.ewm.pending_early_imports.push(surface.clone());

        // Cursor surface: apply role buffer offset to the hotspot and redraw.
        let root = surface_root(surface);
        if matches!(
            self.ewm.cursor_manager.cursor_image(),
            CursorImageStatus::Surface(cursor_surface) if cursor_surface == &root
        ) {
            if surface == &root {
                smithay::wayland::compositor::with_states(surface, |states| {
                    let cursor_image_attributes = states.data_map.get::<CursorImageSurfaceData>();

                    if let Some(mut cursor_image_attributes) =
                        cursor_image_attributes.map(|attrs| attrs.lock().unwrap())
                    {
                        let buffer_delta = states
                            .cached_state
                            .get::<smithay::wayland::compositor::SurfaceAttributes>()
                            .current()
                            .buffer_delta
                            .take();
                        if let Some(buffer_delta) = buffer_delta {
                            cursor_image_attributes.hotspot -= buffer_delta;
                        }
                    }
                });
            }
            self.ewm.queue_redraw_all();
            return;
        }

        // DnD icon surface: update offset from buffer delta and redraw.
        if let Some(icon) = &self.ewm.dnd_icon
            && icon.surface == root
        {
            if surface == &icon.surface {
                let dnd_icon = self.ewm.dnd_icon.as_mut().unwrap();
                smithay::wayland::compositor::with_states(&dnd_icon.surface, |states| {
                    let delta = states
                        .cached_state
                        .get::<smithay::wayland::compositor::SurfaceAttributes>()
                        .current()
                        .buffer_delta
                        .take()
                        .unwrap_or_default();
                    dnd_icon.offset += delta;
                });
            }
            self.ewm.queue_redraw_all();
            return;
        }

        // Surface type dispatch order:
        // 1. Layer surfaces  2. Popups  3. Windows  4. Lock surfaces
        // When adding new surface types, add a branch here.

        // 1. Handle layer surface commits
        if self.ewm.handle_layer_surface_commit(surface) {
            return;
        }

        // 2. Handle popup commits
        if self.ewm.handle_popup_commit(surface) {
            return;
        }

        // Early return for sync subsurfaces - parent commit will handle them
        if is_sync_subsurface(surface) {
            return;
        }

        let root_surface = surface_root(surface);

        // 3. Find the window that owns this root surface
        if let Some(id) = self.ewm.surface_id(&root_surface) {
            if let Some(window) = self.ewm.id_windows.get(&id) {
                window.on_commit();
            }
            // Title before Mapped, so placement rules see a title set on the mapping commit.
            self.ewm.check_surface_info_changes(surface);

            // unmapped->mapped transition: seed the open-fade once. The
            // 16ms delay holds alpha at 0 for one vblank so the empty/white
            // initial buffer some clients ship is rendered invisibly.
            if crate::utils::is_mapped(&root_surface) && self.ewm.unmapped_surfaces.remove(&id) {
                self.ewm.surface_open_anims.insert(
                    id,
                    animation::Animation::ease(
                        self.ewm.animations_clock.clone(),
                        0.0,
                        1.0,
                        OPEN_ANIM_DURATION,
                    )
                    .delayed(Duration::from_millis(16)),
                );

                // Placement is final at map: min/max hints are known, so decide once.
                let (open_floating, parent) = self
                    .ewm
                    .id_windows
                    .get(&id)
                    .and_then(|window| window.toplevel())
                    .map(|toplevel| (compute_open_floating(toplevel), toplevel.parent()))
                    .unwrap_or((false, None));
                let parent_id = parent.and_then(|wl| self.ewm.surface_id(&wl));
                let output = self.ewm.active_output();
                let frame_surface_id = self
                    .ewm
                    .resolve_app_host_frame(parent_id, output.as_deref());
                // Size hint for floating placement, dropped when it fills the working area.
                let working_area = output
                    .as_deref()
                    .and_then(|name| self.ewm.mapped_output_by_name(name))
                    .map(|o| self.ewm.get_working_area(o));
                let (width, height) = self
                    .ewm
                    .id_windows
                    .get(&id)
                    .and_then(natural_open_size)
                    .filter(|s| working_area.is_none_or(|wa| s.w < wa.size.w && s.h < wa.size.h))
                    .map(|s| (s.w as f64, s.h as f64))
                    .unzip();
                self.ewm.queue_event(Event::Mapped {
                    id,
                    open_floating,
                    output,
                    frame_surface_id,
                    width,
                    height,
                });
            }

            self.ewm.queue_redraw_for_surface(id);
            return;
        }

        // 4. Queue redraw for lock surface commits.
        if self.ewm.is_locked() {
            let output = self
                .ewm
                .output_state
                .iter()
                .find(|(_, state)| {
                    state
                        .lock_surface
                        .as_ref()
                        .is_some_and(|ls| ls.wl_surface() == &root_surface)
                })
                .map(|(o, _)| o.clone());
            if let Some(output) = output {
                self.ewm.queue_redraw(&output);
            }
        }
    }
}

// Shared memory
impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.ewm.shm_state
    }
}
