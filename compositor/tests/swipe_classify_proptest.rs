//! S-series: classification of touchpad swipes against the bindings and the gesture knobs.
//!
//! S1  - A binding matching fingers, direction and modifiers wins over the
//!       built-in swipes: an `Action` dispatch runs the action, any other
//!       dispatch runs the key as an Emacs command.
//!
//! S2  - Under fullscreen a binding without the flag yields to the built-in
//!       swipes, as a key without it yields to the client.
//!
//! S3  - Without a binding, a horizontal swipe of the strip's finger count
//!       scrolls the strip, a vertical one of the overview's drives the
//!       overview, and anything else is the client's.
//!
//! S4  - Modifiers match exactly: a binding under other modifiers is no
//!       binding at all.
//!
//! S5  - A binding scoped to one surface applies while that surface has
//!       focus and is no binding elsewhere.

use ewm_core::input::{Bound, Gestures, SwipeAction, SwipeDirection, classify_swipe};
use ewm_core::{Action, InterceptDispatch, InterceptedKey, Trigger};
use proptest::prelude::*;
use smithay::input::keyboard::ModifiersState;

fn mods() -> impl Strategy<Value = ModifiersState> {
    (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
        |(ctrl, alt, shift, logo)| ModifiersState {
            ctrl,
            alt,
            shift,
            logo,
            ..Default::default()
        },
    )
}

fn fingers() -> impl Strategy<Value = u32> {
    3u32..=5
}

fn direction() -> impl Strategy<Value = SwipeDirection> {
    prop::sample::select(SwipeDirection::ALL.to_vec())
}

fn knob() -> impl Strategy<Value = Option<u32>> {
    prop::option::of(3u32..=5)
}

fn gestures() -> impl Strategy<Value = Gestures> {
    (knob(), knob()).prop_map(|(strip_swipe_fingers, overview_swipe_fingers)| Gestures {
        strip_swipe_fingers,
        overview_swipe_fingers,
    })
}

fn dispatch() -> impl Strategy<Value = InterceptDispatch> {
    prop::sample::select(vec![
        InterceptDispatch::Keyboard,
        InterceptDispatch::Command,
        InterceptDispatch::Action(Action::HideToggle),
    ])
}

fn binding(
    fingers: u32,
    direction: SwipeDirection,
    mods: ModifiersState,
    allow_fullscreen: bool,
    dispatch: InterceptDispatch,
) -> InterceptedKey {
    InterceptedKey {
        trigger: Trigger::Swipe { fingers, direction },
        key: format!("swipe-{fingers}-{direction:?}"),
        ctrl: mods.ctrl,
        alt: mods.alt,
        shift: mods.shift,
        logo: mods.logo,
        allow_fullscreen,
        dispatch,
        repeat: false,
        surface: None,
    }
}

/// What the bound key runs: its action, or itself as an Emacs command.
fn bound(ik: &InterceptedKey) -> SwipeAction {
    SwipeAction::Bound(match &ik.dispatch {
        InterceptDispatch::Action(action) => Bound::Action(action.clone()),
        _ => Bound::Command {
            key: ik.key.clone(),
        },
    })
}

/// S3 spelled out: the built-in swipe of this finger count along this axis, else the client's.
fn built_in(gestures: Gestures, fingers: u32, direction: SwipeDirection) -> SwipeAction {
    let (knob, action) = if direction.is_horizontal() {
        (gestures.strip_swipe_fingers, SwipeAction::Strip)
    } else {
        (gestures.overview_swipe_fingers, SwipeAction::Overview)
    };
    if knob == Some(fingers) {
        action
    } else {
        SwipeAction::Forward
    }
}

proptest! {
    #[test]
    fn s1_a_matching_binding_wins(
        fingers in fingers(),
        direction in direction(),
        mods in mods(),
        dispatch in dispatch(),
        gestures in gestures(),
    ) {
        let ik = binding(fingers, direction, mods, false, dispatch);
        let bindings = std::slice::from_ref(&ik);
        let action = classify_swipe(bindings, false, 0, gestures, fingers, direction, &mods);
        prop_assert_eq!(action, bound(&ik));
    }

    #[test]
    fn s2_under_fullscreen_only_a_flagged_binding_runs(
        fingers in fingers(),
        direction in direction(),
        mods in mods(),
        allow_fullscreen in any::<bool>(),
        gestures in gestures(),
    ) {
        let ik = binding(fingers, direction, mods, allow_fullscreen, InterceptDispatch::Command);
        let bindings = std::slice::from_ref(&ik);
        let action = classify_swipe(bindings, true, 0, gestures, fingers, direction, &mods);
        let expected = if allow_fullscreen {
            bound(&ik)
        } else {
            built_in(gestures, fingers, direction)
        };
        prop_assert_eq!(action, expected);
    }

    #[test]
    fn s3_without_a_binding_the_knobs_decide(
        fingers in fingers(),
        direction in direction(),
        mods in mods(),
        fullscreen_active in any::<bool>(),
        gestures in gestures(),
    ) {
        let action =
            classify_swipe(&[], fullscreen_active, 0, gestures, fingers, direction, &mods);
        prop_assert_eq!(action, built_in(gestures, fingers, direction));
    }

    #[test]
    fn s4_other_modifiers_are_no_binding(
        fingers in fingers(),
        direction in direction(),
        bound_mods in mods(),
        held_mods in mods(),
        gestures in gestures(),
    ) {
        prop_assume!(bound_mods != held_mods);
        let ik = binding(fingers, direction, bound_mods, true, InterceptDispatch::Command);
        let action = classify_swipe(&[ik], false, 0, gestures, fingers, direction, &held_mods);
        prop_assert_eq!(action, built_in(gestures, fingers, direction));
    }

    #[test]
    fn s5_a_scoped_binding_applies_on_its_surface_only(
        fingers in fingers(),
        direction in direction(),
        mods in mods(),
        gestures in gestures(),
        surface in 1u64..=4,
        focused in 1u64..=4,
    ) {
        let mut ik = binding(fingers, direction, mods, true, InterceptDispatch::Command);
        ik.surface = Some(surface);
        let bindings = std::slice::from_ref(&ik);
        let action = classify_swipe(bindings, false, focused, gestures, fingers, direction, &mods);
        let expected = if surface == focused {
            bound(&ik)
        } else {
            built_in(gestures, fingers, direction)
        };
        prop_assert_eq!(action, expected);
    }
}
