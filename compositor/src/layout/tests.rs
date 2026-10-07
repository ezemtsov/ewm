use std::time::Duration;

use proptest::prelude::*;

use super::*;
use crate::testing::{
    self, HEAD_A, HEAD_B, PROP_HEADS, add_prop_outputs, frame_with_entry, frame_with_width, nz,
    order_from_keys, set_sorted_output_order, strip_with_frame_ids,
};

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
    let on_float = Some((HEAD_A.to_string(), 200));
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
    fix.ewm_ref()
        .frame_set
        .mapped_strip(name)
        .map(|strip| strip.frames().iter().map(|f| f.surface_id).collect())
        .unwrap_or_default()
}

#[test]
fn frames_survive_losing_every_output() {
    let mut fix = testing::Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1280, 720);
    fix.ewm()
        .frame_set
        .insert_mapped_strip(HEAD_A.to_string(), strip_with_frame_ids(vec![1, 2], 1));
    fix.ewm()
        .frame_set
        .insert_mapped_strip(HEAD_B.to_string(), strip_with_frame_ids(vec![3, 4], 1));
    fix.ewm().set_focus_frame(4, "test", false);

    // Lid close: HEAD_A's frames migrate to HEAD_B.
    fix.remove_output(HEAD_A);
    assert_eq!(strip_ids(&fix, HEAD_B), vec![3, 4, 1, 2]);

    // Undock with the lid closed: no fallback, all frames go homeless.
    fix.remove_output(HEAD_B);
    assert!(fix.ewm_ref().frame_set.mapped_strips().is_empty());
    let homeless: Vec<u64> = fix
        .ewm_ref()
        .frame_set
        .homeless_frames()
        .iter()
        .map(|f| f.surface_id)
        .collect();
    assert_eq!(homeless, vec![3, 4, 1, 2]);
    assert_eq!(fix.ewm_ref().focused_frame_id, 0);

    // Lid open: the first output to map adopts every homeless frame.
    fix.add_output(HEAD_A, 1600, 900);
    assert_eq!(strip_ids(&fix, HEAD_A), vec![3, 4, 1, 2]);
    assert!(fix.ewm_ref().frame_set.homeless_frames().is_empty());
    assert_eq!(fix.ewm_ref().active_frame_surface_id(HEAD_A), Some(2));
    assert_eq!(fix.ewm_ref().focused_frame_id, 2);

    // Dock again: HEAD_B's frames return home with their active restored.
    fix.add_output(HEAD_B, 1280, 720);
    assert_eq!(strip_ids(&fix, HEAD_A), vec![1, 2]);
    assert_eq!(strip_ids(&fix, HEAD_B), vec![3, 4]);
    assert_eq!(fix.ewm_ref().active_frame_surface_id(HEAD_B), Some(4));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn frame_fallbacks_follow_sorted_output_order(
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
            fix.ewm().frame_set.insert_mapped_strip((*name).to_string(), strip);
        }

        let first_output = order
            .iter()
            .copied()
            .find(|idx| frame_counts[*idx] % 4 != 0)
            .unwrap();
        let first_frame_id = 1_000 + first_output as u64 * 100;
        let first_active_id = first_frame_id
            + u64::from(active_indices[first_output] % (frame_counts[first_output] % 4));

        prop_assert_eq!(
            fix.ewm_ref().first_active_frame_surface_id(),
            Some(first_active_id)
        );

        fix.ewm().focused_frame_id = 0;
        fix.ewm().focus_any_frame_if_unfocused("test");
        prop_assert_eq!(fix.ewm_ref().focused_frame_id, first_frame_id);

        let disabled_name = PROP_HEADS[first_output];
        fix.ewm()
            .sorted_outputs
            .retain(|output| output.name() != disabled_name);
        let fallback_output = order
            .iter()
            .copied()
            .find(|idx| *idx != first_output && frame_counts[*idx] % 4 != 0)
            .unwrap();
        let fallback_active_id = 1_000
            + fallback_output as u64 * 100
            + u64::from(active_indices[fallback_output] % (frame_counts[fallback_output] % 4));

        fix.ewm()
            .focus_mapped_frame_if_focused_output_unmapped(disabled_name);
        prop_assert_eq!(fix.ewm_ref().focused_frame_id, fallback_active_id);
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
            fix.ewm().frame_set.insert_mapped_strip((*name).to_string(), strip);
        }

        let output_sizes = PROP_HEADS
            .iter()
            .map(|name| ((*name).to_string(), (100, 100)))
            .collect::<HashMap<_, _>>();
        let best_area = fix.ewm_ref().primary_entry_areas(&output_sizes);
        fix.ewm().assign_primary_entries(&output_sizes, &best_area);

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
                fix.ewm_ref().frame_set.mapped_strip(name)
                    .is_some_and(|strip| {
                        strip.surface_entries().any(|entry| {
                            entry.surface_id() == Some(surface_id) && entry.primary()
                        })
                    })
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
            if let Some(strip) = fix.ewm().frame_set.mapped_strip_mut(removed_name)
                && let Some(frame) = strip.frames_mut().first_mut() {
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
        let strip = fix.ewm().frame_set.mapped_strip_mut(HEAD_A).unwrap();
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
    fn swap_frame_moves_one_slot_and_keeps_focus_active(
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

        fix.ewm().swap_frame(ids[from], delta);

        let mut expected = ids.clone();
        let to = from as i32 + delta;
        if (0..len as i32).contains(&to) {
            expected.swap(from, to as usize);
        }
        let strip = fix.ewm_ref().frame_set.mapped_strip(HEAD_A).unwrap();
        let order: Vec<u64> = strip.frames().iter().map(|f| f.surface_id).collect();
        prop_assert_eq!(order, expected);
        prop_assert_eq!(strip.active_frame().map(|f| f.surface_id), Some(focused));
        prop_assert_eq!(fix.ewm_ref().focused_frame_id, focused);
    }
}
