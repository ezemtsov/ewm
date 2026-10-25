//! Decorations are server-side: the xdg protocol is told so whatever it asks, the KDE one
//! defaults to server but echoes the client's request.

use ewm_core::testing::{ClientEvent, Fixture, TestClient};
use smithay::utils::Size;
use wayland_client::WEnum;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::Mode as XdgMode;
use wayland_protocols_misc::server_decoration::client::org_kde_kwin_server_decoration::Mode;
use wayland_protocols_misc::server_decoration::client::org_kde_kwin_server_decoration_manager::Mode as DefaultMode;

fn decoration_modes(client: &mut TestClient) -> Vec<WEnum<Mode>> {
    client
        .drain_events()
        .into_iter()
        .filter_map(|e| match e {
            ClientEvent::KdeDecorationMode { mode } => Some(mode),
            _ => None,
        })
        .collect()
}

/// Decoration modes announced and toplevel configures drawn since the last drain.
fn answers(client: &mut TestClient) -> (Vec<WEnum<XdgMode>>, usize) {
    let events = client.drain_events();
    let modes = events
        .iter()
        .filter_map(|e| match e {
            ClientEvent::XdgDecorationMode { mode } => Some(*mode),
            _ => None,
        })
        .collect();
    let configures = events
        .iter()
        .filter(|e| matches!(e, ClientEvent::ToplevelConfigure { .. }))
        .count();
    (modes, configures)
}

#[test]
fn xdg_decoration_is_server_side_whatever_the_client_asks() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output("HEAD-A", 1600, 900);

    let toplevel = fix.open_toplevel_unmapped(&mut client, "gtk", None);
    client.drain_events();
    let decoration = client.create_xdg_decoration(&toplevel.xdg_toplevel);
    fix.roundtrip(&mut client);
    assert_eq!(answers(&mut client).0, [WEnum::Value(XdgMode::ServerSide)]);
    fix.map_toplevel(&mut client, &toplevel, Size::from((400, 300)));
    client.drain_events();

    // Each request draws a configure, and the mode stands.
    decoration.set_mode(XdgMode::ClientSide);
    fix.roundtrip(&mut client);
    assert_eq!(answers(&mut client), (vec![], 1));
    decoration.unset_mode();
    fix.roundtrip(&mut client);
    assert_eq!(answers(&mut client), (vec![], 1));
}

#[test]
fn kde_decoration_acknowledges_requested_mode() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output("HEAD-A", 1600, 900);
    assert!(client.drain_events().iter().any(|e| matches!(
        e,
        ClientEvent::KdeDecorationDefaultMode {
            mode: WEnum::Value(DefaultMode::Server)
        }
    )));

    let toplevel = fix.open_toplevel(&mut client, "gtk", Size::from((400, 300)));
    let decoration = client.create_kde_decoration(&toplevel.surface);
    fix.roundtrip(&mut client);
    assert_eq!(decoration_modes(&mut client), [WEnum::Value(Mode::Server)]);

    decoration.request_mode(Mode::Client);
    fix.roundtrip(&mut client);
    assert_eq!(decoration_modes(&mut client), [WEnum::Value(Mode::Client)]);
}
