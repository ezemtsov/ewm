use smithay::desktop::Window;
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::output::Output;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Size};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, is_sync_subsurface, with_states,
};
use smithay::wayland::shell::xdg::{SurfaceCachedState, ToplevelSurface};
use smithay::wayland::shm::{ShmHandler, ShmState};

use crate::utils::ensure_min_max_size;
use crate::{Event, Ewm, LockState, State, client_state, is_portal_surface, surface_root};

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

    fn destroyed(&mut self, surface: &WlSurface) {
        // A disconnecting client can lose its surface before its toplevel.
        if let Some(id) = self.ewm.surface_id(surface) {
            self.store_close_snapshots(&[id]);
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
            if let Some(window) = self.ewm.window(id) {
                window.on_commit();
            }
            // Views take the committed size and fullscreen state.
            let flipped = self
                .ewm
                .surfaces
                .get_mut(&id)
                .is_some_and(|surface| surface.sync_fullscreen());
            if flipped {
                self.ewm.reconcile_fullscreen();
                self.ewm.refresh_primary_assignments();
                self.refresh_pointer_after_layout();
            }
            self.ewm.sync_presented(id);
            // Title before Mapped, so placement rules see a title set on the mapping commit.
            self.ewm.check_surface_info_changes(surface);

            // An app window's first buffer: fade it in and let Lisp place it.
            let newly_mapped = crate::utils::is_mapped(&root_surface)
                && self
                    .ewm
                    .surfaces
                    .get_mut(&id)
                    .is_some_and(|surface| surface.mark_mapped() && !surface.emacs);
            if newly_mapped {
                self.ewm.start_open_fade(id);

                // Placement is final at map: min/max hints are known, so decide once.
                let (open_floating, parent) = self
                    .ewm
                    .window(id)
                    .and_then(|window| window.toplevel())
                    .map(|toplevel| (compute_open_floating(toplevel), toplevel.parent()))
                    .unwrap_or((false, None));
                let parent_id = parent.and_then(|wl| self.ewm.surface_id(&wl));
                // Select it for its token or its focused parent, never over a fullscreen surface.
                let focused = self.ewm.focused_surface_id();
                let activate = self.ewm.surfaces[&id].activation_pending()
                    || match parent_id {
                        Some(parent) => focused == Some(parent),
                        None => !focused.is_some_and(|id| self.ewm.pending_fullscreen(id)),
                    };
                let output = self.ewm.active_output();
                let frame_surface_id = self.ewm.resolve_app_host_frame(parent_id, output);
                // Size hint for floating placement, dropped when it fills the working area.
                let working_area = output.map(|o| self.ewm.get_working_area(o));
                let output = output.map(Output::name);
                let (width, height) = self
                    .ewm
                    .window(id)
                    .and_then(natural_open_size)
                    .filter(|s| working_area.is_none_or(|wa| s.w < wa.size.w && s.h < wa.size.h))
                    .map(|s| (s.w as f64, s.h as f64))
                    .unzip();
                self.ewm.queue_event(Event::Mapped {
                    id,
                    open_floating,
                    activate,
                    output,
                    frame_surface_id,
                    width,
                    height,
                });
            }

            self.ewm.queue_redraw_for_surface(id);
            return;
        }

        // 4. Handle lock surface commits
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
            if matches!(self.ewm.lock_state, LockState::WaitingForSurfaces { .. }) {
                self.ewm.maybe_continue_to_locking();
            } else {
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

/// Whether a toplevel opens floating: parented (dialog), portal or fixed-height.
fn compute_open_floating(toplevel: &ToplevelSurface) -> bool {
    // Windows with a parent (usually dialogs) open as floating by default.
    if toplevel.parent().is_some() {
        return true;
    }

    // Portal dialogs float even when the requester passed no parent_window.
    if is_portal_surface(toplevel.wl_surface()) {
        return true;
    }

    let (min_size, max_size) = with_states(toplevel.wl_surface(), |state| {
        let mut guard = state.cached_state.get::<SurfaceCachedState>();
        let current = guard.current();
        (current.min_size, current.max_size)
    });

    // We open fixed-height windows as floating.
    min_size.h > 0 && min_size.h == max_size.h
}

/// The window's geometry clamped to its min/max size hints.
fn natural_open_size(window: &Window) -> Option<Size<i32, Logical>> {
    let toplevel = window.toplevel()?;
    let geo = window.geometry().size;
    let (min, max) = with_states(toplevel.wl_surface(), |state| {
        let mut guard = state.cached_state.get::<SurfaceCachedState>();
        let current = guard.current();
        (current.min_size, current.max_size)
    });
    let w = ensure_min_max_size(geo.w, min.w, max.w);
    let h = ensure_min_max_size(geo.h, min.h, max.h);
    (w > 0 && h > 0).then(|| Size::from((w, h)))
}

impl Ewm {
    /// Tiled host for an app surface: parent's frame, else focused, else active, never a float.
    fn resolve_app_host_frame(
        &self,
        parent_id: Option<u64>,
        output: Option<&Output>,
    ) -> Option<u64> {
        let tiled_frame_on = |id: u64, output: Option<&Output>| {
            self.layout
                .frame_location(id)
                .filter(|location| !location.floating)
                .filter(|location| output.is_none_or(|o| location.monitor.output() == o))
                .map(|_| id)
        };
        parent_id
            .and_then(|pid| self.frame_entry_for_surface(pid))
            .and_then(|(id, _)| tiled_frame_on(id, None))
            .or_else(|| {
                output
                    .zip(self.focused_frame_id())
                    .and_then(|(o, id)| tiled_frame_on(id, Some(o)))
            })
            .or_else(|| output.and_then(|o| self.layout.active_frame_id(o)))
    }
}
