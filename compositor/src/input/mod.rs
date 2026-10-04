//! Generic input handling shared between backends
//!
//! This module provides keyboard and pointer event processing that works
//! with any Smithay input backend.
//!
//! Ported from niri's `input/mod.rs`: the event dispatcher, device lifecycle,
//! handler names and absolute position mapping follow it.
//!
//! # Design Invariants
//!
//! 1. **Key interception**: Emacs provides intercepted keys via `ewm-intercept-keys-module`. The
//!    first match wins, and a surface-scoped entry matches only while that surface has focus.
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

mod backend_ext;
mod config;
pub(crate) mod floating_move_grab;
pub(crate) mod floating_resize_grab;
pub(crate) mod frame_click_grab;

use std::any::Any;

pub use backend_ext::*;
pub use config::*;
use frame_click_grab::FrameClickGrab;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Device, DeviceCapability, Event,
    InputBackend, InputEvent, InputTime, KeyState, KeyboardKeyEvent, PointerAxisEvent,
    PointerButtonEvent, PointerMotionEvent, ProximityState, Switch, SwitchState, SwitchToggleEvent,
    TabletToolButtonEvent, TabletToolEvent, TabletToolProximityEvent, TabletToolTipEvent,
    TabletToolTipState,
};
use smithay::backend::libinput::LibinputInputBackend;
use smithay::input::keyboard::{
    FilterResult, KeyboardHandle, KeyboardTarget, Keysym, ModifiersState, keysyms, xkb,
};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, ClickGrab, CursorIcon, CursorImageStatus, Focus as PointerFocus,
    GrabStartData as PointerGrabStartData, MotionEvent, RelativeMotionEvent,
};
use smithay::input::tablet::{self, TabletDescriptor, TabletSeatTrait};
use smithay::input::touch::{
    DownEvent as TouchDown, MotionEvent as TouchMotion, UpEvent as TouchUp,
};
use smithay::output::Output;
use smithay::reexports::input as libinput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Transform};
use smithay::wayland::compositor::RegionAttributes;
use smithay::wayland::pointer_constraints::{PointerConstraint, with_pointer_constraint};
use tracing::{info, warn};

use crate::event::TextInputKey;
use crate::im::repeat::{HeldKeyOwner, InterceptRepeat};
use crate::{InterceptDispatch, InterceptedKey, State, SurfaceHit, module, tracy_span};

/// Unified decision from the keyboard filter.
/// Returned by `input_intercept`. The caller decides how to forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// Forward to focused client as-is
    Forward,
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

/// Compositor state that decides where a key goes, resolved by the caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyRouting {
    /// The focused surface has an active text input that Emacs serves.
    pub text_input_intercept: bool,
    pub focus_on_emacs: bool,
    pub fullscreen_active: bool,
    pub focused_surface_id: u64,
}

/// The key event being classified.
#[derive(Debug, Clone, Copy)]
pub struct KeyInput<'a> {
    pub is_press: bool,
    /// Layout-independent keysym of the physical key.
    pub keysym_raw: u32,
    /// Keysym after XKB applied the current modifiers.
    pub modified: Keysym,
    pub mods: &'a ModifiersState,
    /// The key is itself a modifier: pressing it changed `mods`.
    pub changes_modifiers: bool,
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
}

const BTN_LEFT: u32 = 0x110;

const BTN_RIGHT: u32 = 0x111;

/// Result of checking for an active pointer constraint.
pub(crate) enum ActiveConstraint {
    /// Pointer is locked in place, no position change allowed.
    Locked,
    /// Pointer is confined to a surface, optionally within a region.
    Confined(Option<RegionAttributes>),
}

/// In-flight 3-finger swipe: horizontal scrolls a strip, vertical drives the overview.
#[derive(Default)]
pub struct GestureState {
    pub cumulative: Option<(f64, f64)>,
    pub active_output: Option<String>,
}

const GESTURE_DEADZONE_SQ: f64 = 16.0 * 16.0;

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

impl State {
    /// Route a backend event to its handler.
    pub(crate) fn process_input_event<
        B: InputBackend<Device: EwmInputDevice + 'static, GestureSwipeUpdateEvent: 'static>,
    >(
        &mut self,
        event: InputEvent<B>,
    ) {
        match event {
            InputEvent::DeviceAdded { device } => self.on_device_added(device),
            InputEvent::DeviceRemoved { device } => self.on_device_removed(device),
            InputEvent::Keyboard { event } => {
                let action = self.on_keyboard(event.key_code().into(), event.state(), event.time());
                if let KeyboardAction::ChangeVt(vt) = action {
                    self.backend.change_vt(vt);
                }
            }
            InputEvent::PointerMotion { event } => {
                self.on_pointer_motion::<B>(event);
                self.ewm.queue_redraw_for_pointer();
            }
            InputEvent::PointerMotionAbsolute { event } => {
                self.on_pointer_motion_absolute::<B>(event);
                self.ewm.queue_redraw_for_pointer();
            }
            InputEvent::PointerButton { event } => self.on_pointer_button::<B>(event),
            InputEvent::PointerAxis { event } => self.on_pointer_axis::<B>(event),
            InputEvent::GestureSwipeBegin { event } => self.on_gesture_swipe_begin::<B>(event),
            InputEvent::GestureSwipeUpdate { event } => self.on_gesture_swipe_update::<B>(event),
            InputEvent::GestureSwipeEnd { event } => self.on_gesture_swipe_end::<B>(event),
            InputEvent::TouchDown { event } => self.on_touch_down::<B>(event),
            InputEvent::TouchMotion { event } => self.on_touch_motion::<B>(event),
            InputEvent::TouchUp { event } => self.on_touch_up::<B>(event),
            InputEvent::TouchCancel { event } => self.on_touch_cancel::<B>(event),
            InputEvent::TouchFrame { event } => self.on_touch_frame::<B>(event),
            InputEvent::TabletToolAxis { event } => self.on_tablet_tool_axis::<B>(event),
            InputEvent::TabletToolTip { event } => self.on_tablet_tool_tip::<B>(event),
            InputEvent::TabletToolProximity { event } => self.on_tablet_tool_proximity::<B>(event),
            InputEvent::TabletToolButton { event } => self.on_tablet_tool_button::<B>(event),
            InputEvent::SwitchToggle { event } => self.on_switch_toggle::<B>(event),
            _ => {}
        }
    }

    /// Libinput-only bookkeeping, before the generic dispatch.
    pub(crate) fn process_libinput_event(&mut self, event: &mut InputEvent<LibinputInputBackend>) {
        match event {
            InputEvent::DeviceAdded { device } => {
                apply_libinput_settings(device, &self.ewm.input_configs);
                if device.has_capability(libinput::DeviceCapability::TabletTool)
                    && device.size().is_none()
                {
                    warn!("tablet tool device has no size");
                }
                self.ewm.libinput_devices.insert(device.clone());
            }
            InputEvent::DeviceRemoved { device } => {
                self.ewm.libinput_devices.remove(device);
            }
            _ => {}
        }
    }

    fn on_device_added(&mut self, device: impl Device) {
        if device.has_capability(DeviceCapability::TabletTool) {
            let tablet_seat = self.ewm.seat.tablet_seat();
            tablet_seat.add_wp_tablet(&self.ewm.display_handle, &TabletDescriptor::from(&device));
        }
        if device.has_capability(DeviceCapability::Touch) && self.ewm.seat.get_touch().is_none() {
            self.ewm.seat.add_touch();
        }
    }

    fn on_device_removed(&mut self, device: impl Device) {
        if device.has_capability(DeviceCapability::TabletTool) {
            let tablet_seat = self.ewm.seat.tablet_seat();
            tablet_seat.remove_tablet(&TabletDescriptor::from(&device));
            if tablet_seat.count_tablets() == 0 {
                tablet_seat.clear_tools();
            }
        }
        let touch_left = self
            .ewm
            .libinput_devices
            .iter()
            .any(|device| device.has_capability(libinput::DeviceCapability::Touch));
        if device.has_capability(DeviceCapability::Touch) && !touch_left {
            self.ewm.seat.remove_touch();
        }
    }

    fn on_switch_toggle<B: InputBackend>(&mut self, event: B::SwitchToggleEvent) {
        if event.switch() == Some(Switch::Lid) {
            let is_closed = event.state() == SwitchState::On;
            info!("Lid {}", if is_closed { "closed" } else { "opened" });
            self.handle_lid_state(is_closed);
        }
    }

    fn notify_activity(&mut self) {
        self.ewm.notify_activity();
        self.ewm.reset_idle_timer();
    }

    /// Pointer-specific activity: idle notify plus cursor un-hide.
    fn notify_pointer_activity(&mut self) {
        self.notify_activity();
        self.ewm.reset_cursor_hide_timer();
    }

    /// Absolute event position in global space, through the output's transform.
    fn compute_absolute_location<B: InputBackend>(
        &self,
        event: &impl AbsolutePositionEvent<B>,
        fallback_output: Option<&Output>,
    ) -> Option<Point<f64, Logical>>
    where
        B::Device: EwmInputDevice,
    {
        let device = event.device();
        let output = device.output().or(fallback_output)?;
        let output_geo = self.ewm.space.output_geometry(output)?;
        let transform = output.current_transform();
        let size = transform.invert().transform_size(output_geo.size);
        Some(
            transform.transform_point_in(event.position_transformed(size), &size.to_f64())
                + output_geo.loc.to_f64(),
        )
    }

    fn key_repeats(&mut self, keycode: u32) -> bool {
        let keyboard = self.ewm.keyboard.clone();
        keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            // The borrowed keymap is used only inside Smithay's XKB callback.
            unsafe { xkb.keymap().key_repeats(keycode.into()) }
        })
    }

    fn handle_held_intercept(
        &mut self,
        keyboard: &KeyboardHandle<State>,
        keycode: u32,
        key_state: KeyState,
        time: InputTime,
    ) -> Option<KeyboardAction> {
        let owner = self.ewm.held_key_owner(keycode)?;
        let release = key_state == KeyState::Released;

        if release || owner == HeldKeyOwner::KeyboardRedirect {
            let (_, mods_changed) =
                keyboard.input_intercept(self, keycode.into(), key_state, |_, _, _| {});
            if owner == HeldKeyOwner::KeyboardRedirect {
                self.send_intercept_key_to_emacs(keycode, key_state, time, mods_changed);
            }
        }

        if release {
            self.ewm.end_key_hold(keycode);
            self.sync_keyboard_focus();
        }

        self.notify_activity();
        Some(match owner {
            HeldKeyOwner::KeyboardRedirect => KeyboardAction::RedirectToEmacs,
            HeldKeyOwner::Command => KeyboardAction::CommandIntercepted,
            HeldKeyOwner::TextInput => KeyboardAction::TextInputIntercepted,
        })
    }

    /// Advertise `modifiers` to the focus without touching the keyboard's own state.
    fn advertise_modifiers(
        &mut self,
        keyboard: &KeyboardHandle<State>,
        (ctrl, alt, shift, logo): (bool, bool, bool, bool),
    ) {
        let mut mods = keyboard.modifier_state();
        mods.ctrl = ctrl;
        mods.alt = alt;
        mods.shift = shift;
        mods.logo = logo;
        mods.serialized = keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            // The borrowed state is used only inside Smithay's XKB callback.
            mods.serialize_back(unsafe { xkb.state() })
        });
        if let Some(focus) = keyboard.current_focus() {
            let seat = self.ewm.seat.clone();
            focus.modifiers(&seat, self, mods, SERIAL_COUNTER.next_serial());
        }
    }

    /// Hold the key of `repeat` for its owner, deliver it, and arm repeats if `repeats`.
    fn intercept(&mut self, repeat: InterceptRepeat, repeats: bool) {
        self.ewm.begin_key_hold(repeat.keycode(), repeat.owner());
        if repeats {
            self.ewm.start_repeat(repeat.clone());
        }
        self.deliver_intercept(repeat);
    }

    /// Tap `keycode` on the focus under `mods`, then re-advertise the live ones.
    pub(crate) fn tap_key(&mut self, keycode: u32, mods: (bool, bool, bool, bool)) {
        let keyboard = self.ewm.keyboard.clone();
        self.sync_keyboard_focus();
        self.advertise_modifiers(&keyboard, mods);
        for key_state in [KeyState::Pressed, KeyState::Released] {
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.input_forward(
                self,
                keycode.into(),
                key_state,
                serial,
                InputTime::now(),
                false,
            );
        }
        keyboard.advertise_modifier_state(self);
    }

    /// Process a keyboard key event and return the compositor action taken.
    pub(crate) fn on_keyboard(
        &mut self,
        keycode: u32,
        key_state: KeyState,
        time: InputTime,
    ) -> KeyboardAction {
        let keyboard = self.ewm.keyboard.clone();
        tracy_span!("on_keyboard");

        let serial = SERIAL_COUNTER.next_serial();
        let is_press = key_state == KeyState::Pressed;

        if is_press {
            self.ewm.hide_cursor_for_typing();
        }

        if !is_press
            && let Some(action) = self.handle_held_intercept(&keyboard, keycode, key_state, time)
        {
            return action;
        }

        // Handle locked state: only allow VT switch, forward everything else to lock surface
        if self.ewm.is_locked() {
            // Notify idle notifier of activity even when locked
            self.notify_activity();

            // Check for VT switch first (Ctrl+Alt+F1-F12)
            let vt_switch = keyboard.input::<Option<i32>, _>(
                self,
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
            if let Some(lock_focus) = self.ewm.lock_surface_focus() {
                keyboard.set_focus(self, Some(lock_focus), serial);
            }

            return KeyboardAction::Forward;
        }

        // Look up the focused entry on the focused frame; if it's fullscreen,
        // make sure normal forwarded keys still go to that surface.
        let focused_frame_id = self.ewm.focused_frame_id;
        let focused_entry_id = self.ewm.focused_entry_id;
        let fullscreen_entry = focused_entry_id.and_then(|entry_id| {
            self.ewm
                .frame_location(focused_frame_id)
                .map(|location| location.frame)
                .filter(|f| f.entry_fullscreen(entry_id))
                .and_then(|f| {
                    f.entry_surface_id(entry_id)
                        .map(|surface_id| (entry_id, surface_id))
                })
        });
        let fullscreen_active = fullscreen_entry.is_some();

        // Undo temporary redirects before forwarding into fullscreen surfaces.
        if let Some((entry_id, surface_id)) = fullscreen_entry
            && self.ewm.focused_surface_id() != surface_id
        {
            self.ewm
                .set_focus_entry(entry_id, "fullscreen_focus_redirect", true);
        }

        self.sync_keyboard_focus();

        let focus_on_emacs = self.ewm.is_keyboard_focus_on_emacs();
        if is_press && focus_on_emacs {
            self.ewm.record_emacs_keyboard_activity();
        }
        if is_press
            && let Some(action) = self.handle_held_intercept(&keyboard, keycode, key_state, time)
        {
            return action;
        }

        // During keyboard capture, Emacs needs Latin keysyms for keybinding
        // dispatch. Temporarily set base layout for this key event, then restore
        // immediately after. No persistent state needed.
        let prefix_saved_layout =
            if module::get_keyboard_capture() && focus_on_emacs && self.ewm.xkb_current_layout != 0
            {
                let saved = self.ewm.xkb_current_layout;
                keyboard.with_xkb_state(self, |mut context| {
                    context.set_layout(smithay::input::keyboard::Layout(0));
                });
                Some(saved)
            } else {
                None
            };

        let focused_surface_id = self.ewm.focused_surface_id();
        // Gate on smithay's synchronous active-text-input state, not the relay's
        // lagging `is_active()` echo, so the relay stays off the per-key hot path
        // and the first key after activation isn't forwarded raw.
        let text_input_intercept = self.ewm.text_input_intercept
            && self.ewm.active_text_input_surface_id() == Some(focused_surface_id);
        let routing = KeyRouting {
            text_input_intercept,
            focus_on_emacs,
            fullscreen_active,
            focused_surface_id,
        };

        let mods_before = keyboard.modifier_state();
        let intercepted_keys = module::intercepted_keys().read().unwrap();
        let (action, mods_changed) =
            keyboard.input_intercept(self, keycode.into(), key_state, |_, mods, handle| {
                let modified = handle.modified_sym();
                let keysym_raw = handle
                    .raw_latin_sym_or_raw_current_sym()
                    .unwrap_or(modified)
                    .raw();
                classify_key(
                    &intercepted_keys,
                    routing,
                    KeyInput {
                        is_press,
                        keysym_raw,
                        modified,
                        mods,
                        changes_modifiers: mods_before != *mods,
                    },
                )
            });
        if let Some(saved) = prefix_saved_layout {
            keyboard.with_xkb_state(self, |mut context| {
                context.set_layout(smithay::input::keyboard::Layout(saved as u32));
            });
        }

        match action {
            KeyAction::Forward => {
                self.sync_keyboard_focus();
                keyboard.input_forward(self, keycode.into(), key_state, serial, time, mods_changed);
                if is_press && module::DEBUG_MODE.load(std::sync::atomic::Ordering::Relaxed) {
                    tracing::debug!(
                        "key forward: keycode={} surface={}",
                        keycode,
                        self.ewm.focused_surface_id()
                    );
                }
            }

            KeyAction::TextInput {
                keysym,
                utf8,
                ctrl,
                alt,
                shift,
                logo,
            } => {
                let repeat = InterceptRepeat::TextInput(TextInputKey {
                    keycode,
                    keysym,
                    utf8,
                    surface_id: focused_surface_id,
                    ctrl,
                    alt,
                    shift,
                    logo,
                });
                let repeats = self.key_repeats(keycode);
                self.intercept(repeat, repeats);
                self.notify_activity();
                return KeyboardAction::TextInputIntercepted;
            }

            KeyAction::VtSwitch(vt) => return KeyboardAction::ChangeVt(vt),

            KeyAction::KeyboardRedirect(keysym_val) => {
                tracing::info!(
                    "intercept_redirect: keycode={} keysym=0x{:x} from surface {}",
                    keycode,
                    keysym_val,
                    self.ewm.focused_surface_id()
                );

                if self.ewm.emacs_surface_for_focused_output().is_some() {
                    self.ewm.cancel_command_repeat();
                    module::begin_keyboard_redirect_capture();
                    self.ewm
                        .begin_key_hold(keycode, HeldKeyOwner::KeyboardRedirect);

                    // Keep EWM's logical focus on the surface. Only Wayland
                    // keyboard delivery is redirected for this intercepted
                    // physical key.

                    self.send_intercept_key_to_emacs(keycode, key_state, time, mods_changed);
                }
                self.notify_activity();
                return KeyboardAction::RedirectToEmacs;
            }

            KeyAction::Command { key, repeat } => {
                tracing::info!(
                    "intercept_command: keycode={} key={} from surface {}",
                    keycode,
                    key,
                    self.ewm.focused_surface_id()
                );

                self.intercept(InterceptRepeat::Command { keycode, key }, repeat);
                self.notify_activity();
                return KeyboardAction::CommandIntercepted;
            }
        }

        // Check if XKB layout changed (e.g., via grp:caps_toggle)
        let current_layout = keyboard.with_xkb_state(self, |context| {
            context.xkb().lock().unwrap().active_layout().0 as usize
        });
        if current_layout != self.ewm.xkb_current_layout {
            self.ewm.xkb_current_layout = current_layout;
            tracing::info!("XKB layout changed to index {}", current_layout);
            if !self.ewm.xkb_layout_names.is_empty() {
                self.ewm.queue_event(crate::Event::LayoutSwitched {
                    layout: self
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
        self.notify_activity();

        KeyboardAction::Forward
    }

    /// Release all pressed keys (used when window loses focus)
    pub fn release_all_keys(&mut self) {
        let keyboard = self.ewm.keyboard.clone();
        let pressed = keyboard.pressed_keys();
        if pressed.is_empty() {
            return;
        }

        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();

        for keycode in pressed {
            keyboard.input::<(), _>(
                self,
                keycode,
                KeyState::Released,
                serial,
                time,
                |_, _, _| FilterResult::Forward,
            );
        }

        // Clear focus (focus_changed handles text_input)
        keyboard.set_focus(self, None, serial);
        self.ewm.keyboard_focus = None;
    }

    pub(crate) fn active_constraint_for_focus(
        &self,
        pointer: &smithay::input::pointer::PointerHandle<State>,
    ) -> Option<(SurfaceHit, ActiveConstraint)> {
        let (surface, surface_loc) = self.ewm.pointer_focus.as_ref()?;
        let (cx, cy) = self.ewm.pointer_location();
        let pos_within_surface = Point::from((cx, cy)) - *surface_loc;
        let constraint = active_constraint(surface, pointer, pos_within_surface)?;

        Some(((surface.clone(), *surface_loc), constraint))
    }

    fn confined_motion_prevented(
        &self,
        focus_surface: &SurfaceHit,
        region: Option<&RegionAttributes>,
        pos: Point<f64, Logical>,
    ) -> bool {
        let under = self.ewm.surface_under_point(pos);
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
    pub(crate) fn on_pointer_motion<B: InputBackend>(
        &mut self,
        event: B::PointerMotionEvent,
    ) -> bool {
        tracy_span!("on_pointer_motion");
        self.ewm.clear_tablet_cursor();
        let (current_x, current_y) = self.ewm.pointer_location();
        let delta = event.delta();

        // Clip motion against the union of output geometries: if the new
        // position lands in dead space between/outside outputs, snap it back
        // to the previous output's bounds.
        let target = Point::from((current_x + delta.x, current_y + delta.y));
        let previous = Point::from((current_x, current_y));
        let new_pos = self.ewm.clamp_pointer_to_outputs(target, previous);
        let new_x = new_pos.x;
        let new_y = new_pos.y;

        let pointer = self.ewm.pointer.clone();
        let serial = SERIAL_COUNTER.next_serial();

        // Check active pointer constraints (locked or confined).
        let mut pointer_confined = None;
        if let Some((focus_surface, constraint)) = self.active_constraint_for_focus(&pointer) {
            match constraint {
                ActiveConstraint::Locked => {
                    // Pointer locked: send only relative motion, no position change.
                    pointer.relative_motion(
                        self,
                        self.ewm.pointer_focus.clone(),
                        &RelativeMotionEvent {
                            delta: event.delta(),
                            delta_unaccel: event.delta_unaccel(),
                            time: event.time(),
                        },
                    );
                    pointer.frame(self);
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
            if self.confined_motion_prevented(focus_surface, region.as_ref(), new_pos) {
                pointer.relative_motion(
                    self,
                    Some(focus_surface.clone()),
                    &RelativeMotionEvent {
                        delta: event.delta(),
                        delta_unaccel: event.delta_unaccel(),
                        time: event.time(),
                    },
                );
                pointer.frame(self);
                self.notify_pointer_activity();
                return true;
            }
        }

        let (under, focus_changed) = self
            .ewm
            .update_pointer_focus_for_motion((new_x, new_y).into());

        pointer.motion(
            self,
            under.clone(),
            &MotionEvent {
                location: (new_x, new_y).into(),
                serial,
                time: event.time(),
            },
        );

        // Send relative motion event (needed by some games/apps)
        pointer.relative_motion(
            self,
            under,
            &RelativeMotionEvent {
                delta: event.delta(),
                delta_unaccel: event.delta_unaccel(),
                time: event.time(),
            },
        );

        pointer.frame(self);

        // Notify idle notifier of user activity
        self.notify_pointer_activity();

        if focus_changed {
            self.maybe_activate_pointer_constraint();
        }

        true // needs redraw
    }

    /// Handle absolute pointer motion (touchpads in absolute mode, tablets)
    pub(crate) fn on_pointer_motion_absolute<B: InputBackend>(
        &mut self,
        event: B::PointerMotionAbsoluteEvent,
    ) -> bool
    where
        B::Device: EwmInputDevice,
    {
        tracy_span!("on_pointer_motion_absolute");
        self.ewm.clear_tablet_cursor();
        let pos = self
            .compute_absolute_location(&event, None)
            .unwrap_or_else(|| event.position_transformed(self.ewm.output_size));

        let pointer = self.ewm.pointer.clone();
        let serial = SERIAL_COUNTER.next_serial();

        // Check active pointer constraints (locked or confined).
        let mut pointer_confined = None;
        if let Some((focus_surface, constraint)) = self.active_constraint_for_focus(&pointer) {
            match constraint {
                ActiveConstraint::Locked => {
                    pointer.frame(self);
                    return true;
                }
                ActiveConstraint::Confined(region) => {
                    pointer_confined = Some((focus_surface, region));
                }
            }
        }

        // Handle confined pointer: prevent leaving the surface/region.
        if let Some((focus_surface, region)) = &pointer_confined
            && self.confined_motion_prevented(focus_surface, region.as_ref(), pos)
        {
            pointer.frame(self);
            self.notify_pointer_activity();
            return true;
        }

        let (under, focus_changed) = self.ewm.update_pointer_focus_for_motion(pos);

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time: event.time(),
            },
        );
        pointer.frame(self);

        // Notify idle notifier of user activity
        self.notify_pointer_activity();

        if focus_changed {
            self.maybe_activate_pointer_constraint();
        }

        true // needs redraw
    }

    fn focus_pointer_target(&mut self, source: &str) {
        let pos = self.ewm.pointer_location().into();
        self.focus_target_at(pos, source);
    }

    /// Focus a drop's target like a click; a torn-off tab's window then opens there.
    fn focus_pending_drop(&mut self) {
        if let Some(pos) = self.ewm.pending_drop.take() {
            self.focus_target_at(pos, "drop");
            self.sync_keyboard_focus();
            self.maybe_activate_pointer_constraint();
        }
    }

    /// Click-to-focus at `pos`: layer surfaces get on-demand focus, else the frame under.
    fn focus_target_at(&mut self, pos: Point<f64, Logical>, source: &str) {
        // Under a popup grab an outside press dismisses the popup instead
        if self.ewm.is_locked() || self.ewm.popup_grab.is_some() {
            return;
        }
        let layer_under = self.ewm.layer_under_point(pos);
        let on_layer = layer_under.is_some();
        self.ewm.focus_layer_surface_if_on_demand(layer_under);

        if on_layer {
            return;
        }

        let Some((frame_id, entry_id)) = self.ewm.focus_under(pos, source) else {
            return;
        };
        if let Some(focus_id) = entry_id.or_else(|| self.ewm.focus_id_of_frame(frame_id)) {
            let (x, y) = self
                .ewm
                .pointer_target_at(pos)
                .map(|(_, origin)| {
                    let d = pos - origin;
                    (Some(d.x), Some(d.y))
                })
                .unwrap_or((None, None));
            self.ewm.queue_event(crate::Event::Focus {
                focus_id,
                x,
                y,
                pointer: x.is_some() && y.is_some(),
            });
        }
    }

    /// A click on moved-away content picks an overview frame and brings hidden frames back.
    fn content_click_at(&mut self, pos: Point<f64, Logical>) {
        if self.ewm.is_locked()
            || self.ewm.popup_grab.is_some()
            || self.ewm.frames_in_place()
            || self.ewm.layer_under_point(pos).is_some()
        {
            return;
        }
        if self.ewm.overview.is_active() {
            self.ewm.overview_click(pos);
        }
        if self.ewm.frames_hidden() {
            self.ewm.hide_toggle();
        }
        self.refresh_pointer_after_scene_change(false);
    }

    /// Handle pointer button press/release with click-to-focus
    pub(crate) fn on_pointer_button<B: InputBackend>(&mut self, event: B::PointerButtonEvent) {
        tracy_span!("on_pointer_button");
        self.pointer_button(event.state(), event.button_code(), event.time());
    }

    fn try_start_floating_mod_mouse_grab(
        &mut self,
        button_code: u32,
        serial: smithay::utils::Serial,
    ) -> bool {
        if self.ewm.is_locked() || self.ewm.pointer.is_grabbed() {
            return false;
        }

        let mods = self.ewm.keyboard.modifier_state();
        if !mods.logo {
            return false;
        }

        let pointer = self.ewm.pointer.clone();
        let location = pointer.current_location();

        // Check if we need to start an interactive move.
        if button_code == BTN_LEFT {
            let Some((_, frame_id)) = self.ewm.floating_frame_under(location) else {
                return false;
            };

            self.focus_pointer_target("floating_move_grab");

            if self.ewm.interactive_move_floating_frame_begin(frame_id) {
                let start_data = PointerGrabStartData {
                    focus: None,
                    button: button_code,
                    location,
                };
                let grab = floating_move_grab::MoveGrab::new(start_data, frame_id);
                pointer.set_grab(self, grab, serial, PointerFocus::Clear);
                self.ewm
                    .cursor_manager
                    .set_cursor_image(CursorImageStatus::Named(CursorIcon::Grabbing));
                return true;
            }
        }
        // Check if we need to start an interactive resize.
        else if button_code == BTN_RIGHT {
            let Some((_, frame_id, edges)) = self.ewm.floating_resize_edges_under(location) else {
                return false;
            };

            self.focus_pointer_target("floating_resize_grab");

            if self
                .ewm
                .interactive_resize_floating_frame_begin(frame_id, edges)
            {
                let start_data = PointerGrabStartData {
                    focus: None,
                    button: button_code,
                    location,
                };
                let grab = floating_resize_grab::ResizeGrab::new(start_data, frame_id);
                pointer.set_grab(self, grab, serial, PointerFocus::Clear);
                self.ewm
                    .cursor_manager
                    .set_cursor_image(CursorImageStatus::Named(edges.cursor_icon()));
                return true;
            }
        }

        false
    }

    /// Swap the implicit click grab on an Emacs frame's own surface for `FrameClickGrab`.
    fn replace_frame_click_grab(&mut self, serial: smithay::utils::Serial) {
        let pointer = self.ewm.pointer.clone();
        if pointer.with_grab(|_, grab| grab.is::<ClickGrab<State>>()) != Some(true) {
            return;
        }
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let on_frame = start_data
            .focus
            .as_ref()
            .and_then(|(surface, _)| self.ewm.surface_id(surface))
            .is_some_and(|id| self.ewm.is_emacs_frame(id));
        if on_frame {
            let grab = FrameClickGrab::new(start_data);
            pointer.set_grab(self, grab, serial, PointerFocus::Keep);
        }
    }

    pub(crate) fn pointer_button(
        &mut self,
        button_state: ButtonState,
        button_code: u32,
        time: InputTime,
    ) {
        self.ewm.clear_tablet_cursor();
        let pointer = self.ewm.pointer.clone();
        let serial = SERIAL_COUNTER.next_serial();

        if button_state == ButtonState::Pressed
            && self.try_start_floating_mod_mouse_grab(button_code, serial)
        {
            self.sync_keyboard_focus();
            self.notify_pointer_activity();
            return;
        }

        if button_state == ButtonState::Pressed {
            let pos = self.ewm.pointer_location().into();
            if button_code == BTN_LEFT {
                self.content_click_at(pos);
            }
            self.focus_target_at(pos, "click");
        }
        // Sync keyboard focus before forwarding
        self.sync_keyboard_focus();

        pointer.button(
            self,
            &ButtonEvent {
                button: button_code,
                state: button_state,
                serial,
                time,
            },
        );
        if button_state == ButtonState::Pressed {
            self.replace_frame_click_grab(serial);
        }
        pointer.frame(self);
        self.focus_pending_drop();

        if button_state == ButtonState::Released {
            self.refresh_pointer_after_scene_change(false);
        }

        // Notify idle notifier of user activity
        self.notify_pointer_activity();
    }

    fn working_area_width(&self, name: &str) -> f64 {
        self.ewm
            .find_mapped_output(name)
            .map(|o| self.ewm.get_working_area(&o).size.w as f64)
            .unwrap_or(0.0)
    }

    /// Three-finger swipe begin: arm the deadzone; other finger counts pass through.
    pub(crate) fn on_gesture_swipe_begin<B: InputBackend>(
        &mut self,
        event: B::GestureSwipeBeginEvent,
    ) {
        use smithay::backend::input::GestureBeginEvent;
        if event.fingers() == 3 {
            self.ewm.gesture.cumulative = Some((0.0, 0.0));
            return;
        }
        let pointer = self.ewm.pointer.clone();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        pointer.gesture_swipe_begin(
            self,
            &smithay::input::pointer::GestureSwipeBeginEvent {
                serial,
                time: event.time(),
                fingers: event.fingers(),
            },
        );
    }

    /// Three-finger swipe update: accumulate to deadzone, then drive the active output's strip.
    pub(crate) fn on_gesture_swipe_update<B: InputBackend>(
        &mut self,
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
        // Swiping up always opens the overview, whatever the scroll direction setting.
        let uninverted_dy = dy;
        if let Some(dev) =
            (&event.device() as &dyn std::any::Any).downcast_ref::<libinput::Device>()
            && dev.config_scroll_natural_scroll_enabled()
        {
            dx = -dx;
            dy = -dy;
        }
        let timestamp = std::time::Duration::from_micros(event.time().micros());

        if let Some((cx, cy)) = self.ewm.gesture.cumulative.as_mut() {
            *cx += dx;
            *cy += dy;
            let (cx, cy) = (*cx, *cy);
            if cx * cx + cy * cy >= GESTURE_DEADZONE_SQ {
                self.ewm.gesture.cumulative = None;
                if cx.abs() > cy.abs() {
                    // A hidden strip would scroll unseen.
                    if self.ewm.frames_hidden() {
                        return;
                    }
                    if let Some(out) = self.ewm.active_output() {
                        if let Some(strip) = self.ewm.frame_set.mapped_strip_mut(&out) {
                            strip.gesture_begin();
                        }
                        self.ewm.gesture.active_output = Some(out);
                    }
                } else if !self.ewm.is_locked() {
                    self.ewm.overview_gesture_begin();
                }
            }
            return;
        }

        if let Some(changed) = self.ewm.overview.gesture_update(-uninverted_dy, timestamp) {
            if changed {
                self.ewm.queue_redraw_all();
            }
            return;
        }

        if let Some(out) = self.ewm.gesture.active_output.clone() {
            let w = self.working_area_width(&out);
            // Zoomed-out content still follows the fingers on screen.
            let dx = dx / self.ewm.overview.zoom();
            if let Some(strip) = self.ewm.frame_set.mapped_strip_mut(&out) {
                strip.gesture_update(dx, timestamp, w);
            }
            if let Some(o) = self.ewm.find_mapped_output(&out) {
                self.ewm.queue_redraw(&o);
            }
            return;
        }

        let pointer = self.ewm.pointer.clone();
        pointer.gesture_swipe_update(
            self,
            &smithay::input::pointer::GestureSwipeUpdateEvent {
                time: event.time(),
                delta: (dx, dy).into(),
            },
        );
    }

    /// Three-finger swipe end: snap to the nearest column (no-op if deadzone wasn't crossed).
    pub(crate) fn on_gesture_swipe_end<B: InputBackend>(&mut self, event: B::GestureSwipeEndEvent) {
        use smithay::backend::input::{Event, GestureEndEvent};
        self.ewm.gesture.cumulative = None;
        if self.ewm.overview_gesture_end() {
            self.ewm.queue_redraw_all();
            self.refresh_pointer_after_scene_change(false);
            return;
        }
        if let Some(out) = self.ewm.gesture.active_output.take() {
            let w = self.working_area_width(&out);
            self.ewm.strip_gesture_end(&out, w);
            return;
        }
        let pointer = self.ewm.pointer.clone();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        pointer.gesture_swipe_end(
            self,
            &smithay::input::pointer::GestureSwipeEndEvent {
                serial,
                time: event.time(),
                cancelled: event.cancelled(),
            },
        );
    }

    /// Handle pointer axis (scroll wheel, touchpad scroll)
    pub(crate) fn on_pointer_axis<B: InputBackend>(&mut self, event: B::PointerAxisEvent) {
        tracy_span!("on_pointer_axis");
        self.ewm.clear_tablet_cursor();
        let pointer = self.ewm.pointer.clone();

        // Scroll-to-focus: focus the surface under pointer on scroll
        self.focus_pointer_target("scroll");

        // Sync keyboard focus before forwarding the scroll event.
        self.sync_keyboard_focus();

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

        let mut frame = AxisFrame::new(event.time()).source(source);
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

        pointer.axis(self, frame);
        pointer.frame(self);

        // Notify idle notifier of user activity
        self.notify_activity();
    }

    /// The output a touchscreen maps onto, the first output by default.
    fn output_for_touch<'a>(&'a self, device_name: &str) -> Option<&'a Output> {
        let (device_cfg, type_cfg) =
            lookup_configs(&self.ewm.input_configs, device_name, DeviceType::Touch);
        self.configured_output(device_cfg, type_cfg)
            .or_else(|| self.ewm.first_output())
    }

    /// Touch event position in global space: output mapping and the output transform.
    fn compute_touch_location<B: InputBackend>(
        &self,
        event: &impl AbsolutePositionEvent<B>,
    ) -> Option<Point<f64, Logical>>
    where
        B::Device: EwmInputDevice,
    {
        let fallback = self.output_for_touch(&event.device().name());
        self.compute_absolute_location(event, fallback)
    }

    /// Handle a touch-down event: a new finger contact on the screen.
    pub(crate) fn on_touch_down<B: InputBackend>(&mut self, event: B::TouchDownEvent)
    where
        B::Device: EwmInputDevice,
    {
        use smithay::backend::input::{Event, TouchEvent};
        let Some(touch) = self.ewm.seat.get_touch() else {
            return;
        };
        let Some(pos) = self.compute_touch_location(&event) else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();

        let focus = if self.ewm.is_locked() {
            self.ewm
                .lock_surface_focus()
                .map(|s| (s, (0.0, 0.0).into()))
        } else {
            // Only the first finger presses
            if !touch.is_grabbed() {
                self.content_click_at(pos);
                self.focus_target_at(pos, "touch");
                self.sync_keyboard_focus();
            }
            self.ewm.surface_under_point(pos)
        };

        touch.down(
            self,
            focus,
            &TouchDown {
                slot: event.slot(),
                location: pos,
                serial,
                time: event.time(),
            },
        );
        self.notify_activity();
    }

    /// Handle a touch-motion event: a finger moved while in contact.
    pub(crate) fn on_touch_motion<B: InputBackend>(&mut self, event: B::TouchMotionEvent)
    where
        B::Device: EwmInputDevice,
    {
        use smithay::backend::input::{Event, TouchEvent};
        let Some(touch) = self.ewm.seat.get_touch() else {
            return;
        };
        let Some(pos) = self.compute_touch_location(&event) else {
            return;
        };

        let focus = if self.ewm.is_locked() {
            self.ewm
                .lock_surface_focus()
                .map(|s| (s, (0.0, 0.0).into()))
        } else {
            self.ewm.surface_under_point(pos)
        };

        touch.motion(
            self,
            focus,
            &TouchMotion {
                slot: event.slot(),
                location: pos,
                time: event.time(),
            },
        );
        self.notify_activity();
    }

    /// Handle a touch-up event: a finger lifted from the screen.
    pub(crate) fn on_touch_up<B: InputBackend>(&mut self, event: B::TouchUpEvent) {
        use smithay::backend::input::{Event, TouchEvent};
        let serial = SERIAL_COUNTER.next_serial();
        let Some(touch) = self.ewm.seat.get_touch() else {
            return;
        };

        touch.up(
            self,
            &TouchUp {
                slot: event.slot(),
                serial,
                time: event.time(),
            },
        );
        self.focus_pending_drop();
        self.notify_activity();
    }

    /// Handle a touch-cancel event: the touch sequence was interrupted.
    pub(crate) fn on_touch_cancel<B: InputBackend>(&mut self, _event: B::TouchCancelEvent) {
        let Some(touch) = self.ewm.seat.get_touch() else {
            return;
        };
        touch.cancel(self);
    }

    /// Handle a touch-frame event: end of a logical group of touch events.
    pub(crate) fn on_touch_frame<B: InputBackend>(&mut self, _event: B::TouchFrameEvent) {
        let Some(touch) = self.ewm.seat.get_touch() else {
            return;
        };
        touch.frame(self);
    }

    /// The output a tablet maps onto, resolved through the input config cascade.
    fn output_for_tablet<'a>(&'a self, device_name: &str) -> Option<&'a Output> {
        let (device_cfg, type_cfg) =
            lookup_configs(&self.ewm.input_configs, device_name, DeviceType::Tablet);

        if resolve!(device_cfg, type_cfg, map_to_focused_output, false) {
            let name = self.ewm.active_output()?;
            self.ewm.mapped_output_by_name(&name)
        } else {
            self.configured_output(device_cfg, type_cfg)
        }
    }

    /// Tablet event position in global space: output mapping, aspect correction, clamping.
    fn compute_tablet_position<B: InputBackend<Device: 'static>>(
        &self,
        event: &(impl Event<B> + TabletToolEvent<B>),
    ) -> Option<Point<f64, Logical>> {
        let device = event.device();
        let mapped = self.output_for_tablet(&device.name());
        let (target_geo, transform) = match mapped {
            Some(output) => (
                self.ewm.space.output_geometry(output)?,
                output.current_transform(),
            ),
            // Do not keep ratio for the unified mode as this is what OpenTabletDriver expects.
            None => (
                Rectangle::from_size(self.ewm.output_size),
                Transform::Normal,
            ),
        };

        // FIXME: the 1 px margin should come from the output under the clamped position.
        let scale_output = mapped.or_else(|| self.ewm.first_output())?;
        let px = 1. / scale_output.current_scale().fractional_scale();

        let size = transform.invert().transform_size(target_geo.size);
        let mut pos =
            transform.transform_point_in(event.position_transformed(size), &size.to_f64());

        if mapped.is_some()
            && let Some(device) = (&device as &dyn Any).downcast_ref::<libinput::Device>()
            && let Some(tablet_ratio) = device.size().map(|(w, h)| w / h)
        {
            // This code does the same thing as mutter with "keep aspect ratio" enabled.
            let ratio = tablet_ratio / (size.w as f64 / size.h as f64);
            if ratio > 1. {
                pos.x *= ratio;
            } else {
                pos.y /= ratio;
            }
        }

        pos.x = pos.x.clamp(0.0, (target_geo.size.w as f64 - px).max(0.0));
        pos.y = pos.y.clamp(0.0, (target_geo.size.h as f64 - px).max(0.0));
        Some(pos + target_geo.loc.to_f64())
    }

    pub(crate) fn on_tablet_tool_axis<B: InputBackend<Device: 'static>>(
        &mut self,
        event: B::TabletToolAxisEvent,
    ) {
        self.update_tablet_tool::<B>(&event, true);
        self.notify_pointer_activity();
    }

    /// Move the tool to the event position and deliver its axis values.
    fn update_tablet_tool<B: InputBackend<Device: 'static>>(
        &mut self,
        event: &(impl Event<B> + TabletToolEvent<B>),
        send_frame: bool,
    ) {
        let Some(tool) = self.ewm.seat.tablet_seat().get_tool(&event.tool()) else {
            return;
        };
        let Some(pos) = self.compute_tablet_position(event) else {
            return;
        };
        let time = event.time();
        let frame = tablet_axis_frame(event);

        let under = self.ewm.pointer_target_at(pos);
        tool.motion(
            self,
            under,
            &tablet::tool::MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        // Set axis after motion to ensure it reaches the new focus surface.
        tool.axis(self, frame);
        if send_frame {
            tool.frame(self, time);
        }

        self.ewm.tablet_cursor_location = Some(pos);
        self.ewm.queue_redraw_for_pointer();
    }

    pub(crate) fn on_tablet_tool_tip<B: InputBackend<Device: 'static>>(
        &mut self,
        event: B::TabletToolTipEvent,
    ) {
        let Some(tool) = self.ewm.seat.tablet_seat().get_tool(&event.tool()) else {
            return;
        };
        let time = event.time();

        match event.tip_state() {
            TabletToolTipState::Down => {
                // Tip events can come together with axis event data with no separate axis event.
                self.update_tablet_tool::<B>(&event, false);
                let serial = SERIAL_COUNTER.next_serial();
                tool.down(self, &tablet::tool::DownEvent { serial, time });

                if let Some(pos) = self.ewm.tablet_cursor_location {
                    self.focus_target_at(pos, "tablet");
                    self.sync_keyboard_focus();
                }
            }
            TabletToolTipState::Up => {
                let serial = SERIAL_COUNTER.next_serial();
                tool.up(self, &tablet::tool::UpEvent { serial, time });
                self.update_tablet_tool::<B>(&event, false);
            }
        }

        tool.frame(self, time);
        self.notify_pointer_activity();
    }

    pub(crate) fn on_tablet_tool_proximity<B: InputBackend<Device: 'static>>(
        &mut self,
        event: B::TabletToolProximityEvent,
    ) {
        let tablet_seat = self.ewm.seat.tablet_seat();
        let display_handle = self.ewm.display_handle.clone();
        let tool = tablet_seat
            .get_tool(&event.tool())
            .unwrap_or_else(|| tablet_seat.add_wp_tool(self, &display_handle, &event.tool()));
        let Some(tablet) = tablet_seat.get_tablet(&TabletDescriptor::from(&event.device())) else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        let time = event.time();

        match event.state() {
            ProximityState::In => {
                let Some(pos) = self.compute_tablet_position(&event) else {
                    return;
                };
                let under = self.ewm.pointer_target_at(pos);
                tool.proximity_in(
                    self,
                    under,
                    tablet,
                    &tablet::tool::ProximityInEvent {
                        location: pos,
                        axis: Some(tablet_axis_frame(&event)),
                        serial,
                        time,
                    },
                );
                tool.frame(self, time);

                self.ewm.tablet_cursor_location = Some(pos);
                self.ewm.queue_redraw_for_pointer();
            }
            ProximityState::Out => {
                tool.proximity_out(self, &tablet::tool::ProximityOutEvent { serial, time });
                tool.frame(self, time);

                // Land the pointer where the pen left so it doesn't jump.
                if let Some(pos) = self.ewm.tablet_cursor_location {
                    self.warp_pointer(pos.x, pos.y);
                }
                self.ewm.clear_tablet_cursor();
            }
        }
        self.notify_pointer_activity();
    }

    pub(crate) fn on_tablet_tool_button<B: InputBackend>(
        &mut self,
        event: B::TabletToolButtonEvent,
    ) {
        if let Some(tool) = self.ewm.seat.tablet_seat().get_tool(&event.tool()) {
            let time = event.time();
            tool.button(
                self,
                &tablet::tool::ButtonEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    button: event.button(),
                    state: event.button_state(),
                    time,
                },
            );
            tool.frame(self, time);
        }
        self.notify_pointer_activity();
    }

    /// The output a device's :map-to-output names, resolved through the input config cascade.
    fn configured_output<'a>(
        &'a self,
        device_cfg: Option<&InputConfigEntry>,
        type_cfg: Option<&InputConfigEntry>,
    ) -> Option<&'a Output> {
        device_cfg
            .and_then(|c| c.map_to_output.as_deref())
            .or_else(|| type_cfg.and_then(|c| c.map_to_output.as_deref()))
            .and_then(|name| self.ewm.mapped_output_by_name(name))
    }
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
    }
}

/// Pure classification of a keyboard event. Reads no compositor state; the
/// caller resolves the `KeyRouting` upstream and applies side effects
/// downstream. Releases always Forward.
pub fn classify_key(
    intercepted_keys: &[InterceptedKey],
    routing: KeyRouting,
    key: KeyInput<'_>,
) -> KeyAction {
    let KeyRouting {
        text_input_intercept,
        focus_on_emacs,
        fullscreen_active,
        focused_surface_id,
    } = routing;
    let KeyInput {
        is_press,
        keysym_raw,
        modified,
        mods,
        changes_modifiers: key_changes_modifiers,
    } = key;
    let modified_raw = modified.raw();

    if !is_press {
        return KeyAction::Forward;
    }

    if let Some(vt) = decode_vt_switch(modified_raw) {
        return KeyAction::VtSwitch(vt);
    }

    if let Some(ik) = intercepted_keys.iter().find(|ik| {
        ik.surface.is_none_or(|s| s == focused_surface_id)
            && ik.matches(keysym_raw, modified_raw, mods)
    }) {
        if focus_on_emacs {
            return KeyAction::Forward;
        }
        if !fullscreen_active || ik.allow_fullscreen {
            return intercepted_key_action(ik, keysym_raw);
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
        if let Some(region) = constraint.region()
            && !region.contains(pos_within_surface.to_i32_round())
        {
            return None;
        }
        match &*constraint {
            PointerConstraint::Locked(_) => Some(ActiveConstraint::Locked),
            PointerConstraint::Confined(c) => Some(ActiveConstraint::Confined(c.region().cloned())),
        }
    })
}

fn tablet_axis_frame<B: InputBackend>(event: &impl TabletToolEvent<B>) -> tablet::tool::AxisFrame {
    tablet::tool::AxisFrame {
        pressure: event.pressure_has_changed().then(|| event.pressure()),
        distance: event.distance_has_changed().then(|| event.distance()),
        tilt: event.tilt_has_changed().then(|| event.tilt()),
        rotation: event.rotation_has_changed().then(|| event.rotation()),
        slider: event.slider_has_changed().then(|| event.slider_position()),
        wheel: event
            .wheel_has_changed()
            .then(|| (event.wheel_delta(), event.wheel_delta_discrete())),
    }
}

/// Detect the type of a libinput device.
///
/// - Tablet: has TabletTool capability
/// - Touch: has Touch capability
/// - Touchpad: `config_tap_finger_count() > 0`
/// - Trackball/trackpoint: udev properties `ID_INPUT_TRACKBALL` / `ID_INPUT_POINTINGSTICK`
/// - Mouse: has Pointer capability and is not touchpad/trackball/trackpoint
fn detect_device_type(device: &libinput::Device) -> Option<DeviceType> {
    if device.has_capability(libinput::DeviceCapability::TabletTool) {
        return Some(DeviceType::Tablet);
    }

    if device.has_capability(libinput::DeviceCapability::Touch) {
        return Some(DeviceType::Touch);
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
