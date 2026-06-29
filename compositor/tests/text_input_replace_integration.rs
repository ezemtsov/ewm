//! A whole-field replace must reach the client over zwp_text_input_v3.

use ewm_core::strip::Frame;
use ewm_core::testing::{ClientEvent, Fixture, TestClient};
use ewm_core::{LayoutEntry, LayoutEntryId};
use smithay::utils::Size;

const HEAD: &str = "Virtual-1";

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
fn replace_reaches_client_as_delete_then_commit_then_done() {
    let mut fix = Fixture::new().unwrap();
    fix.clear_keyboard_capture();
    fix.set_intercepted_keys(Vec::new());

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

    // Activate the field so the seat has an active text-input.
    let ti = app.create_text_input();
    fix.roundtrip(&mut app);
    ti.enable();
    ti.commit();
    pump(&mut fix, &mut emacs, &mut app);
    assert_eq!(fix.active_text_input_surface_id(), Some(app_id));
    app.drain_events();

    // Delete 2 bytes before / 3 after the cursor, committing in their place.
    fix.replace_active_text_input(2, 3, "world");
    fix.roundtrip(&mut app);

    let text_events: Vec<ClientEvent> = app
        .drain_events()
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                ClientEvent::TextInputDeleteSurroundingText { .. }
                    | ClientEvent::TextInputCommitString { .. }
                    | ClientEvent::TextInputDone { .. }
            )
        })
        .collect();

    assert!(
        matches!(
            text_events.as_slice(),
            [
                ClientEvent::TextInputDeleteSurroundingText { before: 2, after: 3 },
                ClientEvent::TextInputCommitString { text: Some(t) },
                ClientEvent::TextInputDone { .. },
            ] if t == "world"
        ),
        "unexpected text-input events: {text_events:?}"
    );
}
