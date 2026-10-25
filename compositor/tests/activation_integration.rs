//! Activation over the protocol, every serial against every kind of target. A token from the
//! focused client moves focus to a surface shown beside it, asks Emacs to show a hidden one
//! again and waits for an unplaced one until a layout places it. A token without a serial only
//! flags urgency. A stale serial does nothing.

use ewm_core::testing::{ClientEvent, Fixture, frame_with_entries};
use ewm_core::{Event, LayoutEntryId};
use smithay::utils::Size;

const HEAD: &str = "HEAD-A";
const SHOWN: LayoutEntryId = LayoutEntryId::new(1).unwrap();
const TARGET: LayoutEntryId = LayoutEntryId::new(2).unwrap();

#[derive(Clone, Copy, Debug)]
enum Serial {
    Current,
    Stale,
    Absent,
}

#[derive(Clone, Copy, Debug)]
enum Target {
    /// Shown in the window beside the focused one.
    Beside,
    /// Shown once, then left out of the layout.
    Hidden,
    /// Never placed.
    Unplaced,
}

fn activation(serial: Serial, target: Target) {
    let case = format!("{serial:?} serial, {target:?} target");
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output(HEAD, 1600, 900);
    let (shown, shown_sid) =
        fix.open_toplevel_with_id(&mut client, "shown", Size::from((800, 900)));
    let (other, other_sid) =
        fix.open_toplevel_with_id(&mut client, "other", Size::from((800, 900)));
    let both = || frame_with_entries(100, &[(SHOWN, shown_sid), (TARGET, other_sid)]);
    let alone = || frame_with_entries(100, &[(SHOWN, shown_sid)]);
    match target {
        Target::Beside => fix.tile(HEAD, vec![both()]),
        Target::Hidden => {
            fix.tile(HEAD, vec![both()]);
            fix.tile(HEAD, vec![alone()]);
        }
        Target::Unplaced => fix.tile(HEAD, vec![alone()]),
    }
    fix.roundtrip(&mut client);
    client.drain_events();
    fix.enable_event_capture();
    fix.drain_events();
    assert_eq!(
        fix.ewm_ref().focused_surface_id(),
        Some(shown_sid),
        "{case}"
    );
    let entered = client
        .enter_serial()
        .expect("the keyboard entered the shown surface");

    let value = match serial {
        Serial::Current => Some(entered),
        // Older than the pointer enter too, which came with the same layout.
        Serial::Stale => Some(entered.wrapping_sub(1000)),
        Serial::Absent => None,
    };
    client.request_activation_token(Some(&shown.surface), value);
    fix.roundtrip(&mut client);
    let token = client.drain_events().into_iter().find_map(|e| match e {
        ClientEvent::ActivationToken { token } => Some(token),
        _ => None,
    });
    if let Some(token) = &token {
        client.activate(token, &other.surface);
        fix.roundtrip(&mut client);
    }

    let events = fix.drain_events();
    let asked = events
        .iter()
        .any(|e| matches!(e, Event::ActivateSurface { id } if *id == other_sid));
    let (pending, urgent) = {
        let other = &fix.ewm_ref().surfaces[&other_sid];
        (other.activation_pending(), other.urgent)
    };
    let focused = fix.ewm_ref().focused_surface_id();
    match (serial, target) {
        (Serial::Current, Target::Beside) => {
            assert_eq!(focused, Some(other_sid), "{case}");
            assert!(!asked && !pending && !urgent, "{case}: {events:?}");
        }
        (Serial::Current, Target::Hidden) => {
            assert!(asked && pending, "{case}: {events:?}");
            assert_eq!(focused, Some(shown_sid), "{case}");
        }
        (Serial::Current, Target::Unplaced) => {
            assert!(!asked && pending, "{case}: {events:?}");
            assert_eq!(focused, Some(shown_sid), "{case}");
            // The layout that places it settles the request.
            fix.tile(HEAD, vec![both()]);
            assert_eq!(
                fix.ewm_ref().focused_surface_id(),
                Some(other_sid),
                "{case}"
            );
            assert!(
                !fix.ewm_ref().surfaces[&other_sid].activation_pending(),
                "{case}"
            );
        }
        (Serial::Absent, _) => {
            assert!(urgent && !asked && !pending, "{case}: {events:?}");
            assert_eq!(focused, Some(shown_sid), "{case}");
        }
        (Serial::Stale, _) => {
            assert!(!urgent && !asked && !pending, "{case}: {events:?}");
            assert_eq!(focused, Some(shown_sid), "{case}");
        }
    }
}

#[test]
fn activation_follows_the_serial_and_the_target() {
    for serial in [Serial::Current, Serial::Stale, Serial::Absent] {
        for target in [Target::Beside, Target::Hidden, Target::Unplaced] {
            activation(serial, target);
        }
    }
}
