//! Emulated keys end to end: intercepts from the real Lisp payload driven through
//! the seat, and the keys Emacs sends back replayed to real clients.

mod common;

use std::sync::{Mutex, MutexGuard};

use Wire::{Key, Mods};
use ewm_core::event::Event;
use ewm_core::input::KeyboardAction::{self, CommandIntercepted, Forward};
use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient};
use ewm_core::{LayoutEntry, LayoutEntryId};
use smithay::utils::Size;

const HEAD: &str = "Virtual-1";

// Clients by index: the Emacs frame, then the two apps it hosts.
const EMACS: usize = 0;
const APP0: usize = 1;
const APP1: usize = 2;

// XKB keycodes use the evdev code plus 8. Wayland key events report evdev codes.
const KEY_LEFTCTRL: u32 = 29 + 8;
const KEY_A: u32 = 30 + 8;
const KEY_N: u32 = 49 + 8;
const KEY_HOME: u32 = 102 + 8;
const KEY_END: u32 = 107 + 8;
const KEY_DELETE: u32 = 111 + 8;

/// Loads the real Lisp with the module stubbed to keep the last list and key sent.
const LISP_PRELUDE: &str = r#"(setq load-prefer-newer t)
  (dolist (f '(ewm-output-layout ewm-focus-target-module ewm-prepare-frame-close-module
               ewm-get-active-frame-id ewm-get-output-origin))
    (defalias f #'ignore))
  (defvar capture nil)
  (defun ewm-keyboard-redirect-capture-clear-module () (setq capture nil))
  (defun ewm-keyboard-capture-active-module () capture)
  (defvar keys nil)
  (defvar sent nil)
  (defun ewm-intercept-keys-module (k) (setq keys k))
  (defun ewm-send-key-module (id k) (cl-assert (not capture)) (setq sent (vector id k)))
  (provide 'ewm-core)
  (require 'ewm)
  (setq ewm--module-mode t)
  (ewm--send-intercept-keys)"#;

/// One keyboard event as a client sees it, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    Mods(u32),
    Key(u32, bool),
}

const fn down(keycode: u32) -> Wire {
    Key(keycode - 8, true)
}

const fn up(keycode: u32) -> Wire {
    Key(keycode - 8, false)
}

/// A tap under `mask`, then the `live` modifiers advertised again.
const fn tapped(mask: u32, key: u32, live: u32) -> [Wire; 4] {
    [Mods(mask), down(key), up(key), Mods(live)]
}

fn take_wire(client: &mut TestClient) -> Vec<Wire> {
    client
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
            ClientEvent::KeyboardModifiers { depressed, .. } => Some(Mods(depressed)),
            ClientEvent::KeyboardKey { key, pressed } => Some(Key(key, pressed)),
            _ => None,
        })
        .collect()
}

/// The wire of `client` out of per-client wires, the others having seen nothing.
fn only(mut wires: Vec<Vec<Wire>>, client: usize) -> Vec<Wire> {
    let wire = std::mem::take(&mut wires[client]);
    assert!(wires.iter().all(Vec::is_empty), "others saw {wires:?}");
    wire
}

fn entry(app: usize) -> LayoutEntryId {
    LayoutEntryId::new(3001 + app as u64).unwrap()
}

// The intercept list is process-wide, so harnesses run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// An Emacs frame hosting two apps, focused on `APP0`, with the keymap's Ctrl mask.
struct Harness {
    fix: Fixture,
    clients: Vec<TestClient>,
    ids: Vec<u64>,
    focused: usize,
    ctrl: u32,
    _serial: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut fix = Fixture::new().unwrap();
        fix.clear_keyboard_capture();
        let clients = (0..3).map(|_| fix.new_client().unwrap()).collect();
        let mut h = Self {
            fix,
            clients,
            ids: Vec::new(),
            focused: APP0,
            ctrl: 0,
            _serial: serial,
        };
        h.pump();
        h.fix.add_output(HEAD, 1600, 900);
        h.ids = h
            .clients
            .iter_mut()
            .map(|client| {
                let toplevel = h.fix.open_toplevel(client, "app", Size::from((800, 600)));
                h.fix.find_surface_id(client, &toplevel.surface).unwrap()
            })
            .collect();
        h.focus(APP0);
        h.ctrl = h.mask(KEY_LEFTCTRL);
        h
    }

    /// Run `config` after the real Lisp loads and return what `form` prints.
    fn lisp(config: &str, form: &str) -> String {
        let form = format!("(progn {LISP_PRELUDE} {config} (prin1 {form}))");
        common::emacs(&["--eval", &form])
    }

    /// Load the list the real Lisp last sent once `config` has run.
    fn load_lisp(&mut self, config: &str) {
        self.fix
            .set_intercepted_keys_from_lisp(&Self::lisp(config, "keys"));
    }

    /// Replay the last `ewm-send-key` that `config` made and return each client's wire.
    fn send_lisp(&mut self, config: &str) -> Vec<Vec<Wire>> {
        self.fix.send_key_from_lisp(&Self::lisp(config, "sent"));
        self.wires()
    }

    /// Pump every client and return what each was told.
    fn wires(&mut self) -> Vec<Vec<Wire>> {
        self.pump();
        self.clients.iter_mut().map(take_wire).collect()
    }

    fn pump(&mut self) {
        for client in &mut self.clients {
            self.fix.roundtrip(client);
        }
    }

    fn focus(&mut self, client: usize) {
        let mut frame = Frame::new(self.ids[EMACS]);
        frame.width = 1280.0;
        frame.entries = (0..2)
            .map(|app| {
                LayoutEntry::surface(
                    entry(app),
                    "entry".to_string(),
                    self.ids[app + 1],
                    400 * app as i32,
                    0,
                    400,
                    300,
                )
            })
            .collect();
        let selected = (client > EMACS).then(|| entry(client - 1));
        frame.selected_entry_id = selected;
        self.fix.apply_output_layout_command(HEAD, vec![frame]);
        match selected {
            None => self
                .fix
                .ewm()
                .set_focus_frame(self.ids[EMACS], "test", false),
            Some(e) => self.fix.ewm().set_focus_entry(e, "test", false),
        }
        self.fix.sync_keyboard_focus();
        self.wires();
        assert_eq!(self.fix.keyboard_focus_surface_id(), Some(self.ids[client]));
        self.focused = client;
    }

    /// The depressed mask the focused client is told for `modifier` alone.
    fn mask(&mut self, modifier: u32) -> u32 {
        assert_eq!(self.fix.keyboard_key(modifier, true), Forward);
        self.pump();
        let &[Mods(mask), ..] = &take_wire(&mut self.clients[self.focused])[..] else {
            panic!("a modifier press advertises modifiers first")
        };
        assert_eq!(self.fix.keyboard_key(modifier, false), Forward);
        self.wires();
        mask
    }

    /// Tap `key` under `modifier`, expecting `action` on both edges, and return
    /// what the focused client saw, nothing having reached the others.
    fn tap(&mut self, modifier: u32, key: u32, action: KeyboardAction) -> Vec<Wire> {
        assert_eq!(self.fix.keyboard_key(modifier, true), Forward);
        self.wires();
        assert_eq!(self.fix.keyboard_key(key, true), action);
        assert_eq!(self.fix.keyboard_key(key, false), action);
        let wires = self.wires();
        assert_eq!(self.fix.keyboard_key(modifier, false), Forward);
        self.wires();
        only(wires, self.focused)
    }
}

/// A key entry is intercepted as a repeating command on its own surface and
/// forwarded untouched everywhere else.
#[test]
fn key_entries_are_intercepted_on_their_surface_only() {
    let mut h = Harness::new();
    h.load_lisp(&format!(
        r#"(add-hook 'ewm-update-app-hook
                     (lambda () (when (equal ewm-surface-app "firefox")
                                  (setq-local ewm-surface-emulate-keys '(("C-n" . "<down>"))))))
           (ewm--handle-new-surface '((id . {0}) (app . "firefox")))
           (ewm--handle-new-surface '((id . {1}) (app . "other")))"#,
        h.ids[APP0], h.ids[APP1]
    ));
    h.fix.enable_event_capture();
    assert_eq!(h.fix.keyboard_key(KEY_LEFTCTRL, true), Forward);
    h.wires();
    assert_eq!(h.fix.keyboard_key(KEY_N, true), CommandIntercepted);
    let mut commands = 0;
    for _ in 0..100 {
        h.fix.roundtrip(&mut h.clients[APP0]);
        commands += h
            .fix
            .drain_events()
            .iter()
            .filter(|e| matches!(e, Event::InterceptedCommand { key } if key == "C-n"))
            .count();
        if commands >= 2 {
            break;
        }
    }
    assert_eq!(h.fix.keyboard_key(KEY_N, false), CommandIntercepted);
    assert!(h.wires().iter().all(Vec::is_empty), "a client saw the key");
    assert!(commands >= 2, "press plus repeats: {commands}");
    assert_eq!(h.fix.keyboard_key(KEY_LEFTCTRL, false), Forward);

    for client in [APP1, EMACS] {
        h.focus(client);
        assert_eq!(
            h.tap(KEY_LEFTCTRL, KEY_N, Forward),
            [down(KEY_N), up(KEY_N)]
        );
    }
}

/// Emacs sends the mapped keys for key and command entries alike, and a target
/// may be a sequence.
#[test]
fn emacs_sends_the_mapped_keys_for_both_entry_kinds() {
    let mut h = Harness::new();
    let config = format!(
        r#"(setopt ewm-surface-emulate-keys
                   '(("C-n" . "<down>") (kill-ring-save . "C-c") ("C-k" . "<end> <delete>")))
           (ewm--handle-new-surface '((id . {0}) (app . "app")))
           (switch-to-buffer (gethash {0} ewm--surfaces))
           (execute-kbd-macro (kbd "C-n"))
           (cl-assert (equal (plist-get (car (aref sent 1)) :key) "down"))
           (execute-kbd-macro (kbd "M-w"))
           (cl-assert (eql (plist-get (car (aref sent 1)) :key) ?c))
           (execute-kbd-macro (kbd "C-k"))"#,
        h.ids[APP0]
    );
    let wire = only(h.send_lisp(&config), APP0);
    assert_eq!(
        wire,
        [tapped(0, KEY_END, 0), tapped(0, KEY_DELETE, 0)].concat()
    );
}

/// `ewm-send-key` taps each key of a sequence on the surface it names while that
/// surface has focus, or on the focused surface when it names none.
#[test]
fn sent_keys_tap_the_named_surface_while_focused() {
    let mut h = Harness::new();
    let config = format!(r#"(ewm-send-key "C-a <home>" {})"#, h.ids[APP0]);
    let wire = only(h.send_lisp(&config), APP0);
    assert_eq!(
        wire,
        [tapped(h.ctrl, KEY_A, 0), tapped(0, KEY_HOME, 0)].concat()
    );
    let wire = only(h.send_lisp(r#"(ewm-send-key "C-a")"#), APP0);
    assert_eq!(wire, tapped(h.ctrl, KEY_A, 0));
    let config = format!(r#"(ewm-send-key "C-a" {})"#, h.ids[APP1]);
    assert!(
        only(h.send_lisp(&config), APP0).is_empty(),
        "unfocused target"
    );
}

/// A command entry reached through a key sequence sends its key once Emacs
/// releases the keyboard, so `C-x h` selects all in the client.
#[test]
fn command_sequence_sends_its_key_after_capture_ends() {
    let mut h = Harness::new();
    // `capture` plays the redirect the compositor begins on `C-x`.
    let config = format!(
        r#"(setopt ewm-surface-emulate-keys '((mark-whole-buffer . "C-a")))
           (ewm--handle-new-surface '((id . {0}) (app . "app")))
           (add-hook 'post-command-hook #'ewm-input--sync-keyboard-capture)
           (switch-to-buffer (gethash {0} ewm--surfaces))
           (setq capture t)
           (execute-kbd-macro (kbd "C-x h"))
           (cl-assert (eql (aref sent 0) {0}))"#,
        h.ids[APP0]
    );
    let wire = only(h.send_lisp(&config), APP0);
    assert_eq!(wire, tapped(h.ctrl, KEY_A, 0));
}
