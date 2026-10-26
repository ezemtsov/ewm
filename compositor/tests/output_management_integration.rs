//! What a wlr-output-management client leaves behind: a head disabled over the protocol keeps
//! its configuration, so enabling it again without a position puts it back where it was.

use ewm_core::testing::{ClientEvent, Fixture, HEAD_A, HEAD_B, TestClient};
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_configuration_v1::ZwlrOutputConfigurationV1;

/// A configurator bound to the manager, with its first `done` received.
fn configurator(fix: &mut Fixture) -> TestClient {
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    client.bind_output_manager();
    fix.roundtrip(&mut client);
    client
}

/// Where `name` is mapped, if at all.
fn origin(fix: &Fixture, name: &str) -> Option<(i32, i32)> {
    let layout = &fix.ewm_ref().layout;
    let rect = layout.output_rect(layout.output(name)?)?;
    Some((rect.loc.x, rect.loc.y))
}

/// Apply `conf` and wait for the compositor to accept it.
fn apply(fix: &mut Fixture, client: &mut TestClient, conf: &ZwlrOutputConfigurationV1) {
    conf.apply();
    fix.roundtrip(client);
    let events = client.drain_events();
    assert!(
        events.contains(&ClientEvent::OutputConfigurationSucceeded),
        "{events:?}"
    );
}

#[test]
fn a_disabled_head_keeps_its_place() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(HEAD_A, 1600, 900);
    fix.add_output(HEAD_B, 1280, 720);
    let mut client = configurator(&mut fix);
    let qh = client.qh();

    // wdisplays: both heads on, B moved below A.
    let conf = client.new_output_configuration();
    conf.enable_head(client.output_head(HEAD_A), &qh, ());
    let head = conf.enable_head(client.output_head(HEAD_B), &qh, ());
    head.set_position(0, 900);
    apply(&mut fix, &mut client, &conf);
    assert_eq!(origin(&fix, HEAD_B), Some((0, 900)));

    // wdisplays: B off.
    let conf = client.new_output_configuration();
    conf.enable_head(client.output_head(HEAD_A), &qh, ());
    conf.disable_head(client.output_head(HEAD_B));
    apply(&mut fix, &mut client, &conf);
    assert_eq!(origin(&fix, HEAD_B), None);

    // wlr-randr --output HEAD-B --on: back where it was.
    let conf = client.new_output_configuration();
    conf.enable_head(client.output_head(HEAD_A), &qh, ());
    conf.enable_head(client.output_head(HEAD_B), &qh, ());
    apply(&mut fix, &mut client, &conf);
    assert_eq!(origin(&fix, HEAD_B), Some((0, 900)));
}
