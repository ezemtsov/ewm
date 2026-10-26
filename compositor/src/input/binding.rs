//! Bindings Emacs declared and the actions they run.

use serde::{Deserialize, Serialize};
use smithay::input::keyboard::ModifiersState;
use smithay::input::keyboard::xkb::keysyms;

use super::Direction;
use crate::module;

/// What a binding listens for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Trigger {
    Keysym(u32),
    /// A touchpad swipe of `fingers`, told apart by the direction it sets off in.
    Swipe {
        fingers: u32,
        direction: Direction,
    },
    /// A wheel notch or ten units of two-finger travel in `direction`.
    Scroll {
        direction: Direction,
    },
}

/// A binding Emacs declared: a key, swipe or scroll under given modifiers.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InterceptedKey {
    pub trigger: Trigger,
    pub key: String,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(rename = "super", default)]
    pub logo: bool,
    /// Intercepted even while a fullscreen surface has focus.
    #[serde(default)]
    pub allow_fullscreen: bool,
    #[serde(default)]
    pub dispatch: InterceptDispatch,
    #[serde(default)]
    pub repeat: bool,
    /// Applies only to input reaching this surface, or everywhere when `None`.
    pub surface: Option<u64>,
}

impl InterceptedKey {
    /// Whether this binding runs while a fullscreen surface would take the input.
    pub fn runs_under(&self, fullscreen: bool) -> bool {
        self.allow_fullscreen || !fullscreen
    }

    /// Whether this binding applies to input reaching `surface_id`.
    pub fn in_scope(&self, surface_id: Option<u64>) -> bool {
        self.surface
            .is_none_or(|surface| Some(surface) == surface_id)
    }

    /// Check if this key matches the given keysym and modifiers.
    ///
    /// `raw_keysym` is the layout-independent keysym (physical key, no
    /// modifiers). `modified_keysym` is the keysym after XKB applies
    /// modifiers (e.g. Shift+`;` -> `colon`).
    ///
    /// We try `raw_keysym` first for layout-independent matching, then
    /// fall back to `modified_keysym` for shifted punctuation (`:`, `&`,
    /// etc.) where Emacs encodes the character directly rather than
    /// decomposing it into base-key + Shift.
    pub fn matches(&self, raw_keysym: u32, modified_keysym: u32, mods: &ModifiersState) -> bool {
        let Trigger::Keysym(keysym) = self.trigger else {
            return false;
        };
        let raw_match = keysym == raw_keysym
            || ((keysyms::KEY_A..=keysyms::KEY_Z).contains(&raw_keysym)
                && keysym == raw_keysym - keysyms::KEY_A + keysyms::KEY_a);

        if raw_match {
            return self.mods_match(
                mods,
                (keysyms::KEY_A..=keysyms::KEY_Z).contains(&raw_keysym),
            );
        }

        raw_keysym != modified_keysym && keysym == modified_keysym && self.mods_match(mods, true)
    }

    /// Whether `mods` are this binding's, `shift` aside when `ignore_shift`.
    fn mods_match(&self, mods: &ModifiersState, ignore_shift: bool) -> bool {
        self.ctrl == mods.ctrl
            && self.alt == mods.alt
            && (ignore_shift || self.shift == mods.shift)
            && self.logo == mods.logo
    }

    /// Whether `trigger` under exactly `mods` is this binding's.
    pub fn matches_trigger(&self, trigger: Trigger, mods: &ModifiersState) -> bool {
        self.trigger == trigger && self.mods_match(mods, false)
    }

    /// What this binding runs when its trigger fires, nothing for a bare prefix entry.
    pub fn bound(&self) -> Option<Bound> {
        match &self.dispatch {
            InterceptDispatch::Action(action) => Some(Bound::Action(action.clone())),
            InterceptDispatch::Command => Some(Bound::Command {
                key: self.key.clone(),
            }),
            InterceptDispatch::Emulate(keys) => Some(Bound::Emulate { keys: keys.clone() }),
            InterceptDispatch::Keyboard => None,
        }
    }
}

/// The binding that `matches` in scope, a surface-scoped one preferred over a global one.
pub fn find_binding(
    bindings: &[InterceptedKey],
    surface_id: Option<u64>,
    matches: impl Fn(&InterceptedKey) -> bool,
) -> Option<&InterceptedKey> {
    let in_scope_match = |ik: &&InterceptedKey| ik.in_scope(surface_id) && matches(ik);
    bindings
        .iter()
        .filter(in_scope_match)
        .find(|ik| ik.surface.is_some())
        .or_else(|| bindings.iter().find(in_scope_match))
}

/// What a bound trigger runs: a compositor action, or `key` as an Emacs command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Bound {
    Command {
        key: String,
    },
    Action(Action),
    /// Re-emit the target chords straight to the focused client.
    Emulate {
        keys: Vec<module::ChordSpec>,
    },
}

/// How an intercepted key is delivered to Emacs.
///
/// Keyboard delivery temporarily gives Emacs Wayland keyboard focus and lets
/// Emacs's command loop read the following key sequence. Command delivery
/// executes the resolved Emacs binding through the event pipe, so Wayland
/// keyboard focus stays on the client surface.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InterceptDispatch {
    #[default]
    Keyboard,
    Command,
    /// Run in the compositor, Emacs told afterwards.
    Action(Action),
    /// Re-emit the target chords to the focused client, no Emacs round trip.
    Emulate(Vec<module::ChordSpec>),
}

/// What a key or a module call changes in the compositor alone, Emacs told by events.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Show the strip frame `delta` slots away, counting from the shown frame under a float. The
    /// keyboard follows unless a float keeps it on screen.
    FocusFrame { delta: i32 },
    /// Show the `n`th strip frame, counting from 1, the keyboard following likewise.
    FocusNthFrame { n: usize },
    /// Focus frame `id`, as a workspace client asks.
    ActivateFrame { id: u64 },
    /// Focus the frame holding the keyboard on the output in direction `(dx, dy)`.
    FocusOutput { dx: i32, dy: i32 },
    /// Move the keyboard between the strip and the floats of the current output.
    FloatFocusToggle,
    /// Move the focused strip frame `delta` slots.
    MoveFrame { delta: i32 },
    /// Move the focused frame to the next output in order, wrapping around.
    MoveFrameToNextOutput,
    /// Fullscreen the focused view's surface, or bring it back.
    ToggleFullscreen,
    /// Open, close or toggle the overview.
    Overview(module::OverviewAction),
    /// Slide every frame off its output, or back.
    HideToggle,
    /// Scroll the strip under the pointer with the fingers, the way the touchpad scrolls.
    StripScroll,
    /// Run `command` detached from the compositor.
    Spawn { command: Vec<String> },
}

/// The end a two-ended transition heads for: the one its command names, or the other one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    On,
    Off,
    Other,
}

impl End {
    /// The direction whose travel heads for this end when the fingers moving `direction` head
    /// there from a transition that is `on` or not.
    pub fn forward(self, on: bool, direction: Direction) -> Direction {
        match (self, on) {
            (End::On, _) | (End::Other, false) => direction,
            _ => direction.opposite(),
        }
    }
}

/// A swipe the fingers drive as they move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    /// Scrolls the strip under the pointer, the fingers moving `forward` bringing in the next frame.
    Strip { forward: Direction },
    /// Drives the overview toward `end`, the fingers moving `direction` heading there.
    Overview { direction: Direction, end: End },
    /// Drives the hide toward `end`, the fingers moving `direction` heading there.
    Hide { direction: Direction, end: End },
}

impl Action {
    /// The gesture a swipe setting off in `direction` drives when bound to this action, if any.
    pub fn as_gesture(&self, direction: Direction) -> Option<Gesture> {
        use module::OverviewAction::{Close, Open, Toggle};
        let overview = |end| Gesture::Overview { direction, end };
        let hide = |end| Gesture::Hide { direction, end };
        Some(match self {
            Action::StripScroll => Gesture::Strip {
                forward: Direction::Right,
            },
            Action::Overview(Open) => overview(End::On),
            Action::Overview(Close) => overview(End::Off),
            Action::Overview(Toggle) => overview(End::Other),
            Action::HideToggle => hide(End::Other),
            _ => return None,
        })
    }
}
