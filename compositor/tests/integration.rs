//! Integration tests for EWM compositor
//!
//! These tests verify compositor behavior using the headless backend and test fixture.

use ewm_core::backend::closest_representable_scale;
use ewm_core::testing::Fixture;
use ewm_core::utils::output_size;
use ewm_core::OutputConfig;
use proptest::prelude::*;

const CASES: u32 = 24;

proptest! {
    #![proptest_config(ProptestConfig {
        cases: CASES,
        ..ProptestConfig::default()
    })]

    #[test]
    fn output_size_tracks_headless_output_bounds(
        first_w in 640i32..=3840,
        first_h in 480i32..=2160,
        second_w in 640i32..=3840,
        second_h in 480i32..=2160,
        second_x in 0i32..=3840,
        second_y in 0i32..=2160,
    ) {
        let mut fixture = Fixture::new().expect("Failed to create fixture");
        fixture.add_output("Virtual-1", first_w, first_h);
        fixture.ewm().output_config.insert(
            "Virtual-2".to_string(),
            OutputConfig {
                position: Some((second_x, second_y)),
                ..Default::default()
            },
        );
        fixture.add_output("Virtual-2", second_w, second_h);
        fixture.dispatch();

        let ewm = fixture.ewm_ref();
        prop_assert_eq!(ewm.output_size.w, first_w.max(second_x + second_w));
        prop_assert_eq!(ewm.output_size.h, first_h.max(second_y + second_h));
    }

    #[test]
    fn output_config_scale_rounds_and_updates_working_area(
        width in 640i32..=3840,
        height in 480i32..=2160,
        scale_milli in 750i32..=3_000,
    ) {
        let scale = f64::from(scale_milli) / 1_000.0;
        let mut fixture = Fixture::new().expect("Failed to create fixture");
        fixture.add_output("Virtual-1", width, height);
        fixture.dispatch();

        fixture.ewm().output_config.insert(
            "Virtual-1".to_string(),
            OutputConfig {
                scale: Some(scale),
                ..Default::default()
            },
        );
        fixture.apply_output_config("Virtual-1");
        fixture.dispatch();

        let output = fixture
            .ewm_ref()
            .space
            .outputs()
            .find(|o| o.name() == "Virtual-1")
            .unwrap()
            .clone();
        let effective_scale = output.current_scale().fractional_scale();
        let expected_scale = closest_representable_scale(scale);
        prop_assert!((effective_scale - expected_scale).abs() < 1e-10);

        let working_area = fixture
            .ewm_ref()
            .working_areas
            .get("Virtual-1")
            .cloned()
            .unwrap();
        prop_assert_eq!(working_area.size, output_size(&output).to_i32_round());
    }
}
