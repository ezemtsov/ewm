//! Properties of key classification that hold over every routing state: a release never runs a
//! binding, a VT switch comes before everything, and text input takes printable keys whatever
//! modifiers are held while deferring to Emacs focus and to modifier keys. The intercept rules
//! themselves are pinned through real clients in input_hotpath_integration.rs.

use ewm_core::input::{KeyAction, KeyInput, KeyRouting, classify_key};
use ewm_core::{InterceptDispatch, InterceptedKey, Trigger};
use proptest::prelude::*;
use smithay::input::keyboard::{Keysym, ModifiersState, keysyms};

/// The focused surface whose text input Emacs serves in these tests.
const SURFACE: u64 = 1;

fn text_input_action(keysym: u32, utf8: Option<String>, mods: &ModifiersState) -> KeyAction {
    KeyAction::TextInput {
        surface_id: SURFACE,
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
        trigger: Trigger::Keysym(keysym),
        key: format!("key-{keysym}"),
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
        allow_fullscreen,
        dispatch,
        repeat,
        surface: None,
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
        text_input_surface: text_input.then_some(SURFACE),
        focus_on_emacs,
        fullscreen_active,
        focused_surface_id: None,
    };
    classify_key(
        intercepted_keys,
        routing,
        key_input(is_press, keysym, &ModifiersState::default()),
    )
}

proptest! {
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
            &[ik(keysym, false)],
            text_input,
            focus_on_emacs,
            fullscreen_active,
            true,
            keysym,
        );
        prop_assert_eq!(action, KeyAction::VtSwitch(vt as i32));
    }

    /// The intercept decision ignores held modifiers: only whether the key is
    /// itself a modifier matters. A letter under Ctrl/Alt/Logo still intercepts,
    /// and a key with no text, Return, intercepts with none.
    #[test]
    fn text_input_intercepts_keys_regardless_of_held_modifiers(
        key in prop_oneof![
            (0u32..26).prop_map(|letter| {
                let text = char::from_u32(u32::from(b'a') + letter).unwrap().to_string();
                (keysyms::KEY_a + letter, Some(text))
            }),
            Just((keysyms::KEY_Return, None)),
        ],
        ctrl in any::<bool>(),
        alt in any::<bool>(),
        shift in any::<bool>(),
        logo in any::<bool>(),
    ) {
        let (keysym, text) = key;
        let mods = ModifiersState {
            ctrl,
            alt,
            shift,
            logo,
            ..Default::default()
        };
        let routing = KeyRouting {
            text_input_surface: Some(SURFACE),
            ..Default::default()
        };
        let action = classify_key(&[], routing, key_input(true, keysym, &mods));

        prop_assert_eq!(action, text_input_action(keysym, text, &mods));
    }

    #[test]
    fn text_input_defers_to_emacs_focus_or_modifier_keys(
        keysym in keysym_strategy(),
        focus_on_emacs in any::<bool>(),
        key_changes_modifiers in any::<bool>(),
    ) {
        prop_assume!(focus_on_emacs || key_changes_modifiers);
        let routing = KeyRouting {
            text_input_surface: Some(SURFACE),
            focus_on_emacs,
            fullscreen_active: false,
            focused_surface_id: None,
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
