//! Client fullscreen requests are Emacs's call: the compositor reports them and
//! presents only the tiled entries Emacs selects.

use ewm_core::event::Event;
use ewm_core::floating::SizeFracPoint;
use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient, Toplevel};
use ewm_core::{LayoutEntry, LayoutEntryId};
use smithay::utils::{Logical, Point, Size};

const OUTPUT: &str = "HEAD-A";
const CLIENT_W: u32 = 400;
const CLIENT_H: u32 = 300;
const AT: SizeFracPoint = SizeFracPoint { x: 0.05, y: 0.05 };

fn frame(surface_id: u64, entries: Vec<LayoutEntry>, pos: Option<SizeFracPoint>) -> Frame {
    let mut frame = Frame::new(surface_id);
    frame.width = CLIENT_W as f64;
    frame.height = CLIENT_H as f64;
    frame.entries = entries;
    frame.floating_pos = pos;
    frame
}

fn entry(id: u64, surface_id: u64) -> LayoutEntry {
    let id = LayoutEntryId::new(id).unwrap();
    LayoutEntry::surface(id, "view".into(), surface_id, 0, 0, CLIENT_W, CLIENT_H)
}

fn client(fix: &mut Fixture) -> (TestClient, Toplevel, u64) {
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let size = Size::from((CLIENT_W as i32, CLIENT_H as i32));
    let top = fix.open_toplevel(&mut client, "mpv", size);
    let sid = fix.find_surface_id(&client, &top.surface).unwrap();
    (client, top, sid)
}

fn configured_sizes(client: &mut TestClient) -> Vec<(i32, i32)> {
    client
        .drain_events()
        .into_iter()
        .filter_map(|e| match e {
            ClientEvent::ToplevelConfigure { width, height } => Some((width, height)),
            _ => None,
        })
        .collect()
}

/// A request from a floating frame reaches Emacs and changes nothing by itself.
#[test]
fn fullscreen_request_from_a_float_is_handed_to_emacs() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(OUTPUT, 1920, 1080);
    let (mut client, top, sid) = client(&mut fix);
    fix.float(OUTPUT, vec![frame(500, vec![entry(7, sid)], Some(AT))]);
    fix.roundtrip(&mut client);
    fix.enable_event_capture();
    client.drain_events();

    top.xdg_toplevel.set_fullscreen(None);
    fix.roundtrip(&mut client);
    top.xdg_toplevel.unset_fullscreen();
    fix.roundtrip(&mut client);

    let requests: Vec<_> = fix
        .drain_events()
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                Event::FullscreenRequest { .. } | Event::UnfullscreenRequest { .. }
            )
        })
        .collect();
    assert!(
        matches!(requests[..], [Event::FullscreenRequest { id: a }, Event::UnfullscreenRequest { id: b }] if a == sid && b == sid),
        "{requests:?}"
    );
    assert!(!fix.ewm_ref().is_surface_fullscreen(sid));
    assert!(!configured_sizes(&mut client).contains(&(1920, 1080)));
}

/// Emacs's selection on a tiled entry is configured and presented at the output.
#[test]
fn tiled_selection_covers_the_output() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(OUTPUT, 1920, 1080);
    let (mut client, _top, sid) = client(&mut fix);
    let mut tile = frame(500, vec![entry(7, sid)], None);
    tile.width = 1920.0;
    tile.height = 0.0;
    fix.tile(OUTPUT, vec![tile]);
    fix.roundtrip(&mut client);
    client.drain_events();
    let output = fix.ewm_ref().space.outputs().next().unwrap().clone();

    fix.set_fullscreen(LayoutEntryId::new(7).unwrap(), true);
    fix.roundtrip(&mut client);
    fix.roundtrip(&mut client);

    assert!(configured_sizes(&mut client).contains(&(1920, 1080)));
    let ewm = fix.ewm_ref();
    assert!(ewm.is_surface_fullscreen(sid));
    // A 400x300 window centered on a 1920x1080 output.
    assert_eq!(
        ewm.surface_view_origins(sid),
        [Point::<i32, Logical>::from((760, 390))]
    );
    assert!(ewm.render_above_top_layer(&output));
}
