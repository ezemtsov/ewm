//! Key binding matching over every pair of modifier sets.

use ewm_core::{InterceptDispatch, InterceptedKey, Trigger};
use smithay::input::keyboard::{ModifiersState, keysyms};

const NON_UPPERCASE: [u32; 5] = [
    keysyms::KEY_a,
    keysyms::KEY_x,
    keysyms::KEY_1,
    keysyms::KEY_7,
    keysyms::KEY_semicolon,
];

const SHIFTED_PAIRS: [(u32, u32); 2] = [
    (keysyms::KEY_semicolon, keysyms::KEY_colon),
    (keysyms::KEY_7, keysyms::KEY_ampersand),
];

#[derive(Debug, Clone, Copy)]
struct Flags {
    ctrl: bool,
    alt: bool,
    shift: bool,
    logo: bool,
}

fn flags() -> impl Iterator<Item = Flags> {
    (0u8..16).map(|bits| Flags {
        ctrl: bits & 1 != 0,
        alt: bits & 2 != 0,
        shift: bits & 4 != 0,
        logo: bits & 8 != 0,
    })
}

/// Every (required, actual) pair of modifier sets.
fn pairs() -> impl Iterator<Item = (Flags, Flags)> {
    flags().flat_map(|required| flags().map(move |actual| (required, actual)))
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
        trigger: Trigger::Keysym(keysym),
        key: format!("key-{keysym}"),
        ctrl: flags.ctrl,
        alt: flags.alt,
        shift: flags.shift,
        logo: flags.logo,
        allow_fullscreen: false,
        dispatch: InterceptDispatch::Keyboard,
        repeat: false,
        surface: None,
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

#[test]
fn raw_non_uppercase_keysym_requires_exact_modifiers() {
    for keysym in NON_UPPERCASE {
        for (required, actual) in pairs() {
            assert_eq!(
                intercepted(keysym, required).matches(keysym, keysym, &mods(actual)),
                exact_match(required, actual),
                "{keysym:#x} {required:?} {actual:?}"
            );
        }
    }
}

#[test]
fn uppercase_letter_matches_lowercase_binding_and_ignores_shift() {
    for letter in 0..26 {
        let raw = keysyms::KEY_A + letter;
        for (required, actual) in pairs() {
            assert_eq!(
                intercepted(keysyms::KEY_a + letter, required).matches(raw, raw, &mods(actual)),
                match_ignoring_shift(required, actual),
                "{raw:#x} {required:?} {actual:?}"
            );
        }
    }
}

#[test]
fn mismatched_raw_keysym_rejects_without_modified_match() {
    for target in 0..26 {
        for offset in 1..26 {
            let raw = keysyms::KEY_a + (target + offset) % 26;
            for (required, actual) in pairs() {
                assert!(
                    !intercepted(keysyms::KEY_a + target, required).matches(
                        raw,
                        raw,
                        &mods(actual)
                    ),
                    "{target} {offset} {required:?} {actual:?}"
                );
            }
        }
    }
}

#[test]
fn shifted_modified_keysym_fallback_ignores_shift_only() {
    for (raw, modified) in SHIFTED_PAIRS {
        for (required, actual) in pairs() {
            assert_eq!(
                intercepted(modified, required).matches(raw, modified, &mods(actual)),
                match_ignoring_shift(required, actual),
                "{raw:#x} {required:?} {actual:?}"
            );
        }
    }
}

#[test]
fn raw_match_still_requires_shift_when_modified_differs() {
    for (raw, modified) in SHIFTED_PAIRS {
        for required in flags() {
            let actual = Flags {
                shift: !required.shift,
                ..required
            };
            assert!(
                !intercepted(raw, required).matches(raw, modified, &mods(actual)),
                "{raw:#x} {required:?}"
            );
        }
    }
}
