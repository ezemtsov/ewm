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

impl Ewm {
    /// The frame holding the keyboard, 0 without one.
    pub fn focused_frame_id(&self) -> u64 {
        self.layout.focused_frame_id()
    }

    /// The selected view of the frame holding the keyboard.
    pub fn focused_entry_id(&self) -> Option<LayoutEntryId> {
        self.layout
            .focused_frame()
            .and_then(|frame| frame.selected_entry_id)
    }

    /// Surface id receiving keyboard input.
    pub fn focused_surface_id(&self) -> u64 {
        self.layout.focused_frame().map_or(0, |frame| {
            frame
                .selected_entry_id
                .and_then(|entry_id| frame.entry_surface_id(entry_id))
                .unwrap_or(frame.surface_id)
        })
    }

    /// The focused entry and its surface when that entry is fullscreen.
    pub fn focused_fullscreen_entry(&self) -> Option<(LayoutEntryId, u64)> {
        let frame = self.layout.focused_frame()?;
        let entry = frame
            .fullscreen_entry()
            .filter(|entry| Some(entry.id) == frame.selected_entry_id)?;
        Some((entry.id, entry.surface_id()?))
    }

    pub fn set_surface_urgent(&mut self, surface_id: u64, urgent: bool) -> bool {
        if surface_id == 0 || (urgent && self.layout.has_frame(surface_id)) {
            return false;
        }
        if urgent && self.focused_surface_id() == surface_id {
            return false;
        }

        self.surfaces
            .get_mut(&surface_id)
            .is_some_and(|surface| std::mem::replace(&mut surface.urgent, urgent) != urgent)
    }

    pub(crate) fn clear_focused_urgency(&mut self) {
        for id in [self.focused_surface_id(), self.focused_frame_id()] {
            if let Some(surface) = self.surfaces.get_mut(&id) {
                surface.urgent = false;
            }
        }
    }

    pub fn mark_workspace_urgent(&mut self, frame_surface_id: Option<u64>) -> bool {
        let frame_surface_id = frame_surface_id.unwrap_or(self.focused_frame_id());
        if frame_surface_id == 0 || !self.layout.has_frame(frame_surface_id) {
            return false;
        }
        if self.focused_frame_id() == frame_surface_id {
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
        source: &str,
        notify_emacs: bool,
    ) -> Option<u64> {
        let location = self.layout.entry_location(entry_id)?;
        let frame_id = location.frame.surface_id;
        module::record_focus(
            location.entry.surface_id().unwrap_or(frame_id),
            source,
            None,
        );
        self.commit_focus(frame_id, Some(entry_id), entry_id, notify_emacs);
        Some(self.focused_frame_id())
    }

    /// Focus an Emacs frame, routing keyboard to its selected entry's surface if one exists.
    pub fn set_focus_frame(
        &mut self,
        frame_surface_id: u64,
        source: &str,
        notify_emacs: bool,
    ) -> Option<FocusId> {
        let Some(location) = self.layout.frame_location(frame_surface_id) else {
            debug!("set_focus_frame({source}): unknown frame {frame_surface_id}");
            return None;
        };
        let (focus_id, selected) = (location.frame.focus_id, location.frame.selected_entry_id);
        module::record_focus(frame_surface_id, source, None);
        Some(self.commit_focus(frame_surface_id, selected, focus_id, notify_emacs))
    }

    /// Focus `frame_id` with `entry_id` selected, then tell Emacs about `focus_id`.
    fn commit_focus(
        &mut self,
        frame_id: u64,
        entry_id: Option<LayoutEntryId>,
        focus_id: FocusId,
        notify_emacs: bool,
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
        self.clear_focused_urgency();
        if notify_emacs {
            self.queue_event(Event::Focus {
                focus_id,
                x: None,
                y: None,
                pointer: false,
            });
        }
        focus_id
    }

    /// Focus a frame reached with the pointer. Emacs then anchors mouse-follows-focus.
    pub(crate) fn set_focus_frame_from_pointer(
        &mut self,
        frame_surface_id: u64,
        source: &str,
        pos_in_frame: Point<f64, Logical>,
    ) {
        if let Some(focus_id) = self.set_focus_frame(frame_surface_id, source, false) {
            self.queue_event(Event::Focus {
                focus_id,
                x: Some(pos_in_frame.x),
                y: Some(pos_in_frame.y),
                pointer: true,
            });
        }
    }

    /// Check if focus is on an Emacs surface (for key interception decisions)
    pub fn is_focus_on_emacs(&self) -> bool {
        self.layout.has_frame(self.focused_surface_id())
    }

    pub(crate) fn is_keyboard_focus_on_emacs(&self) -> bool {
        self.keyboard_focus
            .as_ref()
            .is_some_and(|surface| self.is_emacs_frame_surface(surface))
    }

    /// The focused Emacs frame's surface, for keyboard redirects.
    pub(crate) fn focused_emacs_frame_surface(&self) -> Option<WlSurface> {
        let id = self.focused_frame_id();
        if id == 0 || !self.layout.has_frame(id) {
            return None;
        }
        self.window(id)?
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
    pub fn set_focus(&mut self, id: u64, notify_emacs: bool, source: &str) -> bool {
        let (frame_id, entry_id) = self.resolve_focus_target(id);
        if let Some(entry_id) = entry_id {
            self.set_focus_entry(entry_id, source, notify_emacs);
            true
        } else if frame_id != 0 {
            if self.layout.has_frame(frame_id) {
                self.set_focus_frame(frame_id, source, notify_emacs);
                true
            } else {
                false
            }
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
        entry_id.is_some() || (frame_id != 0 && self.layout.has_frame(frame_id))
    }

    /// Focus the surface, or ask Emacs to show it again when no window does.
    pub fn activate_or_defer_surface(&mut self, id: u64, source: &str) {
        let focused = self.set_focus(id, true, source);
        if focused {
            self.layer_shell_on_demand_focus = None;
        }
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        surface.pending_activation = (!focused).then(Instant::now);
        // Mapped asks Emacs to show an unplaced surface.
        if !focused && surface.placed {
            self.queue_event(Event::ActivateSurface { id });
        }
    }

    pub(crate) fn focus_pending_activation_surfaces(&mut self) {
        for surface in self.surfaces.values_mut() {
            if !surface.activation_pending() {
                surface.pending_activation = None;
            }
        }
        let mut ready: Vec<u64> = self
            .pending_activations()
            .filter(|&id| self.focus_target_ready(id))
            .collect();
        ready.sort_unstable();

        for id in ready {
            if let Some(surface) = self.surfaces.get_mut(&id) {
                surface.pending_activation = None;
            }
            self.set_focus(id, true, "xdg_activation_pending");
            self.layer_shell_on_demand_focus = None;
        }
    }

    /// Focus the layout entry or frame under `pos`, returning `(frame_id, entry_id)`. The caller
    /// sends the Focus event itself, with the pointer coordinates mouse-follows-focus needs.
    pub fn focus_under(
        &mut self,
        pos: Point<f64, Logical>,
        source: &str,
    ) -> Option<(u64, Option<LayoutEntryId>)> {
        let (frame_id, entry_id) = if let Some(entry_id) = self.layout_entry_id_under(pos) {
            let frame_id = self.set_focus_entry(entry_id, source, false)?;
            (frame_id, Some(entry_id))
        } else {
            let (surface, _) = self.surface_under_point(pos)?;
            let sid = self.surface_id(&surface)?;
            let (fid, resolved_entry_id) = self.resolve_focus_target(sid);
            if let Some(entry_id) = resolved_entry_id {
                self.set_focus_entry(entry_id, source, false);
            } else if fid != 0 {
                self.set_focus_frame(fid, source, false);
            } else {
                return None;
            }
            (fid, resolved_entry_id)
        };
        Some((frame_id, entry_id))
    }

    pub fn focus_follows_mouse(&mut self, under: &Option<SurfaceHit>, pos: Point<f64, Logical>) {
        let Some((_, origin)) = under else { return };
        let Some((frame_id, entry_id)) = self.focus_under(pos, "pointer_motion") else {
            return;
        };
        if let Some(focus_id) = entry_id.or_else(|| self.layout.focus_id_of_frame(frame_id)) {
            self.queue_event(Event::Focus {
                focus_id,
                x: Some(pos.x - origin.x),
                y: Some(pos.y - origin.y),
                pointer: true,
            });
        }
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
            if focused == 0 || !self.surfaces.contains_key(&focused) {
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
        let target = self.layout.frame_location(focused).and_then(|location| {
            let strip = &location.monitor.strip;
            let anchor = if location.floating {
                Some(strip.active_idx())
            } else {
                strip.index_of(focused)
            };
            strip.frame_ids().nth(pick(anchor?)?)
        });
        if let Some(id) = target {
            let before = self.focused_frame_id();
            let shown = self.layout.show_strip_frame(id);
            self.settle_shown(shown);
            // The change hid the frame holding the keyboard, Emacs follows it.
            let after = self.focused_frame_id();
            if after != before {
                self.set_focus_frame(after, "action", true);
            }
        }
    }

    /// Move the keyboard from a float to the active strip frame, else to the topmost shown float.
    pub(crate) fn toggle_float_focus(&mut self) {
        let focused = self.focused_frame_id();
        let target = self.layout.frame_location(focused).and_then(|location| {
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
            self.set_focus_frame(id, "action", true);
        }
    }
}
