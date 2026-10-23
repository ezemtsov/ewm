//! Touchpad swipes and scrolls are bindings in `ewm-mode-map`. The overview, the hide and the
//! strip scroll follow the fingers and settle when they lift, every other swipe binding runs when
//! they lift, and under the bound modifiers a wheel notch or ten units of two-finger travel runs
//! its command once while the client sees no scroll at all. A gesture goes to the surface under
//! the pointer, and that surface is the one a binding yields to.
use std::sync::Mutex;

use ewm_core::testing::{Fixture, TestClient, Toplevel, frame_with_entry, frame_with_width, nz};
use ewm_core::{Event, FocusSource, Notify};
use smithay::backend::input::AxisSource;
use smithay::utils::Size;

// The intercept list is process-wide: tests run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

const OUTPUT: &str = "HEAD-A";
const OUTPUTS: [&str; 2] = [OUTPUT, "HEAD-B"];
const SIZE: (f64, f64) = (1920.0, 1080.0);
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

/// Two outputs of three frames each, ids 1-3 and 4-6, the middle ones active.
fn two_output_fixture(config: &str) -> Fixture {
    let mut fix = Fixture::new().unwrap();
    for output in OUTPUTS {
        fix.add_output(output, SIZE.0 as i32, SIZE.1 as i32);
    }
    fix.configure_lisp(config);
    fix.ewm().animations_clock.set_complete_instantly(true);
    for (index, output) in OUTPUTS.iter().enumerate() {
        let first = 3 * index as u64 + 1;
        fix.tile_command(
            output,
            (first..first + 3)
                .map(|id| frame_with_width(id, SIZE.0))
                .collect(),
        );
    }
    for output in (0..2).rev() {
        fix.ewm()
            .set_focus_frame(middle(output), FocusSource::Test, Notify::None);
    }
    fix
}

/// The centre of `output`, in global coordinates.
fn centre(fix: &Fixture, output: &str) -> (f64, f64) {
    let rect = fix
        .ewm_ref()
        .layout
        .output_rect(&fix.output(output))
        .unwrap();
    (
        f64::from(rect.loc.x) + SIZE.0 / 2.0,
        f64::from(rect.loc.y) + SIZE.1 / 2.0,
    )
}

fn middle(output: usize) -> u64 {
    3 * output as u64 + 2
}

/// A client view in the middle frame of output 0, fullscreen.
fn fullscreen_view(fix: &mut Fixture) -> (TestClient, Toplevel) {
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let (toplevel, sid) = fix.open_toplevel_with_id(&mut client, "game", Size::from((400, 300)));
    let mut frame = frame_with_entry(middle(0), nz(7), sid);
    frame.width = SIZE.0;
    fix.tile_command(
        OUTPUTS[0],
        vec![
            frame_with_width(1, SIZE.0),
            frame,
            frame_with_width(3, SIZE.0),
        ],
    );
    fix.roundtrip(&mut client);
    fix.set_fullscreen(nz(7), true);
    fix.answer_configure(&mut client, &toplevel);
    (client, toplevel)
}

/// Put `fingers` down and move them by (`dx`, `dy`), past the deadzone.
fn swipe(fix: &mut Fixture, fingers: u32, dx: f64, dy: f64) {
    fix.swipe_begin(fingers);
    fix.swipe_update(dx, dy);
}

/// The keys of the commands Emacs was sent since the last drain.
fn commands(fix: &mut Fixture) -> Vec<String> {
    fix.drain_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::InterceptedCommand { key } => Some(key),
            _ => None,
        })
        .collect()
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

    // Sideways scrolls the strip like a page, right brings in the next frame.
    swipe(&mut fix, 3, 1200.0, 0.0);
    assert!(!fix.strip(OUTPUT).view_is_static());
    fix.swipe_end();
    assert_eq!(fix.strip(OUTPUT).active_frame_id(), Some(3));
    swipe(&mut fix, 3, -1200.0, 0.0);
    fix.swipe_end();
    assert_eq!(fix.strip(OUTPUT).active_frame_id(), Some(2));
}

#[test]
fn natural_scrolling_scrolls_the_strip_with_the_fingers() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture("");
    fix.set_natural_scroll(true);

    swipe(&mut fix, 3, -1200.0, 0.0);
    fix.swipe_end();
    assert_eq!(fix.strip(OUTPUT).active_frame_id(), Some(3));
    swipe(&mut fix, 3, 1200.0, 0.0);
    fix.swipe_end();
    assert_eq!(fix.strip(OUTPUT).active_frame_id(), Some(2));
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
fn a_hide_binding_drags_the_frames_and_settles() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(r#"(keymap-set ewm-mode-map "<swipe-4-up>" #'ewm-hide-toggle)"#);

    // Half the travel of a full hide, the frames halfway off.
    swipe(&mut fix, 4, 0.0, -150.0);
    assert!(!fix.ewm_ref().frames_in_place());
    let halfway = fix.ewm_ref().hide.value();
    assert!((0.4..0.6).contains(&halfway), "{halfway}");
    fix.swipe_update(0.0, -250.0);
    fix.swipe_end();
    assert!(fix.ewm_ref().frames_hidden());

    // The same swipe brings them back.
    swipe(&mut fix, 4, 0.0, -400.0);
    fix.swipe_end();
    assert!(!fix.ewm_ref().frames_hidden());
}

#[test]
fn a_command_binding_runs_when_the_fingers_lift() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(r#"(keymap-set ewm-mode-map "<swipe-3-up>" #'ewm-frame-new)"#);
    fix.enable_event_capture();

    swipe(&mut fix, 3, 0.0, -400.0);
    fix.swipe_end();
    assert!(!fix.ewm_ref().overview.is_active());
    assert_eq!(commands(&mut fix), ["<swipe-3-up>"]);

    // The other default swipes are untouched.
    swipe(&mut fix, 3, 1200.0, 0.0);
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
    assert_eq!(commands(&mut fix), ["s-<wheel-left>"]);
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
    fix.set_natural_scroll(true);
    fix.scroll(AxisSource::Finger, 0.0, -12.0);
    assert_eq!(active(&fix), Some(3));
    fix.scroll(AxisSource::Finger, 0.0, 12.0);
    assert_eq!(active(&fix), Some(3));
    fix.keyboard_key(KEY_LEFTMETA, false);
}

#[test]
fn a_binding_yields_to_the_fullscreen_surface_under_the_pointer_only() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = two_output_fixture(r#"(keymap-set ewm-mode-map "<swipe-4-up>" #'ewm-frame-new)"#);
    let (_client, _toplevel) = fullscreen_view(&mut fix);
    fix.ewm()
        .set_focus_frame(middle(0), FocusSource::Test, Notify::None);
    assert!(fix.ewm_ref().focused_fullscreen_entry().is_some());
    fix.enable_event_capture();

    // The keyboard sits on the fullscreen view, the pointer on the other output: the swipe is ours.
    let (x, y) = centre(&fix, OUTPUTS[1]);
    fix.pointer_motion_to(x, y);
    fix.swipe_begin(4);
    fix.swipe_update(0.0, -400.0);
    fix.swipe_end();
    assert_eq!(commands(&mut fix).len(), 1);

    // Over the fullscreen view the swipe is the client's, the keyboard elsewhere or not.
    fix.ewm()
        .set_focus_frame(middle(1), FocusSource::Test, Notify::None);
    let (x, y) = centre(&fix, OUTPUTS[0]);
    fix.pointer_motion_to(x, y);
    fix.drain_events();
    fix.swipe_begin(4);
    fix.swipe_update(0.0, -400.0);
    fix.swipe_end();
    assert!(commands(&mut fix).is_empty());
}

#[test]
fn a_fullscreen_flag_keeps_a_gesture_over_the_fullscreen_view() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = two_output_fixture(
        r#"(keymap-set ewm-mode-map "<swipe-4-up>" (ewm-key #'ewm-frame-new :fullscreen))
           (keymap-set ewm-mode-map "s-<wheel-down>" (ewm-key #'ewm-frame-new :fullscreen))"#,
    );
    let (_client, _toplevel) = fullscreen_view(&mut fix);
    fix.ewm()
        .set_focus_frame(middle(0), FocusSource::Test, Notify::None);
    fix.enable_event_capture();

    let (x, y) = centre(&fix, OUTPUTS[0]);
    fix.pointer_motion_to(x, y);
    fix.swipe_begin(4);
    fix.swipe_update(0.0, -400.0);
    fix.swipe_end();
    fix.keyboard_key(KEY_LEFTMETA, true);
    fix.scroll(AxisSource::Wheel, 0.0, 120.0);
    fix.keyboard_key(KEY_LEFTMETA, false);
    assert_eq!(commands(&mut fix), ["<swipe-4-up>", "s-<wheel-down>"]);
}
