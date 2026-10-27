//! Keyboard focus: setting it, reading it, and the layer and activation rules around it.

use std::time::Instant;

use smithay::desktop::{LayerSurface as DesktopLayerSurface, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::wlr_layer::Layer;
use tracing::debug;

use crate::{Event, Ewm, FocusId, LayoutEntryId, SurfaceHit, module};

/// Where a focus change came from, kept in the focus history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusSource {
    Action,
    Gesture,
    Overview,
    /// Emacs asked for it and hears nothing back.
    LispRequest,
    /// A merge changed the focused frame's selected window.
    LayoutSelected,
    ToplevelDestroyed,
    CleanupDeadWindows,
    FullscreenOver,
    FocusOutputDirection,
    Unlock,
    XdgActivation,
    XdgActivationPending,
    ForeignToplevel,
    PointerMotion,
    Click,
    Scroll,
    Touch,
    Tablet,
    Drop,
    FloatingMoveGrab,
    FloatingResizeGrab,
    Test,
}

/// What Emacs hears of a focus change.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Notify {
    /// Nothing, the change came from Emacs or must stay quiet.
    None,
    /// A Focus event.
    Emacs,
    /// A Focus event with the pointer's position in the focused surface, which anchors
    /// mouse-follows-focus.
    Pointer(Point<f64, Logical>),
}

impl Ewm {
    /// The frame holding the keyboard.
    pub fn focused_frame_id(&self) -> Option<u64> {
        self.layout.focused_frame_id()
    }

    /// The selected view of the frame holding the keyboard.
    pub fn focused_entry_id(&self) -> Option<LayoutEntryId> {
        self.layout
            .focused_frame()
            .and_then(|frame| frame.selected_entry_id)
    }

    /// Surface id receiving keyboard input.
    pub fn focused_surface_id(&self) -> Option<u64> {
        self.layout.focused_frame().map(|frame| {
            frame
                .selected_entry_id
                .and_then(|entry_id| frame.entry_surface_id(entry_id))
                .unwrap_or(frame.surface_id)
        })
    }

    /// Whether the focused view is fullscreen.
    pub fn focused_fullscreen(&self) -> bool {
        self.layout.focused_frame().is_some_and(|frame| {
            frame
                .fullscreen_entry()
                .filter(|entry| Some(entry.id) == frame.selected_entry_id)
                .is_some_and(|entry| entry.surface_id().is_some())
        })
    }

    pub fn set_surface_urgent(&mut self, surface_id: u64, urgent: bool) -> bool {
        if urgent && self.layout.has_frame(surface_id) {
            return false;
        }

        self.surfaces
            .get_mut(&surface_id)
            .is_some_and(|surface| std::mem::replace(&mut surface.urgent, urgent) != urgent)
    }

    /// Outside the overview, whatever holds focus has been seen.
    pub fn refresh_urgency(&mut self) {
        if self.overview.is_open() {
            return;
        }
        for id in [self.focused_surface_id(), self.focused_frame_id()]
            .into_iter()
            .flatten()
        {
            if let Some(surface) = self.surfaces.get_mut(&id) {
                surface.urgent = false;
            }
        }
    }

    pub fn mark_workspace_urgent(&mut self, frame_surface_id: Option<u64>) -> bool {
        let Some(frame_surface_id) = frame_surface_id.or_else(|| self.focused_frame_id()) else {
            return false;
        };
        if !self.layout.has_frame(frame_surface_id) {
            return false;
        }

        self.surfaces
            .get_mut(&frame_surface_id)
            .is_some_and(|frame| !std::mem::replace(&mut frame.urgent, true))
    }

    /// Focus the layout entry identified by `entry_id`. Returns the frame focused.
    pub fn set_focus_entry(
        &mut self,
        entry_id: LayoutEntryId,
        source: FocusSource,
        notify: Notify,
    ) -> Option<u64> {
        let location = self.layout.entry_location(entry_id)?;
        let frame_id = location.frame.surface_id;
        module::record_focus(location.entry.surface_id().unwrap_or(frame_id), source);
        self.commit_focus(frame_id, Some(entry_id), entry_id, notify);
        self.focused_frame_id()
    }

    /// Focus an Emacs frame, routing keyboard to its selected entry's surface if one exists.
    pub fn set_focus_frame(
        &mut self,
        frame_surface_id: u64,
        source: FocusSource,
        notify: Notify,
    ) -> Option<FocusId> {
        let Some(location) = self.layout.frame_location(frame_surface_id) else {
            debug!("set_focus_frame({source:?}): unknown frame {frame_surface_id}");
            return None;
        };
        let (focus_id, selected) = (location.frame.focus_id, location.frame.selected_entry_id);
        module::record_focus(frame_surface_id, source);
        Some(self.commit_focus(frame_surface_id, selected, focus_id, notify))
    }

    /// Focus `frame_id` with `entry_id` selected, then tell Emacs about `focus_id`.
    fn commit_focus(
        &mut self,
        frame_id: u64,
        entry_id: Option<LayoutEntryId>,
        focus_id: FocusId,
        notify: Notify,
    ) -> FocusId {
        // A fullscreen float takes the focus meant for its frame.
        let (frame_id, entry_id, focus_id) = match self.layout.fullscreen_float_over(frame_id) {
            Some((float_id, entry_id)) => (float_id, Some(entry_id), entry_id),
            None => (frame_id, entry_id, focus_id),
        };
        if let Some(surface_id) =
            entry_id.and_then(|entry_id| self.layout.entry_surface_id(entry_id))
        {
            self.flush_pending_title_event(surface_id);
        }
        let shown = self.layout.activate_frame(frame_id, entry_id);
        self.settle_shown(shown);
        let (x, y, pointer) = match notify {
            Notify::None => return focus_id,
            Notify::Emacs => (None, None, false),
            Notify::Pointer(pos) => (Some(pos.x), Some(pos.y), true),
        };
        self.queue_event(Event::Focus {
            focus_id,
            x,
            y,
            pointer,
        });
        focus_id
    }

    /// Check if focus is on an Emacs surface (for key interception decisions)
    pub fn is_focus_on_emacs(&self) -> bool {
        self.focused_surface_id()
            .is_some_and(|id| self.layout.has_frame(id))
    }

    pub(crate) fn is_keyboard_focus_on_emacs(&self) -> bool {
        self.keyboard_focus
            .as_ref()
            .is_some_and(|surface| self.is_emacs_frame_surface(surface))
    }

    /// The focused Emacs frame's surface, for keyboard redirects.
    pub(crate) fn focused_emacs_frame_surface(&self) -> Option<WlSurface> {
        self.window(self.focused_frame_id()?)?
            .wl_surface()
            .map(|surface| surface.into_owned())
    }

    pub(crate) fn emacs_keyboard_redirect_focus(&self) -> Option<WlSurface> {
        if !self.im_repeat.has_keyboard_redirect_hold() && !module::get_keyboard_capture() {
            return None;
        }

        self.focused_emacs_frame_surface().or_else(|| {
            self.keyboard_focus
                .as_ref()
                .filter(|surface| self.is_emacs_frame_surface(surface))
                .cloned()
        })
    }

    /// Focus surface `id` as the layout entry or Emacs frame it is, via the mutator for that kind.
    pub fn set_focus(&mut self, id: u64, source: FocusSource, notify: Notify) -> bool {
        let (frame_id, entry_id) = self.resolve_focus_target(id);
        if let Some(entry_id) = entry_id {
            self.set_focus_entry(entry_id, source, notify);
            true
        } else if self.layout.has_frame(frame_id) {
            self.set_focus_frame(frame_id, source, notify);
            true
        } else {
            false
        }
    }

    fn resolve_focus_target(&self, id: u64) -> (u64, Option<LayoutEntryId>) {
        if self.layout.has_frame(id) {
            return (id, None);
        }
        if let Some((frame_id, entry_id)) = self.frame_entry_for_surface(id) {
            (frame_id, Some(entry_id))
        } else {
            (id, None)
        }
    }

    pub fn focus_target_ready(&self, id: u64) -> bool {
        let (frame_id, entry_id) = self.resolve_focus_target(id);
        entry_id.is_some() || self.layout.has_frame(frame_id)
    }

    /// Focus the surface, or ask Emacs to show it again when no window does.
    pub fn activate_or_defer_surface(&mut self, id: u64, source: FocusSource) {
        let focused = self.set_focus(id, source, Notify::Emacs);
        if focused {
            self.layer_shell_on_demand_focus = None;
        }
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        surface.pending_activation = (!focused).then(Instant::now);
        // Mapped asks Emacs to show an unplaced surface, in the host a new one would get.
        if !focused && surface.placed {
            let parent_id = self
                .window(id)
                .and_then(|window| window.toplevel())
                .and_then(|toplevel| toplevel.parent())
                .and_then(|wl| self.surface_id(&wl));
            let output = self.active_output();
            let frame_surface_id = self.resolve_app_host_frame(parent_id, output);
            self.queue_event(Event::ActivateSurface {
                id,
                output: output.map(Output::name),
                frame_surface_id,
            });
        }
    }

    pub(crate) fn focus_pending_activation_surfaces(&mut self) {
        for surface in self.surfaces.values_mut() {
            if !surface.activation_pending() {
                surface.pending_activation = None;
            }
        }
        let mut pending: Vec<u64> = self.pending_activations().collect();
        pending.sort_unstable();

        for id in pending {
            if self.set_focus(id, FocusSource::XdgActivationPending, Notify::Emacs) {
                if let Some(surface) = self.surfaces.get_mut(&id) {
                    surface.pending_activation = None;
                }
                self.layer_shell_on_demand_focus = None;
            }
        }
    }

    /// Focus the layout entry or frame under `pos`.
    pub fn focus_under(&mut self, pos: Point<f64, Logical>, source: FocusSource, notify: Notify) {
        if let Some(entry_id) = self.layout_entry_id_under(pos) {
            self.set_focus_entry(entry_id, source, notify);
            return;
        }
        let Some((surface, _)) = self.surface_under_point(pos) else {
            return;
        };
        if let Some(id) = self.surface_id(&surface) {
            self.set_focus(id, source, notify);
        }
    }

    pub fn focus_follows_mouse(&mut self, under: &Option<SurfaceHit>, pos: Point<f64, Logical>) {
        let Some((_, origin)) = under else { return };
        self.focus_under(
            pos,
            FocusSource::PointerMotion,
            Notify::Pointer(pos - *origin),
        );
    }

    /// Update on-demand layer shell keyboard focus.
    /// If the surface has OnDemand keyboard interactivity, set it as on-demand focus.
    /// Otherwise, clear on-demand focus.
    pub fn focus_layer_surface_if_on_demand(&mut self, surface: Option<DesktopLayerSurface>) {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        if let Some(surface) = surface
            && surface.cached_state().keyboard_interactivity == KeyboardInteractivity::OnDemand
        {
            if self.layer_shell_on_demand_focus.as_ref() != Some(&surface) {
                self.layer_shell_on_demand_focus = Some(surface);
            }
            return;
        }

        // Something else got clicked, clear on-demand layer-shell focus
        if self.layer_shell_on_demand_focus.is_some() {
            self.layer_shell_on_demand_focus = None;
        }
    }

    /// Resolve layer shell keyboard focus.
    /// Checks for Exclusive interactivity on Overlay/Top layers first,
    /// then OnDemand focus.
    pub(crate) fn resolve_layer_keyboard_focus(&self) -> Option<WlSurface> {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        let excl_on_layer = |output: &Output, layer: Layer| -> Option<WlSurface> {
            let map = layer_map_for_output(output);

            map.layers_on(layer).find_map(|surface| {
                if surface.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
                {
                    Some(surface.wl_surface().clone())
                } else {
                    None
                }
            })
        };

        let on_demand_on_layer = |output: &Output, layer: Layer| -> Option<WlSurface> {
            let on_demand = self.layer_shell_on_demand_focus.as_ref()?;
            let map = layer_map_for_output(output);

            map.layers_on(layer).find_map(|surface| {
                if surface == on_demand {
                    Some(surface.wl_surface().clone())
                } else {
                    None
                }
            })
        };

        // Check all outputs (typically just one for EWM)
        for output in &self.sorted_outputs {
            // Exclusive Overlay takes highest priority
            if let Some(s) = excl_on_layer(output, Layer::Overlay) {
                return Some(s);
            }
            // Exclusive Top
            if let Some(s) = excl_on_layer(output, Layer::Top) {
                return Some(s);
            }
            // OnDemand on any layer
            for layer in [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background] {
                if let Some(s) = on_demand_on_layer(output, layer) {
                    return Some(s);
                }
            }
            // Exclusive Bottom/Background (only when no toplevel has focus)
            let focused = self.focused_surface_id();
            if !focused.is_some_and(|id| self.surfaces.contains_key(&id)) {
                if let Some(s) = excl_on_layer(output, Layer::Bottom) {
                    return Some(s);
                }
                if let Some(s) = excl_on_layer(output, Layer::Background) {
                    return Some(s);
                }
            }
        }

        None
    }

    /// Show strip frame `pick(anchor)`, anchored at the focused frame, else at the shown one. The
    /// keyboard follows a frame the change hides.
    pub(crate) fn focus_strip_frame(&mut self, pick: impl FnOnce(usize) -> Option<usize>) {
        let focused = self.focused_frame_id();
        let target = focused
            .and_then(|focused| self.layout.frame_location(focused))
            .and_then(|location| {
                let strip = &location.monitor.strip;
                let anchor = if location.floating {
                    Some(strip.active_idx())
                } else {
                    Some(location.frame_idx)
                };
                strip.frame_ids().nth(pick(anchor?)?)
            });
        if let Some(id) = target {
            let shown = self.layout.show_strip_frame(id);
            self.settle_shown(shown);
            // The change hid the frame holding the keyboard, Emacs follows it.
            if let Some(after) = self
                .focused_frame_id()
                .filter(|&after| Some(after) != focused)
            {
                self.set_focus_frame(after, FocusSource::Action, Notify::Emacs);
            }
        }
    }

    /// Move the keyboard from a float to the active strip frame, else to the topmost shown float.
    pub(crate) fn toggle_float_focus(&mut self) {
        let target = self
            .focused_frame_id()
            .and_then(|focused| self.layout.frame_location(focused))
            .and_then(|location| {
                let monitor = location.monitor;
                if location.floating {
                    monitor.strip.active_frame_id()
                } else {
                    monitor
                        .floating
                        .frames()
                        .iter()
                        .find(|float| monitor.float_shown(float))
                        .map(|float| float.surface_id)
                }
            });
        if let Some(id) = target {
            self.set_focus_frame(id, FocusSource::Action, Notify::Emacs);
        }
    }
}
