//! Keys bound to compositor actions and workspace activations run in the compositor: the strip
//! and the fullscreen bit change on the press, Emacs is told afterwards and never asked.
use std::sync::Mutex;

use ewm_core::input::KeyboardAction;
use ewm_core::layout::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient, nz, proptest_cases};
use ewm_core::{Event, FocusSource, LayoutEntry, Notify};
use proptest::prelude::*;
use smithay::utils::Size;
use smithay::wayland::xdg_activation::XdgActivationToken;

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
        "[(:key \"F5\" :description \"<f5>\" :dispatch (action focus-frame :delta 1) :repeat t)
          (:key \"F6\" :description \"<f6>\" :dispatch (action focus-nth-frame :n 1))
          (:key \"F8\" :description \"<f8>\" :dispatch (action overview toggle))]",
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
    assert_eq!(fix.keyboard_key(key, true), KeyboardAction::Bound);
    assert_eq!(fix.keyboard_key(key, false), KeyboardAction::Bound);
}

#[test]
fn a_key_switches_the_strip_without_emacs() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(3);
    let output = fix.output(OUTPUT);
    fix.ewm()
        .set_focus_frame(1, FocusSource::Test, Notify::None);
    fix.enable_event_capture();

    tap(&mut fix, KEY_F5);
    assert_eq!(fix.ewm_ref().focused_frame_id(), Some(2));
    assert_eq!(fix.ewm_ref().layout.active_frame_id(&output), Some(2));
    tap(&mut fix, KEY_F6);
    assert_eq!(fix.ewm_ref().focused_frame_id(), Some(1));
    tap(&mut fix, KEY_F8);
    assert!(fix.ewm_ref().overview.is_open());
    tap(&mut fix, KEY_F8);
    assert!(!fix.ewm_ref().overview.is_open());

    let events = fix.drain_events();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::InterceptedCommand { .. })),
        "{events:?}"
    );
}

#[test]
fn a_key_moves_the_focused_frame_round_the_outputs() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(2);
    fix.add_output("HEAD-B", 1920, 1080);
    fix.tile_command("HEAD-B", vec![tile(3, Vec::new())]);
    fix.ewm()
        .set_focus_frame(1, FocusSource::Test, Notify::None);
    fix.set_intercepted_keys_from_lisp(
        "[(:key \"F5\" :description \"<f5>\" :dispatch (action move-frame-to-next-output))]",
    );
    fix.enable_event_capture();

    tap(&mut fix, KEY_F5);
    assert_eq!(fix.strip("HEAD-B").frame_ids().collect::<Vec<_>>(), [3, 1]);
    assert_eq!(fix.strip("HEAD-B").active_frame_id(), Some(1));
    assert_eq!(fix.strip(OUTPUT).active_frame_id(), Some(2));
    assert_eq!(fix.ewm_ref().focused_frame_id(), Some(1));
    let events = fix.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::FrameShown { id: 1 })),
        "{events:?}"
    );

    // Past the last output it comes round, after the frame shown there.
    tap(&mut fix, KEY_F5);
    assert_eq!(fix.strip(OUTPUT).frame_ids().collect::<Vec<_>>(), [2, 1]);
    assert_eq!(fix.strip("HEAD-B").frame_ids().collect::<Vec<_>>(), [3]);
    assert_eq!(fix.ewm_ref().focused_frame_id(), Some(1));
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
    fix.ewm()
        .set_focus_entry(nz(7), FocusSource::Test, Notify::None);
    fix.set_intercepted_keys_from_lisp(
        "[(:key \"F7\" :description \"<f7>\" :dispatch (action toggle-fullscreen) :fullscreen t)]",
    );

    fix.configured(&mut client);
    tap(&mut fix, KEY_F7);
    assert!(fix.configured(&mut client).contains(&(1920, 1080)));
    tap(&mut fix, KEY_F7);
    assert!(!fix.configured(&mut client).contains(&(1920, 1080)));
}

#[test]
fn a_key_spawns_a_command_with_an_activation_token() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(1);
    let marker = std::env::temp_dir().join(format!("ewm-spawn-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let path = marker.to_str().unwrap();
    fix.set_intercepted_keys_from_lisp(&format!(
        "[(:key \"F9\" :description \"<f9>\" :dispatch (action spawn :command \
         (\"sh\" \"-c\" \"printf %s $XDG_ACTIVATION_TOKEN > {path}.tmp && mv {path}.tmp {path}\")))]"
    ));

    tap(&mut fix, KEY_F9);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let token = XdgActivationToken::from(
        std::fs::read_to_string(&marker).expect("the spawned command never ran"),
    );
    let _ = std::fs::remove_file(&marker);
    assert!(
        fix.ewm().activation_state.data_for_token(&token).is_some(),
        "compositor does not know the child's token {token:?}"
    );
}

#[test]
fn a_workspace_activation_focuses_the_frame_in_the_compositor() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(2);
    fix.ewm()
        .set_focus_frame(1, FocusSource::Test, Notify::None);
    fix.enable_event_capture();

    fix.activate_workspace(2);
    assert_eq!(fix.ewm_ref().focused_frame_id(), Some(2));
    let events = fix.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Focus { pointer: false, .. })),
        "{events:?}"
    );
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
    fix.ewm()
        .set_focus_frame(ids[0], FocusSource::Test, Notify::None);
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

/// What gets typed while Emacs may have a focus to read.
#[derive(Debug, Clone, Copy)]
enum Typed {
    /// The key bound to the other frame.
    Action,
    /// `KEY_A` or Shift, pressed or released.
    Key(u32, bool),
    /// Emacs reads the channel.
    Drain,
}

fn typed() -> impl Strategy<Value = Typed> {
    prop_oneof![
        Just(Typed::Action),
        (
            prop::sample::select(vec![KEY_A, KEY_LEFTSHIFT]),
            any::<bool>()
        )
            .prop_map(|(key, pressed)| Typed::Key(key, pressed)),
        Just(Typed::Drain),
    ]
}

/// Keys for Emacs arrive in the order typed and none but a modifier arrives while Emacs has a
/// focus to read, actions never wait.
fn check_typed(ops: &[Typed]) -> Result<(), TestCaseError> {
    let _serial = SERIAL.lock().unwrap();
    let (mut fix, mut emacs, ids) = emacs_fixture(2);
    fix.set_intercepted_keys_from_lisp(
        "[(:key \"F6\" :description \"<f6>\" :dispatch (action focus-nth-frame :n 1))
          (:key \"F7\" :description \"<f7>\" :dispatch (action focus-nth-frame :n 2))]",
    );
    let (mut focused, mut shifted, mut unread_focus) = (0, false, false);
    let (mut typed, mut arrived) = (Vec::new(), Vec::new());
    for &op in ops {
        match op {
            // With Shift held the bound key is a key for Emacs like any other.
            Typed::Action if shifted => {
                for pressed in [true, false] {
                    fix.keyboard_key([KEY_F6, KEY_F7][1 - focused], pressed);
                    typed.push(Seen::Key([KEY_F6, KEY_F7][1 - focused], pressed));
                }
            }
            Typed::Action => {
                focused = 1 - focused;
                tap(&mut fix, [KEY_F6, KEY_F7][focused]);
                prop_assert_eq!(fix.ewm_ref().focused_frame_id(), Some(ids[focused]));
                unread_focus = true;
            }
            Typed::Key(key, pressed) => {
                fix.keyboard_key(key, pressed);
                typed.push(Seen::Key(key, pressed));
                if key == KEY_LEFTSHIFT {
                    shifted = pressed;
                }
            }
            Typed::Drain => {
                fix.drain_channel();
                unread_focus = false;
            }
        }
        tick(&mut fix, &mut emacs);
        let now = keys_seen(&mut emacs);
        if unread_focus {
            prop_assert!(
                now.iter()
                    .all(|key| matches!(key, Seen::Key(KEY_LEFTSHIFT, _))),
                "{:?}",
                now
            );
        }
        arrived.extend(now);
    }
    fix.drain_channel();
    tick(&mut fix, &mut emacs);
    arrived.extend(keys_seen(&mut emacs));
    prop_assert_eq!(arrived, typed);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases(32)))]

    #[test]
    fn keys_for_emacs_wait_until_emacs_reads_the_focus(
        ops in prop::collection::vec(typed(), 1..24),
    ) {
        check_typed(&ops)?;
    }
}

#[test]
fn a_vt_switch_never_waits_behind_deferred_keys() {
    let _serial = SERIAL.lock().unwrap();
    let (mut fix, _emacs, _ids) = emacs_fixture(2);

    tap(&mut fix, KEY_F5);
    for key in [KEY_A, KEY_LEFTCTRL, KEY_LEFTALT] {
        fix.keyboard_key(key, true);
    }
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
    fix.ewm()
        .set_focus_frame(ids[0], FocusSource::Test, Notify::None);
    fix.sync_keyboard_focus();
    tick(&mut fix, &mut emacs);
    fix.drain_channel();
    fix.roundtrip(&mut emacs);
    fix.roundtrip(&mut app);
    let _ = seen(&mut emacs);
    let _ = seen(&mut app);

    tap(&mut fix, KEY_F5);
    fix.keyboard_key(KEY_A, true);
    fix.keyboard_key(KEY_A, false);

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
    fix.ewm()
        .set_focus_entry(nz(7), FocusSource::Test, Notify::None);
    fix.sync_keyboard_focus();
    assert_eq!(fix.keyboard_focus_surface_id(), Some(app_id));
    fix.roundtrip(&mut emacs);
    fix.roundtrip(&mut app);
    assert_eq!(
        keys_seen(&mut emacs),
        vec![Seen::Key(KEY_A, true), Seen::Key(KEY_A, false)]
    );
    assert_eq!(keys_seen(&mut app), vec![]);
}
