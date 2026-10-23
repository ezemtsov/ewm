//! Compositor-side state of one toplevel, from creation to destruction.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgToplevelState;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Serial, Size};
use smithay::wayland::compositor::{HookId, remove_pre_commit_hook, with_states};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgToplevelSurfaceData};

use crate::animation::Animation;
use crate::layout::resize_fade::{Fit, PendingResize, ResizeFade, ViewRect};
use crate::{Ewm, LayoutEntry, LayoutEntryId, handlers};

const OPEN_ANIM_DURATION: Duration = Duration::from_millis(150);
/// Holds alpha at 0 for one vblank so the empty first buffer some clients ship never shows.
const OPEN_ANIM_DELAY: Duration = Duration::from_millis(16);

/// The id of the toplevel a wl_surface backs, 0 between toplevels.
struct SurfaceId(AtomicU64);

/// A title change held back from Lisp while Emacs is busy.
#[derive(Debug)]
pub struct PendingTitle {
    pub app: String,
    pub title: String,
    pub queued_at: Instant,
}

#[derive(Debug)]
pub struct SurfaceState {
    /// The tag this record holds on its wl_surface.
    id: u64,
    pub window: Window,
    /// Watches commits for the close snapshot and the resize fade.
    hook: HookId,
    /// An Emacs frame rather than an app window.
    pub emacs: bool,
    pub app_id: String,
    pub title: String,
    /// Whether a buffer has ever been committed.
    pub mapped: bool,
    /// Whether a layout has ever shown it.
    pub placed: bool,
    /// Asked for attention and not focused since.
    pub urgent: bool,
    /// When an activation was granted before a window showed it.
    pub pending_activation: Option<Instant>,
    pub pending_title: Option<PendingTitle>,
    /// Outputs showing it.
    pub outputs: HashSet<String>,
    /// Told no output shows it.
    pub suspended: bool,
    /// Fullscreen as a client or Emacs asked, what the configure says.
    pub pending_fullscreen: bool,
    /// Fullscreen as the client last committed it, what its views show.
    pub fullscreen: bool,
    /// Fade-in after the first buffer.
    pub open: Option<Animation>,
    /// Resizes configured but not yet answered, oldest first.
    pending_resizes: Vec<(Serial, PendingResize)>,
    /// Old contents dissolving into the new.
    pub resize: Option<ResizeFade>,
    /// How each view fits its contents, held while a resize is pending.
    fitted: HashMap<LayoutEntryId, Fit>,
    /// Where its views drew their contents before a commit that changes them.
    pub(crate) rects_before: Vec<ViewRect>,
}

impl SurfaceState {
    pub(crate) fn new(id: u64, toplevel: ToplevelSurface, emacs: bool, app_id: String) -> Self {
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
            app_id,
            title: String::new(),
            mapped: false,
            placed: false,
            urgent: false,
            pending_activation: None,
            pending_title: None,
            outputs: HashSet::new(),
            suspended: false,
            pending_fullscreen: false,
            fullscreen: false,
            open: None,
            pending_resizes: Vec::new(),
            resize: None,
            fitted: HashMap::new(),
            rects_before: Vec::new(),
        }
    }

    /// Whether an activation granted within the token timeout still waits.
    pub fn activation_pending(&self) -> bool {
        self.pending_activation
            .is_some_and(|granted| granted.elapsed() < handlers::XDG_ACTIVATION_TOKEN_TIMEOUT)
    }

    /// Size the toplevel for a view on `output`, queueing `pending` to fade when the size changes.
    pub(crate) fn configure(
        &mut self,
        output: &Output,
        size: Size<i32, Logical>,
        fullscreen: bool,
        pending: Option<PendingResize>,
    ) {
        crate::utils::set_preferred_scale_transform(&self.window, output);
        let Some(toplevel) = self.window.toplevel() else {
            return;
        };
        let (changed, size_changed) = toplevel.with_pending_state(|state| {
            let was_fullscreen = state.states.contains(XdgToplevelState::Fullscreen);
            let was_maximized = state.states.contains(XdgToplevelState::Maximized);
            let size_changed = state.size != Some(size);
            let changed =
                size_changed || was_fullscreen != fullscreen || was_maximized == fullscreen;
            state.size = Some(size);
            if fullscreen {
                state.states.set(XdgToplevelState::Fullscreen);
                state.states.unset(XdgToplevelState::Maximized);
            } else {
                state.states.set(XdgToplevelState::Maximized);
                state.states.unset(XdgToplevelState::Fullscreen);
            }
            (changed, size_changed)
        });
        if !changed {
            return;
        }
        let serial = toplevel.send_configure();
        if size_changed && let Some(pending) = pending {
            self.pending_resizes.push((serial, pending));
        }
    }

    /// Whether fullscreen `entry` draws at its own size: primary and owing no buffer.
    pub(crate) fn fullscreen_native(&self, entry: &LayoutEntry) -> bool {
        entry.primary() && !self.owes_buffer()
    }

    /// Whether a size-changing configure is still unanswered.
    pub fn owes_buffer(&self) -> bool {
        !self.pending_resizes.is_empty()
    }

    /// The size and fullscreen state last configured, none before the first configure.
    pub(crate) fn configured(&self) -> Option<(Size<i32, Logical>, bool)> {
        let toplevel = self.window.toplevel()?;
        with_states(toplevel.wl_surface(), |states| {
            let role = states.data_map.get::<XdgToplevelSurfaceData>().unwrap();
            let state = role.lock().unwrap().current_server_state();
            Some((
                state.size?,
                state.states.contains(XdgToplevelState::Fullscreen),
            ))
        })
    }

    /// Whether the window has shown a buffer.
    pub(crate) fn has_buffer(&self) -> bool {
        self.window
            .wl_surface()
            .is_some_and(|surface| crate::utils::is_mapped(&surface))
    }

    /// Backdrop alpha under `entry`: whole at fullscreen rest, fading through a flip.
    pub(crate) fn backdrop_alpha(&self, entry: &LayoutEntry) -> f32 {
        let to = f32::from(u8::from(entry.fullscreen()));
        match self.resize_fade_for(entry) {
            Some(fade) if fade.flips() => 1. - to + (2. * to - 1.) * fade.progress(),
            _ => to,
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

    /// Takes the committed fullscreen state, true when the commit changed it.
    pub(crate) fn sync_fullscreen(&mut self) -> bool {
        let committed = self.window.toplevel().is_some_and(|toplevel| {
            toplevel.with_committed_state(|state| {
                state.is_some_and(|state| state.states.contains(XdgToplevelState::Fullscreen))
            })
        });
        std::mem::replace(&mut self.fullscreen, committed) != committed
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

    /// How view `entry` fits its contents, `now` until it has been fitted.
    pub(crate) fn fit_of(&self, entry: LayoutEntryId, now: Fit) -> Fit {
        self.fitted.get(&entry).copied().unwrap_or(now)
    }

    /// Fit view `entry` by `fit`, unless on `hold` it already has one.
    pub(crate) fn fit(&mut self, entry: LayoutEntryId, fit: Fit, hold: bool) {
        if !(hold && self.fitted.contains_key(&entry)) {
            self.fitted.insert(entry, fit);
        }
    }

    /// Forget the views no longer shown.
    pub(crate) fn forget_views(&mut self, shown: &[ViewRect]) {
        self.fitted
            .retain(|entry, _| shown.iter().any(|view| view.entry == *entry));
    }

    /// Alpha of the open fade, 1 once it is over.
    pub(crate) fn open_alpha(&self) -> f32 {
        self.open.as_ref().map_or(1., |anim| anim.value() as f32)
    }

    /// The resize fade `entry` still shows, if any.
    pub(crate) fn resize_fade_for(&self, entry: &LayoutEntry) -> Option<&ResizeFade> {
        self.resize.as_ref().filter(|fade| fade.shows(entry))
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

    /// Surfaces whose activation waits for a window to show them.
    pub fn pending_activations(&self) -> impl Iterator<Item = u64> {
        self.surfaces
            .iter()
            .filter(|(_, surface)| surface.pending_activation.is_some())
            .map(|(&id, _)| id)
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
