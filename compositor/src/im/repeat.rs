//! Held-key and repeat ownership for keys intercepted by EWM.
//!
//! Once a physical press is intercepted, repeats and release must follow the
//! same destination as that first press. Keyboard redirects use Wayland key
//! delivery while Emacs owns keyboard focus. Command and text-input
//! interception use compositor timers because they are delivered over IPC.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::Duration;

use serde::Serialize;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};

use crate::State;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) enum HeldKeyOwner {
    KeyboardRedirect,
    Command,
    TextInput,
    Translate {
        keycode: u32,
        target: crate::TranslateTarget,
    },
}

impl HeldKeyOwner {
    fn debug_kind(&self) -> &'static str {
        match self {
            Self::KeyboardRedirect => "keyboard_redirect",
            Self::Command => "command",
            Self::TextInput => "text_input",
            Self::Translate { .. } => "translate",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;
    use smithay::reexports::calloop::EventLoop;

    use super::*;

    #[derive(Clone, Debug)]
    enum OwnerSeed {
        KeyboardRedirect,
        Command,
        TextInput,
        Translate,
    }

    impl OwnerSeed {
        fn owner(&self) -> HeldKeyOwner {
            match self {
                Self::KeyboardRedirect => HeldKeyOwner::KeyboardRedirect,
                Self::Command => HeldKeyOwner::Command,
                Self::TextInput => HeldKeyOwner::TextInput,
                Self::Translate => HeldKeyOwner::Translate {
                    keycode: 0,
                    target: crate::TranslateTarget::default(),
                },
            }
        }
    }

    #[derive(Clone, Debug)]
    enum RepeatModel {
        Command(u32),
        TextInput(u32),
    }

    #[derive(Clone, Debug)]
    enum Op {
        Begin(u32, OwnerSeed),
        End(u32),
        StartCommandRepeat(u32),
        StartTextRepeat(u32),
        CancelCommandRepeat,
        DisableRepeat,
        EnableRepeat,
    }

    fn key_strategy() -> impl Strategy<Value = u32> {
        1u32..=4
    }

    fn owner_strategy() -> impl Strategy<Value = OwnerSeed> {
        prop_oneof![
            Just(OwnerSeed::KeyboardRedirect),
            Just(OwnerSeed::Command),
            Just(OwnerSeed::TextInput),
            Just(OwnerSeed::Translate),
        ]
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (key_strategy(), owner_strategy()).prop_map(|(key, owner)| Op::Begin(key, owner)),
            key_strategy().prop_map(Op::End),
            key_strategy().prop_map(Op::StartCommandRepeat),
            key_strategy().prop_map(Op::StartTextRepeat),
            Just(Op::CancelCommandRepeat),
            Just(Op::DisableRepeat),
            Just(Op::EnableRepeat),
        ]
    }

    fn assert_matches_model(
        state: &InterceptRepeatState,
        held: &HashMap<u32, HeldKeyOwner>,
        repeat: &Option<RepeatModel>,
    ) {
        for key in 1..=4 {
            assert_eq!(state.held_owner(key), held.get(&key).cloned());
        }

        match (state.repeat_debug(), repeat) {
            (None, None) => {}
            (Some(InterceptRepeat::Command { keycode, key }), Some(RepeatModel::Command(k))) => {
                assert_eq!(keycode, k);
                assert_eq!(key, &format!("key-{k}"));
                assert_eq!(held.get(k), Some(&HeldKeyOwner::Command));
            }
            (
                Some(InterceptRepeat::TextInput(TextInputKey {
                    keycode,
                    keysym,
                    utf8,
                    surface_id,
                    ..
                })),
                Some(RepeatModel::TextInput(k)),
            ) => {
                assert_eq!(keycode, k);
                assert_eq!(keysym, &(100 + k));
                assert_eq!(utf8, &Some(format!("u{k}")));
                assert_eq!(surface_id, &(1000 + u64::from(*k)));
                assert_eq!(held.get(k), Some(&HeldKeyOwner::TextInput));
            }
            (actual, expected) => {
                panic!("repeat mismatch: actual={actual:?} expected={expected:?}");
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn held_owner_lifecycle_matches_model(ops in prop::collection::vec(op_strategy(), 0..80)) {
            let event_loop = EventLoop::<State>::try_new().unwrap();
            let handle = event_loop.handle();
            let mut state = InterceptRepeatState::new(25, 400);
            let mut held = HashMap::new();
            let mut repeat = None;
            let mut repeat_enabled = true;

            for op in ops {
                match op {
                    Op::Begin(key, owner) if !held.contains_key(&key) => {
                        let owner = owner.owner();
                        state.begin_hold(key, owner.clone());
                        held.insert(key, owner);
                    }
                    Op::End(key) => {
                        state.end_hold(&handle, key);
                        held.remove(&key);
                        if matches!(repeat, Some(RepeatModel::Command(k) | RepeatModel::TextInput(k)) if k == key) {
                            repeat = None;
                        }
                    }
                    Op::StartCommandRepeat(key) if held.get(&key) == Some(&HeldKeyOwner::Command) => {
                        state.start_command_repeat(&handle, key, format!("key-{key}"));
                        if repeat_enabled {
                            repeat = Some(RepeatModel::Command(key));
                        }
                    }
                    Op::StartTextRepeat(key) if held.get(&key) == Some(&HeldKeyOwner::TextInput) => {
                        state.start_text_input_repeat(
                            &handle,
                            TextInputKey {
                                keycode: key,
                                keysym: 100 + key,
                                utf8: Some(format!("u{key}")),
                                surface_id: 1000 + u64::from(key),
                                ctrl: false,
                                alt: false,
                                shift: false,
                                logo: false,
                            },
                        );
                        if repeat_enabled {
                            repeat = Some(RepeatModel::TextInput(key));
                        }
                    }
                    Op::CancelCommandRepeat => {
                        state.cancel_command_repeat(&handle);
                        if matches!(repeat, Some(RepeatModel::Command(_))) {
                            repeat = None;
                        }
                    }
                    Op::DisableRepeat => {
                        state.configure_repeat(&handle, 0, 400);
                        repeat_enabled = false;
                        repeat = None;
                    }
                    Op::EnableRepeat => {
                        state.configure_repeat(&handle, 25, 400);
                        repeat_enabled = true;
                    }
                    _ => {}
                }

                assert_matches_model(&state, &held, &repeat);
            }
        }
    }
}

/// A key intercepted for text input, re-sent to Emacs on every repeat tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TextInputKey {
    pub keycode: u32,
    pub keysym: u32,
    pub utf8: Option<String>,
    pub surface_id: u64,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum InterceptRepeat {
    TextInput(TextInputKey),
    Command { keycode: u32, key: String },
}

impl InterceptRepeat {
    fn keycode(&self) -> u32 {
        match self {
            Self::TextInput(TextInputKey { keycode, .. }) | Self::Command { keycode, .. } => {
                *keycode
            }
        }
    }
}

pub(crate) struct InterceptRepeatState {
    held: HashMap<u32, HeldKeyOwner>,
    repeat: Option<InterceptRepeat>,
    timer_token: Option<RegistrationToken>,
    repeat_delay: i32,
    repeat_rate: i32,
}

impl InterceptRepeatState {
    pub fn new(repeat_rate: i32, repeat_delay: i32) -> Self {
        Self {
            held: HashMap::new(),
            repeat: None,
            timer_token: None,
            repeat_delay,
            repeat_rate,
        }
    }

    pub fn begin_hold(&mut self, keycode: u32, owner: HeldKeyOwner) {
        match self.held.entry(keycode) {
            Entry::Vacant(entry) => {
                entry.insert(owner);
            }
            Entry::Occupied(entry) => {
                let existing_kind = entry.get().debug_kind();
                let new_kind = owner.debug_kind();
                debug_assert!(
                    false,
                    "held key owner overwritten without release: keycode={keycode}"
                );
                tracing::warn!(
                    keycode,
                    existing_kind,
                    new_kind,
                    "ignored duplicate held key owner without release"
                );
            }
        }
        self.debug_assert_invariants();
    }

    pub fn held_owner(&self, keycode: u32) -> Option<HeldKeyOwner> {
        self.held.get(&keycode).cloned()
    }

    pub fn end_hold(&mut self, loop_handle: &LoopHandle<State>, keycode: u32) {
        self.held.remove(&keycode);
        if self.repeats_keycode(keycode) {
            self.clear_repeat(loop_handle);
        }
        self.debug_assert_invariants();
    }

    pub fn has_keyboard_redirect_hold(&self) -> bool {
        self.held
            .values()
            .any(|owner| matches!(owner, HeldKeyOwner::KeyboardRedirect))
    }

    pub fn configure_repeat(&mut self, loop_handle: &LoopHandle<State>, rate: i32, delay: i32) {
        self.repeat_rate = rate;
        self.repeat_delay = delay;
        if self.repeat_rate <= 0 {
            self.clear_repeat(loop_handle);
        }
        self.debug_assert_invariants();
    }

    pub fn start_text_input_repeat(&mut self, loop_handle: &LoopHandle<State>, key: TextInputKey) {
        self.start_repeat(loop_handle, InterceptRepeat::TextInput(key));
    }

    pub fn start_command_repeat(
        &mut self,
        loop_handle: &LoopHandle<State>,
        keycode: u32,
        key: String,
    ) {
        self.start_repeat(loop_handle, InterceptRepeat::Command { keycode, key });
    }

    pub fn cancel_command_repeat(&mut self, loop_handle: &LoopHandle<State>) {
        if matches!(self.repeat, Some(InterceptRepeat::Command { .. })) {
            self.clear_repeat(loop_handle);
        }
        self.debug_assert_invariants();
    }

    pub fn take_due_repeat(&mut self, keycode: u32) -> Option<InterceptRepeat> {
        if !self.repeats_keycode(keycode) {
            return None;
        }
        self.timer_token = None;
        self.debug_assert_invariants();
        self.repeat.clone()
    }

    pub fn reschedule_if_active(&mut self, loop_handle: &LoopHandle<State>, keycode: u32) {
        if self.repeats_keycode(keycode) && self.held.contains_key(&keycode) && self.repeat_rate > 0
        {
            let interval_micros = (1_000_000 / self.repeat_rate as u64).max(1);
            let interval = Duration::from_micros(interval_micros);
            self.schedule_timer(loop_handle, interval);
        }
        self.debug_assert_invariants();
    }

    pub fn held_debug(&self) -> Vec<(u32, &'static str)> {
        let mut held: Vec<_> = self
            .held
            .iter()
            .map(|(&keycode, owner)| (keycode, owner.debug_kind()))
            .collect();
        held.sort_unstable();
        held
    }

    pub fn repeat_debug(&self) -> &Option<InterceptRepeat> {
        &self.repeat
    }

    pub fn repeat_config_debug(&self) -> (i32, i32) {
        (self.repeat_rate, self.repeat_delay)
    }

    fn schedule_timer(&mut self, loop_handle: &LoopHandle<State>, duration: Duration) {
        let Some(repeat) = self.repeat.as_ref() else {
            return;
        };
        let keycode = repeat.keycode();
        match loop_handle.insert_source(Timer::from_duration(duration), move |_, _, state| {
            state.on_intercept_repeat_timer(keycode);
            TimeoutAction::Drop
        }) {
            Ok(token) => self.timer_token = Some(token),
            Err(err) => tracing::warn!("Failed to insert intercept repeat timer: {:?}", err),
        }
    }

    fn cancel_timer(&mut self, loop_handle: &LoopHandle<State>) {
        if let Some(token) = self.timer_token.take() {
            loop_handle.remove(token);
        }
    }

    fn clear_repeat(&mut self, loop_handle: &LoopHandle<State>) {
        self.cancel_timer(loop_handle);
        self.repeat = None;
    }

    fn repeats_keycode(&self, keycode: u32) -> bool {
        self.repeat
            .as_ref()
            .is_some_and(|repeat| repeat.keycode() == keycode)
    }

    fn start_repeat(&mut self, loop_handle: &LoopHandle<State>, repeat: InterceptRepeat) {
        if self.repeat_rate <= 0 {
            self.clear_repeat(loop_handle);
            self.debug_assert_invariants();
            return;
        }

        let keycode = repeat.keycode();
        let owner_kind = self.held.get(&keycode).map(HeldKeyOwner::debug_kind);
        let owner_matches = Self::repeat_owner_matches(&repeat, self.held.get(&keycode));
        debug_assert!(
            owner_matches,
            "intercept repeat started without matching held key owner"
        );
        if !owner_matches {
            tracing::warn!(
                ?repeat,
                ?owner_kind,
                "ignored intercept repeat without matching held key owner"
            );
            self.clear_repeat(loop_handle);
            self.debug_assert_invariants();
            return;
        }

        self.cancel_timer(loop_handle);
        self.repeat = Some(repeat);
        self.schedule_timer(
            loop_handle,
            Duration::from_millis(self.repeat_delay.max(0) as u64),
        );
        self.debug_assert_invariants();
    }

    fn debug_assert_invariants(&self) {
        debug_assert!(
            self.repeat.is_some() || self.timer_token.is_none(),
            "intercept repeat timer active without repeat state"
        );
        if let Some(repeat) = &self.repeat {
            let owner = self.held.get(&repeat.keycode());
            debug_assert!(owner.is_some(), "active intercept repeat without held key");
            debug_assert!(
                Self::repeat_owner_matches(repeat, owner),
                "active intercept repeat owner mismatch"
            );
            debug_assert!(
                self.repeat_rate > 0,
                "repeat active while repeat is disabled"
            );
        }
    }

    fn repeat_owner_matches(repeat: &InterceptRepeat, owner: Option<&HeldKeyOwner>) -> bool {
        matches!(
            (repeat, owner),
            (InterceptRepeat::Command { .. }, Some(HeldKeyOwner::Command))
                | (InterceptRepeat::TextInput(_), Some(HeldKeyOwner::TextInput))
        )
    }
}
