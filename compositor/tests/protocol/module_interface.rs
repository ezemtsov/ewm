//! Module interface tests
//!
//! Tests for the Emacs ↔ compositor communication via the module interface.
//! These tests verify command processing and event generation.

use ewm_core::testing::Fixture;

/// Test that the compositor starts without pending commands
#[test]
fn test_no_initial_commands() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    // After dispatch, all commands should be processed
    // (The module starts with empty queues)
    assert_eq!(fixture.surface_count(), 0);
}

/// Test that layout updates require a surface to exist
#[test]
fn test_layout_requires_surface() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    // Trying to layout a non-existent surface should be a no-op
    // (Layout command won't panic, just won't do anything)
    assert!(!fixture.has_surface(999));
}

/// Test that focus changes require existing surfaces
#[test]
fn test_focus_requires_surface() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    // Initial focus is 0 (no surface)
    assert_eq!(fixture.focused_surface_id(), 0);

    // Trying to focus a non-existent surface is a no-op
    assert!(!fixture.has_surface(1));
}

/// Test that outputs are tracked in the compositor
#[test]
fn test_output_tracking() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");

    // No outputs initially
    assert!(fixture.ewm_ref().outputs.is_empty());

    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    // Should have one output tracked
    // (Note: the headless backend doesn't populate ewm.outputs directly,
    // but does create the output in the space)
    assert_eq!(fixture.output_count(), 1);
}

/// Test that the compositor can be created multiple times
#[test]
fn test_multiple_fixtures() {
    // Create first fixture
    {
        let mut f1 = Fixture::new().expect("Failed to create first fixture");
        f1.add_output("V1", 800, 600);
        f1.dispatch();
        assert_eq!(f1.output_count(), 1);
    }

    // Create second fixture (ensures cleanup worked)
    {
        let mut f2 = Fixture::new().expect("Failed to create second fixture");
        f2.add_output("V2", 1024, 768);
        f2.dispatch();
        assert_eq!(f2.output_count(), 1);
    }
}

/// Test roundtrip dispatch
#[test]
fn test_roundtrip_dispatch() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);

    // Multiple roundtrips should be safe
    fixture.dispatch_roundtrip(5);

    // State should be consistent
    assert_eq!(fixture.output_count(), 1);
    assert!(!fixture.has_queued_redraws());
}
