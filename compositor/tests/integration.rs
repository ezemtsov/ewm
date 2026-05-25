//! Integration tests for EWM compositor
//!
//! These tests verify compositor behavior using the headless backend and test fixture.

use ewm_core::backend::{closest_representable_scale, int_to_transform, transform_to_int};
use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient};
use ewm_core::utils::output_size;
use ewm_core::{LayoutEntry, OutputConfig, ViewId};
use proptest::prelude::*;
use smithay::utils::{Logical, Point, Size};
use wayland_client::Proxy as _;

const CASES: u32 = 24;

fn nz(n: u64) -> ViewId {
    std::num::NonZeroU64::new(n).unwrap()
}

fn frame_with_entry(frame_id: u64, view_id: ViewId, surface_id: u64) -> Frame {
    frame_with_entry_at(frame_id, view_id, surface_id, 0, 0)
}

fn frame_with_entry_at(frame_id: u64, view_id: ViewId, surface_id: u64, x: i32, y: i32) -> Frame {
    Frame {
        name: String::new(),
        surface_id: frame_id,
        view_id: nz(frame_id + 1_000),
        width: 1280.0,
        surfaces: vec![LayoutEntry {
            view_id,
            surface_id,
            x,
            y,
            w: 400,
            h: 300,
            primary: false,
            fullscreen: false,
        }],
        selected_view_id: Some(view_id),
        move_anim: None,
    }
}

fn pointer_focus_surface_id(fixture: &Fixture) -> Option<u64> {
    let ewm = fixture.ewm_ref();
    ewm.pointer_focus
        .as_ref()
        .and_then(|(surface, _)| ewm.surface_id(surface))
}

fn has_surface_enter(
    client: &TestClient,
    events: &[ClientEvent],
    client_surface: &wayland_client::protocol::wl_surface::WlSurface,
    output_name: &str,
) -> bool {
    let surface_id = client_surface.id().protocol_id();
    events.iter().any(|event| match event {
        ClientEvent::SurfaceEnter { surface, output } => {
            surface.id().protocol_id() == surface_id
                && client.output_name(output.id().protocol_id()) == Some(output_name)
        }
        _ => false,
    })
}

fn has_surface_leave(
    client: &TestClient,
    events: &[ClientEvent],
    client_surface: &wayland_client::protocol::wl_surface::WlSurface,
    output_name: &str,
) -> bool {
    let surface_id = client_surface.id().protocol_id();
    events.iter().any(|event| match event {
        ClientEvent::SurfaceLeave { surface, output } => {
            surface.id().protocol_id() == surface_id
                && client.output_name(output.id().protocol_id()) == Some(output_name)
        }
        _ => false,
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: CASES,
        ..ProptestConfig::default()
    })]

    #[test]
    fn initial_output_config_matches_output_state_and_report(
        width in 640i32..=3840,
        height in 480i32..=2160,
        scale_milli in 750i32..=3_000,
        transform_idx in 0i32..=7,
    ) {
        let scale = f64::from(scale_milli) / 1_000.0;
        let transform = int_to_transform(transform_idx);
        let mut fixture = Fixture::new().expect("Failed to create fixture");

        fixture.ewm().output_config.insert(
            "Virtual-1".to_string(),
            OutputConfig {
                scale: Some(scale),
                transform: Some(transform),
                ..Default::default()
            },
        );
        fixture.add_output("Virtual-1", width, height);

        let output = fixture
            .ewm_ref()
            .space
            .outputs()
            .find(|o| o.name() == "Virtual-1")
            .unwrap()
            .clone();
        let expected_scale = closest_representable_scale(scale);

        prop_assert!((output.current_scale().fractional_scale() - expected_scale).abs() < 1e-10);
        prop_assert_eq!(output.current_transform(), transform);

        let info = fixture
            .output_info_list()
            .into_iter()
            .find(|output| output.name == "Virtual-1")
            .unwrap();
        prop_assert!((info.scale - expected_scale).abs() < 1e-10);
        prop_assert_eq!(info.transform, transform_to_int(transform));
    }

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

#[test]
fn output_layout_command_refreshes_pointer_focus() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);

    let mut client = fixture.new_client().expect("Failed to create client");
    fixture.roundtrip(&mut client);
    let toplevel = fixture.open_toplevel(
        &mut client,
        "pointer-layout-refresh",
        Size::<i32, Logical>::from((400, 300)),
    );
    let surface_id = fixture
        .find_surface_id(&toplevel.surface)
        .expect("server did not assign a surface id");
    let view_id = nz(1);

    fixture.apply_output_layout_command(
        "Virtual-1",
        vec![frame_with_entry_at(100, view_id, surface_id, 0, 0)],
    );
    fixture.warp_pointer(10.0, 10.0);
    assert_eq!(pointer_focus_surface_id(&fixture), Some(surface_id));

    fixture.apply_output_layout_command(
        "Virtual-1",
        vec![frame_with_entry_at(100, view_id, surface_id, 600, 0)],
    );
    assert!(fixture
        .ewm_ref()
        .surface_under_point(Point::from((10.0, 10.0)))
        .is_none());
    assert_eq!(pointer_focus_surface_id(&fixture), None);
}

#[test]
fn disabling_and_reenabling_headless_output_updates_protocol_state() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);
    fixture.dispatch();

    fixture.ewm().output_management_state.output_heads_changed = false;
    fixture.ewm().output_config.insert(
        "Virtual-1".to_string(),
        OutputConfig {
            enabled: false,
            ..Default::default()
        },
    );
    fixture.apply_output_config("Virtual-1");

    assert!(
        fixture
            .ewm_ref()
            .output_management_state
            .output_heads_changed
    );
    assert!(!fixture
        .ewm_ref()
        .space
        .outputs()
        .any(|output| output.name() == "Virtual-1"));
    let output_infos = fixture.output_info_list();
    assert!(!fixture.ewm_ref().build_output_head_states(&output_infos)["Virtual-1"].enabled);

    fixture.ewm().output_management_state.output_heads_changed = false;
    fixture.ewm().output_config.insert(
        "Virtual-1".to_string(),
        OutputConfig {
            enabled: true,
            ..Default::default()
        },
    );
    fixture.apply_output_config("Virtual-1");

    assert!(
        fixture
            .ewm_ref()
            .output_management_state
            .output_heads_changed
    );
    assert!(fixture
        .ewm_ref()
        .space
        .outputs()
        .any(|output| output.name() == "Virtual-1"));
    let output_infos = fixture.output_info_list();
    let heads = fixture.ewm_ref().build_output_head_states(&output_infos);
    let head = &heads["Virtual-1"];
    assert!(head.enabled);
    assert!(head.current_mode.is_some());
}

#[test]
fn headless_output_respects_initial_disabled_config() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.ewm().output_config.insert(
        "Virtual-1".to_string(),
        OutputConfig {
            enabled: false,
            ..Default::default()
        },
    );

    fixture.add_output("Virtual-1", 1280, 720);
    fixture.dispatch();

    assert!(!fixture
        .ewm_ref()
        .space
        .outputs()
        .any(|output| output.name() == "Virtual-1"));
    let output_infos = fixture.output_info_list();
    assert!(!fixture.ewm_ref().build_output_head_states(&output_infos)["Virtual-1"].enabled);
}

#[test]
fn disabling_focused_output_moves_focus_to_mapped_output() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);
    fixture.add_output("Virtual-2", 1280, 720);
    fixture
        .ewm()
        .apply_output_layout("Virtual-1", vec![Frame::new(100)]);
    fixture
        .ewm()
        .apply_output_layout("Virtual-2", vec![Frame::new(200)]);
    fixture.ewm().set_focus_frame(100, "test", false);

    fixture.ewm().output_config.insert(
        "Virtual-1".to_string(),
        OutputConfig {
            enabled: false,
            ..Default::default()
        },
    );
    fixture.apply_output_config("Virtual-1");

    assert_eq!(fixture.ewm_ref().focused_surface_id(), 200);
    assert_eq!(fixture.ewm_ref().emacs_frame_output(200), Some("Virtual-2"));
}

#[test]
fn focusing_view_on_disabled_output_is_ignored() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);
    fixture.add_output("Virtual-2", 1280, 720);
    let mut disabled_frame = frame_with_entry(100, nz(1), 1);
    disabled_frame.surfaces[0].fullscreen = true;
    fixture
        .ewm()
        .apply_output_layout("Virtual-1", vec![disabled_frame]);
    fixture
        .ewm()
        .apply_output_layout("Virtual-2", vec![Frame::new(200)]);
    fixture.ewm().set_focus_frame(200, "test", false);

    fixture.ewm().output_config.insert(
        "Virtual-1".to_string(),
        OutputConfig {
            enabled: false,
            ..Default::default()
        },
    );
    fixture.apply_output_config("Virtual-1");

    assert!(!fixture.ewm_ref().is_surface_fullscreen(1));
    fixture.ewm().set_focus_view(nz(1), "test", false);

    assert_eq!(fixture.ewm_ref().focused_surface_id(), 200);
    assert_eq!(fixture.ewm_ref().focused_view_id, None);
    assert_eq!(fixture.ewm_ref().frame_with_view_id(nz(1_100)), None);
}

#[test]
fn disabling_output_reassigns_primary_entry_to_mapped_view() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);
    fixture.add_output("Virtual-2", 1280, 720);

    let surface_id = 1;
    let mut disabled_frame = frame_with_entry(100, nz(1), surface_id);
    disabled_frame.surfaces[0].w = 1000;
    disabled_frame.surfaces[0].h = 1000;
    fixture
        .ewm()
        .apply_output_layout("Virtual-1", vec![disabled_frame]);
    fixture
        .ewm()
        .apply_output_layout("Virtual-2", vec![frame_with_entry(200, nz(2), surface_id)]);

    fixture.ewm().output_config.insert(
        "Virtual-1".to_string(),
        OutputConfig {
            enabled: false,
            ..Default::default()
        },
    );
    fixture.apply_output_config("Virtual-1");

    let live_strip = fixture
        .ewm_ref()
        .output_strips
        .get("Virtual-2")
        .expect("Virtual-2 strip exists");
    assert!(live_strip
        .surfaces()
        .any(|entry| entry.surface_id == surface_id && entry.primary));
}

#[test]
fn disabling_and_reenabling_output_updates_surface_membership() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);

    let mut client = fixture.new_client().expect("Failed to create client");
    fixture.roundtrip(&mut client);
    let toplevel = fixture.open_toplevel(
        &mut client,
        "output-membership",
        Size::<i32, Logical>::from((400, 300)),
    );
    let surface_id = fixture
        .find_surface_id(&toplevel.surface)
        .expect("server did not assign a surface id");
    let view_id = nz(1);

    fixture.ewm().apply_output_layout(
        "Virtual-1",
        vec![frame_with_entry(100, view_id, surface_id)],
    );
    fixture.roundtrip(&mut client);

    assert!(fixture
        .ewm_ref()
        .surface_outputs
        .get(&surface_id)
        .is_some_and(|outputs| outputs.contains("Virtual-1")));
    let events = client.drain_events();
    assert!(has_surface_enter(
        &client,
        &events,
        &toplevel.surface,
        "Virtual-1",
    ));

    fixture.ewm().output_config.insert(
        "Virtual-1".to_string(),
        OutputConfig {
            enabled: false,
            ..Default::default()
        },
    );
    fixture.apply_output_config("Virtual-1");
    fixture.roundtrip(&mut client);

    assert!(!fixture
        .ewm_ref()
        .surface_outputs
        .get(&surface_id)
        .is_some_and(|outputs| outputs.contains("Virtual-1")));
    let events = client.drain_events();
    assert!(has_surface_leave(
        &client,
        &events,
        &toplevel.surface,
        "Virtual-1",
    ));

    fixture.ewm().output_config.insert(
        "Virtual-1".to_string(),
        OutputConfig {
            enabled: true,
            ..Default::default()
        },
    );
    fixture.apply_output_config("Virtual-1");
    fixture.roundtrip(&mut client);

    assert!(fixture
        .ewm_ref()
        .surface_outputs
        .get(&surface_id)
        .is_some_and(|outputs| outputs.contains("Virtual-1")));
    let events = client.drain_events();
    assert!(has_surface_enter(
        &client,
        &events,
        &toplevel.surface,
        "Virtual-1",
    ));
}
