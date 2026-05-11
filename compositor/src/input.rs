//! Generic input handling shared between backends
//!
//! This module provides keyboard and pointer event processing that works
//! with any Smithay input backend.
//!
//! # Design Invariants
//!
//! 1. **Key interception**: Super-prefixed bindings are intercepted and redirected to
//!    Emacs. The intercepted key list comes from Emacs via `ewm-intercept-keys-module`.
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
    input::keyboard::{keysyms, xkb, FilterResult, ModifiersState},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER,
    wayland::seat::WaylandFocus,
};

use crate::{module, State};

/// Build target modifier state: replace logo (Super) with the target modifier.
fn emulate_modifiers(current: &ModifiersState, target_mod: &str) -> ModifiersState {
    let mut mods = *current;
    mods.logo = false;
    match target_mod {
        "ctrl" => mods.ctrl = true,
        "alt" => mods.alt = true,
        "shift" => mods.shift = true,
        "super" => mods.logo = true,
        _ => {}
    }
    mods
}

/// Notify the idle notifier of user activity and reset native idle timer
fn notify_activity(state: &mut State) {
    state.ewm.notify_activity();
    state.ewm.reset_idle_timer();
}

/// Pointer-specific activity: idle notify plus cursor un-hide.
fn notify_pointer_activity(state: &mut State) {
    notify_activity(state);
    state.ewm.reset_cursor_hide_timer();
}

/// Unified decision from the keyboard filter.
/// Returned by `input_intercept`. The caller decides how to forward.
#[derive(Debug, Clone)]
enum KeyAction {
    /// Forward to focused client as-is
    Forward,
    /// Emulate: translate modifiers and forward to client
    Emulate(String),
    /// Redirect to Emacs (intercepted key)
    Redirect(u32),
    /// Text input intercept: printable key for Emacs IM processing
    TextInput(u32, Option<String>),
    /// VT switch (Ctrl+Alt+F1-F12)
    VtSwitch(i32),
}

/// Result of processing a keyboard event
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardAction {
    /// Normal key forwarding - nothing special happened
    Forward,
    /// Prefix key intercepted - redirect focus to Emacs
    RedirectToEmacs,
    /// Key intercepted for text input (sent to Emacs via IPC)
    TextInputIntercepted,
    /// VT switch requested (Ctrl+Alt+F1-F12)
    ChangeVt(i32),
    /// Key was emulated (modifier translated and forwarded to client)
    Emulated,
}

/// Process a keyboard key event
///
/// This handles:
/// - Kill combo detection (Ctrl+Alt+Backspace)
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

    if is_press {
        state.ewm.hide_cursor_for_typing();
    }

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
                if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12)
                    .contains(&modified)
                {
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

    // Check if a fullscreen surface is logically focused (Emacs selected-window).
    // Uses the layout's `focused` flag, NOT keyboard focus. After intercept_redirect
    // keyboard focus is temporarily on Emacs even though the surface is fullscreen.
    let fullscreen_view = state
        .ewm
        .output_strips
        .values()
        .flat_map(|strip| strip.surfaces())
        .find(|e| e.focused && state.ewm.effective_fullscreen(e))
        .map(|e| (e.view_id, e.surface_id));
    let fullscreen_surface_id = fullscreen_view.map(|(_, sid)| sid);

    // When a fullscreen surface exists, ensure keyboard focus is on it before processing keys.
    // After intercept_redirect (e.g. s-f to enter fullscreen), Wayland keyboard focus stays on
    // Emacs. Without this, FilterResult::Forward sends keys to Emacs instead of the surface.
    if let Some((view_id, surface_id)) = fullscreen_view {
        if state.ewm.focused_surface_id() != surface_id {
            state.ewm.set_focus_view(view_id, "fullscreen_focus_redirect");
            state.sync_keyboard_focus();
        }
    }

    // Computed after fullscreen restore so it reflects the corrected focus state
    let focus_on_emacs = state.ewm.is_focus_on_emacs();

    // During a prefix sequence (C-x ...), Emacs needs Latin keysyms for
    // keybinding dispatch. Temporarily set base layout for this key event,
    // then restore immediately after. No persistent state needed.
    let prefix_saved_layout = if module::get_in_prefix_sequence()
        && focus_on_emacs
        && state.ewm.xkb_current_layout != 0
    {
        let saved = state.ewm.xkb_current_layout;
        keyboard.with_xkb_state(state, |mut context| {
            context.set_layout(smithay::input::keyboard::Layout(0));
        });
        Some(saved)
    } else {
        None
    };

    // Key emulation release
    // If this is a release of a key we previously emulated, translate its
    // modifiers too so the client sees a consistent press/release pair.
    if !is_press {
        if let Some(target_mod) = state.ewm.emulated_keycodes.remove(&keycode) {
            let (_, _mods_changed) =
                keyboard.input_intercept(state, keycode.into(), key_state, |_, _, _| {});

            let saved_mods = keyboard.modifier_state();
            let target_mods = emulate_modifiers(&saved_mods, &target_mod);
            keyboard.set_modifier_state(target_mods);
            keyboard.advertise_modifier_state(state);
            keyboard.input_forward(state, keycode.into(), key_state, serial, time, false);
            keyboard.set_modifier_state(saved_mods);

            notify_activity(state);
            return KeyboardAction::Emulated;
        }
    }

    // Clone values needed in the filter closure
    let intercepted_keys = crate::module::get_intercepted_keys();
    let emulated_keys = crate::module::get_emulated_keys();
    let text_input_intercept = state.ewm.text_input_intercept;

    // Use input_intercept + explicit input_forward so we can handle
    // emulation (modifier translation) between the two steps.
    let (action, mods_changed) =
        keyboard.input_intercept(state, keycode.into(), key_state, |_, mods, handle| {
            if !is_press {
                return KeyAction::Forward;
            }

            // Get the modified keysym (with modifiers applied by XKB)
            let modified = handle.modified_sym();
            let modified_raw = modified.raw();

            // Check for VT switch keys (Ctrl+Alt+F1-F12 -> XF86Switch_VT_*)
            if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12)
                .contains(&modified_raw)
            {
                let vt = (modified_raw - keysyms::KEY_XF86Switch_VT_1 + 1) as i32;
                return KeyAction::VtSwitch(vt);
            }

            // Get the raw latin keysym for this key (layout-independent)
            let raw_latin = handle.raw_latin_sym_or_raw_current_sym();
            let keysym = raw_latin.unwrap_or(modified);
            let keysym_raw = keysym.raw();

            // Emulation check (before interception)
            // Emulated keys translate modifiers for non-Emacs surfaces
            // (e.g. Super+c -> Ctrl+c for clipboard copy)
            if !focus_on_emacs {
                if let Some(ek) = emulated_keys
                    .iter()
                    .find(|ek| ek.matches(keysym_raw, modified_raw, mods))
                {
                    return KeyAction::Emulate(ek.target_mod.clone());
                }
            }

            // Interception check
            let matched_key = intercepted_keys
                .iter()
                .find(|ik| ik.matches(keysym_raw, modified_raw, mods));

            if let Some(ik) = matched_key {
                // Fullscreen mode: only keys marked allow_fullscreen are redirected
                if fullscreen_surface_id.is_some() {
                    if ik.allow_fullscreen && !focus_on_emacs {
                        module::set_in_prefix_sequence(true);
                        return KeyAction::Redirect(keysym_raw);
                    }
                    return KeyAction::Forward;
                }

                if !focus_on_emacs {
                    module::set_in_prefix_sequence(true);
                    return KeyAction::Redirect(keysym_raw);
                }
                return KeyAction::Forward;
            }

            if text_input_intercept && !focus_on_emacs && !mods.ctrl && !mods.alt && !mods.logo {
                let utf8 = xkb::keysym_to_utf8(modified);
                if !utf8.is_empty() && !utf8.chars().all(|c| c.is_control()) {
                    KeyAction::TextInput(keysym_raw, Some(utf8))
                } else {
                    KeyAction::Forward
                }
            } else {
                KeyAction::Forward
            }
        });

    // Restore layout after prefix sequence temp reset
    if let Some(saved) = prefix_saved_layout {
        keyboard.with_xkb_state(state, |mut context| {
            context.set_layout(smithay::input::keyboard::Layout(saved as u32));
        });
    }

    // Handle the filter decision
    match action {
        KeyAction::Forward => {
            state.sync_keyboard_focus();
            keyboard.input_forward(state, keycode.into(), key_state, serial, time, mods_changed);
            if is_press && module::DEBUG_MODE.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::debug!(
                    "key forward: keycode={} surface={}",
                    keycode,
                    state.ewm.focused_surface_id()
                );
            }
        }

        KeyAction::Emulate(ref target_mod) => {
            // Track this keycode so its release is also emulated
            state
                .ewm
                .emulated_keycodes
                .insert(keycode, target_mod.clone());

            // Swap modifiers (e.g. Super -> Ctrl) and forward to the surface.
            // Smithay sends key BEFORE modifiers in input_forward, but the client
            // needs the new modifiers BEFORE the key to interpret it correctly.
            // So: advertise modifiers first, then forward key without modifiers.
            let saved_mods = keyboard.modifier_state();
            let target_mods = emulate_modifiers(&saved_mods, target_mod);
            keyboard.set_modifier_state(target_mods);
            keyboard.advertise_modifier_state(state);
            keyboard.input_forward(state, keycode.into(), key_state, serial, time, false);
            keyboard.set_modifier_state(saved_mods);

            tracing::debug!(
                "emulate: keycode={} target_mod={} surface={}",
                keycode,
                target_mod,
                state.ewm.focused_surface_id()
            );
            notify_activity(state);
            return KeyboardAction::Emulated;
        }

        KeyAction::TextInput(keysym, ref utf8) => {
            state.ewm.queue_event(crate::Event::Key {
                keysym,
                utf8: utf8.clone(),
            });
            return KeyboardAction::TextInputIntercepted;
        }

        KeyAction::VtSwitch(vt) => return KeyboardAction::ChangeVt(vt),

        KeyAction::Redirect(keysym_val) => {
            tracing::info!(
                "intercept_redirect: keycode={} keysym=0x{:x} from surface {}",
                keycode,
                keysym_val,
                state.ewm.focused_surface_id()
            );

            if let Some(emacs_id) = state.ewm.get_emacs_surface_for_focused_output() {
                let context = format!("keycode={} keysym=0x{:x}", keycode, keysym_val);
                module::record_focus(emacs_id, "intercept_redirect", Some(&context));
                state.ewm.set_focus_state(emacs_id, 0);
                if let Some(window) = state.ewm.id_windows.get(&emacs_id) {
                    if let Some(surface) = window.wl_surface() {
                        let emacs_surface: WlSurface = surface.into_owned();
                        state.ewm.keyboard_focus = Some(emacs_surface.clone());
                        keyboard.set_focus(state, Some(emacs_surface.clone()), serial);

                        // NOTE: We intentionally do NOT send a Focus event here.
                        // The prefix key redirect is temporary for the key sequence,
                        // and sending Focus would cause ewm-layout--refresh to
                        // redirect focus back to the external surface before the
                        // sequence completes (race condition with C-x left/right etc).

                        // Temporarily switch to base layout so the re-sent key
                        // produces Latin keysyms for Emacs keybindings.
                        let saved_layout = state.ewm.xkb_current_layout;
                        if saved_layout != 0 && !state.ewm.xkb_layout_names.is_empty() {
                            keyboard.with_xkb_state(state, |mut context| {
                                context.set_layout(smithay::input::keyboard::Layout(0));
                            });
                        }

                        // Re-send the key to Emacs (sees layout 0)
                        keyboard.input::<(), _>(
                            state,
                            keycode.into(),
                            key_state,
                            serial,
                            time,
                            |_, _, _| FilterResult::Forward,
                        );

                        // Restore layout: external surface keeps its layout
                        if saved_layout != 0 {
                            keyboard.with_xkb_state(state, |mut context| {
                                context.set_layout(smithay::input::keyboard::Layout(
                                    saved_layout as u32,
                                ));
                            });
                        }
                    }
                }
            }
            return KeyboardAction::RedirectToEmacs;
        }
    }

    // Check if XKB layout changed (e.g., via grp:caps_toggle)
    let current_layout = keyboard.with_xkb_state(state, |context| {
        context.xkb().lock().unwrap().active_layout().0 as usize
    });
    if current_layout != state.ewm.xkb_current_layout {
        state.ewm.xkb_current_layout = current_layout;
        tracing::info!("XKB layout changed to index {}", current_layout);
        if !state.ewm.xkb_layout_names.is_empty() {
            state.ewm.queue_event(crate::Event::LayoutSwitched {
                layout: state
                    .ewm
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
pub fn restore_focus(state: &mut State, surface_id: u64) {
    if state.ewm.id_windows.contains_key(&surface_id) {
        state.ewm.set_focus(surface_id, false, "restore_focus", None);
        state.sync_keyboard_focus();
    }
}

// Pointer event handling

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend,
        PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    utils::Point,
    wayland::{
        compositor::RegionAttributes,
        pointer_constraints::{with_pointer_constraint, PointerConstraint},
    },
};

// Libinput device configuration

use smithay::reexports::input as libinput;

#[derive(Clone, Copy, Debug)]
pub enum AccelProfile {
    Flat,
    Adaptive,
}

impl From<AccelProfile> for libinput::AccelProfile {
    fn from(p: AccelProfile) -> Self {
        match p {
            AccelProfile::Flat => libinput::AccelProfile::Flat,
            AccelProfile::Adaptive => libinput::AccelProfile::Adaptive,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ClickMethod {
    ButtonAreas,
    Clickfinger,
}

impl From<ClickMethod> for libinput::ClickMethod {
    fn from(m: ClickMethod) -> Self {
        match m {
            ClickMethod::ButtonAreas => libinput::ClickMethod::ButtonAreas,
            ClickMethod::Clickfinger => libinput::ClickMethod::Clickfinger,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ScrollMethod {
    NoScroll,
    TwoFinger,
    Edge,
    OnButtonDown,
}

impl From<ScrollMethod> for libinput::ScrollMethod {
    fn from(m: ScrollMethod) -> Self {
        match m {
            ScrollMethod::NoScroll => libinput::ScrollMethod::NoScroll,
            ScrollMethod::TwoFinger => libinput::ScrollMethod::TwoFinger,
            ScrollMethod::Edge => libinput::ScrollMethod::Edge,
            ScrollMethod::OnButtonDown => libinput::ScrollMethod::OnButtonDown,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum TapButtonMap {
    LeftRightMiddle,
    LeftMiddleRight,
}

impl From<TapButtonMap> for libinput::TapButtonMap {
    fn from(m: TapButtonMap) -> Self {
        match m {
            TapButtonMap::LeftRightMiddle => libinput::TapButtonMap::LeftRightMiddle,
            TapButtonMap::LeftMiddleRight => libinput::TapButtonMap::LeftMiddleRight,
        }
    }
}

/// Input device type for configuration matching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceType {
    Touchpad,
    Mouse,
    Trackball,
    Trackpoint,
    Keyboard,
}

impl DeviceType {
    /// Parse from string (Elisp symbol name).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "touchpad" => Some(DeviceType::Touchpad),
            "mouse" => Some(DeviceType::Mouse),
            "trackball" => Some(DeviceType::Trackball),
            "trackpoint" => Some(DeviceType::Trackpoint),
            "keyboard" => Some(DeviceType::Keyboard),
            _ => None,
        }
    }
}

/// A single input configuration entry, either a type default or device-specific.
///
/// Type defaults have `device: None` and `device_type: Some(...)`.
/// Device-specific overrides have `device: Some("name")` and optional `device_type`.
#[derive(Clone, Debug, Default)]
pub struct InputConfigEntry {
    /// None = type default, Some("device name") = device-specific override
    pub device: Option<String>,
    /// Required for type defaults, optional for device overrides (auto-detected)
    pub device_type: Option<DeviceType>,
    // All possible settings (superset of touchpad + mouse)
    pub natural_scroll: Option<bool>,
    pub tap: Option<bool>,
    pub dwt: Option<bool>,
    pub accel_speed: Option<f64>,
    pub accel_profile: Option<AccelProfile>,
    pub click_method: Option<ClickMethod>,
    pub scroll_method: Option<ScrollMethod>,
    pub left_handed: Option<bool>,
    pub middle_emulation: Option<bool>,
    pub tap_button_map: Option<TapButtonMap>,
    // Keyboard-specific settings
    pub repeat_delay: Option<i32>,
    pub repeat_rate: Option<i32>,
    pub xkb_layouts: Option<String>,
    pub xkb_variants: Option<String>,
    pub xkb_options: Option<String>,
}

/// Detect the type of a libinput device.
///
/// - Touchpad: `config_tap_finger_count() > 0`
/// - Trackball/trackpoint: udev properties `ID_INPUT_TRACKBALL` / `ID_INPUT_POINTINGSTICK`
/// - Mouse: has Pointer capability and is not touchpad/trackball/trackpoint
fn detect_device_type(device: &libinput::Device) -> Option<DeviceType> {
    if device.config_tap_finger_count() > 0 {
        return Some(DeviceType::Touchpad);
    }

    let mut is_trackball = false;
    let mut is_trackpoint = false;
    if let Some(udev_device) = unsafe { device.udev_device() } {
        is_trackball = udev_device.property_value("ID_INPUT_TRACKBALL").is_some();
        is_trackpoint = udev_device
            .property_value("ID_INPUT_POINTINGSTICK")
            .is_some();
    }

    if is_trackball {
        Some(DeviceType::Trackball)
    } else if is_trackpoint {
        Some(DeviceType::Trackpoint)
    } else if device.has_capability(libinput::DeviceCapability::Pointer) {
        Some(DeviceType::Mouse)
    } else {
        None
    }
}

/// Resolve a config value: device-specific -> type-default -> hardware default.
macro_rules! resolve {
    ($device_cfg:expr, $type_cfg:expr, $field:ident, $default:expr) => {
        $device_cfg
            .and_then(|c| c.$field)
            .or_else(|| $type_cfg.and_then(|c| c.$field))
            .unwrap_or_else(|| $default)
    };
}

/// Resolve an optional config value (for enum settings with Option defaults).
macro_rules! resolve_opt {
    ($device_cfg:expr, $type_cfg:expr, $field:ident, $default:expr) => {
        $device_cfg
            .and_then(|c| c.$field)
            .or_else(|| $type_cfg.and_then(|c| c.$field))
            .map(Into::into)
            .or_else(|| $default)
    };
}

/// Apply libinput settings to a device using 3-level cascade:
/// device-specific config -> type-default config -> hardware default.
pub fn apply_libinput_settings(device: &mut libinput::Device, configs: &[InputConfigEntry]) {
    let detected_type = match detect_device_type(device) {
        Some(t) => t,
        None => return,
    };

    let device_name = device.name().to_owned();
    let device_cfg = configs
        .iter()
        .find(|c| c.device.as_deref() == Some(device_name.as_str()));
    let type_cfg = configs
        .iter()
        .find(|c| c.device.is_none() && c.device_type == Some(detected_type));

    tracing::debug!("Configuring {:?}: {}", detected_type, device_name);

    // Settings common to all pointer devices
    let _ = device.config_scroll_set_natural_scroll_enabled(resolve!(
        device_cfg,
        type_cfg,
        natural_scroll,
        device.config_scroll_default_natural_scroll_enabled()
    ));
    let _ = device.config_accel_set_speed(resolve!(
        device_cfg,
        type_cfg,
        accel_speed,
        device.config_accel_default_speed()
    ));
    let _ = device.config_left_handed_set(resolve!(
        device_cfg,
        type_cfg,
        left_handed,
        device.config_left_handed_default()
    ));
    let _ = device.config_middle_emulation_set_enabled(resolve!(
        device_cfg,
        type_cfg,
        middle_emulation,
        device.config_middle_emulation_default_enabled()
    ));

    if let Some(profile) = resolve_opt!(
        device_cfg,
        type_cfg,
        accel_profile,
        device.config_accel_default_profile()
    ) {
        let _ = device.config_accel_set_profile(profile);
    }

    if let Some(method) = resolve_opt!(
        device_cfg,
        type_cfg,
        scroll_method,
        device.config_scroll_default_method()
    ) {
        let _ = device.config_scroll_set_method(method);
    }

    // Touchpad-specific settings
    if detected_type == DeviceType::Touchpad {
        let _ = device.config_tap_set_enabled(resolve!(
            device_cfg,
            type_cfg,
            tap,
            device.config_tap_default_enabled()
        ));
        let _ = device.config_dwt_set_enabled(resolve!(
            device_cfg,
            type_cfg,
            dwt,
            device.config_dwt_default_enabled()
        ));

        if let Some(method) = resolve_opt!(
            device_cfg,
            type_cfg,
            click_method,
            device.config_click_default_method()
        ) {
            let _ = device.config_click_set_method(method);
        }

        if let Some(map) = resolve_opt!(
            device_cfg,
            type_cfg,
            tap_button_map,
            device.config_tap_default_button_map()
        ) {
            let _ = device.config_tap_set_button_map(map);
        }
    }
}

/// Parse an acceleration profile string.
pub fn parse_accel_profile(s: &str) -> Option<AccelProfile> {
    match s {
        "flat" => Some(AccelProfile::Flat),
        "adaptive" => Some(AccelProfile::Adaptive),
        _ => None,
    }
}

/// Parse a click method string.
pub fn parse_click_method(s: &str) -> Option<ClickMethod> {
    match s {
        "button-areas" => Some(ClickMethod::ButtonAreas),
        "clickfinger" => Some(ClickMethod::Clickfinger),
        _ => None,
    }
}

/// Parse a scroll method string.
pub fn parse_scroll_method(s: &str) -> Option<ScrollMethod> {
    match s {
        "no-scroll" => Some(ScrollMethod::NoScroll),
        "two-finger" => Some(ScrollMethod::TwoFinger),
        "edge" => Some(ScrollMethod::Edge),
        "on-button-down" => Some(ScrollMethod::OnButtonDown),
        _ => None,
    }
}

/// Parse a tap button map string.
pub fn parse_tap_button_map(s: &str) -> Option<TapButtonMap> {
    match s {
        "left-right-middle" => Some(TapButtonMap::LeftRightMiddle),
        "left-middle-right" => Some(TapButtonMap::LeftMiddleRight),
        _ => None,
    }
}

/// Result of checking for an active pointer constraint.
enum ActiveConstraint {
    /// Pointer is locked in place, no position change allowed.
    Locked,
    /// Pointer is confined to a surface, optionally within a region.
    Confined(Option<RegionAttributes>),
}

/// Check for an active constraint on `surface` at `pos_within_surface`.
/// Returns the constraint type if active, or `None`.
fn active_constraint(
    surface: &WlSurface,
    pointer: &smithay::input::pointer::PointerHandle<State>,
    pos_within_surface: Point<f64, smithay::utils::Logical>,
) -> Option<ActiveConstraint> {
    with_pointer_constraint(surface, pointer, |constraint| {
        let constraint = constraint?;
        if !constraint.is_active() {
            return None;
        }
        // Constraint does not apply if not within region.
        if let Some(region) = constraint.region() {
            if !region.contains(pos_within_surface.to_i32_round()) {
                return None;
            }
        }
        match &*constraint {
            PointerConstraint::Locked(_) => Some(ActiveConstraint::Locked),
            PointerConstraint::Confined(c) => Some(ActiveConstraint::Confined(c.region().cloned())),
        }
    })
}

/// Handle relative pointer motion (mice, trackpoints)
pub fn handle_pointer_motion<B: InputBackend>(
    state: &mut State,
    event: B::PointerMotionEvent,
) -> bool {
    tracy_span!("handle_pointer_motion");
    let (current_x, current_y) = state.ewm.pointer_location();
    let delta = event.delta();

    // Clip motion against the union of output geometries: if the new
    // position lands in dead space between/outside outputs, snap it back
    // to the previous output's bounds.
    let target = Point::from((current_x + delta.x, current_y + delta.y));
    let previous = Point::from((current_x, current_y));
    let new_pos = state.ewm.clamp_pointer_to_outputs(target, previous);
    let new_x = new_pos.x;
    let new_y = new_pos.y;

    let pointer = state.ewm.pointer.clone();
    let serial = SERIAL_COUNTER.next_serial();

    // Check active pointer constraints (locked or confined).
    let mut pointer_confined = None;
    if let Some((ref surface, surface_loc)) = state.ewm.pointer_focus {
        let pos_within_surface = Point::from((current_x, current_y)) - surface_loc;

        match active_constraint(surface, &pointer, pos_within_surface) {
            Some(ActiveConstraint::Locked) => {
                // Pointer locked: send only relative motion, no position change.
                pointer.relative_motion(
                    state,
                    state.ewm.pointer_focus.clone(),
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );
                pointer.frame(state);
                return true;
            }
            Some(ActiveConstraint::Confined(region)) => {
                pointer_confined = Some((state.ewm.pointer_focus.clone().unwrap(), region));
            }
            None => {}
        }
    }

    // Handle confined pointer: prevent leaving the surface/region.
    if let Some((focus_surface, region)) = &pointer_confined {
        let new_pos: Point<f64, _> = (new_x, new_y).into();
        let under = state.ewm.surface_under_point(new_pos);
        let mut prevent = false;

        // Prevent leaving the focused surface.
        if Some(&focus_surface.0) != under.as_ref().map(|(s, _)| s) {
            prevent = true;
        }

        // Prevent leaving the confine region, if any.
        if let Some(region) = region {
            let local = new_pos - focus_surface.1;
            if !region.contains(local.to_i32_round()) {
                prevent = true;
            }
        }

        if prevent {
            pointer.relative_motion(
                state,
                Some(focus_surface.clone()),
                &RelativeMotionEvent {
                    delta: event.delta(),
                    delta_unaccel: event.delta_unaccel(),
                    utime: event.time(),
                },
            );
            pointer.frame(state);
            notify_pointer_activity(state);
            return true;
        }
    }

    // When locked, route pointer to lock surface instead of normal surfaces
    let under = if state.ewm.is_locked() {
        state
            .ewm
            .lock_surface_focus()
            .map(|s| (s, (0.0, 0.0).into()))
    } else {
        state.ewm.surface_under_point((new_x, new_y).into())
    };

    // Activate pending constraints only on focus change (not every motion).
    let focus_changed =
        state.ewm.pointer_focus.as_ref().map(|(s, _)| s) != under.as_ref().map(|(s, _)| s);
    state.ewm.pointer_focus = under.clone();

    let view_id = state.ewm.view_id_under((new_x, new_y).into());
    let entry_changed = std::mem::replace(&mut state.ewm.pointer_view_id, view_id) != view_id;

    if state.ewm.focus_follows_mouse_mode
        && (focus_changed || entry_changed)
        && !state.ewm.is_strip_animating_at((new_x, new_y).into())
    {
        state.ewm.focus_follows_mouse(&under, (new_x, new_y).into());
    }

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
    notify_pointer_activity(state);

    if focus_changed {
        state.maybe_activate_pointer_constraint();
    }

    true // needs redraw
}

/// Handle absolute pointer motion (touchpads in absolute mode, tablets)
pub fn handle_pointer_motion_absolute<B: InputBackend>(
    state: &mut State,
    event: B::PointerMotionAbsoluteEvent,
) -> bool {
    tracy_span!("handle_pointer_motion_absolute");
    let output_size = state.ewm.output_size;
    let pos = event.position_transformed(output_size);

    let pointer = state.ewm.pointer.clone();
    let serial = SERIAL_COUNTER.next_serial();

    // Check active pointer constraints (locked or confined).
    let mut pointer_confined = None;
    if let Some((ref surface, surface_loc)) = state.ewm.pointer_focus {
        let (cx, cy) = state.ewm.pointer_location();
        let pos_within_surface = Point::from((cx, cy)) - surface_loc;

        match active_constraint(surface, &pointer, pos_within_surface) {
            Some(ActiveConstraint::Locked) => {
                pointer.frame(state);
                return true;
            }
            Some(ActiveConstraint::Confined(region)) => {
                pointer_confined = Some((state.ewm.pointer_focus.clone().unwrap(), region));
            }
            None => {}
        }
    }

    // Handle confined pointer: prevent leaving the surface/region.
    if let Some((focus_surface, region)) = &pointer_confined {
        let under = state.ewm.surface_under_point(pos);
        let mut prevent = false;

        if Some(&focus_surface.0) != under.as_ref().map(|(s, _)| s) {
            prevent = true;
        }

        if let Some(region) = region {
            let local = pos - focus_surface.1;
            if !region.contains(local.to_i32_round()) {
                prevent = true;
            }
        }

        if prevent {
            pointer.frame(state);
            notify_pointer_activity(state);
            return true;
        }
    }

    // When locked, route pointer to lock surface instead of normal surfaces
    let under = if state.ewm.is_locked() {
        state
            .ewm
            .lock_surface_focus()
            .map(|s| (s, (0.0, 0.0).into()))
    } else {
        state.ewm.surface_under_point(pos)
    };

    // Activate pending constraints only on focus change (not every motion).
    let focus_changed =
        state.ewm.pointer_focus.as_ref().map(|(s, _)| s) != under.as_ref().map(|(s, _)| s);
    state.ewm.pointer_focus = under.clone();

    let view_id = state.ewm.view_id_under(pos);
    let entry_changed = std::mem::replace(&mut state.ewm.pointer_view_id, view_id) != view_id;

    if state.ewm.focus_follows_mouse_mode
        && (focus_changed || entry_changed)
        && !state.ewm.is_strip_animating_at(pos)
    {
        state.ewm.focus_follows_mouse(&under, pos);
    }

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
    notify_pointer_activity(state);

    if focus_changed {
        state.maybe_activate_pointer_constraint();
    }

    true // needs redraw
}

/// Handle pointer button press/release with click-to-focus
pub fn handle_pointer_button<B: InputBackend>(state: &mut State, event: B::PointerButtonEvent) {
    tracy_span!("handle_pointer_button");
    let pointer = state.ewm.pointer.clone();
    let serial = SERIAL_COUNTER.next_serial();

    let button_state = match event.state() {
        ButtonState::Pressed => ButtonState::Pressed,
        ButtonState::Released => ButtonState::Released,
    };

    // When locked, skip click-to-focus and just forward to lock surface
    if !state.ewm.is_locked() {
        // Click-to-focus: on button press, focus the surface under pointer
        if button_state == ButtonState::Pressed {
            let (px, py) = state.ewm.pointer_location();
            let pos = (px, py).into();

            // Check if a layer surface is under the pointer
            let layer_under = state.ewm.layer_under_point(pos);
            let on_layer = layer_under.is_some();
            state.ewm.focus_layer_surface_if_on_demand(layer_under);

            if !on_layer {
                if let Some(id) = state.ewm.focus_under(pos, "click") {
                    state.ewm.queue_event(crate::Event::Focus {
                        id,
                        x: None,
                        y: None,
                    });
                }
            }
        }
    }

    // Sync keyboard focus before forwarding the button event,
    // so the correct surface receives any subsequent key events.
    state.sync_keyboard_focus();

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
    notify_pointer_activity(state);
}

/// In-flight 3-finger swipe gesture state for strip scrolling.
#[derive(Default)]
pub struct GestureState {
    /// 16px deadzone accumulator before committing to a horizontal axis.
    pub cumulative: Option<(f64, f64)>,
    /// Output the gesture is driving (set on horizontal commit).
    pub active_output: Option<String>,
}

/// Pixel² threshold before committing the gesture to an axis (matches niri).
const GESTURE_DEADZONE_SQ: f64 = 16.0 * 16.0;

fn working_area_width(state: &State, name: &str) -> f64 {
    state
        .ewm
        .find_output(name)
        .map(|o| state.ewm.get_working_area(&o).size.w as f64)
        .unwrap_or(0.0)
}

/// Three-finger swipe begin: start the deadzone accumulator and intercept
/// the gesture from any client.  Other finger counts pass through.
pub fn handle_gesture_swipe_begin<B: InputBackend>(
    state: &mut State,
    event: B::GestureSwipeBeginEvent,
) {
    use smithay::backend::input::GestureBeginEvent;
    if event.fingers() == 3 {
        state.ewm.gesture.cumulative = Some((0.0, 0.0));
        return;
    }
    let pointer = state.ewm.pointer.clone();
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    pointer.gesture_swipe_begin(
        state,
        &smithay::input::pointer::GestureSwipeBeginEvent {
            serial,
            time: event.time_msec(),
            fingers: event.fingers(),
        },
    );
}

/// Three-finger swipe update: accumulate until the deadzone is crossed,
/// then drive the active output's strip via `gesture_begin`/`gesture_update`.
pub fn handle_gesture_swipe_update<B: InputBackend>(
    state: &mut State,
    event: B::GestureSwipeUpdateEvent,
) where
    B::GestureSwipeUpdateEvent: 'static,
    B::Device: 'static,
{
    use smithay::backend::input::{Event, GestureSwipeUpdateEvent};
    let mut dx = event.delta_x();
    let mut dy = event.delta_y();
    // Prefer libinput's unaccelerated deltas so velocity tracking matches the
    // physical swipe.  Invert when natural-scroll is on, since smithay's
    // delta has natural-scroll already applied; we want raw direction so the
    // strip follows the fingers in both modes.
    if let Some(le) = (&event as &dyn std::any::Any)
        .downcast_ref::<libinput::event::gesture::GestureSwipeUpdateEvent>()
    {
        use libinput::event::gesture::GestureEventCoordinates;
        dx = le.dx_unaccelerated();
        dy = le.dy_unaccelerated();
    }
    if let Some(dev) = (&event.device() as &dyn std::any::Any).downcast_ref::<libinput::Device>() {
        if dev.config_scroll_natural_scroll_enabled() {
            dx = -dx;
            dy = -dy;
        }
    }
    let timestamp = std::time::Duration::from_micros(event.time());

    if let Some((cx, cy)) = state.ewm.gesture.cumulative.as_mut() {
        *cx += dx;
        *cy += dy;
        let (cx, cy) = (*cx, *cy);
        if cx * cx + cy * cy >= GESTURE_DEADZONE_SQ {
            state.ewm.gesture.cumulative = None;
            if cx.abs() > cy.abs() {
                if let Some(out) = state.ewm.active_output() {
                    if let Some(strip) = state.ewm.output_strips.get_mut(&out) {
                        strip.gesture_begin();
                    }
                    state.ewm.gesture.active_output = Some(out);
                }
            }
        }
        return;
    }

    if let Some(out) = state.ewm.gesture.active_output.clone() {
        let w = working_area_width(state, &out);
        if let Some(strip) = state.ewm.output_strips.get_mut(&out) {
            strip.gesture_update(dx, timestamp, w);
        }
        if let Some(o) = state.ewm.find_output(&out) {
            state.ewm.queue_redraw(&o);
        }
        return;
    }

    let pointer = state.ewm.pointer.clone();
    pointer.gesture_swipe_update(
        state,
        &smithay::input::pointer::GestureSwipeUpdateEvent {
            time: event.time_msec(),
            delta: (dx, dy).into(),
        },
    );
}

/// Three-finger swipe end: snap to the nearest column.  No-op if the
/// gesture never crossed the deadzone or wasn't 3-finger.
pub fn handle_gesture_swipe_end<B: InputBackend>(
    state: &mut State,
    event: B::GestureSwipeEndEvent,
) {
    use smithay::backend::input::{Event, GestureEndEvent};
    state.ewm.gesture.cumulative = None;
    if let Some(out) = state.ewm.gesture.active_output.take() {
        let w = working_area_width(state, &out);
        let target = state.ewm.output_strips.get_mut(&out).and_then(|s| {
            s.gesture_end(w);
            s.frames.get(s.active_idx()).map(|f| f.surface_id)
        });
        if let Some(id) = target {
            state.ewm.set_focus_frame(id, "gesture");
        }
        if let Some(o) = state.ewm.find_output(&out) {
            state.ewm.queue_redraw(&o);
        }
        return;
    }
    let pointer = state.ewm.pointer.clone();
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    pointer.gesture_swipe_end(
        state,
        &smithay::input::pointer::GestureSwipeEndEvent {
            serial,
            time: event.time_msec(),
            cancelled: event.cancelled(),
        },
    );
}

/// Handle pointer axis (scroll wheel, touchpad scroll)
pub fn handle_pointer_axis<B: InputBackend>(state: &mut State, event: B::PointerAxisEvent) {
    tracy_span!("handle_pointer_axis");
    let pointer = state.ewm.pointer.clone();

    // When locked, skip scroll-to-focus
    if !state.ewm.is_locked() {
        // Scroll-to-focus: focus the surface under pointer on scroll
        let (px, py) = state.ewm.pointer_location();
        let pos = (px, py).into();

        // Check if a layer surface is under the pointer
        let layer_under = state.ewm.layer_under_point(pos);
        let on_layer = layer_under.is_some();
        state.ewm.focus_layer_surface_if_on_demand(layer_under);

        if !on_layer {
            if let Some(id) = state.ewm.focus_under(pos, "scroll") {
                state.ewm.queue_event(crate::Event::Focus {
                    id,
                    x: None,
                    y: None,
                });
            }
        }
    }

    // Sync keyboard focus before forwarding the scroll event.
    state.sync_keyboard_focus();

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
