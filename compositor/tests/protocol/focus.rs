//! Focus tracking tests
//!
//! Tests for keyboard focus management, click-to-focus, and focus state transitions.

use ewm_core::testing::Fixture;

/// Test initial focus state
#[test]
fn test_initial_focus_state() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    // Initially, no surface should have focus
    assert_eq!(fixture.focused_surface_id(), 0);

    // Keyboard focus should be None
    assert!(fixture.ewm_ref().keyboard_focus.is_none());
}

/// Test that Emacs surfaces are tracked separately
#[test]
fn test_emacs_surface_tracking() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    // Initially no Emacs surfaces
    assert!(fixture.ewm_ref().emacs_surfaces.is_empty());

    // Emacs PID should be unset
    assert!(fixture.ewm_ref().emacs_pid.is_none());
}

/// Test that pointer location is tracked
#[test]
fn test_pointer_location_tracking() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();

    // Pointer starts at origin
    assert_eq!(ewm.pointer_location, (0.0, 0.0));
}

/// Test that output state includes redraw tracking
#[test]
fn test_output_redraw_state() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);

    // Output should have queued redraw initially
    assert!(fixture.has_queued_redraws());

    fixture.dispatch();

    // After dispatch, should be idle
    assert!(!fixture.has_queued_redraws());
}

/// Test focus is on Emacs check when no surfaces
#[test]
fn test_is_focus_on_emacs_empty() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    // With no surfaces, focus is not on Emacs
    assert!(!fixture.ewm_ref().is_focus_on_emacs());
}

/// Test text input interception state
#[test]
fn test_text_input_intercept_state() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();

    // Text input interception should be off by default
    assert!(!ewm.text_input_intercept);
    assert!(!ewm.text_input_active);
}

/// Test that the XKB layout state is initialized
#[test]
fn test_xkb_layout_initial_state() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();

    // Should have default US layout
    assert!(!ewm.xkb_layout_names.is_empty());
    assert_eq!(ewm.xkb_current_layout, 0);
}
