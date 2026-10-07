//! Compositor-side state of one toplevel, from creation to destruction.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Serial;
use smithay::wayland::compositor::{HookId, remove_pre_commit_hook, with_states};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::ToplevelSurface;

use crate::animation::Animation;
use crate::resize_fade::{PendingResize, ResizeFade};
use crate::{Ewm, LayoutEntry, handlers};

const OPEN_ANIM_DURATION: Duration = Duration::from_millis(150);
/// Holds alpha at 0 for one vblank so the empty first buffer some clients ship never shows.
const OPEN_ANIM_DELAY: Duration = Duration::from_millis(16);

/// The id of the toplevel a wl_surface backs, 0 between toplevels.
struct SurfaceId(AtomicU64);

#[derive(Debug)]
pub struct SurfaceState {
    /// The tag this record holds on its wl_surface.
    id: u64,
    pub window: Window,
    /// Watches commits for the close snapshot and the resize fade.
    hook: HookId,
    /// An Emacs frame rather than an app window.
    pub emacs: bool,
    /// Whether a buffer has ever been committed.
    pub mapped: bool,
    /// Fade-in after the first buffer.
    pub open: Option<Animation>,
    /// Resizes configured but not yet answered, oldest first.
    pub pending_resizes: Vec<(Serial, PendingResize)>,
    /// Old contents dissolving into a resized buffer.
    pub resize: Option<ResizeFade>,
}

impl SurfaceState {
    pub(crate) fn new(id: u64, toplevel: ToplevelSurface, emacs: bool) -> Self {
        let surface = toplevel.wl_surface();
        with_states(surface, |states| {
            states
                .data_map
                .get_or_insert_threadsafe(|| SurfaceId(AtomicU64::new(0)))
                .0
                .store(id, Ordering::Relaxed);
        });
        let hook = handlers::add_toplevel_pre_commit_hook(surface, id);
        Self {
            id,
            window: Window::new_wayland_window(toplevel),
            hook,
            emacs,
            mapped: false,
            open: None,
            pending_resizes: Vec::new(),
            resize: None,
        }
    }

    /// The newest resize a commit acking `serial` answers. The ones it skipped are over too.
    pub(crate) fn take_answered_resize(&mut self, serial: Serial) -> Option<PendingResize> {
        let answered = self
            .pending_resizes
            .iter()
            .take_while(|(configured, _)| serial.is_no_older_than(configured))
            .count();
        self.pending_resizes
            .drain(..answered)
            .next_back()
            .map(|(_, pending)| pending)
    }

    /// Records a committed buffer, true only for the first one.
    pub(crate) fn mark_mapped(&mut self) -> bool {
        !std::mem::replace(&mut self.mapped, true)
    }

    pub(crate) fn is_animating(&self) -> bool {
        self.open.is_some() || self.resize.is_some()
    }

    /// Drop finished animations.
    pub(crate) fn advance(&mut self) {
        if self.open.as_ref().is_some_and(Animation::is_done) {
            self.open = None;
        }
        if self.resize.as_ref().is_some_and(ResizeFade::is_done) {
            self.resize = None;
        }
    }

    /// Alpha of the open fade, 1 once it is over.
    pub(crate) fn open_alpha(&self) -> f32 {
        self.open.as_ref().map_or(1., |anim| anim.value() as f32)
    }

    /// The resize fade `entry` still shows, if any.
    pub(crate) fn resize_fade_for(&self, entry: &LayoutEntry) -> Option<&ResizeFade> {
        if entry.primary() {
            return None;
        }
        self.resize.as_ref().filter(|fade| fade.covers(entry))
    }
}

impl Drop for SurfaceState {
    fn drop(&mut self) {
        let Some(surface) = self.window.wl_surface() else {
            return;
        };
        with_states(&surface, |states| {
            // A newer toplevel on the same wl_surface keeps its own tag.
            if let Some(tag) = states.data_map.get::<SurfaceId>() {
                let _ = tag
                    .0
                    .compare_exchange(self.id, 0, Ordering::Relaxed, Ordering::Relaxed);
            }
        });
        remove_pre_commit_hook(&surface, &self.hook);
    }
}

impl Ewm {
    /// The window of toplevel `id`.
    pub fn window(&self, id: u64) -> Option<&Window> {
        self.surfaces.get(&id).map(|surface| &surface.window)
    }

    /// Every toplevel's window, by id.
    pub fn windows(&self) -> impl Iterator<Item = (u64, &Window)> {
        self.surfaces
            .iter()
            .map(|(&id, surface)| (id, &surface.window))
    }

    /// The toplevel `surface` backs, if one does.
    pub fn surface_id(&self, surface: &WlSurface) -> Option<u64> {
        with_states(surface, |states| {
            states
                .data_map
                .get::<SurfaceId>()
                .map(|tag| tag.0.load(Ordering::Relaxed))
        })
        .filter(|&id| id != 0)
    }

    /// Fade surface `id` in from its first buffer.
    pub(crate) fn start_open_fade(&mut self, id: u64) {
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        let anim = Animation::ease(self.animations_clock.clone(), 0., 1., OPEN_ANIM_DURATION);
        surface.open = Some(anim.delayed(OPEN_ANIM_DELAY));
    }
}
