//! K-series: classification of intercept keys against the fullscreen state.
//!
//! K1  - Allow-fullscreen binding under fullscreen redirects to Emacs.
//!       (`allow_fullscreen=true` + `fullscreen_surface_id=Some` +
//!        `focus_on_emacs=false` -> `Redirect`).
//!
//! K2  - Non-flagged binding under fullscreen forwards to the client.
//!       (`allow_fullscreen=false` + `fullscreen_surface_id=Some` +
//!        `focus_on_emacs=false` -> `Forward`).
//!
//! K3  - Without fullscreen, intercepted bindings redirect regardless of
//!       the flag. (`fullscreen_surface_id=None` + `focus_on_emacs=false`
//!        -> `Redirect`).
//!
//! K4  - When Emacs already holds focus, intercept rules defer — the key
//!       forwards (Emacs is the focused client). Independent of the flag
//!       and of fullscreen.

use ewm_core::input::{classify_key, KeyAction};
use ewm_core::InterceptedKey;
use proptest::prelude::*;
use smithay::input::keyboard::{Keysym, ModifiersState};

fn ik(keysym: u32, allow_fullscreen: bool) -> InterceptedKey {
    InterceptedKey {
        keysym,
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
        allow_fullscreen,
    }
}

/// Keysym range that avoids the XF86Switch_VT_* block and stays in the
/// Latin-1 area so neither VT-switch nor text-input branches steal the
/// classification.
fn keysym_strategy() -> impl Strategy<Value = u32> {
    0x41u32..=0x5a
}

/// Thin call-site helper so each proptest doesn't restate the 10-arg
/// `classify_key` signature with `keysym, keysym, Keysym::from(keysym)`.
fn classify(
    keys: &[InterceptedKey],
    text_input: bool,
    focus_on_emacs: bool,
    fs: Option<u64>,
    is_press: bool,
    keysym: u32,
) -> KeyAction {
    classify_key(
        keys,
        &[],
        text_input,
        focus_on_emacs,
        fs,
        is_press,
        keysym,
        keysym,
        Keysym::from(keysym),
        &ModifiersState::default(),
    )
}

proptest! {
    #[test]
    fn k1_allow_fullscreen_under_fullscreen_redirects(keysym in keysym_strategy()) {
        let action = classify(&[ik(keysym, true)], false, false, Some(1), true, keysym);
        prop_assert_eq!(action, KeyAction::Redirect(keysym));
    }

    #[test]
    fn k2_no_flag_under_fullscreen_forwards(keysym in keysym_strategy()) {
        let action = classify(&[ik(keysym, false)], false, false, Some(1), true, keysym);
        prop_assert_eq!(action, KeyAction::Forward);
    }

    #[test]
    fn k3_no_fullscreen_redirects_regardless_of_flag(
        keysym in keysym_strategy(),
        flag in any::<bool>(),
    ) {
        let action = classify(&[ik(keysym, flag)], false, false, None, true, keysym);
        prop_assert_eq!(action, KeyAction::Redirect(keysym));
    }

    #[test]
    fn k4_focus_on_emacs_forwards(
        keysym in keysym_strategy(),
        flag in any::<bool>(),
        fs in proptest::option::of(Just(1u64)),
    ) {
        let action = classify(&[ik(keysym, flag)], false, true, fs, true, keysym);
        prop_assert_eq!(action, KeyAction::Forward);
    }

    /// Releases always forward; the K-series is press-driven.
    #[test]
    fn release_always_forwards(
        keysym in keysym_strategy(),
        flag in any::<bool>(),
        focus_on_emacs in any::<bool>(),
        fs in proptest::option::of(Just(1u64)),
    ) {
        let action = classify(&[ik(keysym, flag)], false, focus_on_emacs, fs, false, keysym);
        prop_assert_eq!(action, KeyAction::Forward);
    }

    /// classify_key is total over every combination of its inputs.
    #[test]
    fn classify_is_total(
        keysym in keysym_strategy(),
        flag in any::<bool>(),
        focus_on_emacs in any::<bool>(),
        is_press in any::<bool>(),
        fs in proptest::option::of(Just(1u64)),
        text_input in any::<bool>(),
    ) {
        let _ = classify(&[ik(keysym, flag)], text_input, focus_on_emacs, fs, is_press, keysym);
    }
}
