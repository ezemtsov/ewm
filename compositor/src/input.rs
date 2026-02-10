//! Generic input handling shared between backends
//!
//! This module provides keyboard and pointer event processing that works
//! with any Smithay input backend.

use smithay::{
    backend::input::KeyState,
    input::keyboard::{xkb, FilterResult, KeyboardHandle},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER,
    wayland::seat::WaylandFocus,
    wayland::text_input::TextInputSeat,
};

use crate::{is_kill_combo, Ewm};

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
}

/// Process a keyboard key event
///
/// This handles:
/// - Kill combo detection (Super+Ctrl+Backspace)
/// - Prefix key interception (redirect to Emacs)
/// - Normal key forwarding to focused surface
///
/// Returns the action to take based on the key event.
pub fn handle_keyboard_event(
    state: &mut Ewm,
    keyboard: &KeyboardHandle<Ewm>,
    keycode: u32,
    key_state: KeyState,
    time: u32,
) -> KeyboardAction {
    let serial = SERIAL_COUNTER.next_serial();
    let is_press = key_state == KeyState::Pressed;

    // Clone values needed in the filter closure
    let intercepted_keys = state.intercepted_keys.clone();
    let focus_on_emacs = state.is_focus_on_emacs();
    let text_input_intercept = state.text_input_intercept;

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

            // Check for kill combo first (Super+Ctrl+Backspace)
            if is_kill_combo(keycode, mods.ctrl, mods.logo) {
                return FilterResult::Intercept((2, 0, None)); // 2 = kill
            }

            // Get the raw latin keysym for this key (layout-independent)
            // This ensures intercepted keys work regardless of current XKB layout
            let raw_latin = handle.raw_latin_sym_or_raw_current_sym();
            let modified = handle.modified_sym();
            let keysym = raw_latin.unwrap_or(modified);
            let keysym_raw = keysym.raw();
            let is_intercepted = intercepted_keys.iter().any(|ik| ik.matches(keysym_raw, mods));

            if is_intercepted && !focus_on_emacs {
                // This is an intercepted key and focus is on an external app (not Emacs)
                FilterResult::Intercept((1, keysym_raw, None)) // 1 = redirect to emacs
            } else if text_input_intercept && !focus_on_emacs && !mods.ctrl && !mods.alt && !mods.logo {
                // Text input intercept mode: capture printable keys for Emacs IM processing
                // Skip if any command modifiers are held (let those go to Emacs via intercept-keys)
                let utf8 = xkb::keysym_to_utf8(keysym);
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
                state.pending_events.push(crate::IpcEvent::Key {
                    keysym,
                    utf8: utf8.clone(),
                });
                return KeyboardAction::TextInputIntercepted;
            }
            _ => {}
        }
    }

    if filter_result.as_ref().map(|(c, _, _)| *c) == Some(1) {
        // Switch focus to the Emacs frame on the same output as the focused surface
        let emacs_id = state.get_emacs_surface_for_focused_output();
        state.focused_surface_id = emacs_id;
        if let Some(window) = state.id_windows.get(&emacs_id) {
            if let Some(surface) = window.wl_surface() {
                let emacs_surface: WlSurface = surface.into_owned();
                state.keyboard_focus = Some(emacs_surface.clone());
                keyboard.set_focus(state, Some(emacs_surface.clone()), serial);
                // Update text_input focus
                state.seat.text_input().set_focus(Some(emacs_surface.clone()));
                state.seat.text_input().enter();

                // Switch to base layout (index 0) when redirecting to Emacs
                // This ensures Emacs keybindings work correctly
                if state.xkb_current_layout != 0 && !state.xkb_layout_names.is_empty() {
                    keyboard.with_xkb_state(state, |mut context| {
                        context.set_layout(smithay::input::keyboard::Layout(0));
                    });
                    state.xkb_current_layout = 0;
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
        return KeyboardAction::RedirectToEmacs;
    }

    // Normal key handling - ensure focus is on the right surface
    let target_id = state.focused_surface_id;
    if let Some(window) = state.id_windows.get(&target_id) {
        if let Some(surface) = window.wl_surface() {
            let new_focus = surface.into_owned();
            if state.keyboard_focus.as_ref() != Some(&new_focus) {
                state.keyboard_focus = Some(new_focus.clone());
                keyboard.set_focus(state, Some(new_focus.clone()), serial);
                // Update text_input focus
                state.seat.text_input().set_focus(Some(new_focus.clone()));
                state.seat.text_input().enter();
            }
        }
    }

    // Check if XKB layout changed (e.g., via grp:caps_toggle)
    let current_layout = keyboard.with_xkb_state(state, |context| {
        context.xkb().lock().unwrap().active_layout().0 as usize
    });
    if current_layout != state.xkb_current_layout {
        state.xkb_current_layout = current_layout;
        tracing::info!("XKB layout changed to index {}", current_layout);
        // Notify Emacs of layout change
        if !state.xkb_layout_names.is_empty() {
            state.pending_events.push(crate::IpcEvent::LayoutSwitched {
                layout: state.xkb_layout_names.get(current_layout).cloned().unwrap_or_default(),
                index: current_layout,
            });
        }
    }

    KeyboardAction::Forward
}

/// Release all pressed keys (used when window loses focus)
pub fn release_all_keys(state: &mut Ewm, keyboard: &KeyboardHandle<Ewm>) {
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

    // Clear focus
    keyboard.set_focus(state, None, serial);
    state.keyboard_focus = None;
    // Clear text_input focus
    state.seat.text_input().leave();
    state.seat.text_input().set_focus(None);
}

/// Restore focus to a specific surface
pub fn restore_focus(state: &mut Ewm, keyboard: &KeyboardHandle<Ewm>, surface_id: u32) {
    if let Some(window) = state.id_windows.get(&surface_id) {
        if let Some(surface) = window.wl_surface() {
            let serial = SERIAL_COUNTER.next_serial();
            let focus_surface = surface.into_owned();
            state.keyboard_focus = Some(focus_surface.clone());
            keyboard.set_focus(state, Some(focus_surface.clone()), serial);
            // Update text_input focus
            state.seat.text_input().set_focus(Some(focus_surface.clone()));
            state.seat.text_input().enter();
        }
    }
}
