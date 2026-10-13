//! Keys bound to compositor actions run in the compositor: the strip and the fullscreen bit change
//! on the press, Emacs is told afterwards and never asked.
use std::sync::Mutex;

use ewm_core::input::KeyboardAction;
use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient, nz};
use ewm_core::{Event, LayoutEntry};
use proptest::prelude::*;
use smithay::utils::Size;

// The intercept list is process-wide, so tests run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

const OUTPUT: &str = "HEAD-A";
const KEY_F5: u32 = 63 + 8;
const KEY_F6: u32 = 64 + 8;
const KEY_F7: u32 = 65 + 8;
const KEY_F8: u32 = 66 + 8;
const KEY_F9: u32 = 67 + 8;
const KEY_LEFTCTRL: u32 = 29 + 8;
const KEY_A: u32 = 30 + 8;
const KEY_LEFTSHIFT: u32 = 42 + 8;
const KEY_LEFTALT: u32 = 56 + 8;
const KEY_F2: u32 = 60 + 8;

fn tile(surface_id: u64, entries: Vec<LayoutEntry>) -> Frame {
    let mut frame = Frame::new(surface_id);
    frame.width = 1920.0;
    frame.height = 0.0;
    frame.entries = entries;
    frame
}

/// Tile `ids` as the strip of `OUTPUT`, F5 and F6 bound to strip actions, F8 to the overview.
fn strip(fix: &mut Fixture, ids: &[u64]) {
    fix.tile_command(OUTPUT, ids.iter().map(|&id| tile(id, Vec::new())).collect());
    fix.set_intercepted_keys_from_lisp(
        "[(:key \"F5\" :description \"<f5>\" :action (focus-frame :delta 1) :repeat t)
          (:key \"F6\" :description \"<f6>\" :action (focus-nth-frame :n 1))
          (:key \"F8\" :description \"<f8>\" :action (overview toggle))]",
    );
}

/// An output with strip frames `1..=frames` and the strip keys bound.
fn fixture(frames: u64) -> Fixture {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(OUTPUT, 1920, 1080);
    strip(&mut fix, &(1..=frames).collect::<Vec<_>>());
    fix
}

fn tap(fix: &mut Fixture, key: u32) {
    assert_eq!(fix.keyboard_key(key, true), KeyboardAction::ActionRun);
    assert_eq!(fix.keyboard_key(key, false), KeyboardAction::ActionRun);
}

#[test]
fn a_key_switches_the_strip_without_emacs() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(3);
    let output = fix.output(OUTPUT);
    fix.ewm().set_focus_frame(1, "test", false);
    fix.enable_event_capture();

    tap(&mut fix, KEY_F5);
    assert_eq!(fix.ewm_ref().focused_frame_id, 2);
    assert_eq!(fix.ewm_ref().layout.active_frame_id(&output), Some(2));
    tap(&mut fix, KEY_F6);
    assert_eq!(fix.ewm_ref().focused_frame_id, 1);
    tap(&mut fix, KEY_F8);
    assert!(fix.ewm_ref().overview.is_open());
    tap(&mut fix, KEY_F8);
    assert!(!fix.ewm_ref().overview.is_open());

    let events = fix.drain_events();
    let focus = events
        .iter()
        .filter(|event| matches!(event, Event::Focus { pointer: false, .. }))
        .count();
    assert_eq!(focus, 2, "{events:?}");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::InterceptedCommand { .. })),
        "{events:?}"
    );
}

#[test]
fn a_key_toggles_the_focused_view_fullscreen() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(1);
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let top = fix.open_toplevel_unmapped(&mut client, "mpv", None);
    fix.map_toplevel(&mut client, &top, Size::from((400, 300)));
    let sid = fix.find_surface_id(&client, &top.surface).unwrap();
    let view = LayoutEntry::surface(nz(7), "view".into(), sid, 0, 0, 400, 300);
    fix.tile_command(OUTPUT, vec![tile(1, vec![view])]);
    fix.ewm().set_focus_entry(nz(7), "test", false);
    fix.set_intercepted_keys_from_lisp(
        "[(:key \"F7\" :description \"<f7>\" :action toggle-fullscreen :fullscreen t)]",
    );

    tap(&mut fix, KEY_F7);
    assert!(fix.ewm_ref().pending_fullscreen(sid));
    tap(&mut fix, KEY_F7);
    assert!(!fix.ewm_ref().pending_fullscreen(sid));
}

#[test]
fn a_key_spawns_a_command() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(1);
    let marker = std::env::temp_dir().join(format!("ewm-spawn-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    fix.set_intercepted_keys_from_lisp(&format!(
        "[(:key \"F9\" :description \"<f9>\" :action (spawn :command (\"touch\" {:?})))]",
        marker.to_str().unwrap()
    ));

    tap(&mut fix, KEY_F9);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(marker.exists(), "the spawned command never ran");
    let _ = std::fs::remove_file(&marker);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// The strip moves by `delta` from the focused frame and stops at its ends.
    #[test]
    fn focus_frame_moves_by_delta_within_the_strip(
        frames in 1u64..=5,
        focused in 0u64..5,
        delta in -5i32..=5,
    ) {
        let _serial = SERIAL.lock().unwrap();
        let focused = focused % frames + 1;
        let mut fix = fixture(frames);
        fix.ewm().set_focus_frame(focused, "test", false);
        fix.set_intercepted_keys_from_lisp(&format!(
            "[(:key \"F5\" :description \"<f5>\" :action (focus-frame :delta {delta}))]"
        ));
        tap(&mut fix, KEY_F5);
        let target = focused as i32 + delta;
        let expected = if (1..=frames as i32).contains(&target) { target as u64 } else { focused };
        prop_assert_eq!(fix.ewm_ref().focused_frame_id, expected);
    }
}

// Keys typed into Emacs after an action wait until Emacs has read the Focus the action queued.

/// Keyboard traffic a client saw: keys as `(keycode, pressed)`, modifiers as the depressed mask.
#[derive(Debug, PartialEq)]
enum Seen {
    Key(u32, bool),
    Mods(u32),
}

fn seen(client: &mut TestClient) -> Vec<Seen> {
    client
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
            ClientEvent::KeyboardKey { key, pressed } => Some(Seen::Key(key + 8, pressed)),
            ClientEvent::KeyboardModifiers { depressed, .. } => Some(Seen::Mods(depressed)),
            _ => None,
        })
        .collect()
}

fn keys_seen(client: &mut TestClient) -> Vec<Seen> {
    seen(client)
        .into_iter()
        .filter(|event| matches!(event, Seen::Key(..)))
        .collect()
}

/// An Emacs client with `frames` strip frames and the strip keys bound, focus on the first, and
/// the event channel Emacs reads.
fn emacs_fixture(frames: usize) -> (Fixture, TestClient, Vec<u64>) {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(OUTPUT, 1920, 1080);
    let mut emacs = fix.new_client().unwrap();
    fix.roundtrip(&mut emacs);
    let ids: Vec<u64> = (0..frames)
        .map(|_| fix.open_toplevel_id(&mut emacs, "emacs", Size::from((1920, 1080))))
        .collect();
    fix.roundtrip(&mut emacs);
    strip(&mut fix, &ids);
    fix.ewm().set_focus_frame(ids[0], "test", false);
    fix.sync_keyboard_focus();
    fix.roundtrip(&mut emacs);
    assert_eq!(fix.keyboard_focus_surface_id(), Some(ids[0]));
    let _ = seen(&mut emacs);
    fix.install_event_channel();
    (fix, emacs, ids)
}

/// Flush the tick, as the compositor loop does before Emacs can read anything.
fn tick(fix: &mut Fixture, emacs: &mut TestClient) {
    fix.dispatch();
    fix.roundtrip(emacs);
}

#[test]
fn keys_for_emacs_wait_until_emacs_reads_the_focus() {
    let _serial = SERIAL.lock().unwrap();
    let (mut fix, mut emacs, ids) = emacs_fixture(4);

    // Actions never wait, however far behind Emacs is, typed keys do.
    tap(&mut fix, KEY_F5);
    tap(&mut fix, KEY_F5);
    assert_eq!(fix.ewm_ref().focused_frame_id, ids[2]);
    assert_eq!(fix.keyboard_key(KEY_A, true), KeyboardAction::Deferred);
    assert_eq!(fix.keyboard_key(KEY_A, false), KeyboardAction::Deferred);
    tap(&mut fix, KEY_F5);
    assert_eq!(fix.ewm_ref().focused_frame_id, ids[3]);
    assert_eq!(fix.keyboard_key(KEY_A, true), KeyboardAction::Deferred);
    assert_eq!(fix.keyboard_key(KEY_A, false), KeyboardAction::Deferred);
    tick(&mut fix, &mut emacs);
    assert_eq!(keys_seen(&mut emacs), vec![]);
    assert_eq!(fix.deferred_key_count(), 4);

    // One drain reads the coalesced Focus, and every key lands in the frame Emacs now selects.
    let events = fix.drain_channel();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Focus { .. })),
        "{events:?}"
    );
    fix.roundtrip(&mut emacs);
    assert_eq!(
        keys_seen(&mut emacs),
        vec![
            Seen::Key(KEY_A, true),
            Seen::Key(KEY_A, false),
            Seen::Key(KEY_A, true),
            Seen::Key(KEY_A, false)
        ]
    );
    assert_eq!(fix.deferred_key_count(), 0);
    assert_eq!(fix.keyboard_key(KEY_A, true), KeyboardAction::Forward);
    assert_eq!(fix.keyboard_key(KEY_A, false), KeyboardAction::Forward);
}

#[test]
fn deferred_keys_replay_with_the_modifiers_they_had() {
    let _serial = SERIAL.lock().unwrap();
    let (mut fix, mut emacs, _ids) = emacs_fixture(2);

    tap(&mut fix, KEY_F5);
    assert_eq!(
        fix.keyboard_key(KEY_LEFTSHIFT, true),
        KeyboardAction::Forward
    );
    for (key, pressed) in [(KEY_A, true), (KEY_A, false), (KEY_LEFTSHIFT, false)] {
        assert_eq!(fix.keyboard_key(key, pressed), KeyboardAction::Deferred);
    }
    tick(&mut fix, &mut emacs);
    assert_eq!(
        seen(&mut emacs),
        vec![Seen::Mods(0), Seen::Key(KEY_LEFTSHIFT, true), Seen::Mods(1)]
    );

    fix.drain_channel();
    fix.roundtrip(&mut emacs);
    assert_eq!(
        seen(&mut emacs),
        vec![
            Seen::Key(KEY_A, true),
            Seen::Key(KEY_A, false),
            Seen::Key(KEY_LEFTSHIFT, false),
            Seen::Mods(0),
        ]
    );
}

#[test]
fn modifier_only_keys_do_not_make_actions_wait() {
    let _serial = SERIAL.lock().unwrap();
    let (mut fix, _emacs, ids) = emacs_fixture(3);

    // Neither a release nor a second Shift, which changes no modifier state, opens the barrier.
    tap(&mut fix, KEY_F5);
    assert_eq!(
        fix.keyboard_key(KEY_LEFTSHIFT, true),
        KeyboardAction::Forward
    );
    const KEY_RIGHTSHIFT: u32 = 54 + 8;
    for (key, pressed) in [
        (KEY_RIGHTSHIFT, true),
        (KEY_LEFTSHIFT, false),
        (KEY_RIGHTSHIFT, false),
    ] {
        assert_eq!(fix.keyboard_key(key, pressed), KeyboardAction::Forward);
        assert_eq!(fix.deferred_key_count(), 0);
    }
    tap(&mut fix, KEY_F5);
    assert_eq!(fix.ewm_ref().focused_frame_id, ids[2]);
}

#[test]
fn a_vt_switch_never_waits_behind_deferred_keys() {
    let _serial = SERIAL.lock().unwrap();
    let (mut fix, _emacs, _ids) = emacs_fixture(2);

    tap(&mut fix, KEY_F5);
    assert_eq!(fix.keyboard_key(KEY_A, true), KeyboardAction::Deferred);
    assert_eq!(
        fix.keyboard_key(KEY_LEFTCTRL, true),
        KeyboardAction::Deferred
    );
    assert_eq!(
        fix.keyboard_key(KEY_LEFTALT, true),
        KeyboardAction::Deferred
    );
    assert_eq!(fix.keyboard_key(KEY_F2, true), KeyboardAction::ChangeVt(2));
}

#[test]
fn deferred_keys_reach_emacs_before_focus_leaves_it() {
    let _serial = SERIAL.lock().unwrap();
    let (mut fix, mut emacs, ids) = emacs_fixture(2);
    let mut app = fix.new_client().unwrap();
    fix.roundtrip(&mut app);
    let app_id = fix.open_toplevel_id(&mut app, "app", Size::from((400, 300)));
    fix.roundtrip(&mut app);
    fix.ewm().set_focus_frame(ids[0], "test", false);
    fix.sync_keyboard_focus();
    tick(&mut fix, &mut emacs);
    fix.drain_channel();
    fix.roundtrip(&mut emacs);
    fix.roundtrip(&mut app);
    let _ = seen(&mut emacs);
    let _ = seen(&mut app);

    tap(&mut fix, KEY_F5);
    assert_eq!(fix.keyboard_key(KEY_A, true), KeyboardAction::Deferred);
    assert_eq!(fix.keyboard_key(KEY_A, false), KeyboardAction::Deferred);

    let mut frames: Vec<Frame> = ids.iter().map(|&id| tile(id, Vec::new())).collect();
    frames[1].entries = vec![LayoutEntry::surface(
        nz(7),
        "view".into(),
        app_id,
        0,
        0,
        400,
        300,
    )];
    fix.tile_command(OUTPUT, frames);
    fix.ewm().set_focus_entry(nz(7), "test", false);
    fix.sync_keyboard_focus();
    assert_eq!(fix.keyboard_focus_surface_id(), Some(app_id));
    assert_eq!(fix.deferred_key_count(), 0);
    fix.roundtrip(&mut emacs);
    fix.roundtrip(&mut app);
    assert_eq!(
        keys_seen(&mut emacs),
        vec![Seen::Key(KEY_A, true), Seen::Key(KEY_A, false)]
    );
    assert_eq!(keys_seen(&mut app), vec![]);
}
