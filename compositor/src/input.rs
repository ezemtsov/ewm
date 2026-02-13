//! Generic input handling shared between backends
//!
//! This module provides keyboard and pointer event processing that works
//! with any Smithay input backend.
//!
//! # Design Invariants
//!
//! 1. **Key interception**: Super-prefixed bindings are intercepted and redirected to
//!    Emacs. The intercepted key list comes from Emacs via `ewm-set-intercepted-keys`.
//!    Keys are matched using raw Latin keysyms to work regardless of XKB layout.
//!
//! 2. **Focus synchronization**: Before processing any key, we check for pending focus
//!    commands from Emacs. This ensures focus changes are applied before the key event,
//!    avoiding race conditions.
//!
//! 3. **Text input intercept**: When `text_input_intercept` is true, all printable keys
//!    are redirected to Emacs (for input method support in non-Emacs surfaces).
//!
//! 4. **VT switching**: Ctrl+Alt+F1-F12 are special keys handled by XKB as
//!    XF86Switch_VT_N keysyms. We detect these and signal the backend to switch VTs.

use crate::tracy_span;

use smithay::{
    backend::input::KeyState,
    input::keyboard::{keysyms, xkb, FilterResult},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER,
    wayland::seat::WaylandFocus,
    wayland::text_input::TextInputSeat,
};

use crate::{is_kill_combo, State, module};

/// Notify the idle notifier of user activity (if initialized)
/// TODO: ext-idle-notify-v1 support requires architectural changes
fn notify_activity(_state: &mut State) {
    // No-op for now - idle notifier not yet supported
}

/// Result of processing a keyboard event
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardAction {
    /// Normal key forwarding - nothing special happened
    Forward,
    /// Prefix key intercepted - redirect focus to Emacs
    RedirectToEmacs,
    /// Kill combo pressed - shut down compositor
    Shutdown,
    /// Key intercepted for text input (sent to Emacs via IPC)
    TextInputIntercepted,
    /// VT switch requested (Ctrl+Alt+F1-F12)
    ChangeVt(i32),
}

/// Process a keyboard key event
///
/// This handles:
/// - Kill combo detection (Super+Shift+E)
/// - Prefix key interception (redirect to Emacs)
/// - Normal key forwarding to focused surface
///
/// Returns the action to take based on the key event.
pub fn handle_keyboard_event(
    state: &mut State,
    keycode: u32,
    key_state: KeyState,
    time: u32,
) -> KeyboardAction {
    let keyboard = state.ewm.keyboard.clone();
    tracy_span!("handle_keyboard_event");

    let serial = SERIAL_COUNTER.next_serial();
    let is_press = key_state == KeyState::Pressed;

    // Handle locked state: only allow VT switch, forward everything else to lock surface
    if state.ewm.is_locked() {
        // Notify idle notifier of activity even when locked
        notify_activity(state);

        // Check for VT switch first (Ctrl+Alt+F1-F12)
        let vt_switch = keyboard.input::<Option<i32>, _>(
            state,
            keycode.into(),
            key_state,
            serial,
            time,
            |_, _, handle| {
                if !is_press {
                    return FilterResult::Forward;
                }
                let modified = handle.modified_sym().raw();
                if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&modified) {
                    let vt = (modified - keysyms::KEY_XF86Switch_VT_1 + 1) as i32;
                    return FilterResult::Intercept(Some(vt));
                }
                FilterResult::Forward
            },
        );

        if let Some(Some(vt)) = vt_switch {
            return KeyboardAction::ChangeVt(vt);
        }

        // Forward input to lock surface
        if let Some(lock_focus) = state.ewm.lock_surface_focus() {
            keyboard.set_focus(state, Some(lock_focus), serial);
        }

        return KeyboardAction::Forward;
    }

    // Process any pending focus command before handling the key.
    // This ensures focus changes from Emacs are applied immediately,
    // avoiding race conditions where keys arrive before focus is synced.
    if let Some(focus_id) = module::take_pending_focus() {
        if state.ewm.focused_surface_id != focus_id && state.ewm.id_windows.contains_key(&focus_id) {
            state.ewm.focus_surface_with_source(focus_id, false, "pending_focus", Some("keyboard_event"));
        }
    }

    // Clone values needed in the filter closure
    let intercepted_keys = crate::module::get_intercepted_keys();
    let focus_on_emacs = state.ewm.is_focus_on_emacs();
    let text_input_intercept = state.ewm.text_input_intercept;

    // Process key with filter to detect intercepted keys and kill combo
    let filter_result = keyboard.input::<(u8, u32, Option<String>), _>(
        state,
        keycode.into(),
        key_state,
        serial,
        time,
        |_, mods, handle| {
            if !is_press {
                return FilterResult::Forward;
            }

            // Get the modified keysym (with modifiers applied by XKB)
            let modified = handle.modified_sym();
            let modified_raw = modified.raw();

            // Check for VT switch keys (Ctrl+Alt+F1-F12 → XF86Switch_VT_*)
            // XKB transforms Ctrl+Alt+F1-F12 into XF86Switch_VT_1-12 keysyms
            if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12)
                .contains(&modified_raw)
            {
                let vt = (modified_raw - keysyms::KEY_XF86Switch_VT_1 + 1) as i32;
                return FilterResult::Intercept((4, vt as u32, None)); // 4 = VT switch
            }

            // Get the raw latin keysym for this key (layout-independent)
            // This ensures intercepted keys work regardless of current XKB layout
            let raw_latin = handle.raw_latin_sym_or_raw_current_sym();
            let keysym = raw_latin.unwrap_or(modified);
            let keysym_raw = keysym.raw();

            // Check for kill combo (Super+Shift+E)
            if is_kill_combo(keysym_raw, mods.shift, mods.logo) {
                return FilterResult::Intercept((2, 0, None)); // 2 = kill
            }
            // Find if this is an intercepted key and whether it's a prefix
            let matched_key = intercepted_keys
                .iter()
                .find(|ik| ik.matches(keysym_raw, mods));

            if let Some(ik) = matched_key {
                if !focus_on_emacs {
                    // This is an intercepted key and focus is on an external app (not Emacs)
                    // Only SET the flag on prefix keys, never clear it here
                    // Emacs clears the flag when the command sequence completes
                    if ik.is_prefix {
                        module::set_in_prefix_sequence(true);
                    }
                    return FilterResult::Intercept((1, keysym_raw, None)); // 1 = redirect to emacs
                }
                // Intercepted key but already on Emacs - just forward
                return FilterResult::Forward;
            }

            if text_input_intercept
                && !focus_on_emacs
                && !mods.ctrl
                && !mods.alt
                && !mods.logo
            {
                // Text input intercept mode: capture printable keys for Emacs IM processing
                // Skip if any command modifiers are held (let those go to Emacs via intercept-keys)
                // Use modified keysym for UTF-8 (includes Shift for uppercase/@/etc)
                let utf8 = xkb::keysym_to_utf8(modified);
                if !utf8.is_empty() && !utf8.chars().all(|c| c.is_control()) {
                    // This is a printable character - intercept for text input
                    FilterResult::Intercept((3, keysym_raw, Some(utf8))) // 3 = text input
                } else {
                    FilterResult::Forward
                }
            } else {
                FilterResult::Forward
            }
        },
    );

    // Determine action from filter result
    if let Some((code, keysym, ref utf8)) = filter_result {
        match code {
            2 => return KeyboardAction::Shutdown,
            3 => {
                // Text input intercept - send key to Emacs via IPC
                state.ewm.queue_event(crate::Event::Key {
                    keysym,
                    utf8: utf8.clone(),
                });
                return KeyboardAction::TextInputIntercepted;
            }
            4 => {
                // VT switch - keysym contains the VT number
                return KeyboardAction::ChangeVt(keysym as i32);
            }
            _ => {}
        }
    }

    if filter_result.as_ref().map(|(c, _, _)| *c) == Some(1) {
        // Switch focus to the Emacs frame on the same output as the focused surface
        if let Some(emacs_id) = state.ewm.get_emacs_surface_for_focused_output() {
            module::record_focus(emacs_id, "intercept_redirect", Some("prefix_key"));
            state.ewm.focused_surface_id = emacs_id;
            crate::module::set_focused_id(emacs_id);
            if let Some(window) = state.ewm.id_windows.get(&emacs_id) {
                if let Some(surface) = window.wl_surface() {
                    let emacs_surface: WlSurface = surface.into_owned();
                    state.ewm.keyboard_focus = Some(emacs_surface.clone());
                    // focus_changed handles text_input focus
                    keyboard.set_focus(state, Some(emacs_surface.clone()), serial);

                    // NOTE: We intentionally do NOT send a Focus event here.
                    // The prefix key redirect is temporary for the key sequence,
                    // and sending Focus would cause ewm-layout--refresh to
                    // redirect focus back to the external surface before the
                    // sequence completes (race condition with C-x left/right etc).
                    // Emacs frames handle their own focus via Wayland protocol.

                    // Switch to base layout (index 0) when redirecting to Emacs
                    // This ensures Emacs keybindings work correctly
                    if state.ewm.xkb_current_layout != 0 && !state.ewm.xkb_layout_names.is_empty() {
                        keyboard.with_xkb_state(state, |mut context| {
                            context.set_layout(smithay::input::keyboard::Layout(0));
                        });
                        state.ewm.xkb_current_layout = 0;
                        tracing::info!("Switched to base layout for Emacs redirect");
                    }

                    // Re-send the key to Emacs
                    keyboard.input::<(), _>(
                        state,
                        keycode.into(),
                        key_state,
                        serial,
                        time,
                        |_, _, _| FilterResult::Forward,
                    );
                }
            }
        }
        return KeyboardAction::RedirectToEmacs;
    }

    // Normal key handling - ensure focus is on the right surface
    let target_id = state.ewm.focused_surface_id;
    if let Some(window) = state.ewm.id_windows.get(&target_id) {
        if let Some(surface) = window.wl_surface() {
            let new_focus = surface.into_owned();
            if state.ewm.keyboard_focus.as_ref() != Some(&new_focus) {
                state.ewm.keyboard_focus = Some(new_focus.clone());
                // focus_changed handles text_input focus
                keyboard.set_focus(state, Some(new_focus.clone()), serial);
            }
        }
    }

    // Check if XKB layout changed (e.g., via grp:caps_toggle)
    let current_layout = keyboard.with_xkb_state(state, |context| {
        context.xkb().lock().unwrap().active_layout().0 as usize
    });
    if current_layout != state.ewm.xkb_current_layout {
        state.ewm.xkb_current_layout = current_layout;
        tracing::info!("XKB layout changed to index {}", current_layout);
        // Notify Emacs of layout change
        if !state.ewm.xkb_layout_names.is_empty() {
            state.ewm.queue_event(crate::Event::LayoutSwitched {
                layout: state.ewm
                    .xkb_layout_names
                    .get(current_layout)
                    .cloned()
                    .unwrap_or_default(),
                index: current_layout,
            });
        }
    }

    // Notify idle notifier of user activity
    notify_activity(state);

    KeyboardAction::Forward
}

/// Release all pressed keys (used when window loses focus)
pub fn release_all_keys(state: &mut State) {
    let keyboard = state.ewm.keyboard.clone();
    let pressed = keyboard.pressed_keys();
    if pressed.is_empty() {
        return;
    }

    let serial = SERIAL_COUNTER.next_serial();
    let time = 0u32;

    for keycode in pressed {
        keyboard.input::<(), _>(
            state,
            keycode,
            KeyState::Released,
            serial,
            time,
            |_, _, _| FilterResult::Forward,
        );
    }

    // Clear focus (focus_changed handles text_input)
    keyboard.set_focus(state, None, serial);
    state.ewm.keyboard_focus = None;
}

/// Restore focus to a specific surface
pub fn restore_focus(state: &mut State, surface_id: u32) {
    let keyboard = state.ewm.keyboard.clone();
    if let Some(window) = state.ewm.id_windows.get(&surface_id) {
        if let Some(surface) = window.wl_surface() {
            let serial = SERIAL_COUNTER.next_serial();
            let focus_surface = surface.into_owned();
            state.ewm.keyboard_focus = Some(focus_surface.clone());
            keyboard.set_focus(state, Some(focus_surface.clone()), serial);
            // Update text_input focus
            state.ewm
                .seat
                .text_input()
                .set_focus(Some(focus_surface.clone()));
            state.ewm.seat.text_input().enter();
        }
    }
}

// ============================================================================
// Pointer event handling
// ============================================================================

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend,
        PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    backend::libinput::LibinputInputBackend,
    input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
};

/// Configure a newly added libinput device
pub fn handle_device_added(device: &mut <LibinputInputBackend as InputBackend>::Device) {
    // Enable natural scrolling for touchpads
    if device.config_tap_finger_count() > 0 {
        let _ = device.config_scroll_set_natural_scroll_enabled(true);
        tracing::info!("Enabled natural scroll for touchpad: {:?}", device.name());
    }
}

/// Handle relative pointer motion (mice, trackpoints)
pub fn handle_pointer_motion<B: InputBackend>(
    state: &mut State,
    event: B::PointerMotionEvent,
) -> bool {
    tracy_span!("handle_pointer_motion");
    let (current_x, current_y) = state.ewm.pointer_location;
    let delta = event.delta();
    let (output_w, output_h) = state.ewm.output_size;

    // Calculate new position, clamped to output bounds
    let new_x = (current_x + delta.x).clamp(0.0, output_w as f64);
    let new_y = (current_y + delta.y).clamp(0.0, output_h as f64);
    state.ewm.pointer_location = (new_x, new_y);
    module::set_pointer_location(new_x, new_y);

    let pointer = state.ewm.pointer.clone();
    let serial = SERIAL_COUNTER.next_serial();

    // When locked, route pointer to lock surface instead of normal surfaces
    let under = if state.ewm.is_locked() {
        state.ewm.lock_surface_focus().map(|s| (s, (0.0, 0.0).into()))
    } else {
        state.ewm.surface_under_point((new_x, new_y).into())
    };

    pointer.motion(
        state,
        under.clone(),
        &MotionEvent {
            location: (new_x, new_y).into(),
            serial,
            time: event.time_msec(),
        },
    );

    // Send relative motion event (needed by some games/apps)
    pointer.relative_motion(
        state,
        under,
        &RelativeMotionEvent {
            delta: event.delta(),
            delta_unaccel: event.delta_unaccel(),
            utime: event.time(),
        },
    );

    pointer.frame(state);

    // Notify idle notifier of user activity
    notify_activity(state);

    true // needs redraw
}

/// Handle absolute pointer motion (touchpads in absolute mode, tablets)
pub fn handle_pointer_motion_absolute<B: InputBackend>(
    state: &mut State,
    event: B::PointerMotionAbsoluteEvent,
) -> bool {
    tracy_span!("handle_pointer_motion_absolute");
    let (output_w, output_h) = state.ewm.output_size;
    let pos = event.position_transformed((output_w, output_h).into());
    state.ewm.pointer_location = (pos.x, pos.y);
    module::set_pointer_location(pos.x, pos.y);

    let pointer = state.ewm.pointer.clone();
    let serial = SERIAL_COUNTER.next_serial();

    // When locked, route pointer to lock surface instead of normal surfaces
    let under = if state.ewm.is_locked() {
        state.ewm.lock_surface_focus().map(|s| (s, (0.0, 0.0).into()))
    } else {
        state.ewm.surface_under_point(pos)
    };

    pointer.motion(
        state,
        under,
        &MotionEvent {
            location: pos,
            serial,
            time: event.time_msec(),
        },
    );
    pointer.frame(state);

    // Notify idle notifier of user activity
    notify_activity(state);

    true // needs redraw
}

/// Handle pointer button press/release with click-to-focus
pub fn handle_pointer_button<B: InputBackend>(state: &mut State, event: B::PointerButtonEvent) {
    tracy_span!("handle_pointer_button");
    let pointer = state.ewm.pointer.clone();
    let keyboard = state.ewm.keyboard.clone();
    let serial = SERIAL_COUNTER.next_serial();

    let button_state = match event.state() {
        ButtonState::Pressed => ButtonState::Pressed,
        ButtonState::Released => ButtonState::Released,
    };

    // When locked, skip click-to-focus and just forward to lock surface
    if !state.ewm.is_locked() {
        // Click-to-focus: on button press, focus the surface under pointer
        if button_state == ButtonState::Pressed {
            let (px, py) = state.ewm.pointer_location;
            let focus_info = state.ewm.space.element_under((px, py)).and_then(|(window, _)| {
                let id = state.ewm.window_ids.get(&window).copied()?;
                let surface = window.wl_surface()?.into_owned();
                Some((id, surface))
            });

            if let Some((id, surface)) = focus_info {
                module::record_focus(id, "click", None);
                state.ewm.set_focus(id);
                state.ewm.keyboard_focus = Some(surface.clone());
                // keyboard.set_focus triggers SeatHandler::focus_changed which handles text_input
                keyboard.set_focus(state, Some(surface.clone()), serial);
            }
        }
    }

    pointer.button(
        state,
        &ButtonEvent {
            button: event.button_code(),
            state: button_state,
            serial,
            time: event.time_msec(),
        },
    );
    pointer.frame(state);

    // Notify idle notifier of user activity
    notify_activity(state);
}

/// Handle pointer axis (scroll wheel, touchpad scroll)
pub fn handle_pointer_axis<B: InputBackend>(state: &mut State, event: B::PointerAxisEvent) {
    tracy_span!("handle_pointer_axis");
    let pointer = state.ewm.pointer.clone();
    let keyboard = state.ewm.keyboard.clone();
    let serial = SERIAL_COUNTER.next_serial();

    // When locked, skip scroll-to-focus
    if !state.ewm.is_locked() {
        // Scroll-to-focus: focus the surface under pointer on scroll
        let (px, py) = state.ewm.pointer_location;
        let focus_info = state.ewm.space.element_under((px, py)).and_then(|(window, _)| {
            let id = state.ewm.window_ids.get(&window).copied()?;
            let surface = window.wl_surface()?.into_owned();
            Some((id, surface))
        });

        if let Some((id, surface)) = focus_info {
            module::record_focus(id, "scroll", None);
            state.ewm.set_focus(id);
            state.ewm.keyboard_focus = Some(surface.clone());
            // focus_changed handles text_input focus
            keyboard.set_focus(state, Some(surface.clone()), serial);
        }
    }

    let source = event.source();

    // Get scroll amounts (natural scrolling is handled at libinput device level)
    let horizontal_amount = event.amount(Axis::Horizontal);
    let vertical_amount = event.amount(Axis::Vertical);
    let horizontal_v120 = event.amount_v120(Axis::Horizontal);
    let vertical_v120 = event.amount_v120(Axis::Vertical);

    // Compute continuous values, falling back to v120 if no continuous amount
    let horizontal = horizontal_amount
        .or_else(|| horizontal_v120.map(|v| v / 120.0 * 15.0))
        .unwrap_or(0.0);
    let vertical = vertical_amount
        .or_else(|| vertical_v120.map(|v| v / 120.0 * 15.0))
        .unwrap_or(0.0);

    let mut frame = AxisFrame::new(event.time_msec()).source(source);
    if horizontal != 0.0 {
        frame = frame.value(Axis::Horizontal, horizontal);
        // Send discrete v120 value for wheel scrolling (required by Firefox et al.)
        if let Some(v120) = horizontal_v120 {
            frame = frame.v120(Axis::Horizontal, v120 as i32);
        }
    }
    if vertical != 0.0 {
        frame = frame.value(Axis::Vertical, vertical);
        // Send discrete v120 value for wheel scrolling
        if let Some(v120) = vertical_v120 {
            frame = frame.v120(Axis::Vertical, v120 as i32);
        }
    }

    // For finger scroll (touchpad), send stop events when scrolling ends
    if source == AxisSource::Finger {
        if horizontal_amount == Some(0.0) {
            frame = frame.stop(Axis::Horizontal);
        }
        if vertical_amount == Some(0.0) {
            frame = frame.stop(Axis::Vertical);
        }
    }

    pointer.axis(state, frame);
    pointer.frame(state);

    // Notify idle notifier of user activity
    notify_activity(state);
}
