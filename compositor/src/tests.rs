use proptest::prelude::*;

use super::*;
use crate::idle::SEPIA_FADE_DURATION;
use crate::testing::{
    HEAD_A, HEAD_B, PROP_HEADS, add_prop_outputs, frame_with_entry, frame_with_width, nz,
    order_from_keys, set_sorted_output_order, strip_with_frame_ids,
};
use crate::{FocusSource, Notify};

#[cfg(feature = "screencast")]
#[test]
fn layout_entry_window_title_uses_workspace_and_entry_name() {
    let named = LayoutEntry::emacs_window(nz(1), "*scratch*".to_string(), 0, 0, 100, 100);
    assert_eq!(
        layout_entry_window_title("eDP-1", "2", &named, "ignored"),
        "eDP-1 | 2 | *scratch*"
    );

    let unnamed = LayoutEntry::emacs_window(nz(2), String::new(), 0, 0, 100, 100);
    assert_eq!(
        layout_entry_window_title("HDMI-A-1", "1", &unnamed, "fallback"),
        "HDMI-A-1 | 1 | fallback"
    );
}

#[test]
fn new_toplevel_skips_focused_floating_frame_for_tiled_host() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);

    fix.tile(HEAD_A, vec![frame_with_width(100, 800.0)]);

    let floating_entry_id = nz(3001);
    let floating_entry_surface_id = 9000;
    fix.float(
        HEAD_A,
        vec![frame_with_entry(
            200,
            floating_entry_id,
            floating_entry_surface_id,
        )],
    );
    fix.ewm()
        .set_focus_entry(floating_entry_id, FocusSource::Test, Notify::None);

    assert_eq!(fix.ewm_ref().focused_frame_id(), 200);
    assert_eq!(fix.ewm_ref().focused_entry_id(), Some(floating_entry_id));
    assert_eq!(
        fix.ewm_ref().focused_surface_id(),
        floating_entry_surface_id
    );

    fix.enable_event_capture();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let toplevel = fix.open_toplevel(&mut client, "app", Size::from((240, 160)));
    let app_surface_id = fix.find_surface_id(&client, &toplevel.surface).unwrap();
    let placement = fix
        .drain_events()
        .into_iter()
        .find_map(|event| match event {
            Event::Mapped {
                id,
                frame_surface_id,
                ..
            } if id == app_surface_id => frame_surface_id,
            _ => None,
        });

    // Frame 200 is floating and focused, but a new surface must land on the
    // tiled frame (100), not clobber the floating overlay.
    assert_eq!(placement, Some(100));
}

fn output_names(outputs: &[OutputInfo]) -> Vec<String> {
    outputs.iter().map(|output| output.name.clone()).collect()
}

#[test]
fn output_reports_track_backend_lifecycle() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1280, 720);

    assert_eq!(
        output_names(&fix.output_info_list()),
        vec![HEAD_A.to_string(), HEAD_B.to_string()]
    );

    fix.ewm().output_config.insert(
        HEAD_A.to_string(),
        OutputConfig {
            position: Some((320, 40)),
            scale: Some(1.5),
            ..Default::default()
        },
    );
    fix.apply_output_config(HEAD_A);

    let outputs = fix.output_info_list();
    let output = outputs.iter().find(|output| output.name == HEAD_A).unwrap();
    assert_eq!((output.x, output.y), (320, 40));
    assert!((output.scale - backend::closest_representable_scale(1.5)).abs() < f64::EPSILON);

    fix.remove_output(HEAD_B);

    assert_eq!(
        output_names(&fix.output_info_list()),
        vec![HEAD_A.to_string()]
    );
}

fn filler(id: u64) -> strip::Frame {
    let mut frame = frame_with_width(id, 800.0);
    frame.owner = strip::Owner::Compositor;
    frame
}

#[test]
fn removing_an_output_retires_its_unused_fillers() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1280, 720);
    let mut named = filler(102);
    named.workspace_name = Some("dev".into());
    fix.tile(
        HEAD_B,
        vec![frame_with_width(100, 800.0), filler(101), named],
    );
    fix.ewm()
        .set_focus_frame(101, FocusSource::Test, Notify::None);

    fix.enable_event_capture();
    fix.remove_output(HEAD_B);

    let events = fix.drain_events();
    // The output goes first, then the frames it took with it.
    let order: Vec<&Event> = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::OutputDisconnected { .. } | Event::Close { .. }
            )
        })
        .collect();
    assert!(matches!(
        order.as_slice(),
        [Event::OutputDisconnected { name }, Event::Close { id: 101 }] if name == HEAD_B
    ));
    assert_eq!(fix.ewm_ref().layout.frame_output(100), Some(HEAD_A));
    // A name claims a filler, so it migrates like the user's frames.
    assert_eq!(fix.ewm_ref().layout.frame_output(102), Some(HEAD_A));
    assert!(!fix.ewm_ref().layout.has_frame(101));
    // Focus left the dropped filler for the frame that did move.
    assert_eq!(fix.ewm_ref().focused_frame_id(), 100);

    // A redock adopts nothing it was asked to delete.
    fix.add_output(HEAD_B, 1280, 720);
    assert!(!fix.ewm_ref().layout.has_frame(101));
}

#[test]
fn re_enabling_a_disabled_output_asks_emacs_for_a_new_filler() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1280, 720);
    fix.tile(HEAD_A, vec![frame_with_width(100, 800.0)]);
    fix.tile(HEAD_B, vec![filler(101)]);
    fix.ewm().output_config.insert(
        HEAD_B.to_string(),
        OutputConfig {
            enabled: false,
            ..Default::default()
        },
    );
    fix.enable_event_capture();
    fix.apply_output_config(HEAD_B);
    let events = fix.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Close { id: 101 }))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::OutputsComplete))
    );

    fix.ewm()
        .output_config
        .insert(HEAD_B.to_string(), OutputConfig::default());
    fix.apply_output_config(HEAD_B);
    let events = fix.drain_events();
    assert!(!fix.ewm_ref().layout.has_frame(101));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::OutputsComplete))
    );

    // A plain config change does not ask again.
    fix.ewm().output_config.insert(
        HEAD_B.to_string(),
        OutputConfig {
            scale: Some(2.0),
            ..Default::default()
        },
    );
    fix.apply_output_config(HEAD_B);
    let events = fix.drain_events();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::OutputsComplete))
    );
}

/// Emacs cannot make a frame without one, so the last filler parks and comes back.
#[test]
fn disabling_the_only_output_keeps_its_last_filler() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.tile(HEAD_A, vec![filler(101)]);
    fix.ewm().output_config.insert(
        HEAD_A.to_string(),
        OutputConfig {
            enabled: false,
            ..Default::default()
        },
    );
    fix.enable_event_capture();
    fix.apply_output_config(HEAD_A);
    assert!(
        !fix.drain_events()
            .iter()
            .any(|event| matches!(event, Event::Close { .. }))
    );
    assert!(fix.ewm_ref().layout.is_homeless(101));

    fix.ewm()
        .output_config
        .insert(HEAD_A.to_string(), OutputConfig::default());
    fix.apply_output_config(HEAD_A);
    assert_eq!(fix.ewm_ref().layout.frame_output(101), Some(HEAD_A));
}

#[test]
fn cursor_config_change_queues_pointer_output_redraw() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1280, 720);
    fix.warp_pointer(10.0, 10.0);
    for state in fix.ewm().output_state.values_mut() {
        state.redraw_state = RedrawState::Idle;
    }

    fix.handle_module_command(crate::module::ModuleCommand::ConfigureCursor {
        config: cursor::CursorConfig::new("ConfiguredCursorTheme", 24),
    });

    let redraws = fix
        .ewm_ref()
        .output_state
        .iter()
        .filter(|(_, state)| state.redraw_state.is_render_queued())
        .map(|(output, _)| output.name())
        .collect::<Vec<_>>();
    assert_eq!(redraws, vec![HEAD_A.to_string()]);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn surface_output_name_prefers_focus_then_sorted_output_order(
        order_keys in any::<[u8; 4]>(),
        membership in 1u8..16,
        focused_idx in 0usize..=4,
    ) {
        let mut fix = testing::Fixture::new().unwrap();
        add_prop_outputs(&mut fix);
        let order = order_from_keys(&order_keys);
        set_sorted_output_order(&mut fix, &order);

        let mut client = fix.new_client().unwrap();
        fix.roundtrip(&mut client);
        let surface_id = fix.open_toplevel_id(&mut client, "prop", Size::from((400, 400)));
        let selected = PROP_HEADS
            .iter()
            .enumerate()
            .filter_map(|(idx, name)| ((membership & (1 << idx)) != 0).then_some(*name))
            .collect::<Vec<_>>();
        fix.ewm().surfaces.get_mut(&surface_id).unwrap().outputs =
            selected.iter().map(|name| (*name).to_string()).collect();

        for (idx, name) in PROP_HEADS.iter().enumerate() {
            if selected.contains(name) {
                let mut strip = strip::Strip::new(animation::Clock::default());
                let frame = frame_with_entry(
                    2_000 + idx as u64,
                    nz(3_000 + idx as u64),
                    surface_id,
                );
                strip.push_frame(frame);
                fix.install_strip(name, strip);
            }
        }

        let focused = if focused_idx < PROP_HEADS.len() {
            let name = PROP_HEADS[focused_idx];
            selected.contains(&name).then_some(name)
        } else {
            None
        };
        if focused.is_some() {
            fix.ewm()
                .set_focus_frame(2_000 + focused_idx as u64, FocusSource::Test, Notify::None);
        }
        // The first installed strip holds the keyboard until a frame is focused.
        let focused = focused.or_else(|| selected.first().copied());

        let ordered = order
            .iter()
            .map(|idx| PROP_HEADS[*idx])
            .find(|name| selected.contains(name));

        prop_assert_eq!(
            fix.ewm_ref()
                .surface_monitor(surface_id)
                .map(|monitor| monitor.name()),
            focused
        );
        prop_assert_eq!(
            fix.ewm_ref()
                .layout_entry_for_surface(surface_id, |_| true)
                .map(|location| location.monitor.name()),
            ordered,
        );
        prop_assert_eq!(
            fix.ewm_ref()
                .frame_entry_for_surface(surface_id)
                .map(|(id, _)| id),
            focused.map(|name| {
                2_000 + PROP_HEADS.iter().position(|candidate| candidate == &name).unwrap() as u64
            }),
        );
    }

    #[test]
    fn lock_focus_output_prefers_cursor_then_sorted_output_order(
        order_keys in any::<[u8; 4]>(),
        cursor_idx in 0usize..4,
    ) {
        let mut fix = testing::Fixture::new().unwrap();
        add_prop_outputs(&mut fix);
        let order = order_from_keys(&order_keys);
        set_sorted_output_order(&mut fix, &order);

        let cursor_name = PROP_HEADS[cursor_idx];
        let cursor_output = fix.ewm_ref().find_mapped_output(cursor_name).unwrap();
        let cursor_geo = fix.ewm_ref().space.output_geometry(&cursor_output).unwrap();
        let cursor_center = crate::utils::center(cursor_geo).to_f64();
        fix.warp_pointer(cursor_center.x, cursor_center.y);
        prop_assert_eq!(
            fix.ewm_ref().lock_surface_output().map(|output| output.name()),
            Some(cursor_name.to_string()),
        );

        fix.warp_pointer(-10_000.0, -10_000.0);
        prop_assert_eq!(
            fix.ewm_ref().lock_surface_output().map(|output| output.name()),
            Some(PROP_HEADS[order[0]].to_string()),
        );
    }
}

#[derive(Debug, Clone, Copy)]
enum Step {
    Show,
    Hide,
    Activate,
    Expire,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        Just(Step::Show),
        Just(Step::Hide),
        Just(Step::Activate),
        Just(Step::Expire),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn activation_asks_emacs_only_for_a_hidden_surface(
        steps in prop::collection::vec(step(), 1..12),
    ) {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        fix.tile(HEAD_A, vec![frame_with_width(100, 800.0)]);

        let mut client = fix.new_client().unwrap();
        fix.roundtrip(&mut client);
        let id = fix.open_toplevel_id(&mut client, "dialog", Size::from((400, 300)));
        fix.enable_event_capture();

        // A pending grant is fresh (Some(true)) or stale (Some(false)).
        let (mut placed, mut shown) = (false, false);
        let mut pending: Option<bool> = None;
        for step in steps {
            match step {
                Step::Show => {
                    fix.float(HEAD_A, vec![frame_with_entry(200, nz(3001), id)]);
                    placed = true;
                    shown = true;
                    if pending == Some(true) {
                        prop_assert_eq!(fix.focused_surface_id(), id);
                    }
                    pending = None;
                }
                Step::Hide => {
                    fix.float(HEAD_A, vec![frame_with_width(200, 400.0)]);
                    shown = false;
                    if pending == Some(false) {
                        pending = None;
                    }
                }
                Step::Activate => {
                    fix.ewm().activate_or_defer_surface(id, FocusSource::Test);
                    if shown {
                        prop_assert_eq!(fix.focused_surface_id(), id);
                    }
                    pending = (!shown).then_some(true);
                }
                Step::Expire => {
                    let surface = fix.ewm().surfaces.get_mut(&id).unwrap();
                    if surface.pending_activation.is_some() {
                        surface.pending_activation =
                            Some(Instant::now() - XDG_ACTIVATION_TOKEN_TIMEOUT);
                        pending = Some(false);
                    }
                }
            }
            let asked = fix
                .drain_events()
                .iter()
                .any(|event| matches!(event, Event::ActivateSurface { .. }));
            prop_assert_eq!(asked, matches!(step, Step::Activate) && placed && !shown);
            prop_assert_eq!(fix.ewm_ref().pending_activations().eq([id]), pending.is_some());
        }
    }
}

#[test]
fn mapped_asks_to_select_for_a_token_or_a_focused_parent() {
    for parented in [false, true] {
        for parent_focused in [false, true] {
            for fullscreen in [false, true] {
                for token in [false, true] {
                    let mut fix = testing::Fixture::new().unwrap();
                    fix.add_output(HEAD_A, 1600, 900);
                    let mut client = fix.new_client().unwrap();
                    fix.roundtrip(&mut client);
                    let parent = fix.open_toplevel(&mut client, "parent", Size::from((400, 300)));
                    let parent_id = fix.find_surface_id(&client, &parent.surface).unwrap();
                    let other_id =
                        fix.open_toplevel_id(&mut client, "other", Size::from((400, 300)));
                    fix.tile(
                        HEAD_A,
                        vec![
                            frame_with_entry(100, nz(3001), parent_id),
                            frame_with_entry(300, nz(3003), other_id),
                        ],
                    );
                    let focused = if parent_focused { nz(3001) } else { nz(3003) };
                    fix.ewm()
                        .set_focus_entry(focused, FocusSource::Test, Notify::None);
                    if fullscreen {
                        fix.set_fullscreen(focused, true);
                    }

                    let dialog = fix.open_toplevel_unmapped(
                        &mut client,
                        "dialog",
                        parented.then_some(&parent),
                    );
                    let id = fix.find_surface_id(&client, &dialog.surface).unwrap();
                    if token {
                        fix.ewm().activate_or_defer_surface(id, FocusSource::Test);
                    }
                    fix.enable_event_capture();
                    fix.map_toplevel(&mut client, &dialog, Size::from((200, 100)));

                    let activate = fix.drain_events().iter().find_map(|event| match event {
                        Event::Mapped {
                            id: mapped,
                            activate,
                            ..
                        } if *mapped == id => Some(*activate),
                        _ => None,
                    });
                    let expected = token
                        || if parented {
                            parent_focused
                        } else {
                            !fullscreen
                        };
                    assert_eq!(
                        activate,
                        Some(expected),
                        "parented={parented} parent_focused={parent_focused} fullscreen={fullscreen} token={token}"
                    );
                }
            }
        }
    }
}

#[test]
fn surface_request_helpers_queue_expected_events() {
    use smithay::wayland::seat::WaylandFocus as _;

    let mut fix = testing::Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output(HEAD_A, 1600, 900);

    let toplevel = fix.open_toplevel(&mut client, "requests", Size::from((400, 400)));
    let surface_id = fix.find_surface_id(&client, &toplevel.surface).unwrap();
    fix.tile(HEAD_A, vec![frame_with_entry(100, nz(3001), surface_id)]);

    let server_surface = fix
        .ewm_ref()
        .window(surface_id)
        .and_then(|window| window.wl_surface())
        .unwrap()
        .into_owned();

    fix.enable_event_capture();
    fix.ewm()
        .queue_minimize_request_for_surface(&server_surface);
    fix.ewm()
        .queue_maximize_request_for_surface(&server_surface);
    fix.ewm()
        .queue_unmaximize_request_for_surface(&server_surface);

    let events = fix.drain_events();
    assert_eq!(events.len(), 3);
    assert!(matches!(&events[0], Event::Minimize { id } if *id == surface_id));
    assert!(matches!(&events[1], Event::MaximizeRequest { id } if *id == surface_id));
    assert!(matches!(&events[2], Event::UnmaximizeRequest { id } if *id == surface_id));
}

#[test]
fn system_bell_forwards_toplevel_id() {
    let mut fix = testing::Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output(HEAD_A, 1600, 900);

    let toplevel = fix.open_toplevel(&mut client, "bell", Size::from((400, 400)));
    let surface_id = fix.find_surface_id(&client, &toplevel.surface).unwrap();

    fix.enable_event_capture();
    client.ring_bell(Some(&toplevel.surface));
    client.ring_bell(None);
    fix.roundtrip(&mut client);

    let events = fix.drain_events();
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], Event::Bell { id: Some(id) } if *id == surface_id));
    assert!(matches!(&events[1], Event::Bell { id: None }));
}

#[test]
fn background_title_updates_coalesce_to_latest() {
    let mut fix = testing::Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output(HEAD_A, 1600, 900);
    let id = fix.open_toplevel_id(&mut client, "background", Size::from((400, 400)));
    fix.enable_event_capture();

    fix.ewm()
        .queue_title_update(id, "app".to_string(), "one".to_string(), false, false);
    fix.ewm()
        .queue_title_update(id, "app".to_string(), "two".to_string(), false, false);

    assert!(fix.drain_events().is_empty());

    fix.ewm().flush_pending_title_events();
    let events = fix.drain_events();
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::Title { id: titled, app, title }
            if *titled == id && app == "app" && title == "two"));
}

#[test]
fn focused_title_update_is_immediate() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    let entry_id = nz(3001);
    let surface_id = 42;

    let mut strip = strip::Strip::new(animation::Clock::default());
    strip.push_frame(frame_with_entry(100, entry_id, surface_id));
    fix.install_strip(HEAD_A, strip);
    fix.enable_event_capture();

    fix.ewm().queue_title_update(
        surface_id,
        "app".to_string(),
        "focused".to_string(),
        false,
        false,
    );

    let events = fix.drain_events();
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], Event::Title { id, app, title }
            if *id == surface_id && app == "app" && title == "focused"));
}

#[test]
fn fallback_frame_callbacks_hide_unmapped_output_frames() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1600, 900);
    fix.install_strip(HEAD_A, strip_with_frame_ids(vec![100, 101], 0));
    fix.install_strip(HEAD_B, strip_with_frame_ids(vec![200], 0));

    fix.ewm().output_config.insert(
        HEAD_B.to_string(),
        OutputConfig {
            enabled: false,
            ..Default::default()
        },
    );
    fix.apply_output_config(HEAD_B);

    let hidden = fix.ewm_ref().fallback_hidden_emacs_ids();
    assert!(!hidden.contains(&100));
    assert!(hidden.contains(&101));
    assert!(hidden.contains(&200));
}

#[test]
fn close_flushes_pending_title_before_close_event() {
    let mut fix = testing::Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output(HEAD_A, 1600, 900);
    let id = fix.open_toplevel_id(&mut client, "closing", Size::from((400, 400)));
    fix.enable_event_capture();

    fix.ewm()
        .queue_title_update(id, "app".to_string(), "final".to_string(), false, false);
    assert!(fix.drain_events().is_empty());

    fix.ewm().handle_toplevel_destroyed_by_id(id);
    let events = fix.drain_events();
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], Event::Title { id: titled, app, title }
            if *titled == id && app == "app" && title == "final"));
    assert!(matches!(&events[1], Event::Close { id: closed } if *closed == id));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The sepia tint fades from wherever it is toward the latest target, never past it, and
    /// rests on it once the fade has had its full duration.
    #[test]
    fn sepia_fades_to_its_target(steps in prop::collection::vec((any::<bool>(), 0u64..200), 1..20)) {
        let mut fix = testing::Fixture::new().unwrap();
        let clock = fix.ewm().animations_clock.clone();
        let mut now = Duration::from_secs(1);
        clock.freeze(now);
        let mut since_change = Duration::MAX;
        let mut tinted = false;
        for (sepia, wait_ms) in steps {
            let before = fix.ewm_ref().sepia_amount();
            if tinted != sepia {
                tinted = sepia;
                since_change = Duration::ZERO;
            }
            fix.ewm().set_sepia(sepia);
            prop_assert_eq!(fix.ewm_ref().sepia_amount(), before);

            let wait = Duration::from_millis(wait_ms);
            now += wait;
            clock.set(now);
            since_change = since_change.saturating_add(wait);
            let target = if sepia { 1. } else { 0. };
            let amount = fix.ewm_ref().sepia_amount();
            prop_assert!((0.0..=1.0).contains(&amount));
            prop_assert!((target - amount).abs() <= (target - before).abs() + 1e-9);
            if since_change >= SEPIA_FADE_DURATION {
                prop_assert_eq!(amount, target);
            }
        }
    }
}
