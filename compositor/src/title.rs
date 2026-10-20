//! Title and app id changes, coalesced before Emacs hears about them.

use std::time::{Duration, Instant};

use smithay::reexports::calloop::timer::Timer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use tracing::{debug, warn};

use crate::{Event, Ewm, module, surface};

const TITLE_EVENT_IDLE_FLUSH_DELAY: Duration = Duration::from_millis(500);
const TITLE_EVENT_MAX_STALE: Duration = Duration::from_millis(1000);

impl Ewm {
    pub(crate) fn queue_title_update(
        &mut self,
        id: u64,
        app: String,
        title: String,
        first_title: bool,
        app_changed: bool,
    ) {
        if self.layout.has_frame(id) {
            self.discard_pending_title_event(id);
            return;
        }

        // Pre-map titles feed placement rules, so they are never debounced.
        if first_title
            || app_changed
            || self
                .surfaces
                .get(&id)
                .is_some_and(|surface| !surface.mapped)
            || self.focused_surface_id() == id
        {
            self.discard_pending_title_event(id);
            self.queue_event(Event::Title { id, app, title });
            return;
        }

        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        let queued_at = surface
            .pending_title
            .as_ref()
            .map_or_else(Instant::now, |pending| pending.queued_at);
        surface.pending_title = Some(surface::PendingTitle {
            app,
            title,
            queued_at,
        });
        self.ensure_title_flush_timer(TITLE_EVENT_IDLE_FLUSH_DELAY);
    }

    fn ensure_title_flush_timer(&mut self, duration: Duration) {
        if self.title_flush_timer_token.is_some() {
            return;
        }

        match self
            .loop_handle
            .insert_source(Timer::from_duration(duration), |_, _, state| {
                state.ewm.on_title_flush_timer();
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            }) {
            Ok(token) => self.title_flush_timer_token = Some(token),
            Err(err) => warn!("Failed to insert title flush timer: {:?}", err),
        }
    }

    /// Drop the flush timer once no title is pending.
    fn cancel_title_flush_timer(&mut self) {
        let Some(token) = self.title_flush_timer_token else {
            return;
        };
        if self
            .surfaces
            .values()
            .any(|surface| surface.pending_title.is_some())
        {
            return;
        }
        self.title_flush_timer_token = None;
        self.loop_handle.remove(token);
    }

    fn discard_pending_title_event(&mut self, id: u64) {
        if let Some(surface) = self.surfaces.get_mut(&id) {
            surface.pending_title = None;
        }
        self.cancel_title_flush_timer();
    }

    pub(crate) fn flush_pending_title_event(&mut self, id: u64) {
        if let Some(pending) = self
            .surfaces
            .get_mut(&id)
            .and_then(|surface| surface.pending_title.take())
        {
            self.queue_event(Event::Title {
                id,
                app: pending.app,
                title: pending.title,
            });
        }
        self.cancel_title_flush_timer();
    }

    pub(crate) fn flush_pending_title_events(&mut self) {
        let mut pending: Vec<(u64, surface::PendingTitle)> = self
            .surfaces
            .iter_mut()
            .filter_map(|(&id, surface)| Some((id, surface.pending_title.take()?)))
            .collect();
        pending.sort_by_key(|(id, _)| *id);
        for (id, pending) in pending {
            self.queue_event(Event::Title {
                id,
                app: pending.app,
                title: pending.title,
            });
        }
        self.cancel_title_flush_timer();
    }

    fn on_title_flush_timer(&mut self) {
        self.title_flush_timer_token = None;

        let Some(oldest) = self
            .surfaces
            .values()
            .filter_map(|surface| Some(surface.pending_title.as_ref()?.queued_at))
            .min()
        else {
            return;
        };

        if self.is_focus_on_emacs() {
            let idle_elapsed = self.last_emacs_keyboard_activity.elapsed();
            let stale_elapsed = oldest.elapsed();
            if idle_elapsed < TITLE_EVENT_IDLE_FLUSH_DELAY && stale_elapsed < TITLE_EVENT_MAX_STALE
            {
                let idle_remaining = TITLE_EVENT_IDLE_FLUSH_DELAY - idle_elapsed;
                let stale_remaining = TITLE_EVENT_MAX_STALE - stale_elapsed;
                self.ensure_title_flush_timer(idle_remaining.min(stale_remaining));
                return;
            }
        }

        self.flush_pending_title_events();
    }

    pub(crate) fn check_surface_info_changes(&mut self, wl_surface: &WlSurface) {
        let Some(id) = self.surface_id(wl_surface) else {
            return;
        };

        let (app_id, title) = smithay::wayland::compositor::with_states(wl_surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .map(|d| {
                    let data = d.lock().unwrap();
                    (
                        data.app_id.clone().unwrap_or_default(),
                        data.title.clone().unwrap_or_default(),
                    )
                })
                .unwrap_or_default()
        });

        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        let changed = surface.app_id != app_id || surface.title != title;
        if !changed || (app_id.is_empty() && title.is_empty()) {
            return;
        }
        let first_title = !title.is_empty() && surface.title.is_empty();
        let app_changed = surface.app_id != app_id;
        if module::DEBUG_MODE.load(std::sync::atomic::Ordering::Relaxed) {
            debug!(
                "Surface {} info changed: app='{}' title='{}'",
                id, app_id, title
            );
        }
        surface.app_id = app_id.clone();
        surface.title = title.clone();
        self.introspect_windows_dirty = true;
        self.queue_title_update(id, app_id, title, first_title, app_changed);
    }
}
