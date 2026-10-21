//! Touchpad swipes and scrolls are bindings in `ewm-mode-map`. The overview and frame-focus
//! actions follow the fingers and settle when they lift, every other swipe binding runs when they
//! lift, and under the bound modifiers a wheel notch or ten units of two-finger travel runs its
//! command once while the client sees no scroll at all.
use std::sync::Mutex;

use ewm_core::testing::{Fixture, frame_with_width};
use ewm_core::{Event, FocusSource, Notify};
use smithay::backend::input::AxisSource;

// The intercept list is process-wide: tests run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

const OUTPUT: &str = "HEAD-A";
const KEY_LEFTMETA: u32 = 125 + 8;

/// Three frames, the middle one focused and under the pointer, bound as `config` leaves them.
fn fixture(config: &str) -> Fixture {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(OUTPUT, 1920, 1080);
    fix.configure_lisp(config);
    fix.ewm().animations_clock.set_complete_instantly(true);
    fix.tile_command(
        OUTPUT,
        (1..=3).map(|id| frame_with_width(id, 1920.0)).collect(),
    );
    fix.ewm()
        .set_focus_frame(2, FocusSource::Test, Notify::None);
    fix.pointer_motion_to(960.0, 540.0);
    fix
}

/// Put `fingers` down and move them by (`dx`, `dy`), past the deadzone.
fn swipe(fix: &mut Fixture, fingers: u32, dx: f64, dy: f64) {
    fix.swipe_begin(fingers);
    fix.swipe_update(dx, dy);
}

#[test]
fn the_default_swipes_drive_the_overview_and_the_strip() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture("");

    swipe(&mut fix, 3, 0.0, -400.0);
    assert!(fix.ewm_ref().overview.is_active());
    fix.swipe_end();
    assert!(fix.ewm_ref().overview.is_open());
    swipe(&mut fix, 3, 0.0, 400.0);
    fix.swipe_end();
    assert!(!fix.ewm_ref().overview.is_open());

    // Closing a closed overview still follows the fingers, held at the closed end.
    swipe(&mut fix, 3, 0.0, 400.0);
    fix.swipe_end();
    assert!(!fix.ewm_ref().overview.is_open());

    // Swiping left brings in the frame to the right, the content follows the fingers.
    swipe(&mut fix, 3, -1200.0, 0.0);
    assert!(!fix.strip(OUTPUT).view_is_static());
    fix.swipe_end();
    assert_eq!(fix.strip(OUTPUT).active_frame_id(), Some(3));
    swipe(&mut fix, 3, 1200.0, 0.0);
    fix.swipe_end();
    assert_eq!(fix.strip(OUTPUT).active_frame_id(), Some(2));
}

#[test]
fn a_swapped_strip_binding_moves_the_strip_against_the_fingers() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(r#"(keymap-set ewm-mode-map "<swipe-3-left>" #'ewm-frame-left)"#);

    swipe(&mut fix, 3, -1200.0, 0.0);
    fix.swipe_end();
    assert_eq!(fix.strip(OUTPUT).active_frame_id(), Some(1));
}

#[test]
fn a_toggle_binding_drives_the_overview_both_ways() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(r#"(keymap-set ewm-mode-map "<swipe-3-up>" #'ewm-overview-toggle)"#);

    swipe(&mut fix, 3, 0.0, -400.0);
    fix.swipe_end();
    assert!(fix.ewm_ref().overview.is_open());
    swipe(&mut fix, 3, 0.0, -400.0);
    fix.swipe_end();
    assert!(!fix.ewm_ref().overview.is_open());
}

fn order(fix: &Fixture) -> Vec<u64> {
    fix.strip(OUTPUT).frame_ids().collect()
}

#[test]
fn a_bound_action_runs_when_the_fingers_lift() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(r#"(keymap-set ewm-mode-map "<swipe-4-up>" #'ewm-frame-move-right)"#);

    swipe(&mut fix, 4, 0.0, -400.0);
    assert_eq!(order(&fix), [1, 2, 3]);
    fix.swipe_end();
    assert_eq!(order(&fix), [1, 3, 2]);

    // The other directions of a bound finger count are the compositor's too, and run nothing.
    swipe(&mut fix, 4, 0.0, 400.0);
    fix.swipe_end();
    assert_eq!(order(&fix), [1, 3, 2]);
}

#[test]
fn a_command_binding_runs_when_the_fingers_lift() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(r#"(keymap-set ewm-mode-map "<swipe-3-up>" #'ewm-frame-new)"#);
    fix.enable_event_capture();

    swipe(&mut fix, 3, 0.0, -400.0);
    fix.swipe_end();
    assert!(!fix.ewm_ref().overview.is_active());
    let keys: Vec<String> = fix
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::InterceptedCommand { key } => Some(key),
            _ => None,
        })
        .collect();
    assert_eq!(keys, ["<swipe-3-up>"]);

    // The other default swipes are untouched.
    swipe(&mut fix, 3, -1200.0, 0.0);
    fix.swipe_end();
    assert_eq!(fix.strip(OUTPUT).active_frame_id(), Some(3));
}

#[test]
fn gestures_count_as_activity() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture("");

    fix.ewm().idle_timeout.last_activity -= std::time::Duration::from_secs(1);
    let before = fix.ewm_ref().idle_timeout.last_activity;
    fix.swipe_begin(3);
    assert!(fix.ewm_ref().idle_timeout.last_activity > before);
    fix.swipe_end();

    fix.ewm().idle_timeout.last_activity -= std::time::Duration::from_secs(1);
    let before = fix.ewm_ref().idle_timeout.last_activity;
    fix.pinch_begin(2);
    assert!(fix.ewm_ref().idle_timeout.last_activity > before);
    fix.pinch_end();
}

#[test]
fn a_cancelled_swipe_runs_nothing() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(r#"(keymap-set ewm-mode-map "<swipe-4-up>" #'ewm-frame-move-right)"#);

    swipe(&mut fix, 4, 0.0, -400.0);
    fix.swipe_cancel();
    assert_eq!(order(&fix), [1, 2, 3]);
}

#[test]
fn a_held_modifier_picks_the_modified_binding() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(r#"(keymap-set ewm-mode-map "s-<swipe-3-up>" #'ewm-hide-toggle)"#);

    swipe(&mut fix, 3, 0.0, -400.0);
    assert!(fix.ewm_ref().overview.is_active());
    fix.swipe_end();
    assert!(!fix.ewm_ref().frames_hidden());

    fix.keyboard_key(KEY_LEFTMETA, true);
    swipe(&mut fix, 3, 0.0, -400.0);
    fix.swipe_end();
    assert!(fix.ewm_ref().frames_hidden());
    fix.keyboard_key(KEY_LEFTMETA, false);
}

#[test]
fn an_unbound_default_swipe_runs_nothing() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(
        r#"(keymap-set ewm-mode-map "<swipe-4-up>" #'ewm-overview-open)
           (keymap-unset ewm-mode-map "<swipe-3-up>" t)"#,
    );

    // The fullscreen prefix entry alone binds nothing, and three fingers stay the compositor's
    // for the directions still bound.
    swipe(&mut fix, 3, 0.0, -400.0);
    assert!(!fix.ewm_ref().overview.is_active());
    fix.swipe_end();

    swipe(&mut fix, 4, 0.0, -400.0);
    assert!(fix.ewm_ref().overview.is_active());
    fix.swipe_end();
    assert!(fix.ewm_ref().overview.is_open());
}

fn active(fix: &Fixture) -> Option<u64> {
    fix.strip(OUTPUT).active_frame_id()
}

#[test]
fn a_wheel_notch_under_the_bound_modifier_runs_the_command_once() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(
        r#"(keymap-set ewm-mode-map "s-<wheel-down>" #'ewm-frame-right)
           (keymap-set ewm-mode-map "s-<wheel-up>" #'ewm-frame-left)
           (keymap-set ewm-mode-map "s-<wheel-left>" #'ewm-frame-new)"#,
    );
    fix.enable_event_capture();

    // Without the modifier the scroll is the client's.
    fix.scroll(AxisSource::Wheel, 0.0, 120.0);
    assert_eq!(active(&fix), Some(2));

    fix.keyboard_key(KEY_LEFTMETA, true);
    fix.scroll(AxisSource::Wheel, 0.0, 120.0);
    assert_eq!(active(&fix), Some(3));
    fix.scroll(AxisSource::Wheel, 0.0, -240.0);
    assert_eq!(active(&fix), Some(1));
    // A tick bound to an Emacs command sends the key, the unbound direction does nothing.
    fix.scroll(AxisSource::Wheel, -120.0, 0.0);
    fix.scroll(AxisSource::Wheel, 120.0, 0.0);
    let keys: Vec<String> = fix
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::InterceptedCommand { key } => Some(key),
            _ => None,
        })
        .collect();
    assert_eq!(keys, ["s-<wheel-left>"]);
    assert_eq!(active(&fix), Some(1));
    fix.keyboard_key(KEY_LEFTMETA, false);
}

#[test]
fn finger_travel_ticks_every_ten_units_and_a_turn_starts_over() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(r#"(keymap-set ewm-mode-map "s-<wheel-down>" #'ewm-frame-right)"#);
    fix.keyboard_key(KEY_LEFTMETA, true);

    fix.scroll(AxisSource::Finger, 0.0, 4.0);
    fix.scroll(AxisSource::Finger, 0.0, 4.0);
    assert_eq!(active(&fix), Some(2));
    fix.scroll(AxisSource::Finger, 0.0, 4.0);
    assert_eq!(active(&fix), Some(3));

    // Six units up then six down leave six, not none, and five more tick.
    fix.scroll(AxisSource::Finger, 0.0, -6.0);
    fix.scroll(AxisSource::Finger, 0.0, 6.0);
    assert_eq!(active(&fix), Some(3));
    fix.ewm()
        .set_focus_frame(1, FocusSource::Test, Notify::None);
    fix.scroll(AxisSource::Finger, 0.0, 5.0);
    assert_eq!(active(&fix), Some(2));
    fix.keyboard_key(KEY_LEFTMETA, false);
}

#[test]
fn natural_scrolling_keeps_the_direction_the_fingers_moved() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(r#"(keymap-set ewm-mode-map "s-<wheel-down>" #'ewm-frame-right)"#);
    fix.keyboard_key(KEY_LEFTMETA, true);

    // Fingers moving down arrive negative from a natural-scrolling touchpad.
    fix.scroll_natural(AxisSource::Finger, 0.0, -12.0);
    assert_eq!(active(&fix), Some(3));
    fix.scroll_natural(AxisSource::Finger, 0.0, 12.0);
    assert_eq!(active(&fix), Some(3));
    fix.keyboard_key(KEY_LEFTMETA, false);
}
