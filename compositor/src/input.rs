//! Generic input handling shared between backends
//!
//! This module provides keyboard and pointer event processing that works
//! with any Smithay input backend.
//!
//! # Design Invariants
//!
//! 1. **Key interception**: Emacs provides intercepted keys via `ewm-intercept-keys-module`.
//!    Matching uses raw Latin keysyms; delivery uses temporary keyboard focus or command events.
//!
//! 2. **Focus synchronization**: Keyboard focus is derived from logical focus and capture state
//!    before forwarding.
//!
//! 3. **Intercept ownership**: Once EWM consumes an intercepted physical press, backend repeats and
//!    the matching release are consumed by EWM too. They must not leak to the client surface.
//!
//! 4. **Command dispatch focus**: Command-dispatch keys do not move Wayland keyboard focus. Lisp
//!    opens tokened keyboard capture for minibuffers, transients, and read-key states.
//!
//! 5. **Text input intercept**: When requested by Lisp and an active text-input surface matches
//!    keyboard focus, printable keys are redirected to Emacs for input-method translation.
//!    Stale/global intercept state must not blackhole unrelated clients.
//!
//! 6. **VT switching**: Ctrl+Alt+F1-F12 arrive as XF86Switch_VT_N keysyms.

use smithay::backend::input::KeyState;
use smithay::input::keyboard::{
    keysyms, xkb, FilterResult, Keycode, KeyboardHandle, Keysym, ModifiersState,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;

use crate::im::repeat::HeldKeyOwner;
use crate::{
    module, tracy_span, InterceptDispatch, InterceptedKey, State, SurfaceHit, TranslateTarget,
};

fn notify_activity(state: &mut State) {
    state.ewm.notify_activity();
    state.ewm.reset_idle_timer();
}

/// Pointer-specific activity: idle notify plus cursor un-hide.
fn notify_pointer_activity(state: &mut State) {
    notify_activity(state);
    state.ewm.reset_cursor_hide_timer();
}

fn key_repeats(state: &mut State, keycode: u32) -> bool {
    let keyboard = state.ewm.keyboard.clone();
    keyboard.with_xkb_state(state, |context| {
        let xkb = context.xkb().lock().unwrap();
        // The borrowed keymap is used only inside Smithay's XKB callback.
        unsafe { xkb.keymap().key_repeats(keycode.into()) }
    })
}

/// Unified decision from the keyboard filter.
/// Returned by `input_intercept`. The caller decides how to forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// Forward to focused client as-is
    Forward,
    /// Translate to a different key and forward it to the focused client.
    Translate(TranslateTarget),
    /// Redirect to Emacs through Wayland keyboard focus.
    KeyboardRedirect(u32),
    /// Execute an Emacs command binding without moving Wayland keyboard focus.
    Command { key: String, repeat: bool },
    /// Text input intercept: key plus original modifier snapshot for Emacs IM processing.
    TextInput {
        keysym: u32,
        utf8: Option<String>,
        ctrl: bool,
        alt: bool,
        shift: bool,
        logo: bool,
    },
    /// VT switch (Ctrl+Alt+F1-F12)
    VtSwitch(i32),
}

/// `Ctrl+Alt+F1..F12` arrives as `XF86Switch_VT_N`; decode to the 1-based VT.
fn decode_vt_switch(modified_raw: u32) -> Option<i32> {
    if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12).contains(&modified_raw) {
        Some((modified_raw - keysyms::KEY_XF86Switch_VT_1 + 1) as i32)
    } else {
        None
    }
}

fn intercepted_key_action(ik: &InterceptedKey, keysym_raw: u32) -> KeyAction {
    match ik.dispatch {
        InterceptDispatch::Keyboard => KeyAction::KeyboardRedirect(keysym_raw),
        InterceptDispatch::Command => KeyAction::Command {
            key: ik.key.clone(),
            repeat: ik.repeat,
        },
        InterceptDispatch::Translate(target) => KeyAction::Translate(target),
    }
}

/// Pure classification of a keyboard event. Reads no compositor state; the
/// caller resolves `focus_on_emacs` / `fullscreen_active` upstream and
/// applies side effects downstream. Releases always Forward.
pub fn classify_key(
    intercepted_keys: &[InterceptedKey],
    text_input_intercept: bool,
    focus_on_emacs: bool,
    fullscreen_active: bool,
    is_press: bool,
    keysym_raw: u32,
    modified_raw: u32,
    modified: Keysym,
    mods: &ModifiersState,
    key_changes_modifiers: bool,
) -> KeyAction {
    if !is_press {
        return KeyAction::Forward;
    }

    if let Some(vt) = decode_vt_switch(modified_raw) {
        return KeyAction::VtSwitch(vt);
    }

    if let Some(ik) = intercepted_keys
        .iter()
        .find(|ik| ik.matches(keysym_raw, modified_raw, mods))
    {
        if focus_on_emacs {
            return KeyAction::Forward;
        }
        let action = intercepted_key_action(ik, keysym_raw);
        // Translation always applies to surfaces (like clipboard emulation);
        // keyboard/command redirects respect fullscreen.
        if matches!(action, KeyAction::Translate(_)) || !fullscreen_active || ik.allow_fullscreen {
            return action;
        }
        return KeyAction::Forward;
    }

    if text_input_intercept && !focus_on_emacs && !key_changes_modifiers {
        let utf8 = xkb::keysym_to_utf8(modified);
        let utf8 = (!utf8.is_empty() && !utf8.chars().all(char::is_control)).then_some(utf8);
        return KeyAction::TextInput {
            keysym: keysym_raw,
            utf8,
            ctrl: mods.ctrl,
            alt: mods.alt,
            shift: mods.shift,
            logo: mods.logo,
        };
    }

    KeyAction::Forward
}

/// Result of processing a keyboard event
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardAction {
    /// Normal key forwarding - nothing special happened
    Forward,
    /// Prefix/key-sequence input intercepted - redirect keyboard focus to Emacs
    RedirectToEmacs,
    /// Command binding intercepted and queued to Emacs over IPC
    CommandIntercepted,
    /// Key intercepted for text input (sent to Emacs via IPC)
    TextInputIntercepted,
    /// VT switch requested (Ctrl+Alt+F1-F12)
    ChangeVt(i32),
    /// Key was translated to a different key and forwarded to the client.
    Translated,
}

fn handle_held_intercept(
    state: &mut State,
    keyboard: &KeyboardHandle<State>,
    keycode: u32,
    key_state: KeyState,
    serial: smithay::utils::Serial,
    time: u32,
) -> Option<KeyboardAction> {
    let owner = state.ewm.held_key_owner(keycode)?;
    let release = key_state == KeyState::Released;

    if release
        || matches!(
            owner,
            HeldKeyOwner::KeyboardRedirect | HeldKeyOwner::Translate { .. }
        )
    {
        let (_, mods_changed) =
            keyboard.input_intercept(state, keycode.into(), key_state, |_, _, _| {});
        match &owner {
            HeldKeyOwner::KeyboardRedirect => {
                state.send_intercept_key_to_emacs(keycode, key_state, time, mods_changed);
            }
            HeldKeyOwner::Translate { keycode, target } => {
                forward_translate(
                    state,
                    keyboard,
                    Keycode::new(*keycode),
                    key_state,
                    serial,
                    time,
                    target,
                );
            }
            _ => {}
        }
    }

    if release {
        state.ewm.end_key_hold(keycode);
        state.sync_keyboard_focus();
    }

    notify_activity(state);
    Some(match owner {
        HeldKeyOwner::KeyboardRedirect => KeyboardAction::RedirectToEmacs,
        HeldKeyOwner::Command => KeyboardAction::CommandIntercepted,
        HeldKeyOwner::TextInput => KeyboardAction::TextInputIntercepted,
        HeldKeyOwner::Translate { .. } => KeyboardAction::Translated,
    })
}

/// Find a keycode that produces `keysym` in the active layout, or None.
fn resolve_keycode(
    state: &mut State,
    keyboard: &KeyboardHandle<State>,
    keysym: u32,
) -> Option<Keycode> {
    keyboard.with_xkb_state(state, |context| {
        let xkb = context.xkb().lock().unwrap();
        let layout = xkb.active_layout();
        // The borrowed keymap is used only inside Smithay's XKB callback.
        let keymap = unsafe { xkb.keymap() };
        (keymap.min_keycode().raw()..=keymap.max_keycode().raw())
            .map(Keycode::new)
            .find(|&kc| {
                xkb.raw_syms_for_key_in_layout(kc, layout)
                    .iter()
                    .any(|sym| sym.raw() == keysym)
            })
    })
}

/// Run `forward` with `(ctrl, alt, shift, logo)` advertised as the held
/// modifiers, then restore and re-advertise the live modifier state so the
/// client isn't left latched at the target.
pub(crate) fn forward_with_modifiers(
    state: &mut State,
    keyboard: &KeyboardHandle<State>,
    (ctrl, alt, shift, logo): (bool, bool, bool, bool),
    forward: impl FnOnce(&mut State),
) {
    let saved = keyboard.modifier_state();
    let mut target = saved;
    target.ctrl = ctrl;
    target.alt = alt;
    target.shift = shift;
    target.logo = logo;
    keyboard.set_modifier_state(target);
    keyboard.advertise_modifier_state(state);
    forward(&mut *state);
    keyboard.set_modifier_state(saved);
    keyboard.advertise_modifier_state(state);
}

/// Forward `keycode` to the focused surface with the target's modifiers.
fn forward_translate(
    state: &mut State,
    keyboard: &KeyboardHandle<State>,
    keycode: Keycode,
    key_state: KeyState,
    serial: smithay::utils::Serial,
    time: u32,
    target: &TranslateTarget,
) {
    forward_with_modifiers(
        state,
        keyboard,
        (target.ctrl, target.alt, target.shift, target.logo),
        |state| keyboard.input_forward(state, keycode, key_state, serial, time, false),
    );
}

/// Process a keyboard key event and return the compositor action taken.
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

    if !is_press {
        if let Some(action) =
            handle_held_intercept(state, &keyboard, keycode, key_state, serial, time)
        {
            return action;
        }
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
                if let Some(vt) = decode_vt_switch(handle.modified_sym().raw()) {
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

    // Look up the focused entry on the focused frame; if it's fullscreen,
    // make sure normal forwarded keys still go to that surface.
    let focused_frame_id = state.ewm.focused_frame_id;
    let focused_entry_id = state.ewm.focused_entry_id;
    let fullscreen_entry = focused_entry_id.and_then(|entry_id| {
        state
            .ewm
            .frame_set
            .mapped_strips()
            .values()
            .flat_map(|s| s.frames.iter())
            .find(|f| f.surface_id == focused_frame_id)
            .filter(|f| f.entry_fullscreen(entry_id))
            .and_then(|f| {
                f.entry_surface_id(entry_id)
                    .map(|surface_id| (entry_id, surface_id))
            })
    });
    let fullscreen_active = fullscreen_entry.is_some();

    // Undo temporary redirects before forwarding into fullscreen surfaces.
    if let Some((entry_id, surface_id)) = fullscreen_entry {
        if state.ewm.focused_surface_id() != surface_id {
            state
                .ewm
                .set_focus_entry(entry_id, "fullscreen_focus_redirect", true);
        }
    }

    state.sync_keyboard_focus();

    let focus_on_emacs = state.ewm.is_keyboard_focus_on_emacs();
    if is_press && focus_on_emacs {
        state.ewm.record_emacs_keyboard_activity();
    }
    if is_press {
        if let Some(action) =
            handle_held_intercept(state, &keyboard, keycode, key_state, serial, time)
        {
            return action;
        }
    }

    // During keyboard capture, Emacs needs Latin keysyms for keybinding
    // dispatch. Temporarily set base layout for this key event, then restore
    // immediately after. No persistent state needed.
    let prefix_saved_layout =
        if module::get_keyboard_capture() && focus_on_emacs && state.ewm.xkb_current_layout != 0 {
            let saved = state.ewm.xkb_current_layout;
            keyboard.with_xkb_state(state, |mut context| {
                context.set_layout(smithay::input::keyboard::Layout(0));
            });
            Some(saved)
        } else {
            None
        };

    let intercepted_keys = crate::module::get_intercepted_keys();
    let focused_surface_id = state.ewm.focused_surface_id();
    // Gate on smithay's synchronous active-text-input state, not the relay's
    // lagging `is_active()` echo, so the relay stays off the per-key hot path
    // and the first key after activation isn't forwarded raw.
    let text_input_intercept = state.ewm.text_input_intercept
        && state.ewm.active_text_input_surface_id() == Some(focused_surface_id);

    // Use input_intercept + explicit input_forward so we can translate keys
    // (resolve a different keycode/modifiers) between the two steps.
    let mods_before = keyboard.modifier_state();
    let (action, mods_changed) =
        keyboard.input_intercept(state, keycode.into(), key_state, |_, mods, handle| {
            let modified = handle.modified_sym();
            let keysym_raw = handle
                .raw_latin_sym_or_raw_current_sym()
                .unwrap_or(modified)
                .raw();
            classify_key(
                &intercepted_keys,
                text_input_intercept,
                focus_on_emacs,
                fullscreen_active,
                is_press,
                keysym_raw,
                modified.raw(),
                modified,
                mods,
                mods_before != *mods,
            )
        });
    if let Some(saved) = prefix_saved_layout {
        keyboard.with_xkb_state(state, |mut context| {
            context.set_layout(smithay::input::keyboard::Layout(saved as u32));
        });
    }

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

        KeyAction::Translate(target) => {
            // Resolve the target keycode once; the hold reuses it for repeats
            // and release rather than rescanning the keymap each event.
            if let Some(target_keycode) = resolve_keycode(state, &keyboard, target.keysym) {
                state.ewm.begin_key_hold(
                    keycode,
                    HeldKeyOwner::Translate {
                        keycode: target_keycode.raw(),
                        target,
                    },
                );
                forward_translate(state, &keyboard, target_keycode, key_state, serial, time, &target);
                tracing::debug!(
                    "translate: keycode={} -> keysym=0x{:x} surface={}",
                    keycode,
                    target.keysym,
                    state.ewm.focused_surface_id()
                );
            } else {
                tracing::warn!("translate: no keycode for keysym 0x{:x}", target.keysym);
            }
            notify_activity(state);
            return KeyboardAction::Translated;
        }

        KeyAction::TextInput {
            keysym,
            ref utf8,
            ctrl,
            alt,
            shift,
            logo,
        } => {
            state.ewm.begin_key_hold(keycode, HeldKeyOwner::TextInput);
            if key_repeats(state, keycode) {
                state.ewm.start_text_input_repeat(
                    keycode,
                    keysym,
                    utf8.clone(),
                    focused_surface_id,
                    ctrl,
                    alt,
                    shift,
                    logo,
                );
            }
            state.ewm.queue_event(crate::Event::Key {
                keycode,
                keysym,
                utf8: utf8.clone(),
                surface_id: focused_surface_id,
                ctrl,
                alt,
                shift,
                logo,
            });
            notify_activity(state);
            return KeyboardAction::TextInputIntercepted;
        }

        KeyAction::VtSwitch(vt) => return KeyboardAction::ChangeVt(vt),

        KeyAction::KeyboardRedirect(keysym_val) => {
            tracing::info!(
                "intercept_redirect: keycode={} keysym=0x{:x} from surface {}",
                keycode,
                keysym_val,
                state.ewm.focused_surface_id()
            );

            if state.ewm.emacs_surface_for_focused_output().is_some() {
                state.ewm.cancel_command_repeat();
                module::begin_keyboard_redirect_capture();
                state
                    .ewm
                    .begin_key_hold(keycode, HeldKeyOwner::KeyboardRedirect);

                // Keep EWM's logical focus on the surface. Only Wayland
                // keyboard delivery is redirected for this intercepted
                // physical key.

                state.send_intercept_key_to_emacs(keycode, key_state, time, mods_changed);
            }
            notify_activity(state);
            return KeyboardAction::RedirectToEmacs;
        }

        KeyAction::Command { ref key, repeat } => {
            tracing::info!(
                "intercept_command: keycode={} key={} from surface {}",
                keycode,
                key,
                state.ewm.focused_surface_id()
            );

            state.ewm.begin_key_hold(keycode, HeldKeyOwner::Command);
            if repeat {
                state.ewm.start_command_repeat(keycode, key.clone());
            }
            state
                .ewm
                .queue_event(crate::Event::InterceptedCommand { key: key.clone() });
            notify_activity(state);
            return KeyboardAction::CommandIntercepted;
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

// Pointer event handling

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, PointerAxisEvent,
    PointerButtonEvent, PointerMotionEvent, ProximityState, TabletToolButtonEvent, TabletToolEvent,
    TabletToolProximityEvent, TabletToolTipEvent, TabletToolTipState,
};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, Focus as PointerFocus,
    GrabStartData as PointerGrabStartData, MotionEvent, RelativeMotionEvent,
};
use smithay::input::touch::{
    DownEvent as TouchDown, MotionEvent as TouchMotion, UpEvent as TouchUp,
};
use smithay::output::Output;
// Libinput device configuration
use smithay::reexports::input as libinput;
use smithay::utils::{Logical, Point, Rectangle, Transform};
use smithay::wayland::compositor::RegionAttributes;
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraint};
use smithay::wayland::tablet_manager::{TabletDescriptor, TabletSeatTrait};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    Tablet,
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
            "tablet" => Some(DeviceType::Tablet),
            _ => None,
        }
    }
}

pub const DEVICE_TYPE_VALUES: &str = "touchpad, mouse, trackball, trackpoint, keyboard, tablet";
pub const ACCEL_PROFILE_VALUES: &str = "flat, adaptive";
pub const CLICK_METHOD_VALUES: &str = "button-areas, clickfinger";
pub const SCROLL_METHOD_VALUES: &str = "no-scroll, two-finger, edge, on-button-down";
pub const TAP_BUTTON_MAP_VALUES: &str = "left-right-middle, left-middle-right";
pub const DEFAULT_KEYBOARD_REPEAT_DELAY: i32 = 200;
pub const DEFAULT_KEYBOARD_REPEAT_RATE: i32 = 25;

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
    // Tablet-specific settings
    pub calibration_matrix: Option<[f32; 6]>,
    pub map_to_output: Option<String>,
    pub map_to_focused_output: Option<bool>,
}

pub fn effective_keyboard_repeat(configs: &[InputConfigEntry]) -> (i32, i32) {
    let keyboard_config = configs
        .iter()
        .find(|c| c.device.is_none() && c.device_type == Some(DeviceType::Keyboard));
    let rate = keyboard_config
        .and_then(|config| config.repeat_rate)
        .unwrap_or(DEFAULT_KEYBOARD_REPEAT_RATE);
    let delay = keyboard_config
        .and_then(|config| config.repeat_delay)
        .unwrap_or(DEFAULT_KEYBOARD_REPEAT_DELAY);
    (rate, delay)
}

/// Detect the type of a libinput device.
///
/// - Touchpad: `config_tap_finger_count() > 0`
/// - Trackball/trackpoint: udev properties `ID_INPUT_TRACKBALL` / `ID_INPUT_POINTINGSTICK`
/// - Mouse: has Pointer capability and is not touchpad/trackball/trackpoint
/// - Tablet: has TabletTool capability
fn detect_device_type(device: &libinput::Device) -> Option<DeviceType> {
    if device.has_capability(libinput::DeviceCapability::TabletTool) {
        return Some(DeviceType::Tablet);
    }

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

/// The device-specific and type-default config entries for a device.
fn lookup_configs<'a>(
    configs: &'a [InputConfigEntry],
    device_name: &str,
    device_type: DeviceType,
) -> (Option<&'a InputConfigEntry>, Option<&'a InputConfigEntry>) {
    let device_cfg = configs
        .iter()
        .find(|c| c.device.as_deref() == Some(device_name));
    let type_cfg = configs
        .iter()
        .find(|c| c.device.is_none() && c.device_type == Some(device_type));
    (device_cfg, type_cfg)
}

/// Apply libinput settings to a device using 3-level cascade:
/// device-specific config -> type-default config -> hardware default.
pub fn apply_libinput_settings(device: &mut libinput::Device, configs: &[InputConfigEntry]) {
    let detected_type = match detect_device_type(device) {
        Some(t) => t,
        None => return,
    };

    let device_name = device.name().into_owned();
    let (device_cfg, type_cfg) = lookup_configs(configs, &device_name, detected_type);

    tracing::debug!("Configuring {:?}: {}", detected_type, device_name);

    if device.config_calibration_has_matrix() {
        #[rustfmt::skip]
        const IDENTITY_MATRIX: [f32; 6] = [
            1., 0., 0.,
            0., 1., 0.,
        ];

        let _ = device.config_calibration_set_matrix(
            resolve_opt!(
                device_cfg,
                type_cfg,
                calibration_matrix,
                device.config_calibration_default_matrix()
            )
            .unwrap_or(IDENTITY_MATRIX),
        );
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_config_enums_parse_known_values() {
        assert_eq!(DeviceType::parse("touchpad"), Some(DeviceType::Touchpad));
        assert_eq!(DeviceType::parse("mouse"), Some(DeviceType::Mouse));
        assert_eq!(DeviceType::parse("trackball"), Some(DeviceType::Trackball));
        assert_eq!(
            DeviceType::parse("trackpoint"),
            Some(DeviceType::Trackpoint)
        );
        assert_eq!(DeviceType::parse("keyboard"), Some(DeviceType::Keyboard));
        assert_eq!(DeviceType::parse("tablet"), Some(DeviceType::Tablet));

        assert_eq!(parse_accel_profile("flat"), Some(AccelProfile::Flat));
        assert_eq!(
            parse_accel_profile("adaptive"),
            Some(AccelProfile::Adaptive),
        );

        assert_eq!(
            parse_click_method("button-areas"),
            Some(ClickMethod::ButtonAreas),
        );
        assert_eq!(
            parse_click_method("clickfinger"),
            Some(ClickMethod::Clickfinger),
        );

        assert_eq!(
            parse_scroll_method("no-scroll"),
            Some(ScrollMethod::NoScroll)
        );
        assert_eq!(
            parse_scroll_method("two-finger"),
            Some(ScrollMethod::TwoFinger)
        );
        assert_eq!(parse_scroll_method("edge"), Some(ScrollMethod::Edge));
        assert_eq!(
            parse_scroll_method("on-button-down"),
            Some(ScrollMethod::OnButtonDown),
        );

        assert_eq!(
            parse_tap_button_map("left-right-middle"),
            Some(TapButtonMap::LeftRightMiddle),
        );
        assert_eq!(
            parse_tap_button_map("left-middle-right"),
            Some(TapButtonMap::LeftMiddleRight),
        );
    }

    #[test]
    fn input_config_enums_reject_unknown_values() {
        assert_eq!(DeviceType::parse("stylus"), None);
        assert_eq!(parse_accel_profile("linear"), None);
        assert_eq!(parse_click_method("areas"), None);
        assert_eq!(parse_scroll_method("wheel"), None);
        assert_eq!(parse_tap_button_map("middle-left-right"), None);
    }

    #[test]
    fn keyboard_repeat_config_uses_one_effective_source() {
        assert_eq!(
            effective_keyboard_repeat(&[]),
            (DEFAULT_KEYBOARD_REPEAT_RATE, DEFAULT_KEYBOARD_REPEAT_DELAY),
        );

        let xkb_only = InputConfigEntry {
            device_type: Some(DeviceType::Keyboard),
            xkb_layouts: Some("us,ru".to_string()),
            ..InputConfigEntry::default()
        };
        assert_eq!(
            effective_keyboard_repeat(&[xkb_only]),
            (DEFAULT_KEYBOARD_REPEAT_RATE, DEFAULT_KEYBOARD_REPEAT_DELAY),
        );

        let rate_only = InputConfigEntry {
            device_type: Some(DeviceType::Keyboard),
            repeat_rate: Some(17),
            ..InputConfigEntry::default()
        };
        assert_eq!(
            effective_keyboard_repeat(&[rate_only]),
            (17, DEFAULT_KEYBOARD_REPEAT_DELAY),
        );

        let delay_only = InputConfigEntry {
            device_type: Some(DeviceType::Keyboard),
            repeat_delay: Some(450),
            ..InputConfigEntry::default()
        };
        assert_eq!(
            effective_keyboard_repeat(&[delay_only]),
            (DEFAULT_KEYBOARD_REPEAT_RATE, 450),
        );
    }
}

/// Result of checking for an active pointer constraint.
pub(crate) enum ActiveConstraint {
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
    pos_within_surface: Point<f64, Logical>,
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

pub(crate) fn active_constraint_for_focus(
    state: &State,
    pointer: &smithay::input::pointer::PointerHandle<State>,
) -> Option<(SurfaceHit, ActiveConstraint)> {
    let (surface, surface_loc) = state.ewm.pointer_focus.as_ref()?;
    let (cx, cy) = state.ewm.pointer_location();
    let pos_within_surface = Point::from((cx, cy)) - *surface_loc;
    let constraint = active_constraint(surface, pointer, pos_within_surface)?;

    Some(((surface.clone(), *surface_loc), constraint))
}

fn confined_motion_prevented(
    state: &State,
    focus_surface: &SurfaceHit,
    region: Option<&RegionAttributes>,
    pos: Point<f64, Logical>,
) -> bool {
    let under = state.ewm.surface_under_point(pos);
    if Some(&focus_surface.0) != under.as_ref().map(|(s, _)| s) {
        return true;
    }

    if let Some(region) = region {
        let local = pos - focus_surface.1;
        if !region.contains(local.to_i32_round()) {
            return true;
        }
    }

    false
}

/// Handle relative pointer motion (mice, trackpoints)
pub fn handle_pointer_motion<B: InputBackend>(
    state: &mut State,
    event: B::PointerMotionEvent,
) -> bool {
    tracy_span!("handle_pointer_motion");
    state.ewm.clear_tablet_cursor();
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
    if let Some((focus_surface, constraint)) = active_constraint_for_focus(state, &pointer) {
        match constraint {
            ActiveConstraint::Locked => {
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
            ActiveConstraint::Confined(region) => {
                pointer_confined = Some((focus_surface, region));
            }
        }
    }

    // Handle confined pointer: prevent leaving the surface/region.
    if let Some((focus_surface, region)) = &pointer_confined {
        let new_pos: Point<f64, _> = (new_x, new_y).into();
        if confined_motion_prevented(state, focus_surface, region.as_ref(), new_pos) {
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

    let (under, focus_changed) = state
        .ewm
        .update_pointer_focus_for_motion((new_x, new_y).into());

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
    state.ewm.clear_tablet_cursor();
    let output_size = state.ewm.output_size;
    let pos = event.position_transformed(output_size);

    let pointer = state.ewm.pointer.clone();
    let serial = SERIAL_COUNTER.next_serial();

    // Check active pointer constraints (locked or confined).
    let mut pointer_confined = None;
    if let Some((focus_surface, constraint)) = active_constraint_for_focus(state, &pointer) {
        match constraint {
            ActiveConstraint::Locked => {
                pointer.frame(state);
                return true;
            }
            ActiveConstraint::Confined(region) => {
                pointer_confined = Some((focus_surface, region));
            }
        }
    }

    // Handle confined pointer: prevent leaving the surface/region.
    if let Some((focus_surface, region)) = &pointer_confined {
        if confined_motion_prevented(state, focus_surface, region.as_ref(), pos) {
            pointer.frame(state);
            notify_pointer_activity(state);
            return true;
        }
    }

    let (under, focus_changed) = state.ewm.update_pointer_focus_for_motion(pos);

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

fn focus_pointer_target(state: &mut State, source: &str) {
    let pos = state.ewm.pointer_location().into();
    focus_target_at(state, pos, source);
}

/// Click-to-focus at `pos`: layer surfaces get on-demand focus, else the frame under.
fn focus_target_at(state: &mut State, pos: Point<f64, Logical>, source: &str) {
    let layer_under = state.ewm.layer_under_point(pos);
    let on_layer = layer_under.is_some();
    state.ewm.focus_layer_surface_if_on_demand(layer_under);

    if on_layer {
        return;
    }

    let Some((frame_id, entry_id)) = state.ewm.focus_under(pos, source) else {
        return;
    };
    if let Some(focus_id) = entry_id.or_else(|| state.ewm.focus_id_of_frame(frame_id)) {
        let (x, y) = state
            .ewm
            .pointer_target_at(pos)
            .map(|(_, origin)| {
                let d = pos - origin;
                (Some(d.x), Some(d.y))
            })
            .unwrap_or((None, None));
        state.ewm.queue_event(crate::Event::Focus {
            focus_id,
            x,
            y,
            pointer: x.is_some() && y.is_some(),
        });
    }
}

/// Handle pointer button press/release with click-to-focus
pub fn handle_pointer_button<B: InputBackend>(state: &mut State, event: B::PointerButtonEvent) {
    tracy_span!("handle_pointer_button");
    handle_pointer_button_state(state, event.state(), event.button_code(), event.time_msec());
}

fn try_start_floating_mod_mouse_grab(
    state: &mut State,
    button_code: u32,
    serial: smithay::utils::Serial,
) -> bool {
    if state.ewm.is_locked() || state.ewm.pointer.is_grabbed() {
        return false;
    }

    let mods = state.ewm.keyboard.modifier_state();
    if !mods.logo {
        return false;
    }

    let pointer = state.ewm.pointer.clone();
    let location = pointer.current_location();

    // Check if we need to start an interactive move.
    if button_code == BTN_LEFT {
        let Some((_, frame_id)) = state.ewm.floating_frame_under(location) else {
            return false;
        };

        focus_pointer_target(state, "floating_move_grab");

        if state.ewm.interactive_move_floating_frame_begin(frame_id) {
            let start_data = PointerGrabStartData {
                focus: None,
                button: button_code,
                location,
            };
            let grab = crate::floating_move_grab::MoveGrab::new(start_data, frame_id);
            pointer.set_grab(state, grab, serial, PointerFocus::Clear);
            state
                .ewm
                .cursor_manager
                .set_cursor_image(CursorImageStatus::Named(CursorIcon::Grabbing));
            return true;
        }
    }
    // Check if we need to start an interactive resize.
    else if button_code == BTN_RIGHT {
        let Some((_, frame_id, edges)) = state.ewm.floating_resize_edges_under(location) else {
            return false;
        };

        focus_pointer_target(state, "floating_resize_grab");

        if state
            .ewm
            .interactive_resize_floating_frame_begin(frame_id, edges)
        {
            let start_data = PointerGrabStartData {
                focus: None,
                button: button_code,
                location,
            };
            let grab = crate::floating_resize_grab::ResizeGrab::new(start_data, frame_id);
            pointer.set_grab(state, grab, serial, PointerFocus::Clear);
            state
                .ewm
                .cursor_manager
                .set_cursor_image(CursorImageStatus::Named(edges.cursor_icon()));
            return true;
        }
    }

    false
}

pub(crate) fn handle_pointer_button_state(
    state: &mut State,
    button_state: ButtonState,
    button_code: u32,
    time_msec: u32,
) {
    state.ewm.clear_tablet_cursor();
    let pointer = state.ewm.pointer.clone();
    let serial = SERIAL_COUNTER.next_serial();

    if button_state == ButtonState::Pressed
        && try_start_floating_mod_mouse_grab(state, button_code, serial)
    {
        state.sync_keyboard_focus();
        notify_pointer_activity(state);
        return;
    }

    // When locked, skip click-to-focus and just forward to lock surface
    if !state.ewm.is_locked() {
        // Click-to-focus: on button press, focus the surface under pointer
        if button_state == ButtonState::Pressed {
            focus_pointer_target(state, "click");
        }
    }

    // Sync keyboard focus before forwarding the button event,
    // so the correct surface receives any subsequent key events.
    state.sync_keyboard_focus();

    pointer.button(
        state,
        &ButtonEvent {
            button: button_code,
            state: button_state,
            serial,
            time: time_msec,
        },
    );
    pointer.frame(state);

    if button_state == ButtonState::Released {
        state.refresh_pointer_after_scene_change(false);
    }

    // Notify idle notifier of user activity
    notify_pointer_activity(state);
}

/// In-flight 3-finger swipe gesture state for strip scrolling.
#[derive(Default)]
pub struct GestureState {
    pub cumulative: Option<(f64, f64)>,
    pub active_output: Option<String>,
}

const GESTURE_DEADZONE_SQ: f64 = 16.0 * 16.0;

fn working_area_width(state: &State, name: &str) -> f64 {
    state
        .ewm
        .find_mapped_output(name)
        .map(|o| state.ewm.get_working_area(&o).size.w as f64)
        .unwrap_or(0.0)
}

/// Three-finger swipe begin: arm the deadzone; other finger counts pass through.
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

/// Three-finger swipe update: accumulate to deadzone, then drive the active output's strip.
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
    // Use libinput's unaccelerated deltas; invert when natural-scroll is on
    // so the strip follows fingers regardless of scroll mode.
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
                    if let Some(strip) = state.ewm.frame_set.mapped_strip_mut(&out) {
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
        if let Some(strip) = state.ewm.frame_set.mapped_strip_mut(&out) {
            strip.gesture_update(dx, timestamp, w);
        }
        if let Some(o) = state.ewm.find_mapped_output(&out) {
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

/// Three-finger swipe end: snap to the nearest column (no-op if deadzone wasn't crossed).
pub fn handle_gesture_swipe_end<B: InputBackend>(
    state: &mut State,
    event: B::GestureSwipeEndEvent,
) {
    use smithay::backend::input::{Event, GestureEndEvent};
    state.ewm.gesture.cumulative = None;
    if let Some(out) = state.ewm.gesture.active_output.take() {
        let w = working_area_width(state, &out);
        let prev_active_id = state
            .ewm
            .frame_set
            .mapped_strip(&out)
            .and_then(|s| s.active_frame())
            .map(|f| f.surface_id);
        let target = state.ewm.frame_set.mapped_strip_mut(&out).and_then(|s| {
            s.gesture_end(w);
            s.frames.get(s.active_idx()).map(|f| f.surface_id)
        });
        state.ewm.reconcile_active_for_output(&out, prev_active_id);
        if let Some(id) = target {
            state.ewm.set_focus_frame(id, "gesture", true);
        }
        if let Some(o) = state.ewm.find_mapped_output(&out) {
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
    state.ewm.clear_tablet_cursor();
    let pointer = state.ewm.pointer.clone();

    // When locked, skip scroll-to-focus
    if !state.ewm.is_locked() {
        // Scroll-to-focus: focus the surface under pointer on scroll
        focus_pointer_target(state, "scroll");
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

/// Handle a touch-down event: a new finger contact on the screen.
pub fn handle_touch_down<B: InputBackend>(state: &mut State, event: B::TouchDownEvent) {
    use smithay::backend::input::{Event, TouchEvent};
    let output_size = state.ewm.output_size;
    let pos = event.position_transformed(output_size);
    let serial = SERIAL_COUNTER.next_serial();
    let Some(touch) = state.ewm.touch.clone() else {
        return;
    };

    let focus = if state.ewm.is_locked() {
        state
            .ewm
            .lock_surface_focus()
            .map(|s| (s, (0.0, 0.0).into()))
    } else {
        state.ewm.surface_under_point(pos)
    };

    touch.down(
        state,
        focus,
        &TouchDown {
            slot: event.slot(),
            location: pos,
            serial,
            time: event.time_msec(),
        },
    );
    notify_activity(state);
}

/// Handle a touch-motion event: a finger moved while in contact.
pub fn handle_touch_motion<B: InputBackend>(state: &mut State, event: B::TouchMotionEvent) {
    use smithay::backend::input::{Event, TouchEvent};
    let output_size = state.ewm.output_size;
    let pos = event.position_transformed(output_size);
    let Some(touch) = state.ewm.touch.clone() else {
        return;
    };

    let focus = if state.ewm.is_locked() {
        state
            .ewm
            .lock_surface_focus()
            .map(|s| (s, (0.0, 0.0).into()))
    } else {
        state.ewm.surface_under_point(pos)
    };

    touch.motion(
        state,
        focus,
        &TouchMotion {
            slot: event.slot(),
            location: pos,
            time: event.time_msec(),
        },
    );
    notify_activity(state);
}

/// Handle a touch-up event: a finger lifted from the screen.
pub fn handle_touch_up<B: InputBackend>(state: &mut State, event: B::TouchUpEvent) {
    use smithay::backend::input::{Event, TouchEvent};
    let serial = SERIAL_COUNTER.next_serial();
    let Some(touch) = state.ewm.touch.clone() else {
        return;
    };

    touch.up(
        state,
        &TouchUp {
            slot: event.slot(),
            serial,
            time: event.time_msec(),
        },
    );
    notify_activity(state);
}

/// Handle a touch-cancel event: the touch sequence was interrupted.
pub fn handle_touch_cancel<B: InputBackend>(state: &mut State, _event: B::TouchCancelEvent) {
    let Some(touch) = state.ewm.touch.clone() else {
        return;
    };
    touch.cancel(state);
}

/// Handle a touch-frame event: end of a logical group of touch events.
pub fn handle_touch_frame<B: InputBackend>(state: &mut State, _event: B::TouchFrameEvent) {
    let Some(touch) = state.ewm.touch.clone() else {
        return;
    };
    touch.frame(state);
}

// Tablet event handling

/// The output a tablet maps onto, resolved through the input config cascade.
fn output_for_tablet(state: &State, device: &libinput::Device) -> Option<Output> {
    let (device_cfg, type_cfg) =
        lookup_configs(&state.ewm.input_configs, &device.name(), DeviceType::Tablet);

    if resolve!(device_cfg, type_cfg, map_to_focused_output, false) {
        let name = state.ewm.active_output()?;
        state.ewm.find_mapped_output(&name)
    } else {
        let map_to_output = device_cfg
            .and_then(|c| c.map_to_output.as_deref())
            .or_else(|| type_cfg.and_then(|c| c.map_to_output.as_deref()));
        map_to_output.and_then(|name| state.ewm.find_mapped_output(name))
    }
}

/// Tablet event position in global space: output mapping, aspect correction, clamping.
fn compute_tablet_position<B: InputBackend<Device = libinput::Device>>(
    state: &State,
    event: &(impl Event<B> + TabletToolEvent<B>),
) -> Option<Point<f64, Logical>> {
    let device = event.device();
    let mapped = output_for_tablet(state, &device);
    let (target_geo, transform) = match &mapped {
        Some(output) => (
            state.ewm.space.output_geometry(output)?,
            output.current_transform(),
        ),
        // Do not keep ratio for the unified mode as this is what OpenTabletDriver expects.
        None => (
            Rectangle::from_size(state.ewm.output_size),
            Transform::Normal,
        ),
    };

    // FIXME: the 1 px margin should come from the output under the clamped position.
    let scale_output = mapped.as_ref().or_else(|| state.ewm.first_output())?;
    let px = 1. / scale_output.current_scale().fractional_scale();

    let size = transform.invert().transform_size(target_geo.size);
    let mut pos = transform.transform_point_in(event.position_transformed(size), &size.to_f64());

    if mapped.is_some() {
        if let Some(tablet_ratio) = device.size().map(|(w, h)| w / h) {
            // This code does the same thing as mutter with "keep aspect ratio" enabled.
            let ratio = tablet_ratio / (size.w as f64 / size.h as f64);
            if ratio > 1. {
                pos.x *= ratio;
            } else {
                pos.y /= ratio;
            }
        }
    }

    pos.x = pos.x.clamp(0.0, (target_geo.size.w as f64 - px).max(0.0));
    pos.y = pos.y.clamp(0.0, (target_geo.size.h as f64 - px).max(0.0));
    Some(pos + target_geo.loc.to_f64())
}

pub fn handle_tablet_tool_axis<B: InputBackend<Device = libinput::Device>>(
    state: &mut State,
    event: B::TabletToolAxisEvent,
) {
    let tablet_seat = state.ewm.seat.tablet_seat();
    let tablet = tablet_seat.get_tablet(&TabletDescriptor::from(&event.device()));
    let tool = tablet_seat.get_tool(&event.tool());
    let (Some(tablet), Some(tool)) = (tablet, tool) else {
        return;
    };
    let Some(pos) = compute_tablet_position(state, &event) else {
        return;
    };

    if event.pressure_has_changed() {
        tool.pressure(event.pressure());
    }
    if event.distance_has_changed() {
        tool.distance(event.distance());
    }
    if event.tilt_has_changed() {
        tool.tilt(event.tilt());
    }
    if event.slider_has_changed() {
        tool.slider_position(event.slider_position());
    }
    if event.rotation_has_changed() {
        tool.rotation(event.rotation());
    }
    if event.wheel_has_changed() {
        tool.wheel(event.wheel_delta(), event.wheel_delta_discrete());
    }

    let under = state.ewm.pointer_target_at(pos);
    tool.motion(
        pos,
        under,
        &tablet,
        SERIAL_COUNTER.next_serial(),
        event.time_msec(),
    );

    state.ewm.tablet_cursor_location = Some(pos);
    state.ewm.queue_redraw_for_pointer();
    notify_pointer_activity(state);
}

pub fn handle_tablet_tool_tip<B: InputBackend<Device = libinput::Device>>(
    state: &mut State,
    event: B::TabletToolTipEvent,
) {
    let tablet_seat = state.ewm.seat.tablet_seat();
    let tablet = tablet_seat.get_tablet(&TabletDescriptor::from(&event.device()));
    let tool = tablet_seat.get_tool(&event.tool());
    let (Some(tablet), Some(tool)) = (tablet, tool) else {
        return;
    };

    match event.tip_state() {
        TabletToolTipState::Down => {
            let Some(pos) = compute_tablet_position(state, &event) else {
                return;
            };

            // The tip carries its own position; the hover focus may be stale.
            let under = state.ewm.pointer_target_at(pos);
            tool.motion(
                pos,
                under,
                &tablet,
                SERIAL_COUNTER.next_serial(),
                event.time_msec(),
            );
            tool.tip_down(SERIAL_COUNTER.next_serial(), event.time_msec());

            state.ewm.tablet_cursor_location = Some(pos);
            if !state.ewm.is_locked() {
                focus_target_at(state, pos, "tablet");
            }
            state.sync_keyboard_focus();
            state.ewm.queue_redraw_for_pointer();
        }
        TabletToolTipState::Up => {
            tool.tip_up(event.time_msec());
        }
    }
    notify_pointer_activity(state);
}

pub fn handle_tablet_tool_proximity<B: InputBackend<Device = libinput::Device>>(
    state: &mut State,
    event: B::TabletToolProximityEvent,
) {
    let tablet_seat = state.ewm.seat.tablet_seat();
    let display_handle = state.ewm.display_handle.clone();
    let tool = tablet_seat.add_tool::<State>(state, &display_handle, &event.tool());
    let Some(tablet) = tablet_seat.get_tablet(&TabletDescriptor::from(&event.device())) else {
        return;
    };

    match event.state() {
        ProximityState::In => {
            let Some(pos) = compute_tablet_position(state, &event) else {
                return;
            };
            if let Some(under) = state.ewm.pointer_target_at(pos) {
                tool.proximity_in(
                    pos,
                    under,
                    &tablet,
                    SERIAL_COUNTER.next_serial(),
                    event.time_msec(),
                );
            }
            state.ewm.tablet_cursor_location = Some(pos);
            state.ewm.queue_redraw_for_pointer();
        }
        ProximityState::Out => {
            tool.proximity_out(event.time_msec());

            // Land the pointer where the pen left so it doesn't jump.
            if let Some(pos) = state.ewm.tablet_cursor_location {
                state.warp_pointer(pos.x, pos.y);
            }
            state.ewm.clear_tablet_cursor();
        }
    }
    notify_pointer_activity(state);
}

pub fn handle_tablet_tool_button<B: InputBackend>(
    state: &mut State,
    event: B::TabletToolButtonEvent,
) {
    if let Some(tool) = state.ewm.seat.tablet_seat().get_tool(&event.tool()) {
        tool.button(
            event.button(),
            event.button_state(),
            SERIAL_COUNTER.next_serial(),
            event.time_msec(),
        );
    }
    notify_pointer_activity(state);
}
