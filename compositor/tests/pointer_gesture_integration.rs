//! Touchpad gestures the compositor leaves alone reach the client under the pointer through
//! zwp_pointer_gestures_v1: pinches and holds always, swipes when nothing of ours listens.
use ewm_core::testing::{ClientEvent, Fixture, Gesture, TestClient, frame_with_entry, nz};
use smithay::utils::Size;

const HEAD: &str = "HEAD-A";

/// A fixture with one client toplevel in a frame and the pointer over it.
fn fixture() -> (Fixture, TestClient) {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output(HEAD, 1600, 900);
    fix.set_intercepted_keys_from_lisp(
        r#"[(:key "swipe-3-left" :description "<swipe-3-left>" :dispatch (action strip-scroll))]"#,
    );
    let (_app, sid) = fix.open_toplevel_with_id(&mut client, "app", Size::from((400, 400)));
    fix.tile(HEAD, vec![frame_with_entry(100, nz(1), sid)]);
    client.listen_for_gestures();
    fix.pointer_motion_to(200.0, 200.0);
    fix.roundtrip(&mut client);
    client.drain_events();
    (fix, client)
}

#[test]
fn pinches_and_holds_reach_the_client() {
    let (mut fix, mut client) = fixture();

    fix.pinch_begin(2);
    fix.pinch_update(3.0, -2.0, 1.5, 10.0);
    fix.pinch_end();
    fix.hold_begin(3);
    fix.hold_end();

    fix.roundtrip(&mut client);
    assert_eq!(
        client.drain_events(),
        [
            ClientEvent::GestureBegin {
                gesture: Gesture::Pinch,
                fingers: 2
            },
            ClientEvent::PinchUpdate {
                dx: 3.0,
                dy: -2.0,
                scale: 1.5,
                rotation: 10.0
            },
            ClientEvent::GestureEnd {
                gesture: Gesture::Pinch,
                cancelled: false
            },
            ClientEvent::GestureBegin {
                gesture: Gesture::Hold,
                fingers: 3
            },
            ClientEvent::GestureEnd {
                gesture: Gesture::Hold,
                cancelled: false
            },
        ]
    );
}

#[test]
fn a_swipe_nothing_listens_for_reaches_the_client() {
    let (mut fix, mut client) = fixture();

    // Nothing is bound on four fingers, the three-finger swipe left scrolls the strip, and the
    // unbound right of that bound finger count reaches no one.
    fix.swipe_begin(4);
    fix.swipe_update(0.0, -40.0);
    fix.swipe_end();
    fix.swipe_begin(3);
    fix.swipe_update(-60.0, 0.0);
    fix.swipe_end();
    fix.swipe_begin(3);
    fix.swipe_update(60.0, 0.0);
    fix.swipe_end();

    fix.roundtrip(&mut client);
    assert_eq!(
        client.drain_events(),
        [
            ClientEvent::GestureBegin {
                gesture: Gesture::Swipe,
                fingers: 4
            },
            ClientEvent::SwipeUpdate { dx: 0.0, dy: -40.0 },
            ClientEvent::GestureEnd {
                gesture: Gesture::Swipe,
                cancelled: false
            },
        ]
    );
}
