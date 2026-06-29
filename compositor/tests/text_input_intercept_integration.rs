//! The first key after a client text field activates must be intercepted for
//! input-method translation, not forwarded raw (regression test for the gate
//! that keys off smithay's synchronous active-text-input state).

use ewm_core::input::KeyboardAction;
use ewm_core::strip::Frame;
use ewm_core::testing::{Fixture, TestClient};
use ewm_core::{LayoutEntry, LayoutEntryId};
use smithay::utils::Size;

const HEAD: &str = "Virtual-1";
const KEY_A: u32 = 30 + 8;

fn nz(n: u64) -> LayoutEntryId {
    std::num::NonZeroU64::new(n).unwrap()
}

fn pump(fix: &mut Fixture, emacs: &mut TestClient, app: &mut TestClient) {
    fix.roundtrip(emacs);
    fix.roundtrip(app);
    fix.roundtrip(emacs);
}

fn frame_with_app(frame_id: u64, entry_id: LayoutEntryId, app_id: u64) -> Frame {
    Frame {
        name: String::new(),
        workspace_name: None,
        surface_id: frame_id,
        focus_id: nz(frame_id + 1_000),
        width: 1280.0,
        height: 0.0,
        entries: vec![LayoutEntry::surface(
            entry_id,
            "entry".to_string(),
            app_id,
            0,
            0,
            400,
            300,
        )],
        fullscreen: None,
        selected_entry_id: Some(entry_id),
        move_anim: None,
        floating_pos: None,
    }
}

#[test]
fn first_key_after_text_input_activation_is_intercepted() {
    let mut fix = Fixture::new().unwrap();
    fix.clear_keyboard_capture();
    fix.set_intercepted_keys(Vec::new());

    // `emacs` hosts the input method; `app` is the client whose field activates.
    let mut emacs = fix.new_client().unwrap();
    let mut app = fix.new_client().unwrap();
    pump(&mut fix, &mut emacs, &mut app);

    fix.add_output(HEAD, 1600, 900);
    let emacs_surface = fix.open_toplevel(&mut emacs, "emacs", Size::from((800, 600)));
    let app_surface = fix.open_toplevel(&mut app, "app", Size::from((800, 600)));
    pump(&mut fix, &mut emacs, &mut app);

    let emacs_id = fix.find_surface_id(&emacs, &emacs_surface.surface).unwrap();
    let app_id = fix.find_surface_id(&app, &app_surface.surface).unwrap();

    let _im = emacs.create_input_method();
    pump(&mut fix, &mut emacs, &mut app);

    // Focus the app surface, embedded in the emacs frame.
    let app_entry = nz(3001);
    fix.apply_output_layout_command(HEAD, vec![frame_with_app(emacs_id, app_entry, app_id)]);
    fix.ewm().set_focus_entry(app_entry, "test", false);
    fix.sync_keyboard_focus();
    pump(&mut fix, &mut emacs, &mut app);
    assert_eq!(fix.keyboard_focus_surface_id(), Some(app_id));

    // Activate a text field; smithay marks the surface active synchronously.
    let ti = app.create_text_input();
    fix.roundtrip(&mut app);
    ti.enable();
    ti.commit();
    pump(&mut fix, &mut emacs, &mut app);
    assert_eq!(fix.active_text_input_surface_id(), Some(app_id));

    fix.ewm().text_input_intercept = true;

    assert_eq!(
        fix.keyboard_key(KEY_A, true),
        KeyboardAction::TextInputIntercepted,
        "first key after activation was not intercepted"
    );
    assert_eq!(
        fix.keyboard_key(KEY_A, false),
        KeyboardAction::TextInputIntercepted,
    );
}
