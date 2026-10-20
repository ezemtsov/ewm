//! Bindings Emacs declared and the actions they run.

use serde::{Deserialize, Serialize};
use smithay::input::keyboard::ModifiersState;
use smithay::input::keyboard::xkb::keysyms;

use super::SwipeDirection;
use crate::module;

/// What a binding listens for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Trigger {
    Keysym(u32),
    /// A touchpad swipe of `fingers`, told apart by the direction it sets off in.
    Swipe {
        fingers: u32,
        direction: SwipeDirection,
    },
}

/// A binding Emacs declared: a key or a swipe under given modifiers.
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
    /// If true, this key is redirected to Emacs even during fullscreen
    #[serde(default)]
    pub allow_fullscreen: bool,
    #[serde(default)]
    pub dispatch: InterceptDispatch,
    #[serde(default)]
    pub repeat: bool,
    /// Applies only while this surface has focus, or everywhere when `None`.
    pub surface: Option<u64>,
}

impl InterceptedKey {
    /// Whether this binding applies while `focused_surface_id` has focus.
    pub fn in_scope(&self, focused_surface_id: u64) -> bool {
        self.surface
            .is_none_or(|surface| surface == focused_surface_id)
    }

    /// Check if this key matches the given keysym and modifiers.
    ///
    /// `raw_keysym` is the layout-independent keysym (physical key, no
    /// modifiers).  `modified_keysym` is the keysym after XKB applies
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

    /// Whether a swipe of `fingers` setting off in `direction` under `mods` is this binding's.
    pub fn matches_swipe(
        &self,
        fingers: u32,
        direction: SwipeDirection,
        mods: &ModifiersState,
    ) -> bool {
        self.trigger == Trigger::Swipe { fingers, direction } && self.mods_match(mods, false)
    }
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
    /// Run `command` detached from the compositor.
    Spawn { command: Vec<String> },
}
