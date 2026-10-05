//! The session lock confirms whatever mix of disabled outputs and
//! powered-off monitors is showing nothing.

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
    fn lock_confirms_on_dark_outputs(
        count in 1..=3usize,
        disabled in 0u8..8,
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
        let _lock = client.lock_session();
        fixture.roundtrip(&mut client);
        fixture.roundtrip(&mut client);

        let events = client.drain_events();
        prop_assert!(
            events.iter().any(|e| matches!(e, ClientEvent::SessionLocked)),
            "no locked event, got {events:?}"
        );
        prop_assert!(!events.iter().any(|e| matches!(e, ClientEvent::SessionLockFinished)));
        prop_assert!(fixture.ewm_ref().is_locked());
    }
}
