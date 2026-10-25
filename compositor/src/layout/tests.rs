use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use proptest::prelude::*;
use smithay::utils::Size;

use super::*;
use crate::testing::{
    self, HEAD_A, HEAD_B, HEAD_C, PROP_HEADS, add_prop_outputs, frame_with_entry, frame_with_width,
    nz, order_from_keys, proptest_cases, set_sorted_output_order, strip_with_frame_ids,
};
use crate::{Action, FocusSource, Notify};

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

    // Floating frame (400x480, centered at (600,210)) over the tiled entry.
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
    // The tiled entry under the float's centre is the one a detach returns to.
    assert_eq!(
        ewm.tiled_entry_under_floating_center(200),
        Some(tiled_entry)
    );
    assert_eq!(ewm.tiled_entry_under_floating_center(4242), None);

    // The modeline and echo area occlude the tiled entry beneath them.
    assert_eq!(ewm.layout_entry_id_under(modeline), None);
    assert_eq!(ewm.layout_entry_id_under(echo_area), None);
    // The floating entry still resolves. The tiled entry still wins beside the float.
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
    fix.ewm()
        .set_focus_frame(4, FocusSource::Test, Notify::None);

    // Lid close: HEAD_A's frames migrate to HEAD_B.
    fix.remove_output(HEAD_A);
    assert_eq!(strip_ids(&fix, HEAD_B), vec![3, 4, 1, 2]);

    // Undock with the lid closed: no fallback, all frames go homeless.
    fix.remove_output(HEAD_B);
    assert!(fix.ewm_ref().layout.monitors().is_empty());
    assert_eq!(fix.ewm_ref().layout.homeless().ids(), vec![3, 4, 1, 2]);
    assert_eq!(fix.ewm_ref().focused_frame_id(), None);

    // Lid open: the first output to map adopts every homeless frame.
    fix.add_output(HEAD_A, 1600, 900);
    assert_eq!(strip_ids(&fix, HEAD_A), vec![3, 4, 1, 2]);
    assert!(fix.ewm_ref().layout.homeless().tiled.is_empty());
    assert_eq!(fix.strip(HEAD_A).active_frame_id(), Some(2));
    assert_eq!(fix.ewm_ref().focused_frame_id(), Some(2));

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
    fix.ewm()
        .set_focus_frame(3, FocusSource::Test, Notify::None);

    fix.remove_output(HEAD_B);
    assert_eq!(strip_ids(&fix, HEAD_A), vec![1]);
    assert_eq!(strip_ids(&fix, HEAD_C), vec![4, 2, 3]);
    assert_eq!(fix.ewm_ref().focused_frame_id(), Some(3));

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
    #![proptest_config(ProptestConfig::with_cases(proptest_cases(64)))]

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
        prop_assert_eq!(fix.ewm_ref().focused_frame_id(), Some(fallback_active_id));
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
        fix.ewm().layout.assign_primaries(&outputs, |_| false);

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
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases(64)))]

    // Mid-slide, popups and hits round each strip frame's position like the renderer, and a
    // float sits where it floats whatever the strip does.
    #[test]
    fn emacs_frame_origin_follows_the_slide_and_the_float(offset in -2000f64..=2000.) {
        let mut fix = testing::Fixture::new().unwrap();
        fix.add_output(HEAD_A, 1600, 900);
        fix.tile(
            HEAD_A,
            vec![frame_with_width(100, 400.0), frame_with_width(101, 400.0)],
        );
        fix.float(HEAD_A, vec![frame_with_width(200, 640.0)]);
        let strip = fix.strip_mut(HEAD_A);
        strip.gesture_begin();
        strip.gesture_update(offset, Duration::ZERO, strip::VIEW_GESTURE_WORKING_AREA_MOVEMENT);
        let expected = (strip.column_x(1) - strip.view_pos()).round() as i32;

        prop_assert_eq!(
            fix.ewm_ref().emacs_frame_origin(101),
            Some(Point::from((expected, 0))),
        );
        prop_assert_eq!(
            fix.ewm_ref().emacs_frame_origin(100),
            Some(Point::from((expected - 400, 0))),
        );
        prop_assert_eq!(
            fix.ewm_ref().emacs_frame_origin(200),
            Some(Point::from((480, 210))),
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases(64)))]

    // A swap moves one frame past one neighbour and the focused frame stays active.

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
        fix.ewm().set_focus_frame(active, FocusSource::Test, Notify::None);
        fix.float(HEAD_A, vec![frame_with_width(200, 640.0)]);
        if pinned {
            fix.ewm().layout.frame_mut(200).unwrap().pinned_to = Some(active);
        }
        fix.ewm().set_focus_frame(200, FocusSource::Test, Notify::None);

        fix.do_action(Action::FocusFrame { delta });

        let to = active_idx as i32 + delta;
        let shown = if (0..len as i32).contains(&to) { ids[to as usize] } else { active };
        prop_assert_eq!(fix.strip(HEAD_A).active_frame_id(), Some(shown));
        let expected = if pinned && shown != active { shown } else { 200 };
        prop_assert_eq!(fix.ewm_ref().focused_frame_id(), Some(expected));
    }

    // Closing the focused float hands the keyboard to the topmost remaining float when that is
    // shown, else to the strip.
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
    focused: Option<u64>,
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
    if let Some(focused) = focused.filter(|id| moved.ids.contains(id)) {
        prop_assert_eq!(
            layout.focused_frame_id(),
            Some(focused),
            "the keyboard follows its frame"
        );
        if on_strip(focused) {
            prop_assert_eq!(layout.active_frame_id(dst), Some(focused));
        }
    } else if let Some(id) = returned.filter(|&id| on_strip(id) && !focused.is_some_and(on_strip)) {
        prop_assert_eq!(
            layout.active_frame_id(dst),
            Some(id),
            "the remembered frame returns"
        );
    }
    Ok(())
}

/// Every frame of `ids` sitting on `dst` is homed there.
fn check_homed(layout: &Layout, dst: &Output, ids: &[u64]) -> Result<(), TestCaseError> {
    let identity = crate::utils::output_identity(dst);
    for location in ids.iter().filter_map(|&id| layout.frame_location(id)) {
        if location.monitor.output() == dst {
            prop_assert_eq!(&location.frame.home, &identity);
        }
    }
    Ok(())
}

/// A live drag: the output and point of its last update, and whether the grab was inside a float.
type Drag = Option<(usize, Point<f64, Logical>, bool)>;

fn apply_layout_op(
    layout: &mut Layout,
    outputs: &[Output],
    live: &mut BTreeSet<u64>,
    remembered: &mut BTreeMap<String, Option<u64>>,
    drag: &mut Drag,
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
            // Closing the focused float hands the keyboard to the topmost remaining float when
            // that is shown, else to the strip.
            let float_focus = (focused == Some(id)).then(|| {
                layout.monitors.iter().find_map(|monitor| {
                    monitor
                        .floating
                        .frames()
                        .iter()
                        .find(|float| float.surface_id == id)?;
                    let next = monitor
                        .floating
                        .frames()
                        .iter()
                        .find(|float| float.surface_id != id)
                        .map(|float| (float.surface_id, monitor.float_shown(float)));
                    Some((monitor.strip.active_frame_id(), next))
                })
            });
            layout.remove_frame(id, active_close);
            live.remove(&id);
            prop_assert!(!layout.has_frame(id));
            if let Some(Some((Some(active), next))) = float_focus {
                let expected = match next {
                    Some((float, true)) => float,
                    _ => active,
                };
                prop_assert_eq!(layout.focused_frame_id(), Some(expected));
            }
        }
        LayoutOp::MoveFrame { id, delta } => {
            let before = layout.frame_output(id).map(str::to_owned);
            let output = before
                .as_deref()
                .and_then(|name| outputs.iter().find(|output| output.name() == name));
            let strip_before = output.and_then(|output| {
                let strip = layout.strip(output)?;
                Some((
                    strip.frame_ids().collect::<Vec<_>>(),
                    strip.active_frame_id(),
                ))
            });
            layout.move_frame(id, delta);
            prop_assert_eq!(layout.frame_output(id), before.as_deref());
            // The frame moves by `delta` slots within its strip, or not at all past an end, and
            // neither the active frame nor the keyboard moves with it.
            if let (Some(output), Some((ids, active))) = (output, strip_before)
                && let Some(from) = ids.iter().position(|&frame| frame == id)
            {
                let mut expected = ids.clone();
                if let Some(to) = from.checked_add_signed(delta as isize)
                    && to < ids.len()
                {
                    let frame = expected.remove(from);
                    expected.insert(to, frame);
                }
                let strip = layout.strip(output).unwrap();
                prop_assert_eq!(strip.frame_ids().collect::<Vec<_>>(), expected);
                prop_assert_eq!(strip.active_frame_id(), active);
                prop_assert_eq!(layout.focused_frame_id(), focused);
            }
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
            check_homed(layout, dst, &[id])?;
            if !moved.ids.is_empty() {
                check_landed(layout, dst, &moved, focused, remembered)?;
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
            if let Some(change) = layout.activate_frame(id, None)
                && layout
                    .strip(&change.output)
                    .is_some_and(|strip| strip.index_of(id).is_some())
            {
                prop_assert_eq!(layout.active_frame_id(&change.output), Some(id));
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
                let point = Point::from((px, py));
                let inside = layout
                    .monitor_of(&outputs[output])
                    .and_then(|monitor| monitor.floating.frame_with_data(id))
                    .is_some_and(|(_, data)| {
                        Rectangle::new(data.logical_pos(), data.size()).contains(point)
                    });
                *drag = Some((output, point, inside));
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
                // The drag carries the frame onto the output under the pointer and homes it there.
                prop_assert_eq!(layout.frame_output(id), Some(PROP_HEADS[output]));
                check_homed(layout, dst, &[id])?;
                if !moved.ids.is_empty() {
                    check_landed(layout, dst, moved, focused, remembered)?;
                }
            }
            prop_assert_eq!(moved.is_none(), layout.interactive_move.is_none());
            if let (Some(state), Some(_)) = (drag.as_mut(), &moved) {
                state.0 = output;
                state.1 = Point::from((px, py));
            }
        }
        LayoutOp::DragEnd(id) => {
            let dragged = layout.interactive_move.map(|move_| move_.frame_id);
            layout.interactive_move_end(id);
            prop_assert!(
                layout
                    .interactive_move
                    .is_none_or(|move_| move_.frame_id != id)
            );
            // A float dropped on an output stays there, under the pointer that dropped it.
            if dragged == Some(id)
                && let Some((output, point, inside)) = drag.take()
                && let Some(monitor) = layout.monitor_of(&outputs[output])
                && let Some((_, data)) = monitor.floating.frame_with_data(id)
            {
                let rect = Rectangle::new(data.logical_pos(), data.size());
                prop_assert!(
                    !inside || rect.contains(point),
                    "{rect:?} dropped away from {point:?}"
                );
            }
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

fn check_layout_ops(ops: &[LayoutOp]) -> Result<(), TestCaseError> {
    let mut layout = testing::layout_with(
        PROP_HEADS
            .iter()
            .map(|name| (*name, strip::Strip::new(animation::Clock::default())))
            .collect(),
    );
    let outputs: Vec<Output> = layout.outputs().cloned().collect();
    let mut live = BTreeSet::new();
    let mut remembered = BTreeMap::new();
    let mut drag = None;
    for op in ops {
        apply_layout_op(
            &mut layout,
            &outputs,
            &mut live,
            &mut remembered,
            &mut drag,
            op,
        )?;
        verify_live_frames(&layout, &live)?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases(256)))]

    #[test]
    fn layout_invariants_hold_under_any_op_sequence(
        ops in prop::collection::vec(layout_op(), 1..40),
    ) {
        check_layout_ops(&ops)?;
    }
}

/// A sequence the randomised test once found failing.
#[test]
fn a_pin_onto_a_frame_added_after_the_float() {
    let add = |id, floating| LayoutOp::AddFrame {
        id,
        output: 0,
        floating,
        after: None,
        filler: false,
    };
    check_layout_ops(&[
        add(7, true),
        add(1, false),
        add(5, false),
        LayoutOp::Pin { float: 7, frame: 5 },
    ])
    .unwrap();
}
