//! Integration tests for EWM compositor
//!
//! These tests verify compositor behavior using the headless backend and test fixture.

use ewm_core::backend::{closest_representable_scale, int_to_transform, transform_to_int};
use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient};
use ewm_core::utils::output_size;
use ewm_core::{Event, LayoutEntry, LayoutEntryId, OutputConfig};
use proptest::prelude::*;
use smithay::utils::{Logical, Point, Size};
use wayland_client::Proxy as _;

const CASES: u32 = 24;

fn nz(n: u64) -> LayoutEntryId {
    std::num::NonZeroU64::new(n).unwrap()
}

fn frame_with_entry(frame_id: u64, entry_id: LayoutEntryId, surface_id: u64) -> Frame {
    frame_with_entry_at(frame_id, entry_id, surface_id, 0, 0)
}

fn frame_with_entry_at(
    frame_id: u64,
    entry_id: LayoutEntryId,
    surface_id: u64,
    x: i32,
    y: i32,
) -> Frame {
    Frame {
        name: String::new(),
        workspace_name: None,
        surface_id: frame_id,
        focus_id: nz(frame_id + 1_000),
        width: 1280.0,
        height: 0.0,
        entries: vec![LayoutEntry::surface(
            entry_id,
            "entry".to_string(),
            surface_id,
            x,
            y,
            400,
            300,
        )],
        fullscreen: None,
        selected_entry_id: Some(entry_id),
        move_anim: None,
        floating_pos: None,
    }
}

fn entry_fullscreen(fixture: &Fixture, output: &str, entry_id: LayoutEntryId) -> Option<bool> {
    fixture
        .ewm_ref()
        .frame_set
        .mapped_strip(output)?
        .frames
        .iter()
        .find(|frame| frame.entries.iter().any(|entry| entry.id == entry_id))
        .map(|frame| frame.entry_fullscreen(entry_id))
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

fn surface_preferred_scale(fixture: &Fixture, surface_id: u64) -> Option<f64> {
    use smithay::wayland::compositor::with_states;
    use smithay::wayland::fractional_scale::with_fractional_scale;
    use smithay::wayland::seat::WaylandFocus as _;

    let surface = fixture
        .ewm_ref()
        .id_windows
        .get(&surface_id)?
        .wl_surface()?
        .into_owned();
    with_states(&surface, |data| {
        with_fractional_scale(data, |scale| scale.preferred_scale())
    })
}

fn assert_preferred_scale(fixture: &Fixture, surface_id: u64, expected: f64) {
    let actual = surface_preferred_scale(fixture, surface_id).expect("surface has preferred scale");
    assert!(
        (actual - expected).abs() < 1e-10,
        "surface {surface_id} preferred scale: expected {expected}, got {actual}"
    );
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
        .find_surface_id(&client, &toplevel.surface)
        .expect("server did not assign a surface id");
    let entry_id = nz(1);

    fixture.apply_output_layout_command(
        "Virtual-1",
        vec![frame_with_entry_at(100, entry_id, surface_id, 0, 0)],
    );
    fixture.warp_pointer(10.0, 10.0);
    assert_eq!(fixture.pointer_focus_surface_id(), Some(surface_id));

    fixture.apply_output_layout_command(
        "Virtual-1",
        vec![frame_with_entry_at(100, entry_id, surface_id, 600, 0)],
    );
    assert!(
        fixture
            .ewm_ref()
            .surface_under_point(Point::from((10.0, 10.0)))
            .is_none()
    );
    assert_eq!(fixture.pointer_focus_surface_id(), None);
}

#[test]
fn frame_focus_switch_refreshes_stationary_pointer_after_transition() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);

    let mut client = fixture.new_client().expect("Failed to create client");
    fixture.roundtrip(&mut client);
    let first = fixture.open_toplevel(
        &mut client,
        "pointer-frame-refresh-first",
        Size::<i32, Logical>::from((400, 300)),
    );
    let first_surface_id = fixture
        .find_surface_id(&client, &first.surface)
        .expect("server did not assign first surface id");
    let second = fixture.open_toplevel(
        &mut client,
        "pointer-frame-refresh-second",
        Size::<i32, Logical>::from((400, 300)),
    );
    let second_surface_id = fixture
        .find_surface_id(&client, &second.surface)
        .expect("server did not assign second surface id");

    fixture.apply_output_layout_command(
        "Virtual-1",
        vec![
            frame_with_entry(100, nz(1), first_surface_id),
            frame_with_entry(200, nz(2), second_surface_id),
        ],
    );
    fixture.warp_pointer(10.0, 10.0);
    assert_eq!(fixture.pointer_focus_surface_id(), Some(first_surface_id));

    fixture.ewm().animations_clock.set_complete_instantly(true);
    fixture.ewm().set_focus_frame(200, "test", false);
    assert_eq!(fixture.pointer_focus_surface_id(), Some(first_surface_id));

    fixture.dispatch();

    assert_eq!(fixture.ewm_ref().focused_surface_id(), second_surface_id);
    assert_eq!(fixture.pointer_focus_surface_id(), Some(second_surface_id));
}

#[test]
fn strip_swipe_focuses_the_snapped_frame_without_moving_the_pointer() {
    let mut fixture = Fixture::new().unwrap();
    fixture.add_output("Virtual-1", 1280, 720);
    fixture.ewm().animations_clock.set_complete_instantly(true);
    fixture.apply_output_layout_command(
        "Virtual-1",
        vec![
            frame_with_entry(10, nz(1), 100),
            frame_with_entry(11, nz(2), 101),
        ],
    );
    fixture.dispatch_roundtrip(2);
    let neighbour_focus_id = fixture
        .ewm_ref()
        .frame_set
        .mapped_strip("Virtual-1")
        .unwrap()
        .frames[1]
        .focus_id;
    fixture.pointer_motion_to(640.0, 360.0);
    fixture.enable_event_capture();

    {
        let strip = fixture
            .ewm()
            .frame_set
            .mapped_strip_mut("Virtual-1")
            .unwrap();
        strip.gesture_begin();
        strip.gesture_update(700.0, std::time::Duration::ZERO, 1280.0);
        // A late zero delta drains the velocity history so the snap is decided by position.
        strip.gesture_update(0.0, std::time::Duration::from_millis(200), 1280.0);
    }
    fixture.ewm().strip_gesture_end("Virtual-1", 1280.0);
    fixture.dispatch_roundtrip(2);

    let strip = fixture
        .ewm_ref()
        .frame_set
        .mapped_strip("Virtual-1")
        .unwrap();
    assert_eq!(strip.active_idx(), 1);
    assert_eq!(fixture.ewm_ref().pointer_location(), (640.0, 360.0));
    // Pointer focus, so Emacs anchors mouse-follows-focus instead of warping.
    assert!(fixture.drain_events().iter().any(|event| matches!(
        event,
        Event::Focus { focus_id, x: Some(x), y: Some(y), pointer: true }
            if *focus_id == neighbour_focus_id && *x == 640.0 && *y == 360.0
    )));

    // A swipe past the strip end snaps back to the same frame and changes no focus.
    {
        let strip = fixture
            .ewm()
            .frame_set
            .mapped_strip_mut("Virtual-1")
            .unwrap();
        strip.gesture_begin();
        strip.gesture_update(700.0, std::time::Duration::ZERO, 1280.0);
        strip.gesture_update(0.0, std::time::Duration::from_millis(200), 1280.0);
    }
    fixture.ewm().strip_gesture_end("Virtual-1", 1280.0);
    fixture.dispatch_roundtrip(2);

    assert_eq!(
        fixture
            .ewm_ref()
            .frame_set
            .mapped_strip("Virtual-1")
            .unwrap()
            .active_idx(),
        1
    );
    assert!(
        !fixture
            .drain_events()
            .iter()
            .any(|event| matches!(event, Event::Focus { .. }))
    );
}

#[test]
fn strip_swipe_with_the_pointer_on_another_output_warps_like_a_key() {
    let mut fixture = Fixture::new().unwrap();
    fixture.add_output("Virtual-1", 1280, 720);
    fixture.add_output("Virtual-2", 1280, 720);
    fixture.ewm().animations_clock.set_complete_instantly(true);
    fixture.apply_output_layout_command(
        "Virtual-1",
        vec![
            frame_with_entry(10, nz(1), 100),
            frame_with_entry(11, nz(2), 101),
        ],
    );
    fixture.dispatch_roundtrip(2);
    let other = fixture
        .ewm_ref()
        .space
        .outputs()
        .find(|output| output.name() == "Virtual-2")
        .cloned()
        .unwrap();
    let geo = fixture.ewm_ref().space.output_geometry(&other).unwrap();
    fixture.pointer_motion_to(f64::from(geo.loc.x) + 100.0, f64::from(geo.loc.y) + 100.0);
    fixture.enable_event_capture();

    {
        let strip = fixture
            .ewm()
            .frame_set
            .mapped_strip_mut("Virtual-1")
            .unwrap();
        strip.gesture_begin();
        strip.gesture_update(700.0, std::time::Duration::ZERO, 1280.0);
        strip.gesture_update(0.0, std::time::Duration::from_millis(200), 1280.0);
    }
    fixture.ewm().strip_gesture_end("Virtual-1", 1280.0);
    fixture.dispatch_roundtrip(2);

    // Keyboard-style focus, so mouse-follows-focus brings the pointer over.
    assert!(
        fixture
            .drain_events()
            .iter()
            .any(|event| matches!(event, Event::Focus { pointer: false, .. }))
    );
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
    assert!(
        !fixture
            .ewm_ref()
            .space
            .outputs()
            .any(|output| output.name() == "Virtual-1")
    );
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
    assert!(
        fixture
            .ewm_ref()
            .space
            .outputs()
            .any(|output| output.name() == "Virtual-1")
    );
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

    assert!(
        !fixture
            .ewm_ref()
            .space
            .outputs()
            .any(|output| output.name() == "Virtual-1")
    );
    let output_infos = fixture.output_info_list();
    assert!(!fixture.ewm_ref().build_output_head_states(&output_infos)["Virtual-1"].enabled);
}

#[test]
fn disabling_focused_output_migrates_focused_frame_to_mapped_output() {
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

    assert_eq!(fixture.ewm_ref().focused_surface_id(), 100);
    assert_eq!(fixture.ewm_ref().emacs_frame_output(100), Some("Virtual-2"));
}

#[test]
fn disabling_output_migrates_entries_to_mapped_output() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);
    fixture.add_output("Virtual-2", 1280, 720);
    let disabled_frame = frame_with_entry(100, nz(1), 1);
    fixture
        .ewm()
        .apply_output_layout("Virtual-1", vec![disabled_frame]);
    fixture.ewm().set_surface_fullscreen(1, true);
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

    assert!(fixture.ewm_ref().is_surface_fullscreen(1));
    fixture.ewm().set_focus_entry(nz(1), "test", false);

    assert_eq!(fixture.ewm_ref().focused_surface_id(), 1);
    assert_eq!(fixture.ewm_ref().focused_entry_id, Some(nz(1)));
    assert_eq!(fixture.ewm_ref().frame_with_focus_id(nz(1_100)), Some(100));
    assert_eq!(fixture.ewm_ref().emacs_frame_output(100), Some("Virtual-2"));
}

#[test]
fn disabling_output_reassigns_primary_entry_to_mapped_entry() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);
    fixture.add_output("Virtual-2", 1280, 720);

    let surface_id = 1;
    let mut disabled_frame = frame_with_entry(100, nz(1), surface_id);
    disabled_frame.entries[0].w = 1000;
    disabled_frame.entries[0].h = 1000;
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
        .frame_set
        .mapped_strip("Virtual-2")
        .expect("Virtual-2 strip exists");
    assert!(
        live_strip
            .surface_entries()
            .any(|entry| entry.surface_id() == Some(surface_id) && entry.primary())
    );
}

// A physical display, identified by a stable EDID serial. Its connector name
// is re-minted on every (re)connect to model DRM re-enumerating the panel on a
// different port after a replug -- the case the connector-name-keyed home logic
// used to mishandle.
struct Display {
    serial: String,
    frames: Vec<u64>,
    connector: Option<String>,
}

#[derive(Debug, Clone)]
enum HotplugOp {
    Connect(usize),
    Disconnect(usize),
}

fn strip_ids(fixture: &Fixture, output: &str) -> Vec<u64> {
    fixture
        .ewm_ref()
        .frame_set
        .mapped_strip(output)
        .map(|strip| strip.frames.iter().map(|f| f.surface_id).collect())
        .unwrap_or_default()
}

fn frame_is_homeless(fixture: &Fixture, id: u64) -> bool {
    fixture
        .ewm_ref()
        .frame_set
        .homeless_frames()
        .iter()
        .any(|f| f.surface_id == id)
}

/// Invariants that must hold after every hotplug op:
/// - conservation: each frame appears exactly once across all strips + pool;
/// - home-return: a connected display holds all of its own frames, in their canonical relative
///   order, regardless of the connector it came back on;
/// - no-loss: a disconnected display's frames sit on some mapped strip (the fallback) when any
///   output is mapped, else in the homeless pool.
fn assert_hotplug_invariants(fixture: &Fixture, displays: &[Display]) {
    let any_mapped = fixture.ewm_ref().space.outputs().next().is_some();
    let all_frames: Vec<u64> = fixture
        .ewm_ref()
        .frame_set
        .all_frames()
        .map(|f| f.surface_id)
        .collect();

    for display in displays {
        for &id in &display.frames {
            let count = all_frames.iter().filter(|&&f| f == id).count();
            assert_eq!(count, 1, "frame {id} present {count} times, want exactly 1");
        }
        match &display.connector {
            Some(connector) => {
                let own: Vec<u64> = strip_ids(fixture, connector)
                    .into_iter()
                    .filter(|id| display.frames.contains(id))
                    .collect();
                assert_eq!(
                    own, display.frames,
                    "display {} on {connector} must hold its frames {:?} in order, got {own:?}",
                    display.serial, display.frames,
                );
            }
            None => {
                for &id in &display.frames {
                    if any_mapped {
                        assert!(
                            fixture.ewm_ref().emacs_frame_output(id).is_some(),
                            "frame {id} of disconnected display {} was lost",
                            display.serial,
                        );
                    } else {
                        assert!(
                            frame_is_homeless(fixture, id),
                            "frame {id} must be homeless when no output is mapped",
                        );
                    }
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// Workspaces return to their home display across any sequence of output
    /// disconnects/reconnects, even though every reconnect re-enumerates the
    /// panel on a fresh connector. Pins home-return over the whole hotplug state
    /// space; the old connector-name home stranded frames (and let Emacs spawn a
    /// spurious workspace) as soon as a connector renamed.
    #[test]
    fn frames_return_to_home_display_across_hotplug(
        ops in prop::collection::vec(
            prop_oneof![
                (0usize..3).prop_map(HotplugOp::Connect),
                (0usize..3).prop_map(HotplugOp::Disconnect),
            ],
            1..24,
        ),
    ) {
        let mut fixture = Fixture::new().unwrap();
        let mut connector_seq = 0u32;
        let mut mint = |serial: &str| {
            connector_seq += 1;
            format!("conn-{serial}-{connector_seq}")
        };

        // Three displays, each with two frames whose home is set by the initial
        // layout while every display is connected.
        let mut displays: Vec<Display> = (0..3)
            .map(|i| {
                let serial = format!("SER{i}");
                let frames = vec![(i as u64 + 1) * 10, (i as u64 + 1) * 10 + 1];
                let connector = mint(&serial);
                fixture.add_output_with_identity(&connector, 1280, 720, "Acme", "Panel", &serial);
                fixture
                    .ewm()
                    .apply_output_layout(&connector, frames.iter().copied().map(Frame::new).collect());
                Display { serial, frames, connector: Some(connector) }
            })
            .collect();
        assert_hotplug_invariants(&fixture, &displays);

        for op in ops {
            match op {
                HotplugOp::Connect(i) => {
                    if displays[i].connector.is_none() {
                        let connector = mint(&displays[i].serial);
                        fixture.add_output_with_identity(
                            &connector, 1280, 720, "Acme", "Panel", &displays[i].serial,
                        );
                        displays[i].connector = Some(connector);
                    }
                }
                HotplugOp::Disconnect(i) => {
                    if let Some(connector) = displays[i].connector.take() {
                        fixture.remove_output(&connector);
                    }
                }
            }
            assert_hotplug_invariants(&fixture, &displays);
        }
    }
}

#[test]
fn layout_merge_preserves_fullscreen_for_same_surface_view() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);

    let surface_id = 1;
    let entry_id = nz(1);

    fixture.ewm().apply_output_layout(
        "Virtual-1",
        vec![frame_with_entry(100, entry_id, surface_id)],
    );
    fixture.ewm().set_layout_entry_fullscreen(entry_id, true);
    assert_eq!(
        entry_fullscreen(&fixture, "Virtual-1", entry_id),
        Some(true)
    );

    fixture.ewm().apply_output_layout(
        "Virtual-1",
        vec![frame_with_entry(100, entry_id, surface_id)],
    );
    assert_eq!(
        entry_fullscreen(&fixture, "Virtual-1", entry_id),
        Some(true)
    );
    assert!(fixture.ewm_ref().is_surface_fullscreen(surface_id));

    fixture
        .ewm()
        .apply_output_layout("Virtual-1", vec![frame_with_entry(100, entry_id, 2)]);
    assert_eq!(
        entry_fullscreen(&fixture, "Virtual-1", entry_id),
        Some(false)
    );
    assert!(!fixture.ewm_ref().is_surface_fullscreen(surface_id));
}

#[test]
fn migrated_frame_preserves_fullscreen_for_same_surface_view() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);
    fixture.add_output("Virtual-2", 1280, 720);

    let surface_id = 1;
    let entry_id = nz(1);

    fixture.ewm().apply_output_layout(
        "Virtual-1",
        vec![frame_with_entry(100, entry_id, surface_id)],
    );
    fixture.ewm().set_layout_entry_fullscreen(entry_id, true);
    fixture.ewm().apply_output_layout(
        "Virtual-2",
        vec![frame_with_entry(100, entry_id, surface_id)],
    );

    assert_eq!(
        entry_fullscreen(&fixture, "Virtual-2", entry_id),
        Some(true)
    );
    assert!(fixture.ewm_ref().is_surface_fullscreen(surface_id));
}

#[test]
fn surface_fullscreen_request_prefers_requested_output_view() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);
    fixture.add_output("Virtual-2", 1280, 720);

    let surface_id = 1;
    fixture
        .ewm()
        .apply_output_layout("Virtual-1", vec![frame_with_entry(100, nz(1), surface_id)]);
    fixture
        .ewm()
        .apply_output_layout("Virtual-2", vec![frame_with_entry(200, nz(2), surface_id)]);

    fixture
        .ewm()
        .set_surface_fullscreen_on_output(surface_id, Some("Virtual-2"), true);

    assert_eq!(entry_fullscreen(&fixture, "Virtual-1", nz(1)), Some(false));
    assert_eq!(entry_fullscreen(&fixture, "Virtual-2", nz(2)), Some(true));
}

#[test]
fn output_scale_change_reassigns_fullscreen_primary_entry() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1000, 1000);
    fixture.add_output("Virtual-2", 1000, 1000);

    let surface_id = 1;
    let first = frame_with_entry(100, nz(1), surface_id);
    let second = frame_with_entry(200, nz(2), surface_id);
    fixture.ewm().apply_output_layout("Virtual-1", vec![first]);
    fixture.ewm().apply_output_layout("Virtual-2", vec![second]);
    fixture.ewm().set_layout_entry_fullscreen(nz(1), true);
    fixture.ewm().set_layout_entry_fullscreen(nz(2), true);

    fixture.ewm().output_config.insert(
        "Virtual-1".to_string(),
        OutputConfig {
            scale: Some(2.0),
            ..Default::default()
        },
    );
    fixture.apply_output_config("Virtual-1");

    let primary_output =
        fixture
            .ewm_ref()
            .frame_set
            .mapped_strips()
            .iter()
            .find_map(|(name, strip)| {
                strip
                    .surface_entries()
                    .any(|entry| entry.surface_id() == Some(surface_id) && entry.primary())
                    .then_some(name.as_str())
            });
    assert_eq!(primary_output, Some("Virtual-2"));
}

#[test]
fn migrated_layout_surfaces_receive_destination_preferred_scale() {
    let mut fixture = Fixture::new().expect("Failed to create fixture");
    fixture.add_output("Virtual-1", 1280, 720);
    fixture.ewm().output_config.insert(
        "Virtual-2".to_string(),
        OutputConfig {
            position: Some((1280, 0)),
            scale: Some(2.0),
            ..Default::default()
        },
    );
    fixture.add_output("Virtual-2", 1280, 720);

    let mut client = fixture.new_client().expect("Failed to create client");
    fixture.roundtrip(&mut client);
    let frame = fixture.open_toplevel(
        &mut client,
        "migrated-frame",
        Size::<i32, Logical>::from((400, 300)),
    );
    let entry = fixture.open_toplevel(
        &mut client,
        "migrated-entry",
        Size::<i32, Logical>::from((400, 300)),
    );

    let frame_id = fixture
        .find_surface_id(&client, &frame.surface)
        .expect("server did not assign a frame surface id");
    let entry_surface_id = fixture
        .find_surface_id(&client, &entry.surface)
        .expect("server did not assign an entry surface id");
    let entry_id = nz(100);

    fixture.ewm().apply_output_layout(
        "Virtual-1",
        vec![frame_with_entry(frame_id, entry_id, entry_surface_id)],
    );
    assert_preferred_scale(&fixture, frame_id, 1.0);
    assert_preferred_scale(&fixture, entry_surface_id, 1.0);

    fixture.ewm().apply_output_layout(
        "Virtual-2",
        vec![frame_with_entry(frame_id, entry_id, entry_surface_id)],
    );
    assert_preferred_scale(&fixture, frame_id, 2.0);
    assert_preferred_scale(&fixture, entry_surface_id, 2.0);

    fixture.ewm().output_config.insert(
        "Virtual-2".to_string(),
        OutputConfig {
            enabled: false,
            ..Default::default()
        },
    );
    fixture.apply_output_config("Virtual-2");
    assert_preferred_scale(&fixture, frame_id, 1.0);
    assert_preferred_scale(&fixture, entry_surface_id, 1.0);
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
        .find_surface_id(&client, &toplevel.surface)
        .expect("server did not assign a surface id");
    let entry_id = nz(1);

    fixture.ewm().apply_output_layout(
        "Virtual-1",
        vec![frame_with_entry(100, entry_id, surface_id)],
    );
    fixture.roundtrip(&mut client);

    assert!(
        fixture
            .ewm_ref()
            .surface_outputs
            .get(&surface_id)
            .is_some_and(|outputs| outputs.contains("Virtual-1"))
    );
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

    assert!(
        !fixture
            .ewm_ref()
            .surface_outputs
            .get(&surface_id)
            .is_some_and(|outputs| outputs.contains("Virtual-1"))
    );
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

    assert!(
        fixture
            .ewm_ref()
            .surface_outputs
            .get(&surface_id)
            .is_some_and(|outputs| outputs.contains("Virtual-1"))
    );
    let events = client.drain_events();
    assert!(has_surface_enter(
        &client,
        &events,
        &toplevel.surface,
        "Virtual-1",
    ));
}
