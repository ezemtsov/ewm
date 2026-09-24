//! Overview through the headless fixture: zoom state, hit-testing and
//! click-to-activate.

use ewm_core::strip::Frame;
use ewm_core::testing::Fixture;
use ewm_core::{Event, LayoutEntry, LayoutEntryId};
use smithay::utils::{Logical, Point};

const OUTPUT: &str = "HEAD-A";
const WIDTH: i32 = 1920;
const HEIGHT: i32 = 1080;
/// `BTN_LEFT` from linux/input-event-codes.h.
const BTN_LEFT: u32 = 0x110;

fn nz(n: u64) -> LayoutEntryId {
    std::num::NonZeroU64::new(n).unwrap()
}

fn frame(surface_id: u64, width: f64) -> Frame {
    let mut frame = Frame::new(surface_id);
    frame.width = width;
    frame.height = f64::from(HEIGHT);
    frame
}

fn frame_with_entry(surface_id: u64, entry_id: u64, entry_surface: u64) -> Frame {
    let mut frame = frame(surface_id, f64::from(WIDTH));
    frame.entries.push(LayoutEntry::surface(
        nz(entry_id),
        "entry".to_string(),
        entry_surface,
        0,
        0,
        WIDTH as u32,
        HEIGHT as u32,
    ));
    frame.selected_entry_id = Some(nz(entry_id));
    frame
}

fn fixture(frames: Vec<Frame>) -> Fixture {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(OUTPUT, WIDTH, HEIGHT);
    fix.ewm().animations_clock.set_complete_instantly(true);
    fix.apply_output_layout_command(OUTPUT, frames);
    fix.dispatch_roundtrip(2);
    fix
}

fn active_idx(fix: &Fixture) -> usize {
    fix.ewm_ref()
        .frame_set
        .mapped_strip(OUTPUT)
        .unwrap()
        .active_idx()
}

fn click(fix: &mut Fixture, x: f64, y: f64) {
    fix.pointer_motion_to(x, y);
    fix.pointer_button(BTN_LEFT, true);
    fix.pointer_button(BTN_LEFT, false);
    fix.dispatch_roundtrip(2);
}

#[test]
fn open_close_toggle_track_state_and_zoom() {
    let mut fix = fixture(vec![frame_with_entry(10, 1, 100)]);
    let ewm = fix.ewm();
    assert!(!ewm.overview.is_open());
    assert_eq!(ewm.overview.zoom(), 1.0);

    ewm.overview_toggle();
    assert!(ewm.overview.is_open());
    assert_eq!(ewm.overview.zoom(), 0.5);
    assert!(!ewm.overview_open(), "open is a no-op while open");

    assert!(ewm.overview_close());
    assert!(!ewm.overview.is_open());
    assert!(!ewm.overview_close(), "close is a no-op while closed");
    fix.dispatch_roundtrip(2);
    assert_eq!(fix.ewm_ref().overview.zoom(), 1.0);
}

#[test]
fn overview_events_follow_the_open_state() {
    let mut fix = fixture(vec![frame_with_entry(10, 1, 100)]);
    fix.enable_event_capture();

    fix.ewm().overview_toggle();
    fix.ewm().overview_open();
    fix.ewm().overview_close();
    fix.ewm().overview_close();
    let opens: Vec<bool> = fix
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Overview { open } => Some(open),
            _ => None,
        })
        .collect();
    assert_eq!(opens, [true, false]);

    // A gesture reports opening when it begins and closing when it snaps back.
    fix.ewm().overview_gesture_begin();
    fix.ewm().overview_gesture_end();
    let opens: Vec<bool> = fix
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Overview { open } => Some(open),
            _ => None,
        })
        .collect();
    assert_eq!(opens, [true, false]);
}

#[test]
fn zoomed_content_takes_no_pointer_hits() {
    let mut fix = fixture(vec![frame_with_entry(10, 1, 100)]);
    let pos: Point<f64, Logical> = Point::from((960.0, 540.0));
    assert_eq!(fix.ewm_ref().layout_entry_id_under(pos), Some(nz(1)));

    fix.ewm().overview_toggle();
    assert_eq!(fix.ewm_ref().layout_entry_id_under(pos), None);
    assert!(fix.ewm_ref().surface_under_point(pos).is_none());

    fix.ewm().overview_toggle();
    // Headless has no redraw loop, so settle the finished close animation by hand.
    fix.ewm().overview.advance();
    assert!(!fix.ewm_ref().overview.is_active());
    assert_eq!(fix.ewm_ref().layout_entry_id_under(pos), Some(nz(1)));
}

#[test]
fn click_on_zoomed_neighbour_activates_it_and_closes() {
    let mut fix = fixture(vec![
        frame_with_entry(10, 1, 100),
        frame_with_entry(11, 2, 101),
    ]);
    let neighbour_focus_id =
        fix.ewm_ref().frame_set.mapped_strip(OUTPUT).unwrap().frames[1].focus_id;
    fix.ewm().overview_toggle();
    fix.enable_event_capture();

    // Frame 1 starts at strip x 1920; at zoom 0.5 about the output centre that is screen x 1440.
    click(&mut fix, 1500.0, 540.0);

    assert_eq!(active_idx(&fix), 1);
    assert!(!fix.ewm_ref().overview.is_open());
    let strip = fix.ewm_ref().frame_set.mapped_strip(OUTPUT).unwrap();
    assert!(
        strip.view_offset.is_static(),
        "the strip snaps onto the picked frame"
    );
    assert_eq!(strip.view_offset.current(), 0.0);
    // Pointer focus with the click mapped into the frame, so Emacs does not warp the pointer.
    assert!(fix.drain_events().iter().any(|event| matches!(
        event,
        Event::Focus { focus_id, x: Some(x), y: Some(y), pointer: true }
            if *focus_id == neighbour_focus_id && *x == 120.0 && *y == 540.0
    )));
}

#[test]
fn click_on_empty_zoomed_output_closes_without_switching() {
    let mut fix = fixture(vec![frame(10, 800.0), frame(11, 800.0)]);
    fix.ewm().overview_toggle();

    // Strip x 1840 is past the second frame; on screen that is x 1400, inside the zoomed output.
    click(&mut fix, 1400.0, 540.0);

    assert_eq!(active_idx(&fix), 0);
    assert!(!fix.ewm_ref().overview.is_open());
}

#[test]
fn click_on_backdrop_is_ignored() {
    let mut fix = fixture(vec![frame_with_entry(10, 1, 100)]);
    fix.ewm().overview_toggle();

    click(&mut fix, 100.0, 100.0);

    assert_eq!(active_idx(&fix), 0);
    assert!(fix.ewm_ref().overview.is_open());
}

#[test]
fn overview_hides_top_layer_deferral_for_fullscreen() {
    let mut fix = fixture(vec![frame_with_entry(10, 1, 100)]);
    fix.ewm().frame_set.mapped_strip_mut(OUTPUT).unwrap().frames[0].set_fullscreen_entry(nz(1));
    let output = fix
        .ewm_ref()
        .space
        .outputs()
        .find(|output| output.name() == OUTPUT)
        .cloned()
        .unwrap();
    assert!(fix.ewm_ref().render_above_top_layer(&output));

    fix.ewm().overview_toggle();
    assert!(!fix.ewm_ref().render_above_top_layer(&output));
}
