//! Integration tests for EWM compositor
//!
//! These tests verify compositor behavior using the headless backend and test fixture.

mod protocol;

use ewm_core::testing::Fixture;
use ewm_core::OutputConfig;

/// Test that the fixture can be created and basic operations work
#[test]
fn test_basic_fixture() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);

    // Dispatch should succeed
    fixture.dispatch();

    assert_eq!(fixture.output_count(), 1);
    assert_eq!(fixture.render_count("Virtual-1"), 1);
}

/// Test that multiple outputs can be added and managed
#[test]
fn test_multi_output() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");

    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.add_output("Virtual-2", 2560, 1440);

    assert_eq!(fixture.output_count(), 2);

    // Both outputs should have queued redraws initially
    assert!(fixture.has_queued_redraws());

    // Dispatch to process redraws
    fixture.dispatch();

    // Both should have rendered once
    assert_eq!(fixture.render_count("Virtual-1"), 1);
    assert_eq!(fixture.render_count("Virtual-2"), 1);
}

/// Test that removing an output works correctly
#[test]
fn test_output_removal() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");

    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.add_output("Virtual-2", 1920, 1080);
    fixture.dispatch();

    assert_eq!(fixture.output_count(), 2);

    fixture.remove_output("Virtual-1");
    assert_eq!(fixture.output_count(), 1);
    assert_eq!(fixture.render_count("Virtual-2"), 1);
}

/// Test that queueing redraw triggers another render
#[test]
fn test_redraw_queue() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);

    // Initial dispatch
    fixture.dispatch();
    assert_eq!(fixture.render_count("Virtual-1"), 1);

    // Queue another redraw
    fixture.queue_redraw_all();
    assert!(fixture.has_queued_redraws());

    // Dispatch again
    fixture.dispatch();
    assert_eq!(fixture.render_count("Virtual-1"), 2);
}

/// Test that output_size accounts for y-offsets (step 4)
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
    assert_eq!(ewm.output_size.0, 3840);
    // Height should account for y-offset: 500 + 1080 = 1580
    assert_eq!(ewm.output_size.1, 1580);
}

/// Test that working area updates after scale change via apply_output_config (step 6)
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
