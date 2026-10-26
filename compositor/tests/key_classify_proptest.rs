//! Characterization proptests for the pure key router. Every match-path property
//! draws its binding and input from one coordinated generator and meta-asserts the
//! two still `matches`, so a matcher that silently stops matching fails here.

use ewm_core::input::{
    Bound, Direction, KeyAction, KeyInput, KeyRouting, Scope, SwipeAction, classify_key,
    classify_scroll, classify_swipe,
};
use ewm_core::testing::proptest_cases;
use ewm_core::{Action, ChordSpec, InterceptDispatch, InterceptedKey, KeyName, Trigger};
use proptest::prelude::*;
use smithay::input::keyboard::{Keysym, ModifiersState, keysyms};

/// Keysyms outside the `KEY_A..=KEY_Z` uppercase-fold range and the VT range, so
/// a raw keysym match needs exact modifiers and never decodes as a VT switch.
const SAFE_KEYSYMS: &[u32] = &[
    keysyms::KEY_a,
    keysyms::KEY_b,
    keysyms::KEY_c,
    keysyms::KEY_f,
    keysyms::KEY_x,
    keysyms::KEY_z,
    keysyms::KEY_7,
    keysyms::KEY_semicolon,
    keysyms::KEY_colon,
    keysyms::KEY_ampersand,
    keysyms::KEY_space,
    keysyms::KEY_Escape,
];

/// A test-only builder for `InterceptedKey`, which has no `Default`.
struct Build(InterceptedKey);

impl Build {
    fn trigger(trigger: Trigger, mods: &ModifiersState) -> Self {
        Build(InterceptedKey {
            trigger,
            key: String::from("key"),
            ctrl: mods.ctrl,
            alt: mods.alt,
            shift: mods.shift,
            logo: mods.logo,
            allow_fullscreen: false,
            dispatch: InterceptDispatch::Keyboard,
            repeat: false,
            surface: None,
        })
    }

    fn keysym(sym: u32, mods: &ModifiersState) -> Self {
        Self::trigger(Trigger::Keysym(sym), mods)
    }

    fn dispatch(mut self, dispatch: InterceptDispatch) -> Self {
        self.0.dispatch = dispatch;
        self
    }

    fn key(mut self, key: &str) -> Self {
        self.0.key = key.to_owned();
        self
    }

    fn surface(mut self, surface: Option<u64>) -> Self {
        self.0.surface = surface;
        self
    }

    fn allow_fullscreen(mut self, allow: bool) -> Self {
        self.0.allow_fullscreen = allow;
        self
    }

    fn repeat(mut self, repeat: bool) -> Self {
        self.0.repeat = repeat;
        self
    }

    fn build(self) -> InterceptedKey {
        self.0
    }
}

/// Owned key-event parts a generator can hand back; `as_input` borrows the mods.
#[derive(Debug, Clone)]
struct InputSeed {
    is_press: bool,
    keysym_raw: u32,
    modified_raw: u32,
    mods: ModifiersState,
    changes_modifiers: bool,
}

impl InputSeed {
    fn as_input(&self) -> KeyInput<'_> {
        KeyInput {
            is_press: self.is_press,
            keysym_raw: self.keysym_raw,
            modified: Keysym::new(self.modified_raw),
            mods: &self.mods,
            changes_modifiers: self.changes_modifiers,
        }
    }
}

/// A press on `sym` under `mods`, which `matches` a binding on the same key.
fn press(sym: u32, mods: &ModifiersState) -> InputSeed {
    InputSeed {
        is_press: true,
        keysym_raw: sym,
        modified_raw: sym,
        mods: *mods,
        changes_modifiers: false,
    }
}

fn arb_safe_keysym() -> impl Strategy<Value = u32> {
    prop::sample::select(SAFE_KEYSYMS)
}

fn arb_mods() -> impl Strategy<Value = ModifiersState> {
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

fn arb_surface() -> impl Strategy<Value = Option<u64>> {
    prop_oneof![Just(None), any::<u64>().prop_map(Some)]
}

fn arb_direction() -> impl Strategy<Value = Direction> {
    prop::sample::select(Direction::ALL.to_vec())
}

fn arb_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        any::<i32>().prop_map(|delta| Action::FocusFrame { delta }),
        any::<i32>().prop_map(|delta| Action::MoveFrame { delta }),
        Just(Action::ToggleFullscreen),
        Just(Action::FloatFocusToggle),
        Just(Action::HideToggle),
    ]
}

/// A short, non-empty target sequence for an `Emulate` binding, drawn from a fixed name
/// pool since the chords are only carried and compared, never resolved.
fn arb_chords() -> impl Strategy<Value = Vec<ChordSpec>> {
    let chord = (
        prop::sample::select(vec!["down", "end", "delete", "home"]),
        arb_mods(),
    )
        .prop_map(|(name, mods)| ChordSpec {
            key: KeyName::Name(name.to_owned()),
            ctrl: mods.ctrl,
            alt: mods.alt,
            shift: mods.shift,
            logo: mods.logo,
        });
    prop::collection::vec(chord, 1..4)
}

/// Every `InterceptDispatch` kind. The match on it in the gate and mapping tests has
/// no wildcard, so a new variant breaks the build until covered.
fn arb_dispatch() -> impl Strategy<Value = InterceptDispatch> {
    prop_oneof![
        Just(InterceptDispatch::Keyboard),
        Just(InterceptDispatch::Command),
        arb_action().prop_map(InterceptDispatch::Action),
        arb_chords().prop_map(InterceptDispatch::Emulate),
    ]
}

fn arb_routing() -> impl Strategy<Value = KeyRouting> {
    (arb_surface(), any::<bool>(), any::<bool>(), arb_surface()).prop_map(
        |(text_input_surface, focus_on_emacs, fullscreen_active, focused_surface_id)| KeyRouting {
            text_input_surface,
            focus_on_emacs,
            fullscreen_active,
            focused_surface_id,
        },
    )
}

/// A global binding and a press guaranteed to `matches` it.
fn arb_binding_and_matching_input() -> impl Strategy<Value = (InterceptedKey, InputSeed)> {
    (arb_safe_keysym(), arb_mods(), arb_dispatch(), any::<bool>()).prop_map(
        |(sym, mods, dispatch, allow_fullscreen)| {
            let ik = Build::keysym(sym, &mods)
                .dispatch(dispatch)
                .allow_fullscreen(allow_fullscreen)
                .build();
            (ik, press(sym, &mods))
        },
    )
}

/// A binding and a press on a different keysym, guaranteed never to `matches`.
fn arb_binding_and_other_input() -> impl Strategy<Value = (InterceptedKey, InputSeed)> {
    (
        arb_safe_keysym(),
        arb_safe_keysym(),
        arb_mods(),
        arb_mods(),
        arb_dispatch(),
    )
        .prop_filter("distinct keysyms", |(bind, input, ..)| bind != input)
        .prop_map(|(bind_sym, input_sym, bind_mods, input_mods, dispatch)| {
            let ik = Build::keysym(bind_sym, &bind_mods)
                .dispatch(dispatch)
                .build();
            (ik, press(input_sym, &input_mods))
        })
}

#[test]
fn matches_punctuation_on_modified() {
    // Shift+`;` becomes `colon` after XKB. Emacs encodes the character directly,
    // so a binding on `colon` matches on the modified keysym, shift aside.
    let mods = ModifiersState {
        shift: true,
        ..Default::default()
    };
    let ik = Build::keysym(keysyms::KEY_colon, &mods).build();
    assert!(ik.matches(keysyms::KEY_semicolon, keysyms::KEY_colon, &mods));
    // The raw keysym alone does not match.
    assert!(!ik.matches(keysyms::KEY_semicolon, keysyms::KEY_semicolon, &mods));
    // Note: the `KEY_A..=KEY_Z` uppercase fold in `matches` is dead for real
    // input, which always delivers lowercase raw keysyms. That is a code finding,
    // not an invariant to pin here.
}

#[test]
fn release_on_vt_keysym_forwards() {
    // The press gate precedes the VT decode: a release on an XF86Switch_VT keysym
    // forwards, never switches VT.
    let mods = ModifiersState::default();
    let mut seed = press(keysyms::KEY_XF86Switch_VT_2, &mods);
    seed.is_press = false;
    let action = classify_key(&[], KeyRouting::default(), seed.as_input());
    assert_eq!(action, KeyAction::Forward);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases(256)))]

    #[test]
    fn press_gate_forwards_release(
        (ik, mut seed) in arb_binding_and_matching_input(),
        routing in arb_routing(),
    ) {
        prop_assert!(ik.matches(seed.keysym_raw, seed.modified_raw, &seed.mods));
        seed.is_press = false;
        let action = classify_key(std::slice::from_ref(&ik), routing, seed.as_input());
        prop_assert_eq!(action, KeyAction::Forward);
    }

    #[test]
    fn vt_precedes_bindings(n in 1i32..=12, mods in arb_mods(), routing in arb_routing()) {
        let vt_sym = keysyms::KEY_XF86Switch_VT_1 + (n as u32 - 1);
        // A binding that would otherwise match the VT keysym.
        let ik = Build::keysym(vt_sym, &mods)
            .dispatch(InterceptDispatch::Command)
            .allow_fullscreen(true)
            .build();
        prop_assert!(ik.matches(vt_sym, vt_sym, &mods));
        let action = classify_key(std::slice::from_ref(&ik), routing, press(vt_sym, &mods).as_input());
        prop_assert_eq!(action, KeyAction::VtSwitch(n));
    }

    /// A surface-scoped match beats a global one on the same key, whatever the list
    /// order. Both orderings run, so this is red on the old first-in-list `find_binding`.
    #[test]
    fn scoped_beats_global(
        sym in arb_safe_keysym(),
        mods in arb_mods(),
        a in any::<u64>(),
    ) {
        let scoped = Build::keysym(sym, &mods)
            .dispatch(InterceptDispatch::Command)
            .key("scoped")
            .surface(Some(a))
            .build();
        let global = Build::keysym(sym, &mods)
            .dispatch(InterceptDispatch::Command)
            .key("global")
            .surface(None)
            .build();
        // Both match the press and both are in scope for the focused surface, so the
        // scope class, not `matches`, is the only thing that can decide the winner.
        prop_assert!(scoped.matches(sym, sym, &mods));
        prop_assert!(global.matches(sym, sym, &mods));
        prop_assert!(scoped.in_scope(Some(a)));
        prop_assert!(global.in_scope(Some(a)));
        let routing = KeyRouting {
            text_input_surface: None,
            focus_on_emacs: false,
            fullscreen_active: false,
            focused_surface_id: Some(a),
        };
        // global-before-scoped is the ordering the old first-in-list router gets wrong.
        for bindings in [
            vec![global.clone(), scoped.clone()],
            vec![scoped.clone(), global.clone()],
        ] {
            let action = classify_key(&bindings, routing, press(sym, &mods).as_input());
            prop_assert_eq!(
                action,
                KeyAction::Bound {
                    bound: Bound::Command { key: "scoped".to_owned() },
                    repeat: false,
                }
            );
        }
    }

    /// Within one scope class, list order still decides: the first match fires.
    #[test]
    fn first_within_scope_class_wins(
        sym in arb_safe_keysym(),
        decoy in arb_safe_keysym(),
        mods in arb_mods(),
        a in any::<u64>(),
        scoped in any::<bool>(),
        n in 1usize..5,
    ) {
        prop_assume!(decoy != sym);
        // One class per case: all global, or all scoped to the focused surface.
        let surface = if scoped { Some(a) } else { None };
        let mut bindings = vec![
            Build::keysym(decoy, &mods)
                .dispatch(InterceptDispatch::Command)
                .key("decoy")
                .surface(surface)
                .build(),
        ];
        for i in 0..n {
            bindings.push(
                Build::keysym(sym, &mods)
                    .dispatch(InterceptDispatch::Command)
                    .key(&format!("cmd-{i}"))
                    .surface(surface)
                    .build(),
            );
        }
        for ik in &bindings[1..] {
            prop_assert!(ik.matches(sym, sym, &mods));
            prop_assert!(ik.in_scope(Some(a)));
        }
        let routing = KeyRouting {
            text_input_surface: None,
            focus_on_emacs: false,
            fullscreen_active: false,
            focused_surface_id: Some(a),
        };
        let action = classify_key(&bindings, routing, press(sym, &mods).as_input());
        prop_assert_eq!(
            action,
            KeyAction::Bound {
                bound: Bound::Command { key: "cmd-0".to_owned() },
                repeat: false,
            }
        );
    }

    #[test]
    fn scope_isolation(
        (ik, seed) in arb_binding_and_matching_input(),
        a in any::<u64>(),
        focused in arb_surface(),
    ) {
        prop_assume!(focused != Some(a));
        prop_assert!(ik.matches(seed.keysym_raw, seed.modified_raw, &seed.mods));
        let mut ik = ik;
        ik.surface = Some(a);
        let routing = KeyRouting {
            text_input_surface: None,
            focus_on_emacs: false,
            fullscreen_active: false,
            focused_surface_id: focused,
        };
        let action = classify_key(std::slice::from_ref(&ik), routing, seed.as_input());
        // Out of scope, so it never fires. With no text input the key forwards.
        prop_assert_eq!(action, KeyAction::Forward);
    }

    #[test]
    fn fullscreen_gate(
        sym in arb_safe_keysym(),
        mods in arb_mods(),
        dispatch in arb_dispatch(),
        allow_fullscreen in any::<bool>(),
        fullscreen_active in any::<bool>(),
        text_input in arb_surface(),
    ) {
        let ik = Build::keysym(sym, &mods)
            .dispatch(dispatch.clone())
            .allow_fullscreen(allow_fullscreen)
            .key("cmd")
            .build();
        prop_assert!(ik.matches(sym, sym, &mods));
        // Off Emacs so the dispatch arms are reachable; a text input may be active
        // so the gate's forward is told from the text-input tail.
        let routing = KeyRouting {
            text_input_surface: text_input,
            focus_on_emacs: false,
            fullscreen_active,
            focused_surface_id: None,
        };
        let action = classify_key(std::slice::from_ref(&ik), routing, press(sym, &mods).as_input());
        if allow_fullscreen || !fullscreen_active {
            // The gate is open, the first match fires whatever the dispatch kind.
            match dispatch {
                InterceptDispatch::Action(action_kind) => prop_assert_eq!(
                    action,
                    KeyAction::Bound { bound: Bound::Action(action_kind), repeat: false }
                ),
                InterceptDispatch::Keyboard => {
                    prop_assert_eq!(action, KeyAction::KeyboardRedirect(sym))
                }
                InterceptDispatch::Command => prop_assert_eq!(
                    action,
                    KeyAction::Bound { bound: Bound::Command { key: "cmd".to_owned() }, repeat: false }
                ),
                InterceptDispatch::Emulate(keys) => prop_assert_eq!(
                    action,
                    KeyAction::Bound { bound: Bound::Emulate { keys }, repeat: false }
                ),
            }
        } else {
            // Gated shut, the key forwards to the client ahead of the text-input tail.
            prop_assert_eq!(action, KeyAction::Forward);
        }
    }

    #[test]
    fn fullscreen_gate_reads_first_match(
        sym in arb_safe_keysym(),
        mods in arb_mods(),
    ) {
        // First match gates shut over a fullscreen surface; a later allow_fullscreen
        // match on the same key is never reached, so the key forwards.
        let bindings = vec![
            Build::keysym(sym, &mods)
                .dispatch(InterceptDispatch::Command)
                .key("first")
                .allow_fullscreen(false)
                .build(),
            Build::keysym(sym, &mods)
                .dispatch(InterceptDispatch::Command)
                .key("second")
                .allow_fullscreen(true)
                .build(),
        ];
        for ik in &bindings {
            prop_assert!(ik.matches(sym, sym, &mods));
        }
        let routing = KeyRouting {
            text_input_surface: None,
            focus_on_emacs: false,
            fullscreen_active: true,
            focused_surface_id: None,
        };
        let action = classify_key(&bindings, routing, press(sym, &mods).as_input());
        prop_assert_eq!(action, KeyAction::Forward);
    }

    #[test]
    fn emacs_focus_gate(
        sym in arb_safe_keysym(),
        mods in arb_mods(),
        dispatch in arb_dispatch(),
        repeat in any::<bool>(),
    ) {
        let ik = Build::keysym(sym, &mods)
            .dispatch(dispatch.clone())
            .repeat(repeat)
            .allow_fullscreen(true)
            .build();
        prop_assert!(ik.matches(sym, sym, &mods));
        let routing = KeyRouting {
            text_input_surface: None,
            focus_on_emacs: true,
            fullscreen_active: false,
            focused_surface_id: None,
        };
        let action = classify_key(std::slice::from_ref(&ik), routing, press(sym, &mods).as_input());
        match dispatch {
            InterceptDispatch::Action(action_kind) => prop_assert_eq!(
                action,
                KeyAction::Bound { bound: Bound::Action(action_kind), repeat }
            ),
            InterceptDispatch::Keyboard
            | InterceptDispatch::Command
            | InterceptDispatch::Emulate(_) => {
                prop_assert_eq!(action, KeyAction::Forward)
            }
        }
    }

    #[test]
    fn dispatch_mapping(
        sym in arb_safe_keysym(),
        mods in arb_mods(),
        dispatch in arb_dispatch(),
        repeat in any::<bool>(),
    ) {
        let ik = Build::keysym(sym, &mods)
            .dispatch(dispatch.clone())
            .repeat(repeat)
            .key("cmd")
            .build();
        prop_assert!(ik.matches(sym, sym, &mods));
        let routing = KeyRouting::default();
        let action = classify_key(std::slice::from_ref(&ik), routing, press(sym, &mods).as_input());
        match dispatch {
            InterceptDispatch::Action(action_kind) => prop_assert_eq!(
                action,
                KeyAction::Bound { bound: Bound::Action(action_kind), repeat }
            ),
            InterceptDispatch::Keyboard => {
                prop_assert_eq!(action, KeyAction::KeyboardRedirect(sym))
            }
            InterceptDispatch::Command => prop_assert_eq!(
                action,
                KeyAction::Bound { bound: Bound::Command { key: "cmd".to_owned() }, repeat }
            ),
            InterceptDispatch::Emulate(keys) => prop_assert_eq!(
                action,
                KeyAction::Bound { bound: Bound::Emulate { keys }, repeat }
            ),
        }
    }

    #[test]
    fn fallthrough_text_input(
        (ik, mut seed) in arb_binding_and_other_input(),
        text_input in arb_surface(),
        focus_on_emacs in any::<bool>(),
        changes_modifiers in any::<bool>(),
    ) {
        prop_assert!(!ik.matches(seed.keysym_raw, seed.modified_raw, &seed.mods));
        seed.changes_modifiers = changes_modifiers;
        let routing = KeyRouting {
            text_input_surface: text_input,
            focus_on_emacs,
            fullscreen_active: false,
            focused_surface_id: None,
        };
        let action = classify_key(std::slice::from_ref(&ik), routing, seed.as_input());
        let expect_text = text_input.is_some() && !focus_on_emacs && !changes_modifiers;
        if expect_text {
            match &action {
                KeyAction::TextInput { surface_id, keysym, .. } => {
                    prop_assert_eq!(*surface_id, text_input.unwrap());
                    prop_assert_eq!(*keysym, seed.keysym_raw);
                }
                other => prop_assert!(false, "expected TextInput, got {other:?}"),
            }
        } else {
            prop_assert_eq!(action, KeyAction::Forward);
        }
    }

    /// Text input carries the key utf8 and the modifier snapshot, for a printable key
    /// or a control key like Return, whatever modifiers are held.
    #[test]
    fn text_input_carries_utf8(
        key in prop_oneof![
            (0u32..26).prop_map(|l| {
                let text = char::from_u32(u32::from(b'a') + l).unwrap().to_string();
                (keysyms::KEY_a + l, Some(text))
            }),
            Just((keysyms::KEY_Return, None)),
        ],
        mods in arb_mods(),
        a in any::<u64>(),
    ) {
        let (keysym, text) = key;
        let routing = KeyRouting {
            text_input_surface: Some(a),
            ..KeyRouting::default()
        };
        let action = classify_key(&[], routing, press(keysym, &mods).as_input());
        prop_assert_eq!(
            action,
            KeyAction::TextInput {
                surface_id: a,
                keysym,
                utf8: text,
                ctrl: mods.ctrl,
                alt: mods.alt,
                shift: mods.shift,
                logo: mods.logo,
            }
        );
    }

    #[test]
    fn gestures_share_scope_fullscreen(
        fingers in 1u32..=4,
        direction in arb_direction(),
        mods in arb_mods(),
        scoped in any::<bool>(),
        same in any::<bool>(),
        sid in any::<u64>(),
        other in any::<u64>(),
        fullscreen in any::<bool>(),
        allow_fullscreen in any::<bool>(),
    ) {
        prop_assume!(sid != other);
        // A global binding (None) is always in scope; a surface-scoped one only when
        // the focused surface equals it. Drawing both halves keeps the fire branch live.
        let binding_surface = if scoped { Some(sid) } else { None };
        let focused = if same { Some(sid) } else { Some(other) };
        let in_scope = binding_surface.is_none() || binding_surface == focused;
        let runs = allow_fullscreen || !fullscreen;
        let scope = Scope { surface_id: focused, fullscreen };

        // A swipe bound to a plain command (not a gesture) yields `Bound` when it fires.
        let swipe_trigger = Trigger::Swipe { fingers, direction };
        let swipe_ik = Build::trigger(swipe_trigger, &mods)
            .dispatch(InterceptDispatch::Command)
            .key("swipe")
            .surface(binding_surface)
            .allow_fullscreen(allow_fullscreen)
            .build();
        prop_assert!(swipe_ik.matches_trigger(swipe_trigger, &mods));
        let swipe = classify_swipe(std::slice::from_ref(&swipe_ik), scope, fingers, direction, &mods);
        if in_scope && runs {
            prop_assert_eq!(swipe, SwipeAction::Bound(Bound::Command { key: "swipe".to_owned() }));
        } else {
            prop_assert_eq!(swipe, SwipeAction::Forward);
        }

        let scroll_trigger = Trigger::Scroll { direction };
        let scroll_ik = Build::trigger(scroll_trigger, &mods)
            .dispatch(InterceptDispatch::Command)
            .key("scroll")
            .surface(binding_surface)
            .allow_fullscreen(allow_fullscreen)
            .build();
        prop_assert!(scroll_ik.matches_trigger(scroll_trigger, &mods));
        let scroll = classify_scroll(std::slice::from_ref(&scroll_ik), scope, direction, &mods);
        if in_scope && runs {
            prop_assert_eq!(scroll, Some(Bound::Command { key: "scroll".to_owned() }));
        } else {
            prop_assert_eq!(scroll, None);
        }
    }

    /// Scope ranks for gestures too, whatever the list order. No surface-scoped swipe
    /// or scroll bindings exist today, so this guards a future invariant.
    #[test]
    fn gestures_scoped_beats_global(
        fingers in 1u32..=4,
        direction in arb_direction(),
        mods in arb_mods(),
        a in any::<u64>(),
    ) {
        let scope = Scope { surface_id: Some(a), fullscreen: false };

        let swipe_trigger = Trigger::Swipe { fingers, direction };
        let scoped_swipe = Build::trigger(swipe_trigger, &mods)
            .dispatch(InterceptDispatch::Command)
            .key("scoped")
            .surface(Some(a))
            .build();
        let global_swipe = Build::trigger(swipe_trigger, &mods)
            .dispatch(InterceptDispatch::Command)
            .key("global")
            .surface(None)
            .build();
        prop_assert!(scoped_swipe.matches_trigger(swipe_trigger, &mods));
        prop_assert!(global_swipe.matches_trigger(swipe_trigger, &mods));
        prop_assert!(scoped_swipe.in_scope(Some(a)));
        prop_assert!(global_swipe.in_scope(Some(a)));
        for bindings in [
            vec![global_swipe.clone(), scoped_swipe.clone()],
            vec![scoped_swipe.clone(), global_swipe.clone()],
        ] {
            let swipe = classify_swipe(&bindings, scope, fingers, direction, &mods);
            prop_assert_eq!(swipe, SwipeAction::Bound(Bound::Command { key: "scoped".to_owned() }));
        }

        let scroll_trigger = Trigger::Scroll { direction };
        let scoped_scroll = Build::trigger(scroll_trigger, &mods)
            .dispatch(InterceptDispatch::Command)
            .key("scoped")
            .surface(Some(a))
            .build();
        let global_scroll = Build::trigger(scroll_trigger, &mods)
            .dispatch(InterceptDispatch::Command)
            .key("global")
            .surface(None)
            .build();
        prop_assert!(scoped_scroll.matches_trigger(scroll_trigger, &mods));
        prop_assert!(global_scroll.matches_trigger(scroll_trigger, &mods));
        prop_assert!(scoped_scroll.in_scope(Some(a)));
        prop_assert!(global_scroll.in_scope(Some(a)));
        for bindings in [
            vec![global_scroll.clone(), scoped_scroll.clone()],
            vec![scoped_scroll.clone(), global_scroll.clone()],
        ] {
            let scroll = classify_scroll(&bindings, scope, direction, &mods);
            prop_assert_eq!(scroll, Some(Bound::Command { key: "scoped".to_owned() }));
        }
    }

    #[test]
    fn determinism(
        (ik, seed) in prop_oneof![arb_binding_and_matching_input(), arb_binding_and_other_input()],
        routing in arb_routing(),
    ) {
        let first = classify_key(std::slice::from_ref(&ik), routing, seed.as_input());
        let second = classify_key(std::slice::from_ref(&ik), routing, seed.as_input());
        prop_assert_eq!(first, second);
    }

    /// A surface-scoped key-type emulation is compositor-native: off Emacs it classifies
    /// to `Bound::Emulate` carrying the target chords, bound for the application rather
    /// than Emacs.
    #[test]
    fn surface_scoped_emulate_targets_application(
        sym in arb_safe_keysym(),
        mods in arb_mods(),
        keys in arb_chords(),
        a in any::<u64>(),
        repeat in any::<bool>(),
    ) {
        let ik = Build::keysym(sym, &mods)
            .dispatch(InterceptDispatch::Emulate(keys.clone()))
            .repeat(repeat)
            .surface(Some(a))
            .build();
        prop_assert!(ik.matches(sym, sym, &mods));
        let routing = KeyRouting {
            text_input_surface: None,
            focus_on_emacs: false,
            fullscreen_active: false,
            focused_surface_id: Some(a),
        };
        let action = classify_key(std::slice::from_ref(&ik), routing, press(sym, &mods).as_input());
        prop_assert_eq!(
            action,
            KeyAction::Bound { bound: Bound::Emulate { keys }, repeat }
        );
    }
}
