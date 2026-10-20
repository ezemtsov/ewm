//! Client fullscreen requests are the compositor's call: a tiled view goes fullscreen, a float's
//! view covers the output in place, pinned to the strip frame active then, and what the views show
//! follows the client's commit.

use std::time::Duration;

use ewm_core::layout::floating::SizeFracPoint;
use ewm_core::layout::strip::Frame;
use ewm_core::testing::{Fixture, TestClient, Toplevel, nz};
use ewm_core::{Event, FocusSource, LayoutEntry, Notify};
use smithay::output::Output;
use smithay::utils::{Logical, Point, Size};

const OUTPUT: &str = "HEAD-A";
const CLIENT_W: u32 = 400;
const CLIENT_H: u32 = 300;
const AT: SizeFracPoint = SizeFracPoint { x: 0.05, y: 0.05 };
const MID: SizeFracPoint = SizeFracPoint { x: 0.45, y: 0.45 };

fn frame(surface_id: u64, entries: Vec<LayoutEntry>, pos: Option<SizeFracPoint>) -> Frame {
    let mut frame = Frame::new(surface_id);
    frame.width = CLIENT_W as f64;
    frame.height = CLIENT_H as f64;
    frame.entries = entries;
    frame.floating_pos = pos;
    frame
}

fn tile(surface_id: u64, entries: Vec<LayoutEntry>) -> Frame {
    let mut frame = frame(surface_id, entries, None);
    frame.width = 1920.0;
    frame.height = 0.0;
    frame
}

fn entry(id: u64, surface_id: u64) -> LayoutEntry {
    LayoutEntry::surface(nz(id), "view".into(), surface_id, 0, 0, CLIENT_W, CLIENT_H)
}

fn size() -> Size<i32, Logical> {
    Size::from((CLIENT_W as i32, CLIENT_H as i32))
}

fn fixture() -> Fixture {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(OUTPUT, 1920, 1080);
    fix
}

fn unmapped_client(fix: &mut Fixture) -> (TestClient, Toplevel) {
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let top = fix.open_toplevel_unmapped(&mut client, "mpv", None);
    (client, top)
}

fn client(fix: &mut Fixture) -> (TestClient, Toplevel, u64) {
    let (mut client, top) = unmapped_client(fix);
    fix.map_toplevel(&mut client, &top, size());
    let sid = fix.find_surface_id(&client, &top.surface).unwrap();
    (client, top, sid)
}

fn settle(fix: &mut Fixture, client: &mut TestClient) {
    fix.roundtrip(client);
    fix.roundtrip(client);
}

/// The frame whose fullscreen view covers `output`.
fn presenting(fix: &Fixture, output: &Output) -> Option<u64> {
    fix.ewm_ref()
        .layout
        .monitor_of(output)
        .unwrap()
        .presenting_fullscreen()
}

/// Where float `id` on `output` sits, and its size.
fn float_rect(
    fix: &Fixture,
    output: &Output,
    id: u64,
) -> (Point<f64, Logical>, Size<f64, Logical>) {
    let floating = fix.ewm_ref().layout.floating(output).unwrap();
    let (_, data) = floating.frame_with_data(id).expect("not a float");
    (data.logical_pos(), data.size())
}

/// The strip frame float `id` on `output` is pinned to.
fn pinned_to(fix: &Fixture, output: &Output, id: u64) -> Option<u64> {
    fix.ewm_ref()
        .layout
        .monitor_of(output)
        .unwrap()
        .floating
        .frame(id)
        .unwrap()
        .pinned_to
}

/// Emacs's command and the client's request set the bit alike, every request is answered with a
/// configure at once, and what the views show follows the client's commit.
#[test]
fn views_follow_the_client_commit() {
    let mut fix = fixture();
    fix.freeze_clock();
    fix.freeze_clock();
    let (mut client, top, sid) = client(&mut fix);
    fix.tile(OUTPUT, vec![tile(500, vec![entry(7, sid)])]);
    settle(&mut fix, &mut client);
    client.drain_events();
    let output = fix.output(OUTPUT);
    let shown = |fix: &Fixture| {
        let ewm = fix.ewm_ref();
        (
            ewm.layout.is_surface_fullscreen(sid),
            ewm.render_above_top_layer(&output),
            ewm.surfaces[&sid].fullscreen,
        )
    };

    fix.set_fullscreen(nz(7), true);
    assert!(fix.configured(&mut client).contains(&(1920, 1080)));
    assert!(fix.ewm_ref().pending_fullscreen(sid));
    assert_eq!(
        shown(&fix),
        (false, false, false),
        "shown before the commit"
    );
    // The client keeps its 400x300 buffer, which the output centers.
    fix.map_toplevel(&mut client, &top, size());
    assert_eq!(
        shown(&fix),
        (true, true, true),
        "not shown after the commit"
    );
    assert_eq!(
        fix.ewm_ref().surface_view_origins(sid),
        [Point::<i32, Logical>::from((760, 390))]
    );

    top.xdg_toplevel.unset_fullscreen();
    assert!(
        fix.configured(&mut client)
            .contains(&(CLIENT_W as i32, CLIENT_H as i32))
    );
    assert!(!fix.ewm_ref().pending_fullscreen(sid));
    assert_eq!(shown(&fix), (true, true, true), "left before the commit");
    fix.map_toplevel(&mut client, &top, size());
    assert_eq!(shown(&fix), (false, false, false), "shown after the commit");

    // A request that changes nothing is answered all the same.
    settle(&mut fix, &mut client);
    client.drain_events();
    top.xdg_toplevel.unset_fullscreen();
    assert_eq!(
        fix.configured(&mut client),
        [(CLIENT_W as i32, CLIENT_H as i32)]
    );
    top.xdg_toplevel.set_fullscreen(None);
    assert!(
        fix.answer_configure(&mut client, &top)
            .contains(&(1920, 1080))
    );
    assert_eq!(shown(&fix), (true, true, true));
    settle(&mut fix, &mut client);
    client.drain_events();
    top.xdg_toplevel.set_fullscreen(None);
    assert_eq!(fix.configured(&mut client), [(1920, 1080)]);
}

/// A float stays a float, focused or not: its view covers the output on the commit that shows it
/// fullscreen, pinned to the strip frame active then, and takes the focus from that frame. Its
/// size and place are untouched when the client leaves fullscreen or the view leaves the layout.
#[test]
fn float_fullscreens_in_place_over_the_active_frame() {
    let mut fix = fixture();
    fix.freeze_clock();
    fix.freeze_clock();
    let (mut client, top, sid) = client(&mut fix);
    fix.tile(OUTPUT, vec![tile(400, vec![])]);
    fix.float(OUTPUT, vec![frame(500, vec![entry(7, sid)], Some(AT))]);
    fix.roundtrip(&mut client);
    client.drain_events();
    let output = fix.output(OUTPUT);
    let strip_ids = |fix: &Fixture| fix.strip(OUTPUT).frame_ids().collect::<Vec<_>>();
    let before = float_rect(&fix, &output, 500);
    fix.ewm()
        .set_focus_frame(400, FocusSource::Test, Notify::None);
    fix.enable_event_capture();

    top.xdg_toplevel.set_fullscreen(None);
    assert!(fix.configured(&mut client).contains(&(1920, 1080)));
    assert!(
        !fix.ewm_ref().layout.is_surface_fullscreen(sid),
        "shown before the commit"
    );
    fix.map_toplevel(&mut client, &top, Size::from((1920, 1080)));
    assert!(
        fix.drain_events()
            .iter()
            .any(|event| matches!(event, Event::Focus { focus_id, .. } if *focus_id == nz(7))),
        "Emacs was not told the view took the focus"
    );
    {
        let ewm = fix.ewm_ref();
        assert_eq!(strip_ids(&fix), [400], "the strip changed");
        assert_eq!(pinned_to(&fix, &output, 500), Some(400));
        assert_eq!(presenting(&fix, &output), Some(500));
        assert!(ewm.layout.is_surface_fullscreen(sid));
        assert!(ewm.render_above_top_layer(&output));
        assert_eq!(
            (ewm.focused_frame_id(), ewm.focused_entry_id()),
            (500, Some(nz(7))),
            "the view did not take the focus"
        );
        assert_eq!(float_rect(&fix, &output, 500), before, "the float moved");
        // The view covers the output, so the strip beneath takes no hits.
        assert_eq!(
            ewm.layout_entry_id_under(Point::from((960., 540.))),
            Some(nz(7))
        );
        assert_eq!(
            ewm.layout_entry_id_under(Point::from((5., 5.))),
            Some(nz(7))
        );
        assert!(
            ewm.layout.entry_outputs().any(|(id, _)| id == nz(7)),
            "the float's view is not castable"
        );
    }

    top.xdg_toplevel.unset_fullscreen();
    settle(&mut fix, &mut client);
    assert!(
        fix.ewm_ref().layout.is_surface_fullscreen(sid),
        "left before the commit"
    );
    fix.map_toplevel(&mut client, &top, size());
    assert!(!fix.ewm_ref().layout.is_surface_fullscreen(sid));
    assert_eq!(pinned_to(&fix, &output, 500), None);
    assert_eq!(presenting(&fix, &output), None);
    assert_eq!(float_rect(&fix, &output, 500), before);

    top.xdg_toplevel.set_fullscreen(None);
    fix.answer_configure(&mut client, &top);
    assert!(fix.ewm_ref().layout.is_surface_fullscreen(sid));
    fix.float(OUTPUT, vec![frame(500, vec![], Some(AT))]);
    let ewm = fix.ewm_ref();
    assert!(!ewm.layout.is_surface_fullscreen(sid));
    // Left without views, the float stays with its frame.
    assert_eq!(pinned_to(&fix, &output, 500), Some(400));
    assert_eq!(presenting(&fix, &output), None);
    // The surface stays fullscreen for when a view shows it again, and external tools see that.
    assert!(ewm.pending_fullscreen(sid));
    assert!(ewm.surfaces[&sid].fullscreen);
}

/// A fullscreen float is pinned to the strip frame it went fullscreen over: another frame coming
/// to the front hides it, frame and views, its own frame brings it back with the focus, and
/// focusing it brings its frame to the front.
#[test]
fn fullscreen_float_stays_with_its_frame() {
    let mut fix = fixture();
    fix.ewm().animations_clock.set_complete_instantly(true);
    let (mut client, top, sid) = client(&mut fix);
    fix.tile(OUTPUT, vec![tile(400, vec![]), tile(401, vec![])]);
    fix.float(OUTPUT, vec![frame(500, vec![entry(7, sid)], Some(AT))]);
    fix.ewm()
        .set_focus_frame(500, FocusSource::Test, Notify::None);
    top.xdg_toplevel.set_fullscreen(None);
    fix.answer_configure(&mut client, &top);
    let output = fix.output(OUTPUT);
    let center: Point<f64, Logical> = Point::from((960., 540.));
    let shown = |fix: &Fixture| {
        let monitor = fix.ewm_ref().layout.monitor_of(&output).unwrap();
        monitor.float_shown(monitor.floating.frame(500).unwrap())
    };
    assert_eq!(fix.strip(OUTPUT).active_frame_id(), Some(400));
    assert_eq!(pinned_to(&fix, &output, 500), Some(400));
    assert_eq!(fix.ewm_ref().layout_entry_id_under(center), Some(nz(7)));
    fix.enable_event_capture();

    fix.ewm()
        .set_focus_frame(401, FocusSource::Test, Notify::None);
    assert_eq!(fix.strip(OUTPUT).active_frame_id(), Some(401));
    assert!(!shown(&fix), "travelled with the strip");
    assert_eq!(presenting(&fix, &output), None);
    assert!(!fix.ewm_ref().render_above_top_layer(&output));
    assert_ne!(fix.ewm_ref().layout_entry_id_under(center), Some(nz(7)));
    assert!(
        fix.drain_events()
            .iter()
            .any(|event| matches!(event, Event::FrameHidden { id: 500 })),
        "no frame_hidden for the float"
    );

    fix.ewm()
        .set_focus_frame(400, FocusSource::Test, Notify::Emacs);
    assert!(shown(&fix));
    assert_eq!(presenting(&fix, &output), Some(500));
    {
        let ewm = fix.ewm_ref();
        assert_eq!(
            (ewm.focused_frame_id(), ewm.focused_entry_id()),
            (500, Some(nz(7))),
            "the frame kept the focus under the view"
        );
    }
    assert!(
        fix.drain_events()
            .iter()
            .any(|event| matches!(event, Event::Focus { focus_id, .. } if *focus_id == nz(7))),
        "Emacs was not told the view took the focus"
    );
    fix.ewm()
        .set_focus_frame(401, FocusSource::Test, Notify::None);
    fix.ewm()
        .set_focus_frame(500, FocusSource::Test, Notify::None);
    assert_eq!(
        fix.strip(OUTPUT).active_frame_id(),
        Some(400),
        "focusing the float left its frame behind"
    );
    fix.ewm()
        .set_focus_frame(401, FocusSource::Test, Notify::None);
    fix.drain_events();

    // Its view dying there leaves it with its frame, off screen.
    top.xdg_toplevel.destroy();
    top.xdg_surface.destroy();
    top.surface.destroy();
    fix.roundtrip(&mut client);
    assert_eq!(pinned_to(&fix, &output, 500), Some(400));
    assert!(!shown(&fix), "came to the front with its view gone");
    assert!(
        !fix.drain_events()
            .iter()
            .any(|event| matches!(event, Event::FrameShown { id: 500 })),
        "frame_shown for a float that stays off screen"
    );

    fix.ewm()
        .set_focus_frame(400, FocusSource::Test, Notify::None);
    assert!(shown(&fix));
    assert_eq!(presenting(&fix, &output), None);
    assert!(
        fix.drain_events()
            .iter()
            .any(|event| matches!(event, Event::FrameShown { id: 500 })),
        "no frame_shown for the float"
    );
}

/// A pinned float rides in its frame's slot: a frame switch slides it out with the frame and
/// back in with it, for the renderer and the pointer alike.
#[test]
fn pinned_float_slides_with_its_frame() {
    let mut fix = fixture();
    let (mut client, top, sid) = client(&mut fix);
    fix.tile(OUTPUT, vec![tile(400, vec![]), tile(401, vec![])]);
    fix.float(OUTPUT, vec![frame(500, vec![entry(7, sid)], Some(AT))]);
    fix.ewm()
        .set_focus_frame(400, FocusSource::Test, Notify::None);
    top.xdg_toplevel.set_fullscreen(None);
    fix.answer_configure(&mut client, &top);
    let center: Point<f64, Logical> = Point::from((960., 540.));
    // The view's slot starts where its frame does.
    let view_x = |fix: &Fixture| fix.ewm_ref().surface_view_origins(sid)[0].x;
    let hit = |fix: &Fixture| fix.ewm_ref().layout_entry_id_under(center) == Some(nz(7));
    assert_eq!(view_x(&fix), 0);
    assert!(hit(&fix));

    // With the clock held, the slide has not begun.
    let now = fix.ewm_ref().animations_clock.now();
    fix.ewm().animations_clock.set(now);
    fix.ewm()
        .set_focus_frame(401, FocusSource::Test, Notify::None);
    assert_eq!(fix.strip(OUTPUT).active_frame_id(), Some(401));
    assert_eq!(fix.strip(OUTPUT).screen_x_of_frame(0), 0.);
    assert_eq!(view_x(&fix), 0, "popped off with the switch");
    assert!(hit(&fix));

    // Slid out, it has gone as far as its frame.
    let later = now + Duration::from_secs(5);
    fix.ewm().animations_clock.set(later);
    let gone = fix.strip(OUTPUT).screen_x_of_frame(0);
    assert!(gone <= -1920., "{gone}");
    assert_eq!(view_x(&fix), gone.round() as i32);
    assert!(!hit(&fix));

    // And back in with it.
    fix.ewm()
        .set_focus_frame(400, FocusSource::Test, Notify::None);
    assert_eq!(
        view_x(&fix),
        gone.round() as i32,
        "popped on with the switch"
    );
    fix.ewm()
        .animations_clock
        .set(later + Duration::from_secs(5));
    assert_eq!(view_x(&fix), 0);
    assert!(hit(&fix));
}

/// Super+drag on a fullscreen float takes it out of fullscreen and moves it, keeping the
/// pointer's grip on it.
#[test]
fn dragging_a_fullscreen_float_unfullscreens_and_moves_it() {
    const BTN_LEFT: u32 = 0x110;
    const KEY_LEFTMETA: u32 = 125 + 8;
    let mut fix = fixture();
    fix.ewm().animations_clock.set_complete_instantly(true);
    let (mut client, top, sid) = client(&mut fix);
    fix.tile(OUTPUT, vec![tile(400, vec![])]);
    fix.float(OUTPUT, vec![frame(500, vec![entry(7, sid)], Some(AT))]);
    fix.roundtrip(&mut client);
    fix.ewm()
        .set_focus_frame(500, FocusSource::Test, Notify::None);
    top.xdg_toplevel.set_fullscreen(None);
    settle(&mut fix, &mut client);
    fix.map_toplevel(&mut client, &top, Size::from((1920, 1080)));
    let output = fix.output(OUTPUT);
    assert_eq!(presenting(&fix, &output), Some(500));
    client.drain_events();

    // Grab the view a quarter of the way in.
    fix.keyboard_key(KEY_LEFTMETA, true);
    fix.pointer_motion_to(480.0, 270.0);
    fix.pointer_button(BTN_LEFT, true);
    assert!(fix.ewm_ref().pointer.is_grabbed(), "no drag started");
    assert!(!fix.ewm_ref().pending_fullscreen(sid));
    assert!(
        fix.configured(&mut client)
            .contains(&(CLIENT_W as i32, CLIENT_H as i32)),
        "not configured back to its float size"
    );

    // The view covers the output until the client commits.
    fix.pointer_motion_to(960.0, 540.0);
    assert_eq!(
        presenting(&fix, &output),
        Some(500),
        "left fullscreen before the commit"
    );
    fix.map_toplevel(&mut client, &top, size());
    assert_eq!(presenting(&fix, &output), None, "did not leave fullscreen");

    // Back at its size, it sits under the pointer with the same grip.
    fix.pointer_motion_to(960.0, 540.0);
    let (pos, _) = float_rect(&fix, &output, 500);
    let expected: Point<f64, Logical> =
        Point::from((960. - 0.25 * CLIENT_W as f64, 540. - 0.25 * CLIENT_H as f64));
    assert!(
        (pos.x - expected.x).abs() < 0.5 && (pos.y - expected.y).abs() < 0.5,
        "{pos:?} is not {expected:?}"
    );
    fix.ewm_ref().layout.verify_invariants();

    fix.pointer_button(BTN_LEFT, false);
    fix.keyboard_key(KEY_LEFTMETA, false);
    assert!(!fix.ewm_ref().pointer.is_grabbed());
}

/// Floats hide under a fullscreen view, for the renderer and the pointer, unless one has focus.
#[test]
fn floats_hide_under_a_fullscreen_view_unless_focused() {
    let mut fix = fixture();
    let (mut tiled_client, tiled_top, tiled_id) = client(&mut fix);
    let (mut float_client, _, float_id) = client(&mut fix);
    fix.tile(OUTPUT, vec![tile(500, vec![entry(7, tiled_id)])]);
    fix.float(
        OUTPUT,
        vec![frame(600, vec![entry(8, float_id)], Some(MID))],
    );
    let output = fix.output(OUTPUT);
    // Inside the float and inside the 400x300 window a fullscreen view centers.
    let pos = float_rect(&fix, &output, 600).0 + Point::from((40.0, 20.0));
    fix.ewm()
        .set_focus_entry(nz(7), FocusSource::Test, Notify::None);
    assert_eq!(fix.ewm_ref().layout_entry_id_under(pos), Some(nz(8)));

    fix.set_fullscreen(nz(7), true);
    fix.answer_configure(&mut tiled_client, &tiled_top);
    settle(&mut fix, &mut float_client);
    assert_eq!(fix.ewm_ref().layout_entry_id_under(pos), Some(nz(7)));

    fix.ewm()
        .set_focus_entry(nz(8), FocusSource::Test, Notify::None);
    assert_eq!(fix.ewm_ref().layout_entry_id_under(pos), Some(nz(8)));
}

/// A request before the first buffer sizes the client for the active output and is applied to
/// its first view; one withdrawn before then leaves nothing behind.
#[test]
fn request_before_the_first_view_waits_for_it() {
    let mut fix = fixture();
    fix.add_output("HEAD-B", 2560, 1440);
    let (mut client, kept) = unmapped_client(&mut fix);
    let kept_id = fix.find_surface_id(&client, &kept.surface).unwrap();
    let withdrawn = fix.open_toplevel_unmapped(&mut client, "mpv", None);
    let withdrawn_id = fix.find_surface_id(&client, &withdrawn.surface).unwrap();
    client.drain_events();

    kept.xdg_toplevel.set_fullscreen(None);
    withdrawn.xdg_toplevel.set_fullscreen(None);
    withdrawn.xdg_toplevel.unset_fullscreen();
    let sizes = fix.configured(&mut client);
    assert!(sizes.contains(&(1920, 1080)), "{sizes:?}");
    assert!(sizes.iter().all(|&(w, _)| w <= 1920), "{sizes:?}");

    fix.map_toplevel(&mut client, &kept, size());
    fix.map_toplevel(&mut client, &withdrawn, size());
    assert!(!fix.ewm_ref().layout.is_surface_fullscreen(kept_id));

    fix.tile_command(
        OUTPUT,
        vec![
            tile(500, vec![entry(7, kept_id)]),
            tile(600, vec![entry(8, withdrawn_id)]),
        ],
    );
    settle(&mut fix, &mut client);
    assert!(fix.ewm_ref().layout.is_surface_fullscreen(kept_id));
    assert!(!fix.ewm_ref().layout.is_surface_fullscreen(withdrawn_id));
}

/// One view is fullscreen per strip frame: a request from the frame's own view puts the float
/// fullscreen over it back, and the other way round, while a float over another frame is left be.
#[test]
fn one_fullscreen_per_frame() {
    let mut fix = fixture();
    let (mut tiled_client, tiled_top, tiled_id) = client(&mut fix);
    let (mut float_client, float_top, float_id) = client(&mut fix);
    let (mut other_client, other_top, other_id) = client(&mut fix);
    fix.tile(
        OUTPUT,
        vec![tile(400, vec![entry(7, tiled_id)]), tile(401, vec![])],
    );
    fix.float(
        OUTPUT,
        vec![
            frame(500, vec![entry(8, float_id)], Some(AT)),
            frame(600, vec![entry(9, other_id)], Some(MID)),
        ],
    );
    let output = fix.output(OUTPUT);
    // The other float goes fullscreen over frame 401 first.
    fix.ewm()
        .set_focus_frame(401, FocusSource::Test, Notify::None);
    other_top.xdg_toplevel.set_fullscreen(None);
    fix.answer_configure(&mut other_client, &other_top);
    assert_eq!(presenting(&fix, &output), Some(600));
    fix.ewm()
        .set_focus_frame(400, FocusSource::Test, Notify::None);
    assert_eq!(presenting(&fix, &output), None);

    float_top.xdg_toplevel.set_fullscreen(None);
    fix.answer_configure(&mut float_client, &float_top);
    assert_eq!(presenting(&fix, &output), Some(500));
    settle(&mut fix, &mut tiled_client);
    tiled_client.drain_events();
    float_client.drain_events();

    // The frame's own view takes fullscreen from the float over it.
    tiled_top.xdg_toplevel.set_fullscreen(None);
    settle(&mut fix, &mut tiled_client);
    settle(&mut fix, &mut float_client);
    assert!(
        !fix.ewm_ref().pending_fullscreen(float_id),
        "the float keeps its bit"
    );
    assert!(fix.ewm_ref().pending_fullscreen(tiled_id));
    assert!(
        fix.ewm_ref().pending_fullscreen(other_id),
        "a float over another frame lost its bit"
    );
    assert!(
        float_client
            .configured_sizes()
            .contains(&(CLIENT_W as i32, CLIENT_H as i32)),
        "the float was not configured back"
    );
    fix.map_toplevel(&mut float_client, &float_top, size());
    fix.answer_configure(&mut tiled_client, &tiled_top);
    assert_eq!(presenting(&fix, &output), Some(400));
    assert_eq!(pinned_to(&fix, &output, 500), None);

    // And the float takes it back from the frame's view.
    tiled_client.drain_events();
    float_top.xdg_toplevel.set_fullscreen(None);
    settle(&mut fix, &mut float_client);
    settle(&mut fix, &mut tiled_client);
    assert!(!fix.ewm_ref().pending_fullscreen(tiled_id));
    assert!(
        tiled_client
            .configured_sizes()
            .iter()
            .any(|&size| size != (1920, 1080)),
        "the frame's view was not configured back"
    );
    fix.answer_configure(&mut tiled_client, &tiled_top);
    fix.answer_configure(&mut float_client, &float_top);
    assert_eq!(presenting(&fix, &output), Some(500));
}
