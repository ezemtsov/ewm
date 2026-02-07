//! Generic input handling shared between backends
//!
//! This module provides keyboard and pointer event processing that works
//! with any Smithay input backend.

use smithay::{
    backend::input::KeyState,
    input::keyboard::{FilterResult, KeyboardHandle},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER,
    wayland::seat::WaylandFocus,
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
    let prefix_keys = state.prefix_keys.clone();
    let current_focus_id = state.focused_surface_id;

    // Process key with filter to detect prefix keys and kill combo
    let filter_result = keyboard.input::<u8, _>(
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
                return FilterResult::Intercept(2); // 2 = kill
            }

            // Get the keysym for this key and check if it matches any prefix key
            let keysym = handle.modified_sym();
            let is_prefix = prefix_keys.iter().any(|pk| pk.matches(keysym.raw(), mods));

            if is_prefix && current_focus_id != 1 {
                // This is a prefix key and focus is not on Emacs
                FilterResult::Intercept(1) // 1 = redirect to emacs
            } else {
                FilterResult::Forward
            }
        },
    );

    // Determine action from filter result
    if filter_result == Some(2) {
        return KeyboardAction::Shutdown;
    }

    if filter_result == Some(1) {
        // Switch focus to Emacs (surface 1)
        state.focused_surface_id = 1;
        if let Some(window) = state.id_windows.get(&1) {
            if let Some(surface) = window.wl_surface() {
                let emacs_surface: WlSurface = surface.into_owned();
                state.keyboard_focus = Some(emacs_surface.clone());
                keyboard.set_focus(state, Some(emacs_surface.clone()), serial);

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
                keyboard.set_focus(state, Some(new_focus), serial);
            }
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
}

/// Restore focus to a specific surface
pub fn restore_focus(state: &mut Ewm, keyboard: &KeyboardHandle<Ewm>, surface_id: u32) {
    if let Some(window) = state.id_windows.get(&surface_id) {
        if let Some(surface) = window.wl_surface() {
            let serial = SERIAL_COUNTER.next_serial();
            let focus_surface = surface.into_owned();
            state.keyboard_focus = Some(focus_surface.clone());
            keyboard.set_focus(state, Some(focus_surface), serial);
        }
    }
}
