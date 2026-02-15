//! XDG Shell protocol tests
//!
//! Tests for xdg_toplevel lifecycle, configure sequences, and surface management.

use ewm_core::testing::Fixture;
use insta::assert_snapshot;

/// Test that the fixture initializes correctly with an output
#[test]
fn test_initial_state() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    // Compositor should start with no surfaces
    assert_eq!(fixture.surface_count(), 0);
    assert_eq!(fixture.focused_surface_id(), 0);
}

/// Test that output size is calculated correctly
#[test]
fn test_output_geometry() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");

    // Add first output
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();
    let size = ewm.output_size;

    // Size should be at least the output size
    assert!(size.w >= 1920);
    assert!(size.h >= 1080);
}

/// Test multiple outputs are positioned correctly
#[test]
fn test_multi_output_layout() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");

    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();
    let first_width = fixture.ewm_ref().output_size.w;

    fixture.add_output("Virtual-2", 2560, 1440);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();
    // Second output should extend the total width
    assert!(ewm.output_size.w > first_width);
    // Height should be the max of both
    assert!(ewm.output_size.h >= 1440);
}

/// Test that output removal updates geometry
#[test]
fn test_output_removal_updates_geometry() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");

    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.add_output("Virtual-2", 1920, 1080);
    fixture.dispatch();

    let size_with_two = fixture.ewm_ref().output_size.w;

    fixture.remove_output("Virtual-2");
    fixture.dispatch();

    // Width should decrease after removal
    assert!(fixture.ewm_ref().output_size.w < size_with_two);
}

/// Test that the fixture tracks render counts
#[test]
fn test_render_tracking() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);

    // Initial dispatch renders the output
    fixture.dispatch();
    assert_eq!(fixture.render_count("Virtual-1"), 1);

    // Queue another redraw
    fixture.queue_redraw_all();
    fixture.dispatch();
    assert_eq!(fixture.render_count("Virtual-1"), 2);

    // Without queuing, no additional renders
    fixture.dispatch();
    assert_eq!(fixture.render_count("Virtual-1"), 2);
}

/// Test client creation and tracking
#[test]
fn test_client_creation() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);

    let client1 = fixture.add_client();
    let client2 = fixture.add_client();

    // Clients should have unique IDs
    assert_ne!(client1, client2);

    // Clients should be retrievable
    assert!(fixture.get_client(client1).is_some());
    assert!(fixture.get_client(client2).is_some());
}

/// Snapshot test for output state
#[test]
fn test_output_state_snapshot() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.add_output("Virtual-2", 2560, 1440);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();
    let state = format!(
        "output_count: {}\noutput_size: ({}, {})\nfocused_surface: {}\nsurface_count: {}",
        fixture.output_count(),
        ewm.output_size.w,
        ewm.output_size.h,
        ewm.focused_surface_id,
        fixture.surface_count()
    );

    assert_snapshot!(state);
}
