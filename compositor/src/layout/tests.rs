use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use proptest::prelude::*;
use smithay::utils::Size;

use super::*;
use crate::Action;
use crate::testing::{
    self, HEAD_A, HEAD_B, HEAD_C, PROP_HEADS, add_prop_outputs, frame_with_entry, frame_with_width,
    nz, order_from_keys, set_sorted_output_order, strip_with_frame_ids,
};

#[test]
fn dragging_a_float_to_another_output_carries_and_rehomes_it() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1600, 900);
    fix.float(HEAD_A, vec![frame_with_entry(200, nz(3001), 9000)]);
    assert_eq!(fix.ewm_ref().layout.frame_output(200), Some(HEAD_A));

    let (head_a, head_b) = (fix.output(HEAD_A), fix.output(HEAD_B));
    let a_geo = fix.ewm_ref().space.output_geometry(&head_a).unwrap();
    let b_geo = fix.ewm_ref().space.output_geometry(&head_b).unwrap();

    // Grab the float just inside its top-left corner.
    let grab = {
        let monitor = fix.ewm_ref().layout.monitor_of(&head_a).unwrap();
        let (_, data) = monitor.floating.frame_with_data(200).unwrap();
        a_geo.loc.to_f64() + data.logical_pos() + Point::from((5.0, 5.0))
    };
    assert!(fix.ewm().interactive_move_floating_frame_begin(200, grab));

    let target = b_geo.loc.to_f64() + Point::from((800.0, 450.0));
    assert!(fix.ewm().interactive_move_floating_frame_update(target));
    assert_eq!(fix.ewm_ref().layout.frame_output(200), Some(HEAD_B));

    fix.ewm().interactive_move_floating_frame_end(200);
    assert_eq!(fix.ewm_ref().layout.frame_output(200), Some(HEAD_B));
    assert_eq!(
        fix.ewm_ref().layout.home_outputs.get(&200),
        Some(&crate::utils::output_identity(&head_b)),
        "a deliberate drag rehomes the frame"
    );

    // The frame stays under the pointer that dropped it.
    let monitor = fix.ewm_ref().layout.monitor_of(&head_b).unwrap();
    let (_, data) = monitor.floating.frame_with_data(200).unwrap();
    let rect = Rectangle::new(b_geo.loc.to_f64() + data.logical_pos(), data.size());
    assert!(
        rect.contains(target),
        "{rect:?} dropped away from {target:?}"
    );
    fix.ewm_ref().layout.verify_invariants();
}

#[test]
fn grabbing_a_sliding_float_keeps_it_where_it_is_drawn() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.float(HEAD_A, vec![frame_with_entry(200, nz(3001), 9000)]);
    let head_a = fix.output(HEAD_A);
    let a_geo = fix.ewm_ref().space.output_geometry(&head_a).unwrap();

    // A keyboard move leaves the frame sliding in from where it stood.
    assert!(fix.ewm().move_floating_frame(200, 300.0, 200.0));
    let drawn_at = {
        let monitor = fix.ewm_ref().layout.monitor_of(&head_a).unwrap();
        let (_, data) = monitor.floating.frame_with_data(200).unwrap();
        assert_ne!(
            data.render_pos(),
            data.logical_pos(),
            "the slide should be live"
        );
        data.render_pos()
    };

    let grab = a_geo.loc.to_f64() + drawn_at + Point::from((5.0, 5.0));
    assert!(fix.ewm().interactive_move_floating_frame_begin(200, grab));
    assert!(fix.ewm().interactive_move_floating_frame_update(grab));

    let monitor = fix.ewm_ref().layout.monitor_of(&head_a).unwrap();
    let (_, data) = monitor.floating.frame_with_data(200).unwrap();
    let drift = data.render_pos() - drawn_at;
    assert!(
        drift.x.abs() < 0.5 && drift.y.abs() < 0.5,
        "grab jumped by {drift:?}"
    );
}

#[test]
fn tiled_entry_under_floating_center_hits_active_window() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    // Tiled frame whose single entry spans the whole working area.
    let mut tiled = strip::Frame::new(100);
    tiled.width = 1600.0;
    tiled.selected_entry_id = Some(nz(1));
    tiled.entries.push(LayoutEntry::surface(
        nz(1),
        "app".to_string(),
        5000,
        0,
        0,
        1600,
        900,
    ));
    fix.tile(HEAD_A, vec![tiled]);
    fix.float(HEAD_A, vec![frame_with_entry(200, nz(3001), 9000)]);

    // The floating frame (200) sits over the tiled entry -> that entry wins.
    assert_eq!(
        fix.ewm_ref().tiled_entry_under_floating_center(200),
        Some(nz(1))
    );
    // Unknown floating id -> nothing.
    assert_eq!(fix.ewm_ref().tiled_entry_under_floating_center(4242), None);
}

#[test]
fn floating_frame_occludes_the_strip_when_hit_testing() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);

    // Tiled entry spanning the whole output (the background frame).
    let tiled_entry = nz(1);
    let mut tiled = strip::Frame::new(100);
    tiled.width = 1600.0;
    tiled.selected_entry_id = Some(tiled_entry);
    tiled.entries.push(LayoutEntry::surface(
        tiled_entry,
        "app".to_string(),
        5000,
        0,
        0,
        1600,
        900,
    ));
    fix.tile(HEAD_A, vec![tiled]);

    // Floating frame (400x480, centered at (600,210)) over the tiled entry;
    // its 400x400 entry leaves an 80px strip of the frame's own surface.
    let floating_entry = nz(3001);
    fix.float(HEAD_A, vec![frame_with_entry(200, floating_entry, 9000)]);

    let over_entry = Point::from((800.0, 400.0)); // inside the floating entry
    // The frame's own surface below its entry: the modeline and echo area --
    // the only region where focus fell through to the strip.
    let modeline = Point::from((800.0, 620.0));
    let echo_area = Point::from((800.0, 685.0));
    let outside = Point::from((100.0, 100.0)); // tiled area beside the float

    let ewm = fix.ewm_ref();
    let on_float = Some(200);
    // Scene sanity: the anchors are where the geometry above puts them.
    assert_eq!(ewm.floating_frame_under(over_entry), on_float);
    assert_eq!(ewm.floating_frame_under(modeline), on_float);
    assert_eq!(ewm.floating_frame_under(echo_area), on_float);
    assert!(ewm.floating_frame_under(outside).is_none());

    // The modeline and echo area occlude the tiled entry beneath them.
    assert_eq!(ewm.layout_entry_id_under(modeline), None);
    assert_eq!(ewm.layout_entry_id_under(echo_area), None);
    // The floating entry still resolves; the tiled entry still wins beside the float.
    assert_eq!(ewm.layout_entry_id_under(over_entry), Some(floating_entry));
    assert_eq!(ewm.layout_entry_id_under(outside), Some(tiled_entry));

    // No point over the floating frame ever hit-tests to the tiled entry beneath.
    proptest!(|(x in 0i32..1600, y in 0i32..900)| {
        let p = Point::from((x as f64, y as f64));
        if ewm.floating_frame_under(p).is_some() {
            prop_assert_ne!(ewm.layout_entry_id_under(p), Some(tiled_entry));
        }
    });
}

fn strip_ids(fix: &testing::Fixture, name: &str) -> Vec<u64> {
    fix.strip(name).frame_ids().collect()
}

#[test]
fn frames_survive_losing_every_output() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1280, 720);
    fix.install_strip(HEAD_A, strip_with_frame_ids(vec![1, 2], 1));
    fix.install_strip(HEAD_B, strip_with_frame_ids(vec![3, 4], 1));
    fix.ewm().set_focus_frame(4, "test", false);

    // Lid close: HEAD_A's frames migrate to HEAD_B.
    fix.remove_output(HEAD_A);
    assert_eq!(strip_ids(&fix, HEAD_B), vec![3, 4, 1, 2]);

    // Undock with the lid closed: no fallback, all frames go homeless.
    fix.remove_output(HEAD_B);
    assert!(fix.ewm_ref().layout.monitors().is_empty());
    assert_eq!(fix.ewm_ref().layout.homeless().ids(), vec![3, 4, 1, 2]);
    assert_eq!(fix.ewm_ref().focused_frame_id(), 0);

    // Lid open: the first output to map adopts every homeless frame.
    fix.add_output(HEAD_A, 1600, 900);
    assert_eq!(strip_ids(&fix, HEAD_A), vec![3, 4, 1, 2]);
    assert!(fix.ewm_ref().layout.homeless().tiled.is_empty());
    assert_eq!(fix.strip(HEAD_A).active_frame_id(), Some(2));
    assert_eq!(fix.ewm_ref().focused_frame_id(), 2);

    // Dock again: HEAD_B's frames return home with their active restored.
    fix.add_output(HEAD_B, 1280, 720);
    assert_eq!(strip_ids(&fix, HEAD_A), vec![1, 2]);
    assert_eq!(strip_ids(&fix, HEAD_B), vec![3, 4]);
    assert_eq!(fix.strip(HEAD_B).active_frame_id(), Some(4));
}

#[test]
fn an_unplugged_middle_output_hands_its_frames_to_the_next_one() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1600, 900);
    fix.add_output(HEAD_C, 1600, 900);
    fix.install_strip(HEAD_A, strip_with_frame_ids(vec![1], 0));
    fix.install_strip(HEAD_B, strip_with_frame_ids(vec![2, 3], 1));
    fix.install_strip(HEAD_C, strip_with_frame_ids(vec![4], 0));
    fix.ewm().set_focus_frame(3, "test", false);

    fix.remove_output(HEAD_B);
    assert_eq!(strip_ids(&fix, HEAD_A), vec![1]);
    assert_eq!(strip_ids(&fix, HEAD_C), vec![4, 2, 3]);
    assert_eq!(fix.ewm_ref().focused_frame_id(), 3);

    // The last output in order wraps round to the first.
    fix.remove_output(HEAD_C);
    assert_eq!(strip_ids(&fix, HEAD_A), vec![1, 4, 2, 3]);
}

#[test]
fn floats_survive_losing_every_output() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1280, 720);
    fix.float(HEAD_A, vec![frame_with_width(200, 640.0)]);

    // Lid close: the float moves with HEAD_A's strip onto HEAD_B.
    fix.remove_output(HEAD_A);
    assert_eq!(fix.ewm_ref().layout.frame_output(200), Some(HEAD_B));
    assert!(
        fix.ewm_ref()
            .layout
            .floating(&fix.output(HEAD_B))
            .unwrap()
            .has_frame(200)
    );
    // Undock with the lid closed: parked, still known.
    fix.remove_output(HEAD_B);
    assert_eq!(fix.ewm_ref().layout.homeless().floating.len(), 1);
    assert!(fix.ewm_ref().layout.has_frame(200));

    // Lid open: the first output to map adopts it, floating again.
    fix.add_output(HEAD_A, 1600, 900);
    assert!(
        fix.ewm_ref()
            .layout
            .floating(&fix.output(HEAD_A))
            .unwrap()
            .has_frame(200)
    );
    assert!(fix.ewm_ref().layout.homeless().floating.is_empty());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn keyboard_fallback_follows_sorted_output_order(
        order_keys in any::<[u8; 4]>(),
        frame_counts in any::<[u8; 4]>(),
        active_indices in any::<[u8; 4]>(),
    ) {
        prop_assume!(frame_counts.iter().filter(|count| **count % 4 != 0).count() >= 2);

        let mut fix = testing::Fixture::new().unwrap();
        add_prop_outputs(&mut fix);
        let order = order_from_keys(&order_keys);
        set_sorted_output_order(&mut fix, &order);
        prop_assert_eq!(
            fix.ewm_ref()
                .output_at(Point::from((-10_000.0, -10_000.0)))
                .map(|output| output.name()),
            Some(PROP_HEADS[order[0]].to_string())
        );
        prop_assert_eq!(
            fix.ewm_ref()
                .active_outputs_snapshot()
                .into_iter()
                .map(|output| output.name)
                .collect::<Vec<_>>(),
            order
                .iter()
                .map(|idx| PROP_HEADS[*idx].to_string())
                .collect::<Vec<_>>()
        );

        for (idx, name) in PROP_HEADS.iter().enumerate() {
            let count = usize::from(frame_counts[idx] % 4);
            if count == 0 {
                continue;
            }
            let base = 1_000 + idx as u64 * 100;
            let ids = (0..count).map(|frame_idx| base + frame_idx as u64).collect();
            let strip = strip_with_frame_ids(ids, usize::from(active_indices[idx]));
            fix.install_strip(name, strip);
        }

        // The keyboard sits on the first installed strip. Unplugging that output with nowhere to
        // send its frames drops it onto the first frame in output order.
        let installed = (0..PROP_HEADS.len())
            .find(|idx| frame_counts[*idx] % 4 != 0)
            .unwrap();
        let fallback = order
            .iter()
            .copied()
            .find(|idx| *idx != installed && frame_counts[*idx] % 4 != 0)
            .unwrap();
        let fallback_active_id = 1_000
            + fallback as u64 * 100
            + u64::from(active_indices[fallback] % (frame_counts[fallback] % 4));
        let disabled = fix.output(PROP_HEADS[installed]);
        fix.ewm().layout.detach_output(&disabled, None);
        prop_assert_eq!(fix.ewm_ref().focused_frame_id(), fallback_active_id);
    }

    #[test]
    fn primary_entry_ties_follow_sorted_output_order(
        order_keys in any::<[u8; 4]>(),
        membership in 1u8..16,
        entry_w in 1u32..800,
        entry_h in 1u32..800,
    ) {
        let mut fix = testing::Fixture::new().unwrap();
        add_prop_outputs(&mut fix);
        let order = order_from_keys(&order_keys);
        set_sorted_output_order(&mut fix, &order);

        let surface_id = 10_000;
        for (idx, name) in PROP_HEADS.iter().enumerate() {
            if membership & (1 << idx) == 0 {
                continue;
            }
            let mut strip = strip::Strip::new(animation::Clock::default());
            let surface_entry_id = nz(5_000 + idx as u64);
            let plain_entry_id = nz(6_000 + idx as u64);
            let mut frame = frame_with_entry(
                4_000 + idx as u64,
                surface_entry_id,
                surface_id,
            );
            frame.entries[0].w = entry_w;
            frame.entries[0].h = entry_h;
            frame.entries.push(LayoutEntry::emacs_window(
                plain_entry_id,
                "plain".to_string(),
                entry_w as i32,
                0,
                entry_w,
                entry_h,
            ));
            strip.push_frame(frame);
            fix.install_strip(name, strip);
        }

        let outputs = fix.ewm_ref().sorted_outputs.clone();
        fix.ewm().layout.assign_primaries(&outputs);

        let expected = order
            .iter()
            .map(|idx| PROP_HEADS[*idx])
            .find(|name| {
                let idx = PROP_HEADS.iter().position(|candidate| candidate == name).unwrap();
                membership & (1 << idx) != 0
            })
            .unwrap();
        let primary = PROP_HEADS
            .iter()
            .copied()
            .filter(|name| {
                fix.strip(name)
                    .surface_entries()
                    .any(|entry| entry.surface_id() == Some(surface_id) && entry.primary())
            })
            .collect::<Vec<_>>();

        prop_assert_eq!(primary, vec![expected]);

        #[cfg(feature = "screencast")]
        {
            fix.ewm().refresh_mapped_cast_outputs();
            for (idx, name) in PROP_HEADS.iter().enumerate() {
                if membership & (1 << idx) == 0 {
                    continue;
                }

                for entry_id in [nz(5_000 + idx as u64), nz(6_000 + idx as u64)] {
                    prop_assert_eq!(
                        fix.ewm_ref()
                            .casting
                            .mapped_cast_output
                            .get(&entry_id)
                            .map(Output::name),
                        Some((*name).to_string()),
                    );
                    prop_assert_eq!(
                        fix.ewm_ref()
                            .layout_entry_cast_params(entry_id)
                            .map(|(size, _)| size),
                        Some(Size::from((entry_w as i32, entry_h as i32))),
                    );
                }
            }

            let removed_idx = order
                .iter()
                .copied()
                .find(|idx| membership & (1 << idx) != 0)
                .unwrap();
            let removed_name = PROP_HEADS[removed_idx];
            if let Some(frame) = fix.strip_mut(removed_name).frames_mut().first_mut() {
                frame.entries.clear();
            }
            fix.ewm().refresh_mapped_cast_outputs();
            prop_assert!(!fix
                .ewm_ref()
                .casting
                .mapped_cast_output
                .contains_key(&nz(5_000 + removed_idx as u64)));
            prop_assert!(!fix
                .ewm_ref()
                .casting
                .mapped_cast_output
                .contains_key(&nz(6_000 + removed_idx as u64)));
        }
    }
}

#[test]
fn emacs_frame_origin_uses_screen_relative_strip_position() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.tile(
        HEAD_A,
        vec![frame_with_width(100, 400.0), frame_with_width(101, 400.0)],
    );
    fix.ewm().animations_clock.set_complete_instantly(true);

    fix.ewm().set_focus_frame(101, "test", false);

    assert_eq!(
        fix.ewm_ref().emacs_frame_origin(101),
        Some(Point::from((0, 0))),
    );
    assert_eq!(
        fix.ewm_ref().emacs_frame_origin(100),
        Some(Point::from((-400, 0))),
    );
}

#[test]
fn emacs_frame_origin_uses_floating_position() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.float(HEAD_A, vec![frame_with_width(200, 640.0)]);

    assert_eq!(
        fix.ewm_ref().emacs_frame_origin(200),
        Some(Point::from((480, 210))),
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // Mid-slide, popups and hits round the frame's position like the renderer.
    #[test]
    fn emacs_frame_origin_rounds_a_sliding_frame(offset in -2000f64..=2000.) {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        fix.tile(
            HEAD_A,
            vec![frame_with_width(100, 400.0), frame_with_width(101, 400.0)],
        );
        let strip = fix.strip_mut(HEAD_A);
        strip.gesture_begin();
        strip.gesture_update(offset, Duration::ZERO, strip::VIEW_GESTURE_WORKING_AREA_MOVEMENT);
        let expected = (strip.column_x(1) - strip.view_pos()).round() as i32;

        prop_assert_eq!(
            fix.ewm_ref().emacs_frame_origin(101),
            Some(Point::from((expected, 0))),
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // A swap moves one frame past one neighbour and the focused frame stays active.
    #[test]
    fn move_frame_moves_one_slot_and_keeps_focus_active(
        len in 1usize..=6,
        raw_focus in 0usize..6,
        raw_from in 0usize..6,
        left in any::<bool>(),
    ) {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        let ids: Vec<u64> = (0..len).map(|idx| 100 + idx as u64).collect();
        fix.tile(HEAD_A, ids.iter().map(|&id| frame_with_width(id, 800.0)).collect());
        let focused = ids[raw_focus % len];
        fix.ewm().set_focus_frame(focused, "test", false);
        let from = raw_from % len;
        let delta = if left { -1 } else { 1 };

        fix.ewm().move_frame(ids[from], delta);

        let mut expected = ids.clone();
        let to = from as i32 + delta;
        if (0..len as i32).contains(&to) {
            expected.swap(from, to as usize);
        }
        let strip = fix.strip(HEAD_A);
        let order: Vec<u64> = strip.frame_ids().collect();
        prop_assert_eq!(order, expected);
        prop_assert_eq!(strip.active_frame_id(), Some(focused));
        prop_assert_eq!(fix.ewm_ref().focused_frame_id(), focused);
    }

    // A strip switch leaves the keyboard on a float it keeps on screen and takes it from one it
    // hides.
    #[test]
    fn a_strip_switch_leaves_the_keyboard_on_a_shown_float(
        len in 1usize..=4,
        raw_active in 0usize..4,
        pinned in any::<bool>(),
        delta in -2i32..=2,
    ) {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        let ids: Vec<u64> = (0..len).map(|idx| 100 + idx as u64).collect();
        fix.tile(HEAD_A, ids.iter().map(|&id| frame_with_width(id, 800.0)).collect());
        let active_idx = raw_active % len;
        let active = ids[active_idx];
        fix.ewm().set_focus_frame(active, "test", false);
        fix.float(HEAD_A, vec![frame_with_width(200, 640.0)]);
        if pinned {
            fix.ewm().layout.frame_mut(200).unwrap().pinned_to = Some(active);
        }
        fix.ewm().set_focus_frame(200, "test", false);

        fix.do_action(Action::FocusFrame { delta });

        let to = active_idx as i32 + delta;
        let shown = if (0..len as i32).contains(&to) { ids[to as usize] } else { active };
        prop_assert_eq!(fix.strip(HEAD_A).active_frame_id(), Some(shown));
        let expected = if pinned && shown != active { shown } else { 200 };
        prop_assert_eq!(fix.ewm_ref().focused_frame_id(), expected);
    }

    // Closing the focused float hands the keyboard to the topmost remaining float when that is
    // shown, else to the strip.
    #[test]
    fn a_closing_float_hands_the_keyboard_to_the_next_shown_float(
        len in 1usize..=4,
        raw_focused in 0usize..4,
        hidden in any::<[bool; 4]>(),
    ) {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        fix.tile(HEAD_A, vec![frame_with_width(100, 800.0), frame_with_width(101, 800.0)]);
        fix.ewm().set_focus_frame(100, "test", false);
        let ids: Vec<u64> = (0..len).map(|idx| 200 + idx as u64).collect();
        fix.float(HEAD_A, ids.iter().map(|&id| frame_with_width(id, 640.0)).collect());
        for (idx, &id) in ids.iter().enumerate() {
            if hidden[idx] {
                // Pinned to the inactive frame, off screen.
                fix.ewm().layout.frame_mut(id).unwrap().pinned_to = Some(101);
            }
        }
        let focused = ids[raw_focused % len];
        fix.ewm().set_focus_frame(focused, "test", false);
        let output = fix.output(HEAD_A);
        let (active, next) = {
            let monitor = fix.ewm_ref().layout.monitor_of(&output).unwrap();
            let next = monitor
                .floating
                .frames()
                .iter()
                .find(|float| float.surface_id != focused)
                .map(|float| (float.surface_id, monitor.float_shown(float)));
            (monitor.strip.active_frame_id().unwrap(), next)
        };

        fix.ewm().handle_toplevel_destroyed_by_id(focused);

        let expected = match next {
            Some((id, true)) => id,
            _ => active,
        };
        prop_assert_eq!(fix.ewm_ref().focused_frame_id(), expected);
    }
}

/// Every way the compositor moves frames around, on the bare layout.
#[derive(Debug, Clone)]
enum LayoutOp {
    AddOutput(usize),
    DetachOutput(usize),
    AddFrame {
        id: u64,
        output: usize,
        floating: bool,
        after: Option<u64>,
        filler: bool,
    },
    RemoveFrame {
        id: u64,
        active_close: bool,
    },
    MoveFrame {
        id: u64,
        delta: i32,
    },
    MoveFrames {
        src: usize,
        dst: usize,
        ids: Vec<u64>,
    },
    MoveFrameToOutput {
        id: u64,
        dst: usize,
    },
    AdoptHomeless(usize),
    /// A viewless float joins a strip frame of its monitor and stays while that frame lasts.
    Pin {
        float: u64,
        frame: u64,
    },
    Activate(u64),
    /// A strip frame comes into view without taking the keyboard, as a switch under a float.
    Show(u64),
    Merge(Vec<u64>),
    DragBegin {
        id: u64,
        output: usize,
        px: f64,
        py: f64,
    },
    DragUpdate {
        output: usize,
        px: f64,
        py: f64,
    },
    DragEnd(u64),
}

const LAYOUT_FRAME_IDS: std::ops::RangeInclusive<u64> = 1..=8;

/// Pointer positions well past the 1600x900 heads, so drags leave the output too.
const DRAG_COORD: std::ops::Range<f64> = -1000.0..2600.0;

fn layout_op() -> impl Strategy<Value = LayoutOp> {
    let output = 0..PROP_HEADS.len();
    let id = LAYOUT_FRAME_IDS;
    prop_oneof![
        1 => output.clone().prop_map(LayoutOp::AddOutput),
        1 => output.clone().prop_map(LayoutOp::DetachOutput),
        4 => (
            id.clone(),
            output.clone(),
            any::<bool>(),
            prop::option::of(id.clone()),
            any::<bool>()
        )
            .prop_map(|(id, output, floating, after, filler)| LayoutOp::AddFrame {
                id,
                output,
                floating,
                after,
                filler,
            }),
        1 => (id.clone(), any::<bool>())
            .prop_map(|(id, active_close)| LayoutOp::RemoveFrame { id, active_close }),
        1 => (id.clone(), -3i32..=3).prop_map(|(id, delta)| LayoutOp::MoveFrame { id, delta }),
        1 => (
            output.clone(),
            output.clone(),
            prop::collection::vec(id.clone(), 0..4)
        )
            .prop_map(|(src, dst, ids)| LayoutOp::MoveFrames { src, dst, ids }),
        1 => (id.clone(), output.clone())
            .prop_map(|(id, dst)| LayoutOp::MoveFrameToOutput { id, dst }),
        1 => output.clone().prop_map(LayoutOp::AdoptHomeless),
        1 => (id.clone(), id.clone()).prop_map(|(float, frame)| LayoutOp::Pin { float, frame }),
        1 => id.clone().prop_map(LayoutOp::Activate),
        1 => id.clone().prop_map(LayoutOp::Show),
        1 => prop::collection::vec(id.clone(), 0..6).prop_map(LayoutOp::Merge),
        1 => (id.clone(), output.clone(), DRAG_COORD, DRAG_COORD)
            .prop_map(|(id, output, px, py)| LayoutOp::DragBegin { id, output, px, py }),
        2 => (output, DRAG_COORD, DRAG_COORD)
            .prop_map(|(output, px, py)| LayoutOp::DragUpdate { output, px, py }),
        1 => id.prop_map(LayoutOp::DragEnd),
    ]
}

/// `moved` sits on `dst`. A moved `focused` keeps the keyboard, active when tiled, else the
/// remembered frame returns.
fn check_landed(
    layout: &Layout,
    dst: &Output,
    moved: &Moved,
    focused: u64,
    remembered: &mut BTreeMap<String, Option<u64>>,
) -> Result<(), TestCaseError> {
    let dst_name = dst.name();
    for &id in &moved.ids {
        prop_assert_eq!(layout.frame_output(id), Some(dst_name.as_str()));
    }
    let strip = layout.strip(dst);
    let on_strip = |id: u64| strip.is_some_and(|strip| strip.index_of(id).is_some());
    let returned = remembered
        .get_mut(&dst_name)
        .and_then(|slot| slot.take_if(|id| moved.ids.contains(id)));
    if moved.ids.contains(&focused) {
        prop_assert_eq!(
            layout.focused_frame_id(),
            focused,
            "the keyboard follows its frame"
        );
        if on_strip(focused) {
            prop_assert_eq!(layout.active_frame_id(dst), Some(focused));
        }
    } else if let Some(id) = returned.filter(|&id| on_strip(id) && !on_strip(focused)) {
        prop_assert_eq!(
            layout.active_frame_id(dst),
            Some(id),
            "the remembered frame returns"
        );
    }
    Ok(())
}

fn apply_layout_op(
    layout: &mut Layout,
    outputs: &[Output],
    live: &mut BTreeSet<u64>,
    remembered: &mut BTreeMap<String, Option<u64>>,
    op: &LayoutOp,
) -> Result<(), TestCaseError> {
    let focused = layout.focused_frame_id();
    match *op {
        LayoutOp::AddOutput(i) => layout.add_output(
            outputs[i].clone(),
            Rectangle::from_size(Size::from((1600, 900))),
        ),
        LayoutOp::DetachOutput(i) => {
            let output = &outputs[i];
            let fallback = layout.outputs().find(|other| *other != output).cloned();
            // Fillers are dropped unless they are Emacs's last frames, everything else moves or
            // parks.
            let on_output: Vec<&strip::Frame> = layout
                .monitor_of(output)
                .into_iter()
                .flat_map(Monitor::frames)
                .collect();
            let fillers: Vec<u64> = on_output
                .iter()
                .filter(|frame| frame.is_filler())
                .map(|frame| frame.surface_id)
                .collect();
            let last_frames =
                fillers.len() == on_output.len() && layout.all_frames().count() == on_output.len();
            let expected = if last_frames { Vec::new() } else { fillers };
            let moved = layout.detach_output(output, fallback.as_ref());
            prop_assert!(layout.monitor_of(output).is_none());
            if moved.src.is_some() {
                remembered.insert(output.name(), moved.src_prev_active);
            }
            prop_assert_eq!(&moved.retired, &expected);
            for id in &moved.retired {
                live.remove(id);
                prop_assert!(!layout.has_frame(*id));
            }
            if let Some(dst) = &moved.dst {
                check_landed(layout, dst, &moved, focused, remembered)?;
            }
        }
        LayoutOp::AddFrame {
            id,
            output,
            floating,
            after,
            filler,
        } => {
            if !live.contains(&id) && layout.monitor_of(&outputs[output]).is_some() {
                let placement = if floating {
                    Placement::Floating { pos: None }
                } else {
                    Placement::Tiled { after }
                };
                layout.add_frame(id, &outputs[output], placement);
                if filler {
                    layout.frame_mut(id).unwrap().owner = strip::Owner::Compositor;
                }
                live.insert(id);
                prop_assert_eq!(layout.frame_output(id), Some(PROP_HEADS[output]));
            }
        }
        LayoutOp::RemoveFrame { id, active_close } => {
            layout.remove_frame(id, active_close);
            live.remove(&id);
            prop_assert!(!layout.has_frame(id));
        }
        LayoutOp::MoveFrame { id, delta } => {
            let before = layout.frame_output(id).map(str::to_owned);
            layout.move_frame(id, delta);
            prop_assert_eq!(layout.frame_output(id), before.as_deref());
        }
        LayoutOp::MoveFrames { src, dst, ref ids } => {
            let moved = layout.move_frames(&outputs[src], &outputs[dst], ids, None);
            if let Some(dst) = &moved.dst {
                check_landed(layout, dst, &moved, focused, remembered)?;
            }
        }
        LayoutOp::MoveFrameToOutput { id, dst } => {
            let dst = &outputs[dst];
            let moved = layout.move_frame_to_output(id, dst);
            if !moved.ids.is_empty() {
                check_landed(layout, dst, &moved, focused, remembered)?;
                prop_assert_eq!(
                    layout.home_outputs.get(&id),
                    Some(&crate::utils::output_identity(dst)),
                    "a deliberate move rehomes the frame"
                );
                if let Some(strip) = layout.strip(dst)
                    && let Some(idx) = strip.index_of(id)
                {
                    let prev = moved.dst_prev_active.and_then(|prev| strip.index_of(prev));
                    prop_assert_eq!(
                        idx,
                        prev.map_or(0, |prev| prev + 1),
                        "lands after the active frame"
                    );
                }
            }
        }
        LayoutOp::AdoptHomeless(i) => {
            let moved = layout.adopt_homeless(&outputs[i]);
            if let Some(dst) = &moved.dst {
                prop_assert!(layout.homeless().ids().is_empty());
                check_landed(layout, dst, &moved, focused, remembered)?;
            }
        }
        LayoutOp::Pin { float, frame } => {
            if let Some(float) = layout
                .monitors
                .iter_mut()
                .filter(|monitor| monitor.strip.index_of(frame).is_some())
                .find_map(|monitor| monitor.floating.frame_mut(float))
            {
                float.pinned_to = Some(frame);
            }
            // The compositor settles the focus after every pin change.
            layout.settle_focus();
        }
        LayoutOp::Show(id) => {
            layout.show_strip_frame(id);
        }
        LayoutOp::Activate(id) => {
            if let Some((output, _)) = layout.activate_frame(id, None)
                && layout
                    .strip(&output)
                    .is_some_and(|strip| strip.index_of(id).is_some())
            {
                prop_assert_eq!(layout.active_frame_id(&output), Some(id));
            }
        }
        LayoutOp::DragBegin { id, output, px, py } => {
            let was_dragging = layout.interactive_move.is_some();
            let began = layout.interactive_move_begin(id, &outputs[output], Point::from((px, py)));
            if began {
                // A drag starts only when none is live, and it names the frame asked for.
                prop_assert!(!was_dragging);
                prop_assert_eq!(
                    layout.interactive_move.map(|move_| move_.frame_id),
                    Some(id)
                );
            } else {
                prop_assert_eq!(layout.interactive_move.is_some(), was_dragging);
            }
        }
        LayoutOp::DragUpdate { output, px, py } => {
            let dst = &outputs[output];
            let dragged = layout.interactive_move.map(|move_| move_.frame_id);
            let moved = layout.interactive_move_update(dst, Point::from((px, py)));
            if let (Some(moved), Some(id)) = (&moved, dragged)
                && layout.monitor_of(dst).is_some()
            {
                // The drag carries the frame onto the output under the pointer.
                prop_assert_eq!(layout.frame_output(id), Some(PROP_HEADS[output]));
                if !moved.ids.is_empty() {
                    check_landed(layout, dst, moved, focused, remembered)?;
                }
            }
            prop_assert_eq!(moved.is_none(), layout.interactive_move.is_none());
        }
        LayoutOp::DragEnd(id) => {
            layout.interactive_move_end(id);
            prop_assert!(
                layout
                    .interactive_move
                    .is_none_or(|move_| move_.frame_id != id)
            );
        }
        LayoutOp::Merge(ref ids) => {
            let strips = layout.strip_frames_snapshot();
            let placed = layout.frame_outputs_snapshot();
            layout.merge_frames(ids.iter().map(|&id| strip::Frame::new(id)).collect());
            prop_assert_eq!(layout.strip_frames_snapshot(), strips);
            prop_assert_eq!(layout.frame_outputs_snapshot(), placed);
        }
    }
    Ok(())
}

/// The layout holds exactly the `live` frames and nothing else.
fn verify_live_frames(layout: &Layout, live: &BTreeSet<u64>) -> Result<(), TestCaseError> {
    layout.verify_invariants();
    let mut everywhere: Vec<u64> = layout.all_frames().map(|frame| frame.surface_id).collect();
    everywhere.sort_unstable();
    prop_assert_eq!(&everywhere, &live.iter().copied().collect::<Vec<_>>());
    for id in LAYOUT_FRAME_IDS {
        prop_assert_eq!(layout.has_frame(id), live.contains(&id));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn layout_invariants_hold_under_any_op_sequence(
        ops in prop::collection::vec(layout_op(), 1..40),
    ) {
        let mut layout = testing::layout_with(
            PROP_HEADS
                .iter()
                .map(|name| (*name, strip::Strip::new(animation::Clock::default())))
                .collect(),
        );
        let outputs: Vec<Output> = layout.outputs().cloned().collect();
        let mut live = BTreeSet::new();
        let mut remembered = BTreeMap::new();
        for op in &ops {
            apply_layout_op(&mut layout, &outputs, &mut live, &mut remembered, op)?;
            verify_live_frames(&layout, &live)?;
        }
    }
}
