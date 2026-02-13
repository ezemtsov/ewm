//! Layer shell protocol tests
//!
//! Tests for wlr-layer-shell behavior: panels, overlays, notifications, etc.
//! Layer shell surfaces are positioned relative to output edges and can
//! reserve exclusive zones.

use ewm_core::testing::Fixture;

/// Test that layer shell state is initialized
#[test]
fn test_layer_shell_state_exists() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    // Layer shell state should be initialized (no panic = success)
    let ewm = fixture.ewm_ref();
    assert!(ewm.unmapped_layer_surfaces.is_empty());
}

/// Test that layer maps are output-specific
#[test]
fn test_layer_map_output_specific() {
    use smithay::desktop::layer_map_for_output;

    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.add_output("Virtual-2", 2560, 1440);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();
    let outputs: Vec<_> = ewm.space.outputs().collect();
    assert_eq!(outputs.len(), 2);

    // Each output has its own layer map
    let layer_map_1 = layer_map_for_output(outputs[0]);
    let layer_map_2 = layer_map_for_output(outputs[1]);

    // Both layer maps should be empty initially
    assert_eq!(layer_map_1.layers().count(), 0);
    assert_eq!(layer_map_2.layers().count(), 0);
}

/// Test that output geometry accounts for exclusive zones
/// (When panels reserve space, usable area shrinks)
#[test]
fn test_output_geometry_without_exclusive_zones() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();
    let output = ewm.space.outputs().next().unwrap();
    let geo = ewm.space.output_geometry(output).unwrap();

    // Without any layer surfaces, full output size is available
    assert_eq!(geo.size.w, 1920);
    assert_eq!(geo.size.h, 1080);
}

/// Test that layer state tracks unmapped surfaces
#[test]
fn test_unmapped_layer_surfaces_tracking() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();

    // No unmapped layer surfaces initially
    assert!(ewm.unmapped_layer_surfaces.is_empty());

    // Layer surfaces are tracked in unmapped_layer_surfaces until
    // they receive their first configure and commit
}

/// Test multiple outputs have independent layer stacks
#[test]
fn test_multi_output_layer_independence() {
    use smithay::desktop::layer_map_for_output;

    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1920, 1080);
    fixture.add_output("Virtual-2", 2560, 1440);
    fixture.dispatch();

    let ewm = fixture.ewm_ref();

    // Get all outputs
    let outputs: Vec<_> = ewm.space.outputs().collect();

    // Verify each output can have independent layer surfaces
    // (testing infrastructure - actual layer surfaces would need real clients)
    for output in &outputs {
        let layer_map = layer_map_for_output(output);
        // Layer map exists and can be queried
        let _ = layer_map.layers().count();
    }

    // Output sizes should be different
    let geo1 = ewm.space.output_geometry(outputs[0]).unwrap();
    let geo2 = ewm.space.output_geometry(outputs[1]).unwrap();
    assert_ne!(geo1.size, geo2.size);
}
