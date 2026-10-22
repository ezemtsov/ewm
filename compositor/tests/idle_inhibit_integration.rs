//! The lock parks the manual, D-Bus and fullscreen inhibit sources until unlock.
//! Headless renders set no primary scanout output, so protocol inhibitors are covered live.

use std::sync::atomic::Ordering;

use ewm_core::testing::{Fixture, TestClient, Toplevel, frame_with_entry, nz};
use smithay::utils::Size;

const OUTPUT: &str = "HEAD-A";
const ENTRY: u64 = 7;

fn tiled_client(fix: &mut Fixture) -> (TestClient, Toplevel) {
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    let (toplevel, sid) =
        fix.open_toplevel_with_id(&mut client, "idleprobe", Size::from((400, 300)));
    fix.tile(OUTPUT, vec![frame_with_entry(500, nz(ENTRY), sid)]);
    fix.roundtrip(&mut client);
    (client, toplevel)
}

/// Put the view fullscreen and let its client commit it.
fn go_fullscreen(fix: &mut Fixture, client: &mut TestClient, toplevel: &Toplevel) {
    fix.set_fullscreen(nz(ENTRY), true);
    fix.answer_configure(client, toplevel);
}

fn inhibited(fix: &mut Fixture) -> bool {
    fix.ewm().refresh_idle_inhibit();
    let ewm = fix.ewm();
    assert_eq!(
        ewm.idle_is_inhibited,
        ewm.idle_notifier_state.is_inhibited()
    );
    ewm.idle_is_inhibited
}

fn timer_armed(fix: &Fixture) -> bool {
    fix.ewm_ref().idle_timeout.timer_token.is_some()
}

#[test]
fn lock_parks_every_hidden_source_until_unlock() {
    for mask in 0u8..8 {
        let (manual, fdo, fullscreen) = (mask & 1 != 0, mask & 2 != 0, mask & 4 != 0);
        let case = format!("manual={manual} fdo={fdo} fullscreen={fullscreen}");
        let any = manual || fdo || fullscreen;

        let mut fix = Fixture::new().unwrap();
        fix.add_output(OUTPUT, 1920, 1080);
        let (mut client, toplevel) = tiled_client(&mut fix);
        fix.configure_idle(300);
        if manual {
            fix.set_idle_inhibited(true);
        }
        if fdo {
            fix.ewm()
                .is_fdo_idle_inhibited
                .store(true, Ordering::SeqCst);
        }
        if fullscreen {
            go_fullscreen(&mut fix, &mut client, &toplevel);
        }
        assert_eq!(inhibited(&mut fix), any, "{case}: before lock");
        assert_eq!(timer_armed(&fix), !any, "{case}: timer before lock");

        let lock = client.lock_session();
        client.lock_output(&lock, OUTPUT);
        fix.roundtrip(&mut client);
        fix.roundtrip(&mut client);
        assert!(fix.ewm_ref().is_locked(), "{case}: not locked");
        assert!(!inhibited(&mut fix), "{case}: inhibited behind the lock");
        assert!(timer_armed(&fix), "{case}: timer not armed behind the lock");

        lock.unlock_and_destroy();
        fix.roundtrip(&mut client);
        fix.roundtrip(&mut client);
        assert!(!fix.ewm_ref().is_locked(), "{case}: still locked");
        assert_eq!(inhibited(&mut fix), any, "{case}: after unlock");
        assert_eq!(timer_armed(&fix), !any, "{case}: timer after unlock");
    }
}

#[test]
fn idle_blanks_behind_the_lock_despite_fullscreen() {
    let mut fix = Fixture::new().unwrap();
    fix.add_output(OUTPUT, 1920, 1080);
    let (mut client, toplevel) = tiled_client(&mut fix);
    fix.configure_idle(0);
    go_fullscreen(&mut fix, &mut client, &toplevel);
    assert!(inhibited(&mut fix));
    assert!(!timer_armed(&fix));
    fix.dispatch_roundtrip(3);
    assert!(fix.ewm_ref().monitors_active, "blanked while inhibited");

    let lock = client.lock_session();
    client.lock_output(&lock, OUTPUT);
    fix.roundtrip(&mut client);
    fix.roundtrip(&mut client);
    assert!(fix.ewm_ref().is_locked());
    assert!(!inhibited(&mut fix));
    fix.dispatch_roundtrip(3);
    assert!(
        fix.ewm_ref().idle_timeout.is_idle,
        "idle timer did not fire"
    );
    assert!(
        !fix.ewm_ref().monitors_active,
        "monitors still on behind the lock"
    );
}
