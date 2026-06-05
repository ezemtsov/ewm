//! Production-path tests for keyboard delivery through EWM-managed surfaces.

use ewm_core::input::KeyboardAction;
use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient};
use ewm_core::{EmulatedKey, InterceptDispatch, InterceptedKey, LayoutEntry, ViewId};
use smithay::input::keyboard::keysyms;
use smithay::utils::Size;

const HEAD: &str = "Virtual-1";

// XKB keycodes use the evdev code plus 8. Wayland key events report evdev codes.
const KEY_3: u32 = 4 + 8;
const KEY_P: u32 = 25 + 8;
const KEY_ENTER: u32 = 28 + 8;
const KEY_LEFTCTRL: u32 = 29 + 8;
const KEY_A: u32 = 30 + 8;
const KEY_X: u32 = 45 + 8;
const KEY_B: u32 = 48 + 8;
const KEY_LEFTALT: u32 = 56 + 8;

type KeyEvent = (u32, bool);
type Binding = (&'static str, u32, bool, bool, bool);

const CTRL_X: Binding = ("C-x", keysyms::KEY_x, true, false, false);
const META_X: Binding = ("M-x", keysyms::KEY_x, false, true, false);
const FULLSCREEN_P: Binding = ("fullscreen-p", keysyms::KEY_p, false, false, true);
const PLAIN_X_TO_CTRL: (u32, &str) = (keysyms::KEY_x, "ctrl");

#[derive(Clone, Copy)]
enum FocusMode {
    EmbeddedApp,
    EmacsBuffer,
    FullscreenApp,
}

#[derive(Clone, Copy)]
enum Target {
    App,
    Emacs,
}

fn nz(n: u64) -> ViewId {
    std::num::NonZeroU64::new(n).unwrap()
}

const fn wl_key(keycode: u32) -> u32 {
    keycode - 8
}

const fn down(keycode: u32) -> KeyEvent {
    (wl_key(keycode), true)
}

const fn up(keycode: u32) -> KeyEvent {
    (wl_key(keycode), false)
}

fn frame_with_entry(frame_id: u64, view_id: ViewId, surface_id: u64, fullscreen: bool) -> Frame {
    Frame {
        name: String::new(),
        surface_id: frame_id,
        view_id: nz(frame_id + 1_000),
        width: 1280.0,
        entries: vec![LayoutEntry::surface(
            view_id, surface_id, 0, 0, 400, 300, fullscreen,
        )],
        selected_view_id: Some(view_id),
        move_anim: None,
    }
}

fn intercepted((key, keysym, ctrl, alt, allow_fullscreen): Binding) -> InterceptedKey {
    InterceptedKey {
        keysym,
        key: key.to_string(),
        ctrl,
        alt,
        shift: false,
        logo: false,
        allow_fullscreen,
        dispatch: InterceptDispatch::Keyboard,
        repeat: false,
    }
}

fn emulated((keysym, target_mod): (u32, &str)) -> EmulatedKey {
    EmulatedKey {
        keysym,
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
        target_mod: target_mod.to_string(),
    }
}

fn take_keyboard_keys(client: &mut TestClient) -> Vec<KeyEvent> {
    client
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
            ClientEvent::KeyboardKey { key, pressed } => Some((key, pressed)),
            _ => None,
        })
        .collect()
}

fn assert_ordered(actual: &[KeyEvent], expected: &[KeyEvent]) {
    let mut matched = 0;
    for event in actual {
        if expected.get(matched) == Some(event) {
            matched += 1;
        }
    }
    assert_eq!(
        matched,
        expected.len(),
        "expected ordered subsequence {expected:?}, actual {actual:?}"
    );
}

struct Harness {
    fix: Fixture,
    emacs: TestClient,
    app: TestClient,
    emacs_id: u64,
    app_id: u64,
    app_view_id: ViewId,
}

impl Harness {
    fn new() -> Self {
        let mut fix = Fixture::new().unwrap();
        fix.clear_keyboard_capture();
        fix.set_intercepted_keys(Vec::new());
        fix.set_emulated_keys(Vec::new());

        let mut emacs = fix.new_client().unwrap();
        let mut app = fix.new_client().unwrap();
        Self::roundtrip(&mut fix, &mut emacs, &mut app);
        assert!(emacs.has_keyboard());
        assert!(app.has_keyboard());

        fix.add_output(HEAD, 1600, 900);
        let emacs_surface = fix.open_toplevel(&mut emacs, "emacs", Size::from((800, 600)));
        let app_surface = fix.open_toplevel(&mut app, "app", Size::from((800, 600)));
        Self::roundtrip(&mut fix, &mut emacs, &mut app);

        let emacs_id = fix.find_surface_id(&emacs, &emacs_surface.surface).unwrap();
        let app_id = fix.find_surface_id(&app, &app_surface.surface).unwrap();
        assert_ne!(emacs_id, app_id);

        Self {
            fix,
            emacs,
            app,
            emacs_id,
            app_id,
            app_view_id: nz(3001),
        }
    }

    fn focus(&mut self, mode: FocusMode) -> &mut Self {
        let mut frame = frame_with_entry(
            self.emacs_id,
            self.app_view_id,
            self.app_id,
            matches!(mode, FocusMode::FullscreenApp),
        );
        if matches!(mode, FocusMode::EmacsBuffer) {
            frame.selected_view_id = None;
        }

        self.fix.apply_output_layout_command(HEAD, vec![frame]);
        match mode {
            FocusMode::EmbeddedApp | FocusMode::FullscreenApp => {
                self.fix
                    .ewm()
                    .set_focus_view(self.app_view_id, "test", false)
            }
            FocusMode::EmacsBuffer => self.fix.ewm().set_focus_frame(self.emacs_id, "test", false),
        }
        self.fix.sync_keyboard_focus();
        self.pump();
        self.drain_all();
        self.expect_keyboard_focus(match mode {
            FocusMode::EmbeddedApp | FocusMode::FullscreenApp => Target::App,
            FocusMode::EmacsBuffer => Target::Emacs,
        });
        self
    }

    fn bindings(&mut self, bindings: &[Binding]) -> &mut Self {
        self.fix
            .set_intercepted_keys(bindings.iter().copied().map(intercepted).collect());
        self
    }

    fn emulations(&mut self, emulations: &[(u32, &str)]) -> &mut Self {
        self.fix
            .set_emulated_keys(emulations.iter().copied().map(emulated).collect());
        self
    }

    fn clear_capture(&mut self) -> &mut Self {
        self.fix.clear_keyboard_capture();
        self
    }

    fn tap(&mut self, key: u32, action: KeyboardAction) -> &mut Self {
        let logical_focus = self.fix.focused_surface_id();
        assert_eq!(self.fix.keyboard_key(key, true), action);
        if action == KeyboardAction::RedirectToEmacs {
            self.expect_keyboard_focus(Target::Emacs);
            assert_eq!(self.fix.focused_surface_id(), logical_focus);
        }
        assert_eq!(self.fix.keyboard_key(key, false), action);
        self
    }

    fn hold_redirect_after_capture_clear(&mut self, key: u32) -> &mut Self {
        let logical_focus = self.fix.focused_surface_id();
        let redirect = KeyboardAction::RedirectToEmacs;
        assert_eq!(self.fix.keyboard_key(key, true), redirect);
        self.expect_keyboard_focus(Target::Emacs);
        self.clear_capture();
        self.expect_keyboard_focus(Target::Emacs);
        assert_eq!(self.fix.keyboard_key(key, true), redirect);
        self.expect_keyboard_focus(Target::Emacs);
        assert_eq!(self.fix.keyboard_key(key, false), redirect);
        self.expect_keyboard_focus(Target::App);
        assert_eq!(self.fix.focused_surface_id(), logical_focus);
        self
    }

    fn hold_emulated(&mut self, key: u32) -> &mut Self {
        self.expect_keyboard_focus(Target::App);
        assert_eq!(self.fix.keyboard_key(key, true), KeyboardAction::Emulated);
        assert_eq!(self.fix.keyboard_key(key, true), KeyboardAction::Emulated);
        assert_eq!(self.fix.keyboard_key(key, false), KeyboardAction::Emulated);
        self.expect_keyboard_focus(Target::App)
    }

    fn chord(&mut self, modifier: u32, key: u32, action: KeyboardAction) -> &mut Self {
        assert_eq!(
            self.fix.keyboard_key(modifier, true),
            KeyboardAction::Forward
        );
        self.tap(key, action);
        assert_eq!(
            self.fix.keyboard_key(modifier, false),
            KeyboardAction::Forward
        );
        self
    }

    fn expect_keyboard_focus(&mut self, target: Target) -> &mut Self {
        assert_eq!(
            self.fix.keyboard_focus_surface_id(),
            Some(self.surface_id(target))
        );
        self
    }

    fn expect_capture(&mut self, active: bool) -> &mut Self {
        assert_eq!(self.fix.keyboard_capture_active(), active);
        self
    }

    fn expect_exact(&mut self, target: Target, expected: &[KeyEvent]) -> &mut Self {
        self.pump();
        assert_eq!(self.take_keys(target), expected);
        self
    }

    fn expect_ordered(&mut self, target: Target, expected: &[KeyEvent]) -> &mut Self {
        self.pump();
        assert_ordered(&self.take_keys(target), expected);
        self
    }

    fn expect_no_keys(&mut self, target: Target, keys: &[u32]) -> &mut Self {
        self.pump();
        let actual = self.take_keys(target);
        for key in keys {
            assert!(
                actual
                    .iter()
                    .all(|(event_key, _)| *event_key != wl_key(*key)),
                "unexpected key {} in {actual:?}",
                wl_key(*key)
            );
        }
        self
    }

    fn surface_id(&self, target: Target) -> u64 {
        match target {
            Target::App => self.app_id,
            Target::Emacs => self.emacs_id,
        }
    }

    fn take_keys(&mut self, target: Target) -> Vec<KeyEvent> {
        match target {
            Target::App => take_keyboard_keys(&mut self.app),
            Target::Emacs => take_keyboard_keys(&mut self.emacs),
        }
    }

    fn drain_all(&mut self) {
        let _ = take_keyboard_keys(&mut self.app);
        let _ = take_keyboard_keys(&mut self.emacs);
    }

    fn pump(&mut self) {
        Self::roundtrip(&mut self.fix, &mut self.emacs, &mut self.app);
    }

    fn roundtrip(fix: &mut Fixture, emacs: &mut TestClient, app: &mut TestClient) {
        for _ in 0..4 {
            fix.roundtrip(emacs);
            fix.roundtrip(app);
        }
    }
}

#[test]
fn keyboard_hotpath_guarantees_for_embedded_and_fullscreen_surfaces() {
    use FocusMode::{EmacsBuffer, EmbeddedApp, FullscreenApp};
    use KeyboardAction::{Forward, RedirectToEmacs};
    use Target::{App, Emacs};

    Harness::new()
        .focus(EmbeddedApp)
        .tap(KEY_A, Forward)
        .tap(KEY_ENTER, Forward)
        .tap(KEY_B, Forward)
        .expect_exact(
            App,
            &[
                down(KEY_A),
                up(KEY_A),
                down(KEY_ENTER),
                up(KEY_ENTER),
                down(KEY_B),
                up(KEY_B),
            ],
        )
        .expect_exact(Emacs, &[])
        .emulations(&[PLAIN_X_TO_CTRL])
        .hold_emulated(KEY_X)
        .expect_exact(App, &[down(KEY_X), down(KEY_X), up(KEY_X)])
        .expect_exact(Emacs, &[])
        .emulations(&[])
        .bindings(&[CTRL_X, META_X])
        .chord(KEY_LEFTCTRL, KEY_X, RedirectToEmacs)
        .expect_capture(true)
        .tap(KEY_3, Forward)
        .expect_no_keys(App, &[KEY_X, KEY_3])
        .expect_ordered(Emacs, &[down(KEY_X), up(KEY_X), down(KEY_3), up(KEY_3)])
        .clear_capture()
        .expect_keyboard_focus(App)
        .focus(EmbeddedApp)
        .chord(KEY_LEFTALT, KEY_X, RedirectToEmacs)
        .expect_no_keys(App, &[KEY_X])
        .expect_ordered(Emacs, &[down(KEY_X), up(KEY_X)])
        .clear_capture()
        .expect_keyboard_focus(App)
        .focus(EmacsBuffer)
        .chord(KEY_LEFTCTRL, KEY_X, Forward)
        .expect_exact(App, &[])
        .expect_ordered(
            Emacs,
            &[down(KEY_LEFTCTRL), down(KEY_X), up(KEY_X), up(KEY_LEFTCTRL)],
        )
        .clear_capture()
        .bindings(&[CTRL_X, FULLSCREEN_P])
        .focus(FullscreenApp)
        .tap(KEY_A, Forward)
        .tap(KEY_ENTER, Forward)
        .chord(KEY_LEFTCTRL, KEY_X, Forward)
        .expect_keyboard_focus(App)
        .expect_exact(
            App,
            &[
                down(KEY_A),
                up(KEY_A),
                down(KEY_ENTER),
                up(KEY_ENTER),
                down(KEY_LEFTCTRL),
                down(KEY_X),
                up(KEY_X),
                up(KEY_LEFTCTRL),
            ],
        )
        .expect_exact(Emacs, &[])
        .hold_redirect_after_capture_clear(KEY_P)
        .expect_exact(App, &[])
        .expect_exact(Emacs, &[down(KEY_P), down(KEY_P), up(KEY_P)])
        .bindings(&[]);
}
