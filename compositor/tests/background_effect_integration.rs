//! ext-background-effect blur regions are double-buffered and follow commits.

use ewm_core::handlers::background_effect::get_cached_blur_region;
use ewm_core::testing::{ClientEvent, Fixture};
use smithay::utils::{Logical, Rectangle, Size};
use smithay::wayland::compositor::with_states;
use wayland_client::WEnum;
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::Capability;

fn rect(x1: i32, y1: i32, x2: i32, y2: i32) -> Rectangle<i32, Logical> {
    Rectangle::from_extremities((x1, y1), (x2, y2))
}

#[test]
fn manager_advertises_blur() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    assert!(client.drain_events().iter().any(|e| matches!(
        e,
        ClientEvent::BackgroundEffectCapabilities {
            flags: WEnum::Value(flags)
        } if flags.contains(Capability::Blur)
    )));
}

#[test]
fn blur_region_follows_commits() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output("HEAD-A", 1600, 900);

    let toplevel = fix.open_toplevel(&mut client, "foot", Size::from((400, 300)));
    let surface = fix.server_surface(&client, &toplevel.surface);
    assert_eq!(with_states(&surface, get_cached_blur_region), None);

    let effect = client.create_background_effect(&toplevel.surface);
    let region = client.create_region();
    region.add(0, 0, 400, 300);
    region.subtract(100, 100, 200, 100);
    effect.set_blur_region(Some(&region));
    fix.roundtrip(&mut client);
    // Pending until the surface commits.
    assert_eq!(with_states(&surface, get_cached_blur_region), None);

    toplevel.surface.commit();
    fix.roundtrip(&mut client);
    let rects = with_states(&surface, get_cached_blur_region).unwrap();
    assert_eq!(
        *rects,
        [
            rect(0, 0, 400, 100),
            rect(0, 100, 100, 200),
            rect(300, 100, 400, 200),
            rect(0, 200, 400, 300),
        ]
    );

    effect.set_blur_region(None);
    toplevel.surface.commit();
    fix.roundtrip(&mut client);
    assert_eq!(with_states(&surface, get_cached_blur_region), None);
}

#[test]
fn destroying_the_effect_clears_the_region() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    fix.add_output("HEAD-A", 1600, 900);

    let toplevel = fix.open_toplevel(&mut client, "foot", Size::from((400, 300)));
    let surface = fix.server_surface(&client, &toplevel.surface);

    let effect = client.create_background_effect(&toplevel.surface);
    let region = client.create_region();
    region.add(10, 20, 30, 40);
    effect.set_blur_region(Some(&region));
    toplevel.surface.commit();
    fix.roundtrip(&mut client);
    assert_eq!(
        *with_states(&surface, get_cached_blur_region).unwrap(),
        [rect(10, 20, 40, 60)]
    );

    effect.destroy();
    toplevel.surface.commit();
    fix.roundtrip(&mut client);
    assert_eq!(with_states(&surface, get_cached_blur_region), None);

    // The surface is free for a new effect object afterwards.
    let effect = client.create_background_effect(&toplevel.surface);
    effect.set_blur_region(Some(&region));
    toplevel.surface.commit();
    fix.roundtrip(&mut client);
    assert_eq!(
        *with_states(&surface, get_cached_blur_region).unwrap(),
        [rect(10, 20, 40, 60)]
    );
}
