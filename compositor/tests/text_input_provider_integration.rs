//! The text input provider decides who owns the seat's input_method_v2 slot.
//! With Emacs as provider the relay holds it and Emacs frames stay out of
//! text-input focus. With an external provider the relay is gone, a client
//! such as fcitx5 binds and is activated, and Emacs frames get `enter` too.

use ewm_core::im::TextInputProvider;
use ewm_core::layout::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient, nz};
use ewm_core::{LayoutEntry, LayoutEntryId};
use smithay::utils::Size;
use smithay::wayland::input_method::InputMethodSeat;

const HEAD: &str = "Virtual-1";

fn pump(fix: &mut Fixture, emacs: &mut TestClient, app: &mut TestClient, ime: &mut TestClient) {
    for _ in 0..2 {
        fix.roundtrip(emacs);
        fix.roundtrip(app);
        fix.roundtrip(ime);
    }
}

fn saw(client: &mut TestClient, pred: impl Fn(&ClientEvent) -> bool) -> bool {
    client.drain_events().iter().any(pred)
}

/// The app entry stays unselected so focusing the frame lands keyboard focus on Emacs itself.
fn frame_with_app(frame_id: u64, entry_id: LayoutEntryId, app_id: u64) -> Frame {
    let mut frame = Frame::new(frame_id);
    frame.width = 1280.0;
    frame.entries = vec![LayoutEntry::surface(
        entry_id,
        "entry".to_string(),
        app_id,
        0,
        0,
        400,
        300,
    )];
    frame
}

/// Dispatch until the seat's input method slot is (or is not) taken.
fn wait_instance(fix: &mut Fixture, bound: bool) {
    for _ in 0..200 {
        if fix.ewm_ref().seat.input_method().has_instance() == bound {
            return;
        }
        fix.dispatch();
    }
    panic!("input method instance never became {bound}");
}

#[test]
fn external_provider_yields_the_seat_to_an_input_method_client() {
    let mut fix = Fixture::new().unwrap();
    fix.clear_keyboard_capture();
    fix.set_intercepted_keys(Vec::new());

    let mut emacs = fix.new_client().unwrap();
    let mut app = fix.new_client().unwrap();
    let mut ime = fix.new_client().unwrap();
    pump(&mut fix, &mut emacs, &mut app, &mut ime);

    fix.add_output(HEAD, 1600, 900);
    let emacs_surface = fix.open_toplevel(&mut emacs, "emacs", Size::from((800, 600)));
    let app_surface = fix.open_toplevel(&mut app, "app", Size::from((800, 600)));
    pump(&mut fix, &mut emacs, &mut app, &mut ime);

    let emacs_id = fix.find_surface_id(&emacs, &emacs_surface.surface).unwrap();
    let app_id = fix.find_surface_id(&app, &app_surface.surface).unwrap();
    let app_entry = nz(3001);
    fix.tile_command(HEAD, vec![frame_with_app(emacs_id, app_entry, app_id)]);

    // Emacs provider: the relay takes the slot and Emacs frames stay out of
    // text-input focus.
    fix.set_text_input_provider(TextInputProvider::Emacs);
    wait_instance(&mut fix, true);
    let _emacs_ti = emacs.create_text_input();
    fix.ewm().set_focus_frame(emacs_id, "test", true);
    fix.sync_keyboard_focus();
    pump(&mut fix, &mut emacs, &mut app, &mut ime);
    assert_eq!(fix.keyboard_focus_surface_id(), Some(emacs_id));
    assert!(!saw(&mut emacs, |e| matches!(
        e,
        ClientEvent::TextInputEnter
    )));

    // External provider: the relay leaves and the Emacs frame gets `enter`.
    fix.set_text_input_provider(TextInputProvider::External);
    wait_instance(&mut fix, false);
    pump(&mut fix, &mut emacs, &mut app, &mut ime);
    assert!(saw(&mut emacs, |e| matches!(
        e,
        ClientEvent::TextInputEnter
    )));

    // An external input method binds and is activated by the app's field.
    let _im = ime.create_input_method();
    pump(&mut fix, &mut emacs, &mut app, &mut ime);
    assert!(fix.ewm_ref().seat.input_method().has_instance());
    assert!(!saw(&mut ime, |e| matches!(
        e,
        ClientEvent::InputMethodUnavailable
    )));

    fix.ewm().set_focus_entry(app_entry, "test", false);
    fix.sync_keyboard_focus();
    pump(&mut fix, &mut emacs, &mut app, &mut ime);
    assert_eq!(fix.keyboard_focus_surface_id(), Some(app_id));

    let ti = app.create_text_input();
    pump(&mut fix, &mut emacs, &mut app, &mut ime);
    ti.enable();
    ti.commit();
    pump(&mut fix, &mut emacs, &mut app, &mut ime);
    assert!(saw(&mut ime, |e| matches!(
        e,
        ClientEvent::InputMethodActivate
    )));
    assert_eq!(fix.active_text_input_surface_id(), Some(app_id));
}
