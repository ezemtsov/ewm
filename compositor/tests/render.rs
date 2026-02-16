//! Render pipeline tests
//!
//! Tests for per-output rendering behavior and element collection.
//! Note: These tests verify the logical render state, not actual GPU rendering
//! which requires a GlesRenderer.

use ewm_core::testing::Fixture;

/// Test that layer maps are created for each output
#[test]
fn test_layer_map_per_output() {
    use smithay::desktop::layer_map_for_output;

    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.add_output("Virtual-2", 2560, 1440);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();

    // Each output should have its own layer map
    for output in ewm.space.outputs() {
        let layer_map = layer_map_for_output(output);
        // Layer map should exist (even if empty)
        assert!(layer_map.layers().count() == 0, "No layers initially");
    }
}

/// Test that outputs are positioned horizontally
#[test]
fn test_output_horizontal_layout() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");

    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();
    let first_output = ewm.space.outputs().next().unwrap();
    let first_geo = ewm.space.output_geometry(first_output).unwrap();
    let first_x = first_geo.loc.x;
    assert_eq!(first_geo.loc.y, 0, "First output should be at y=0");

    let _ = ewm; // Release borrow before adding second output
    fixture.add_output("Virtual-2", 2560, 1440);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();
    let outputs: Vec<_> = ewm.space.outputs().collect();
    assert_eq!(outputs.len(), 2);

    // Second output should be positioned after first
    let second_geo = ewm.space.output_geometry(outputs[1]).unwrap();
    assert!(
        second_geo.loc.x >= first_x + 1920,
        "Second output should be after first (got x={}, expected >= {})",
        second_geo.loc.x,
        first_x + 1920
    );
    assert_eq!(
        second_geo.loc.y, 0,
        "Outputs should be horizontally aligned"
    );
}

/// Test that pointer location is tracked globally
#[test]
fn test_pointer_location_tracking() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.add_output("Virtual-2", 1920, 1080);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();

    // Pointer starts at origin
    assert_eq!(ewm.pointer_location, (0.0, 0.0));

    // Total output size should span both outputs
    assert!(ewm.output_size.w >= 3840, "Should span both outputs");
}

/// Test that space elements are empty initially
#[test]
fn test_initial_space_empty() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();

    // No windows should be mapped initially
    assert_eq!(ewm.space.elements().count(), 0);
    assert!(ewm.id_windows.is_empty());
    assert!(ewm.window_ids.is_empty());
}

/// Test that output layouts map is empty initially
#[test]
fn test_initial_views_empty() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();

    // No output layouts initially
    assert!(ewm.output_layouts.is_empty());
}

/// Test that damage tracking is initialized per output
#[test]
fn test_damage_tracker_per_output() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.add_output("Virtual-2", 2560, 1440);
    fixture.dispatch();

    // Each output should render once on first dispatch
    assert_eq!(fixture.render_count("Virtual-1"), 1);
    assert_eq!(fixture.render_count("Virtual-2"), 1);

    // Queue redraw for all outputs
    fixture.queue_redraw_all();
    fixture.dispatch();

    // Both should have rendered again
    assert_eq!(fixture.render_count("Virtual-1"), 2);
    assert_eq!(fixture.render_count("Virtual-2"), 2);
}
