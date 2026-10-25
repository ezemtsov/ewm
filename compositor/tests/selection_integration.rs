//! The clipboard over the protocol: the focused client's selection reaches Emacs as text, the
//! offer follows keyboard focus to the next client, and an unfocused client cannot take it.

use std::time::{Duration, Instant};

use ewm_core::testing::{ClientEvent, Fixture, TestClient, frame_with_entries, received};
use ewm_core::{Event, FocusSource, LayoutEntryId, Notify};
use smithay::utils::Size;

const HEAD: &str = "HEAD-A";
const MIME: &str = "text/plain;charset=utf-8";
const LEFT: LayoutEntryId = LayoutEntryId::new(1).unwrap();
const RIGHT: LayoutEntryId = LayoutEntryId::new(2).unwrap();

/// Two apps side by side in one frame, the left one focused, both with a data device.
fn pair(fix: &mut Fixture) -> (TestClient, TestClient) {
    let mut left = fix.new_client().unwrap();
    let mut right = fix.new_client().unwrap();
    fix.roundtrip(&mut left);
    fix.roundtrip(&mut right);
    left.data_device();
    right.data_device();
    fix.add_output(HEAD, 1600, 900);
    let (_, a_sid) = fix.open_toplevel_with_id(&mut left, "left", Size::from((800, 900)));
    let (_, b_sid) = fix.open_toplevel_with_id(&mut right, "right", Size::from((800, 900)));
    fix.tile(
        HEAD,
        vec![frame_with_entries(100, &[(LEFT, a_sid), (RIGHT, b_sid)])],
    );
    fix.roundtrip(&mut left);
    fix.roundtrip(&mut right);
    left.drain_events();
    right.drain_events();
    fix.enable_event_capture();
    fix.drain_events();
    (left, right)
}

/// Whether Emacs hears TEXT within a few seconds, the compositor reads the source on a thread.
fn emacs_hears(fix: &mut Fixture, client: &mut TestClient, text: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        fix.roundtrip(client);
        if fix
            .drain_events()
            .iter()
            .any(|e| matches!(e, Event::SelectionChanged { text: heard } if heard == text))
        {
            return true;
        }
    }
    false
}

fn offered_mimes(client: &mut TestClient) -> Vec<Vec<String>> {
    client
        .drain_events()
        .into_iter()
        .filter_map(|e| match e {
            ClientEvent::SelectionOffered { mimes } => Some(mimes),
            _ => None,
        })
        .collect()
}

#[test]
fn the_focused_client_selection_reaches_emacs() {
    let mut fix = Fixture::new().unwrap();
    let (mut left, _right) = pair(&mut fix);
    left.set_selection(MIME, b"from the left");
    assert!(emacs_hears(&mut fix, &mut left, "from the left"));
}

#[test]
fn the_offer_follows_keyboard_focus() {
    let mut fix = Fixture::new().unwrap();
    let (mut left, mut right) = pair(&mut fix);
    left.set_selection(MIME, b"from the left");
    fix.roundtrip(&mut left);
    assert!(offered_mimes(&mut right).is_empty());

    fix.ewm()
        .set_focus_entry(RIGHT, FocusSource::Test, Notify::None);
    fix.sync_keyboard_focus();
    fix.roundtrip(&mut right);
    assert_eq!(offered_mimes(&mut right), [[MIME]]);
    let data = right.receive_selection(MIME).expect("an offer");
    fix.roundtrip(&mut right);
    fix.roundtrip(&mut left);
    assert_eq!(received(data).as_deref(), Some(&b"from the left"[..]));
}

#[test]
fn an_unfocused_client_cannot_take_the_selection() {
    let mut fix = Fixture::new().unwrap();
    let (mut left, mut right) = pair(&mut fix);
    right.set_selection(MIME, b"from the right");
    fix.roundtrip(&mut right);
    fix.roundtrip(&mut left);
    assert!(offered_mimes(&mut left).is_empty());
    assert!(
        fix.drain_events()
            .iter()
            .all(|e| !matches!(e, Event::SelectionChanged { .. }))
    );
}
