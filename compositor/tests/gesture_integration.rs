//! Touchpad swipes bound in `ewm-mode-map` run like keys once the fingers lift: an action in the
//! compositor, or the key as an Emacs command over the pipe. The built-in strip and overview
//! swipes keep the finger counts the gesture knobs give them.
use std::sync::Mutex;

use ewm_core::input::{Gestures, Swipe};
use ewm_core::testing::{Fixture, frame_with_width};
use ewm_core::{Event, FocusSource, Notify};

// The intercept list is process-wide, so tests run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

const OUTPUT: &str = "HEAD-A";
const KEY_LEFTMETA: u32 = 125 + 8;

/// An output with three strip frames and the pointer on it, bindings from `keys`.
fn fixture(keys: &str) -> Fixture {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(OUTPUT, 1920, 1080);
    fix.ewm().animations_clock.set_complete_instantly(true);
    fix.tile_command(
        OUTPUT,
        (1..=3).map(|id| frame_with_width(id, 1920.0)).collect(),
    );
    fix.ewm()
        .set_focus_frame(1, FocusSource::Test, Notify::None);
    fix.pointer_motion_to(960.0, 540.0);
    fix.set_intercepted_keys_from_lisp(keys);
    fix
}

/// Put `fingers` down and move them by (`dx`, `dy`), past the deadzone.
fn swipe(fix: &mut Fixture, fingers: u32, dx: f64, dy: f64) {
    fix.swipe_begin(fingers);
    fix.swipe_update(dx, dy);
}

#[test]
fn a_bound_swipe_runs_its_action_when_the_fingers_lift() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix =
        fixture(r#"[(:key "swipe-4-up" :description "<swipe-4-up>" :action (overview toggle))]"#);

    swipe(&mut fix, 4, 0.0, -40.0);
    assert!(matches!(fix.ewm_ref().swipe, Swipe::Bound(_)));
    assert!(!fix.ewm_ref().overview.is_open());
    fix.swipe_end();
    assert!(fix.ewm_ref().overview.is_open());
    assert!(matches!(fix.ewm_ref().swipe, Swipe::Client));

    // The other directions of a bound finger count are the compositor's too, and run nothing.
    swipe(&mut fix, 4, 0.0, 40.0);
    assert!(matches!(fix.ewm_ref().swipe, Swipe::Swallowed));
    fix.swipe_end();
    assert!(fix.ewm_ref().overview.is_open());
}

#[test]
fn a_bound_swipe_sends_its_key_to_emacs_in_place_of_the_built_in() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(r#"[(:key "swipe-3-up" :description "<swipe-3-up>" :dispatch command)]"#);
    fix.enable_event_capture();

    swipe(&mut fix, 3, 0.0, -40.0);
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

    // The binding shadows one direction, the strip swipe keeps the other axis.
    swipe(&mut fix, 3, 700.0, 0.0);
    assert!(!fix.strip(OUTPUT).view_is_static());
    fix.swipe_end();
    assert!(fix.strip(OUTPUT).view_is_static());
}

#[test]
fn a_cancelled_swipe_runs_nothing() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix =
        fixture(r#"[(:key "swipe-4-up" :description "<swipe-4-up>" :action (overview toggle))]"#);

    swipe(&mut fix, 4, 0.0, -40.0);
    fix.swipe_cancel();
    assert!(!fix.ewm_ref().overview.is_open());
    assert!(matches!(fix.ewm_ref().swipe, Swipe::Client));
}

#[test]
fn a_held_modifier_picks_the_modified_binding() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture(
        r#"[(:key "swipe-3-up" :super t :description "s-<swipe-3-up>" :action hide-toggle)]"#,
    );

    // Without Super the plain swipe is still the overview's.
    swipe(&mut fix, 3, 0.0, -40.0);
    assert!(fix.ewm_ref().overview.is_active());
    fix.swipe_end();
    assert!(!fix.ewm_ref().frames_hidden());

    fix.keyboard_key(KEY_LEFTMETA, true);
    swipe(&mut fix, 3, 0.0, -40.0);
    fix.swipe_end();
    assert!(fix.ewm_ref().frames_hidden());
    fix.keyboard_key(KEY_LEFTMETA, false);
}

#[test]
fn the_gesture_knobs_move_the_built_in_swipes() {
    let _serial = SERIAL.lock().unwrap();
    let mut fix = fixture("[]");
    fix.ewm().gestures = Gestures {
        strip_swipe_fingers: Some(4),
        overview_swipe_fingers: None,
    };

    // Nothing listens for three fingers any more: the client gets the swipe.
    swipe(&mut fix, 3, 0.0, -40.0);
    assert!(matches!(fix.ewm_ref().swipe, Swipe::Client));
    assert!(!fix.ewm_ref().overview.is_active());
    fix.swipe_end();

    swipe(&mut fix, 4, 700.0, 0.0);
    assert!(!fix.strip(OUTPUT).view_is_static());
    fix.swipe_end();
    assert!(fix.strip(OUTPUT).view_is_static());
}
