//! K-series: classification of intercept keys against the fullscreen state.
//!
//! K1  - Allow-fullscreen binding under fullscreen redirects to Emacs.
//!       (`allow_fullscreen=true` + `fullscreen_active=true` +
//!        `focus_on_emacs=false` -> `KeyboardRedirect` or `Command`).
//!
//! K2  - Non-flagged binding under fullscreen forwards to the client.
//!       (`allow_fullscreen=false` + `fullscreen_active=true` +
//!        `focus_on_emacs=false` -> `Forward`).
//!
//! K3  - Without fullscreen, intercepted bindings redirect regardless of
//!       the flag. (`fullscreen_active=false` + `focus_on_emacs=false`
//!        -> `KeyboardRedirect` or `Command`).
//!
//! K4  - When Emacs already holds focus, intercept rules defer — the key
//!       forwards (Emacs is the focused client). Independent of the flag
//!       and of fullscreen.

use ewm_core::input::{KeyAction, KeyInput, KeyRouting, classify_key};
use ewm_core::{InterceptDispatch, InterceptedKey, TranslateTarget};
use proptest::prelude::*;
use smithay::input::keyboard::{Keysym, ModifiersState, keysyms};

fn text_input_action(keysym: u32, utf8: Option<String>, mods: &ModifiersState) -> KeyAction {
    KeyAction::TextInput {
        keysym,
        utf8,
        ctrl: mods.ctrl,
        alt: mods.alt,
        shift: mods.shift,
        logo: mods.logo,
    }
}

fn ik(keysym: u32, allow_fullscreen: bool) -> InterceptedKey {
    ik_with_dispatch_and_repeat(keysym, allow_fullscreen, InterceptDispatch::Keyboard, false)
}

fn ik_with_dispatch_and_repeat(
    keysym: u32,
    allow_fullscreen: bool,
    dispatch: InterceptDispatch,
    repeat: bool,
) -> InterceptedKey {
    InterceptedKey {
        keysym,
        key: format!("key-{keysym}"),
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
        allow_fullscreen,
        dispatch,
        repeat,
    }
}

fn ik_translate(keysym: u32, target: TranslateTarget) -> InterceptedKey {
    InterceptedKey {
        keysym,
        key: format!("key-{keysym}"),
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
        allow_fullscreen: false,
        dispatch: InterceptDispatch::Translate(target),
        repeat: false,
    }
}

fn ctrl_target(keysym: u32) -> TranslateTarget {
    TranslateTarget {
        keysym,
        ctrl: true,
        ..Default::default()
    }
}

/// Keysym range that avoids the XF86Switch_VT_* block and stays in the
/// Latin-1 area so neither VT-switch nor text-input branches steal the
/// classification.
fn keysym_strategy() -> impl Strategy<Value = u32> {
    0x41u32..=0x5a
}

/// A non-modifier key whose raw and modified keysyms are both `keysym`.
fn key_input(is_press: bool, keysym: u32, mods: &ModifiersState) -> KeyInput<'_> {
    KeyInput {
        is_press,
        keysym_raw: keysym,
        modified: Keysym::from(keysym),
        mods,
        changes_modifiers: false,
    }
}

/// Thin call-site helper so each proptest can state the routing inline.
fn classify(
    intercepted_keys: &[InterceptedKey],
    text_input: bool,
    focus_on_emacs: bool,
    fullscreen_active: bool,
    is_press: bool,
    keysym: u32,
) -> KeyAction {
    let routing = KeyRouting {
        text_input_intercept: text_input,
        focus_on_emacs,
        fullscreen_active,
    };
    classify_key(
        intercepted_keys,
        routing,
        key_input(is_press, keysym, &ModifiersState::default()),
    )
}

proptest! {
    #[test]
    fn k1_allow_fullscreen_under_fullscreen_redirects(keysym in keysym_strategy()) {
        let action = classify(&[ik(keysym, true)], false, false, true, true, keysym);
        prop_assert_eq!(action, KeyAction::KeyboardRedirect(keysym));
    }

    #[test]
    fn k2_no_flag_under_fullscreen_forwards(keysym in keysym_strategy()) {
        let action = classify(&[ik(keysym, false)], false, false, true, true, keysym);
        prop_assert_eq!(action, KeyAction::Forward);
    }

    #[test]
    fn k3_no_fullscreen_redirects_regardless_of_flag(
        keysym in keysym_strategy(),
        flag in any::<bool>(),
    ) {
        let action = classify(&[ik(keysym, flag)], false, false, false, true, keysym);
        prop_assert_eq!(action, KeyAction::KeyboardRedirect(keysym));
    }

    #[test]
    fn command_dispatch_intercepts_without_keyboard_redirect(
        keysym in keysym_strategy(),
        flag in any::<bool>(),
        repeat in any::<bool>(),
    ) {
        let key = format!("key-{keysym}");
        let action = classify(
            &[ik_with_dispatch_and_repeat(keysym, flag, InterceptDispatch::Command, repeat)],
            false,
            false,
            false,
            true,
            keysym,
        );
        prop_assert_eq!(action, KeyAction::Command { key, repeat });
    }

    #[test]
    fn k4_focus_on_emacs_forwards(
        keysym in keysym_strategy(),
        flag in any::<bool>(),
        fullscreen_active in any::<bool>(),
    ) {
        let action = classify(&[ik(keysym, flag)], false, true, fullscreen_active, true, keysym);
        prop_assert_eq!(action, KeyAction::Forward);
    }

    /// Releases always forward; the K-series is press-driven.
    #[test]
    fn release_always_forwards(
        keysym in keysym_strategy(),
        flag in any::<bool>(),
        focus_on_emacs in any::<bool>(),
        fullscreen_active in any::<bool>(),
    ) {
        let action = classify(
            &[ik(keysym, flag)],
            false,
            focus_on_emacs,
            fullscreen_active,
            false,
            keysym,
        );
        prop_assert_eq!(action, KeyAction::Forward);
    }

    #[test]
    fn vt_switch_precedes_bindings_on_press(
        vt in 1u32..=12,
        text_input in any::<bool>(),
        focus_on_emacs in any::<bool>(),
        fullscreen_active in any::<bool>(),
    ) {
        let keysym = keysyms::KEY_XF86Switch_VT_1 + vt - 1;
        let action = classify(
            &[ik_translate(keysym, ctrl_target(keysym))],
            text_input,
            focus_on_emacs,
            fullscreen_active,
            true,
            keysym,
        );
        prop_assert_eq!(action, KeyAction::VtSwitch(vt as i32));
    }

    #[test]
    fn translate_applies_when_focus_is_outside_emacs(keysym in keysym_strategy()) {
        let target = ctrl_target(keysym);
        let action = classify(
            &[ik_translate(keysym, target)],
            false,
            false,
            false,
            true,
            keysym,
        );
        prop_assert_eq!(action, KeyAction::Translate(target));
    }

    #[test]
    fn translate_defers_when_emacs_already_has_focus(keysym in keysym_strategy()) {
        let action = classify(
            &[ik_translate(keysym, ctrl_target(keysym))],
            false,
            true,
            false,
            true,
            keysym,
        );
        prop_assert_eq!(action, KeyAction::Forward);
    }

    #[test]
    fn text_input_intercepts_printable_keys_with_utf8(
        letter in 0u32..26,
    ) {
        let keysym = keysyms::KEY_a + letter;
        let action = classify(&[], true, false, false, true, keysym);
        let expected = char::from_u32(u32::from(b'a') + letter).unwrap().to_string();

        prop_assert_eq!(
            action,
            text_input_action(keysym, Some(expected), &ModifiersState::default())
        );
    }

    /// The intercept decision ignores held modifiers: only whether the key is
    /// itself a modifier matters. A letter under Ctrl/Alt/Logo still intercepts.
    #[test]
    fn text_input_intercepts_printable_keys_regardless_of_held_modifiers(
        letter in 0u32..26,
        ctrl in any::<bool>(),
        alt in any::<bool>(),
        shift in any::<bool>(),
        logo in any::<bool>(),
    ) {
        let mods = ModifiersState {
            ctrl,
            alt,
            shift,
            logo,
            ..Default::default()
        };
        let keysym = keysyms::KEY_a + letter;
        let routing = KeyRouting {
            text_input_intercept: true,
            ..Default::default()
        };
        let action = classify_key(&[], routing, key_input(true, keysym, &mods));
        let expected = char::from_u32(u32::from(b'a') + letter).unwrap().to_string();

        prop_assert_eq!(action, text_input_action(keysym, Some(expected), &mods));
    }

    #[test]
    fn text_input_intercepts_non_printable_keys_without_utf8(_ in Just(())) {
        let keysym = keysyms::KEY_Return;
        let action = classify(&[], true, false, false, true, keysym);

        prop_assert_eq!(
            action,
            text_input_action(keysym, None, &ModifiersState::default())
        );
    }

    #[test]
    fn text_input_defers_to_emacs_focus_or_modifier_keys(
        keysym in keysym_strategy(),
        focus_on_emacs in any::<bool>(),
        key_changes_modifiers in any::<bool>(),
    ) {
        prop_assume!(focus_on_emacs || key_changes_modifiers);
        let routing = KeyRouting {
            text_input_intercept: true,
            focus_on_emacs,
            fullscreen_active: false,
        };
        let mods = ModifiersState::default();
        let action = classify_key(
            &[],
            routing,
            KeyInput {
                changes_modifiers: key_changes_modifiers,
                ..key_input(true, keysym, &mods)
            },
        );

        prop_assert_eq!(action, KeyAction::Forward);
    }
}
