//! The session lock waits for a lock surface on every mapped output, up to a
//! deadline, then confirms once every output renders or shows nothing.

use std::time::Duration;

use ewm_core::OutputConfig;
use ewm_core::testing::{ClientEvent, Fixture};
use proptest::prelude::*;

const OUTPUTS: [&str; 3] = ["HEAD-A", "HEAD-B", "HEAD-C"];

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 24,
        ..ProptestConfig::default()
    })]

    #[test]
    fn lock_confirms_once_mapped_outputs_show_surfaces(
        count in 1..=3usize,
        disabled in 0u8..8,
        painted_first in 0u8..8,
        monitors_active in any::<bool>(),
    ) {
        let mut fixture = Fixture::new().unwrap();
        for (i, name) in OUTPUTS.iter().take(count).enumerate() {
            fixture.add_output(name, 1280, 720);
            if disabled & (1 << i) != 0 {
                fixture.ewm().output_config.insert(
                    name.to_string(),
                    OutputConfig {
                        enabled: false,
                        ..Default::default()
                    },
                );
                fixture.apply_output_config(name);
            }
        }
        if !monitors_active {
            fixture.ewm().deactivate_monitors();
        }
        fixture.dispatch();

        let mut client = fixture.new_client().unwrap();
        fixture.roundtrip(&mut client);
        let lock = client.lock_session();
        let (first, rest): (Vec<usize>, Vec<usize>) = (0..count)
            .filter(|i| disabled & (1 << i) == 0)
            .partition(|i| painted_first & (1 << i) != 0);
        for &i in &first {
            client.lock_output(&lock, OUTPUTS[i]);
        }
        fixture.roundtrip(&mut client);
        fixture.roundtrip(&mut client);

        let mut events = client.drain_events();
        prop_assert_eq!(
            events.iter().any(|e| matches!(e, ClientEvent::SessionLocked)),
            rest.is_empty(),
            "locked with lock surfaces on {:?} of {:?}, got {:?}",
            first,
            OUTPUTS,
            events
        );
        prop_assert_eq!(fixture.ewm_ref().is_locked(), rest.is_empty());

        for &i in &rest {
            client.lock_output(&lock, OUTPUTS[i]);
        }
        fixture.roundtrip(&mut client);
        fixture.roundtrip(&mut client);

        events.extend(client.drain_events());
        prop_assert!(
            events.iter().any(|e| matches!(e, ClientEvent::SessionLocked)),
            "no locked event, got {events:?}"
        );
        prop_assert!(!events.iter().any(|e| matches!(e, ClientEvent::SessionLockFinished)));
        prop_assert!(fixture.ewm_ref().is_locked());
    }
}

#[test]
fn lock_confirms_at_the_surface_deadline() {
    let mut fixture = Fixture::new().unwrap();
    fixture.add_output("HEAD-A", 1280, 720);
    fixture.dispatch();
    let mut client = fixture.new_client().unwrap();
    fixture.roundtrip(&mut client);
    let _lock = client.lock_session();
    fixture.roundtrip(&mut client);
    fixture.roundtrip(&mut client);
    assert!(!fixture.ewm_ref().is_locked());
    assert!(!client.drain_events().contains(&ClientEvent::SessionLocked));

    std::thread::sleep(Duration::from_millis(1100));
    fixture.roundtrip(&mut client);
    fixture.roundtrip(&mut client);
    assert!(fixture.ewm_ref().is_locked());
    assert!(client.drain_events().contains(&ClientEvent::SessionLocked));
}
