//! Properties for key binding matching.

use ewm_core::{InterceptDispatch, InterceptedKey};
use proptest::prelude::*;
use smithay::input::keyboard::{ModifiersState, keysyms};

#[derive(Debug, Clone, Copy)]
struct Flags {
    ctrl: bool,
    alt: bool,
    shift: bool,
    logo: bool,
}

fn flags() -> impl Strategy<Value = Flags> {
    (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
        |(ctrl, alt, shift, logo)| Flags {
            ctrl,
            alt,
            shift,
            logo,
        },
    )
}

fn mods(flags: Flags) -> ModifiersState {
    ModifiersState {
        ctrl: flags.ctrl,
        alt: flags.alt,
        shift: flags.shift,
        logo: flags.logo,
        ..Default::default()
    }
}

fn intercepted(keysym: u32, flags: Flags) -> InterceptedKey {
    InterceptedKey {
        keysym,
        key: format!("key-{keysym}"),
        ctrl: flags.ctrl,
        alt: flags.alt,
        shift: flags.shift,
        logo: flags.logo,
        allow_fullscreen: false,
        dispatch: InterceptDispatch::Keyboard,
        repeat: false,
    }
}

fn exact_match(left: Flags, right: Flags) -> bool {
    left.ctrl == right.ctrl
        && left.alt == right.alt
        && left.shift == right.shift
        && left.logo == right.logo
}

fn match_ignoring_shift(left: Flags, right: Flags) -> bool {
    left.ctrl == right.ctrl && left.alt == right.alt && left.logo == right.logo
}

fn non_uppercase_keysym() -> impl Strategy<Value = u32> {
    prop::sample::select(vec![
        keysyms::KEY_a,
        keysyms::KEY_x,
        keysyms::KEY_1,
        keysyms::KEY_7,
        keysyms::KEY_semicolon,
    ])
}

fn shifted_pair() -> impl Strategy<Value = (u32, u32)> {
    prop::sample::select(vec![
        (keysyms::KEY_semicolon, keysyms::KEY_colon),
        (keysyms::KEY_7, keysyms::KEY_ampersand),
    ])
}

proptest! {
    #[test]
    fn raw_non_uppercase_keysym_requires_exact_modifiers(
        keysym in non_uppercase_keysym(),
        required in flags(),
        actual in flags(),
    ) {
        let key = intercepted(keysym, required);

        prop_assert_eq!(
            key.matches(keysym, keysym, &mods(actual)),
            exact_match(required, actual),
        );
    }

    #[test]
    fn uppercase_letter_matches_lowercase_binding_and_ignores_shift(
        letter in 0u32..26,
        required in flags(),
        actual in flags(),
    ) {
        let raw = keysyms::KEY_A + letter;
        let key = intercepted(keysyms::KEY_a + letter, required);

        prop_assert_eq!(
            key.matches(raw, raw, &mods(actual)),
            match_ignoring_shift(required, actual),
        );
    }

    #[test]
    fn mismatched_raw_keysym_rejects_without_modified_match(
        target in 0u32..26,
        offset in 1u32..26,
        required in flags(),
        actual in flags(),
    ) {
        let target_keysym = keysyms::KEY_a + target;
        let raw = keysyms::KEY_a + ((target + offset) % 26);
        let key = intercepted(target_keysym, required);

        prop_assert!(!key.matches(raw, raw, &mods(actual)));
    }

    #[test]
    fn shifted_modified_keysym_fallback_ignores_shift_only(
        (raw, modified) in shifted_pair(),
        required in flags(),
        actual in flags(),
    ) {
        let key = intercepted(modified, required);

        prop_assert_eq!(
            key.matches(raw, modified, &mods(actual)),
            match_ignoring_shift(required, actual),
        );
    }

    #[test]
    fn raw_match_still_requires_shift_when_modified_differs(
        (raw, modified) in shifted_pair(),
        required in flags(),
    ) {
        let actual = Flags {
            shift: !required.shift,
            ..required
        };
        let key = intercepted(raw, required);

        prop_assert!(!key.matches(raw, modified, &mods(actual)));
    }
}
