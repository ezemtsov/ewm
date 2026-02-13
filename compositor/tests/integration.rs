//! Integration tests for EWM compositor
//!
//! These tests verify compositor behavior using the headless backend and test fixture.

mod protocol;

use ewm_core::testing::Fixture;

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
