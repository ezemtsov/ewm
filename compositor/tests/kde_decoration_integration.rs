//! KDE server decorations are always negotiated as server-side.

use ewm_core::testing::{ClientEvent, Fixture, TestClient};
use smithay::utils::Size;
use wayland_client::WEnum;
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

#[test]
fn kde_decoration_always_answers_server_mode() {
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
    assert_eq!(decoration_modes(&mut client), [WEnum::Value(Mode::Server)]);
}
