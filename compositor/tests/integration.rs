//! Integration tests for EWM compositor
//!
//! These tests verify compositor behavior using the headless backend and test fixture.

use ewm_core::testing::Fixture;
use ewm_core::OutputConfig;

/// Test that output_size accounts for y-offsets
#[test]
fn test_output_size_with_y_offset() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");

    fixture.add_output("Virtual-1", 1920, 1080);

    // Place second output at y=500
    fixture.ewm().output_config.insert(
        "Virtual-2".to_string(),
        OutputConfig {
            position: Some((1920, 500)),
            ..Default::default()
        },
    );
    fixture.add_output("Virtual-2", 1920, 1080);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();
    // Width should be 1920 + 1920 = 3840
    assert_eq!(ewm.output_size.w, 3840);
    // Height should account for y-offset: 500 + 1080 = 1580
    assert_eq!(ewm.output_size.h, 1580);
}

/// Test that working area updates after scale change via apply_output_config
#[test]
fn test_working_area_updates_on_scale_change() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    // Initial working area should be full output
    let wa = fixture.ewm_ref().working_areas.get("Virtual-1").cloned();
    assert!(wa.is_some());
    let wa = wa.unwrap();
    assert_eq!(wa.size.w, 1920);
    assert_eq!(wa.size.h, 1080);

    // Apply scale 2.0 via output config
    fixture.ewm().output_config.insert(
        "Virtual-1".to_string(),
        OutputConfig {
            scale: Some(2.0),
            ..Default::default()
        },
    );
    fixture.apply_output_config("Virtual-1");
    fixture.dispatch();

    // Working area should now reflect logical dimensions at scale 2.0
    let wa = fixture
        .ewm_ref()
        .working_areas
        .get("Virtual-1")
        .cloned()
        .unwrap();
    assert_eq!(wa.size.w, 960);
    assert_eq!(wa.size.h, 540);
}

/// Test that scale is rounded to nearest N/120 representable value
#[test]
fn test_scale_rounded_to_representable() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    // Configure with non-representable scale 1.3333
    fixture.ewm().output_config.insert(
        "Virtual-1".to_string(),
        OutputConfig {
            scale: Some(1.3333),
            ..Default::default()
        },
    );
    fixture.apply_output_config("Virtual-1");

    // Read back the effective scale from the output
    let output = fixture
        .ewm_ref()
        .space
        .outputs()
        .find(|o| o.name() == "Virtual-1")
        .unwrap()
        .clone();
    let effective = output.current_scale().fractional_scale();
    // 1.3333 should round to 160/120 = 1.33333...
    assert!((effective - 160.0 / 120.0).abs() < 1e-10);
}
