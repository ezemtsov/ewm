//! Session lock protocol, ext-session-lock-v1.

use std::mem;
use std::time::Duration;

use smithay::output::Output;
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::with_states;
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};
use tracing::{error, info, trace, warn};

use crate::utils::is_mapped;
use crate::{Ewm, FocusSource, Notify, State};

/// Session lock state machine for secure screen locking.
///
/// Follows the ext-session-lock-v1 protocol requirements:
/// - Lock surfaces get a second to paint before the first locked frame.
/// - Lock is confirmed only after all outputs render a locked frame.
/// - Input is blocked during locking/locked states.
#[derive(Default)]
pub enum LockState {
    /// Session is not locked.
    #[default]
    Unlocked,
    /// Lock requested, waiting for a mapped lock surface on every mapped output.
    WaitingForSurfaces {
        confirmation: SessionLocker,
        deadline_token: RegistrationToken,
    },
    /// Lock requested, waiting for all outputs to render a locked frame.
    Locking(SessionLocker),
    /// Session is fully locked, the lock object detects a dead client.
    Locked(ExtSessionLockV1),
}

/// Per-output lock render state for tracking lock confirmation.
#[derive(Default, PartialEq, Eq, Clone, Copy, Debug)]
pub enum LockRenderState {
    /// Output is showing normal content, or has not rendered locked yet.
    #[default]
    Unlocked,
    /// Output has rendered a locked frame.
    Locked,
}

impl Ewm {
    /// Check if the session is locked (locking or fully locked)
    pub fn is_locked(&self) -> bool {
        matches!(
            self.lock_state,
            LockState::Locking(_) | LockState::Locked(_)
        )
    }

    /// Check whether a live client holds the session lock, or is taking it.
    pub fn lock_client_is_live(&self) -> bool {
        match &self.lock_state {
            LockState::Unlocked => false,
            LockState::WaitingForSurfaces { confirmation, .. }
            | LockState::Locking(confirmation) => confirmation.ext_session_lock().is_alive(),
            LockState::Locked(lock) => lock.is_alive(),
        }
    }

    /// Leave the lock surface wait once every mapped output shows one.
    pub fn maybe_continue_to_locking(&mut self) {
        if !matches!(self.lock_state, LockState::WaitingForSurfaces { .. }) {
            return;
        }
        let ready = self.layout.outputs().all(|output| {
            self.output_state[output]
                .lock_surface
                .as_ref()
                .is_some_and(|surface| is_mapped(surface.wl_surface()))
        });
        if ready {
            trace!("lock surfaces are ready, continuing");
            self.continue_to_locking();
        }
    }

    /// Render the locked frame on every output, which confirms at once without any.
    fn continue_to_locking(&mut self) {
        match mem::take(&mut self.lock_state) {
            LockState::WaitingForSurfaces {
                confirmation,
                deadline_token,
            } => {
                self.loop_handle.remove(deadline_token);
                self.lock_state = LockState::Locking(confirmation);
                self.check_lock_complete();
                self.queue_redraw_all();
            }
            other => {
                error!("continue_to_locking() called in the wrong lock state");
                self.lock_state = other;
            }
        }
    }

    /// Check if all outputs have rendered locked frames and confirm lock if so
    pub fn check_lock_complete(&mut self) {
        // Check if we're in Locking state and all outputs have rendered
        let should_confirm = matches!(&self.lock_state, LockState::Locking(_))
            && self
                .output_state
                .values()
                .all(|s| s.lock_render_state == LockRenderState::Locked);

        if should_confirm {
            // Take ownership of the SessionLocker to call lock()
            // Use a temporary Unlocked state (will be replaced immediately)
            let old_state = mem::replace(&mut self.lock_state, LockState::Unlocked);
            if let LockState::Locking(confirmation) = old_state {
                info!("All outputs rendered locked frame, confirming lock");
                let lock = confirmation.ext_session_lock().clone();
                confirmation.lock();
                self.lock_state = LockState::Locked(lock);
            }
        }
    }

    pub(crate) fn lock_surface_output(&self) -> Option<&Output> {
        self.output_under_cursor().or_else(|| self.first_output())
    }

    /// Get the lock surface for keyboard focus when locked.
    pub fn lock_surface_focus(&self) -> Option<WlSurface> {
        let output = self.lock_surface_output()?;
        self.output_state
            .get(output)?
            .lock_surface
            .as_ref()
            .map(|s| s.wl_surface().clone())
    }

    /// Check lock state after output removal.
    /// If in Locking state, the removed output no longer needs to render a locked frame.
    pub fn check_lock_on_output_removed(&mut self) {
        if matches!(&self.lock_state, LockState::Locking(_)) {
            // Re-check if all remaining outputs are locked
            self.check_lock_complete();
        }
    }

    /// Abort the lock if we failed to render during Locking state.
    /// This prevents the session from being stuck in an unlockable state.
    pub fn abort_lock_on_render_failure(&mut self) {
        if matches!(&self.lock_state, LockState::Locking(_)) {
            warn!("Aborting session lock due to render failure");
            // Reset to unlocked - the SessionLocker will be dropped, signaling failure
            self.lock_state = LockState::Unlocked;
            // Clear any lock surfaces
            for state in self.output_state.values_mut() {
                state.lock_surface = None;
                state.lock_render_state = LockRenderState::Unlocked;
            }
            self.queue_redraw_all();
        }
    }
}

// Session Lock protocol (ext-session-lock-v1) for screen locking
impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.ewm.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        // Liveness, not the state, decides: a replacement is the only way out
        // of a lock whose client is gone.
        if self.ewm.lock_client_is_live() {
            info!("Session lock request ignored: a live client holds the lock");
            return;
        }

        info!("Session lock requested");

        // Save current focus to restore after unlock
        self.ewm.pre_lock_focus = self.ewm.focused_surface_id();

        // Give the lock surfaces a second to paint before the first locked frame.
        let timer = Timer::from_duration(Duration::from_millis(1000));
        let deadline_token = self
            .ewm
            .loop_handle
            .insert_source(timer, |_, _, state| {
                trace!("lock deadline expired, continuing");
                state.ewm.continue_to_locking();
                TimeoutAction::Drop
            })
            .unwrap();
        self.ewm.lock_state = LockState::WaitingForSurfaces {
            confirmation,
            deadline_token,
        };
        self.ewm.maybe_continue_to_locking();

        // Monitors on for the lock screen. After the state is set, so the wake
        // spares the lock client.
        self.ewm.wake_from_idle();
    }

    fn unlock(&mut self) {
        info!("Session unlock requested");
        if let LockState::WaitingForSurfaces { deadline_token, .. } =
            mem::take(&mut self.ewm.lock_state)
        {
            self.ewm.loop_handle.remove(deadline_token);
        }

        // Clear lock surfaces and reset render states
        for state in self.ewm.output_state.values_mut() {
            state.lock_surface = None;
            state.lock_render_state = LockRenderState::Unlocked;
        }

        // Invalidate tracked focus (was on lock surface) to force sync
        self.ewm.keyboard_focus = None;

        // Restore focus to the surface that was focused before locking
        if let Some(id) = self.ewm.pre_lock_focus.take()
            && self.ewm.surfaces.contains_key(&id)
        {
            info!("Restoring focus to surface {} after unlock", id);
            self.ewm.set_focus(id, FocusSource::Unlock, Notify::None);
        }
        self.sync_keyboard_focus();

        self.ewm.queue_redraw_all();

        // Monitors on, so unlocking from a fingerprint reader lights the screen.
        self.ewm.activate_monitors();
        self.ewm.notify_activity();
        self.ewm.reset_idle_timer();

        info!("Session unlocked");
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let Some(output) = self.ewm.output_from_resource(&wl_output) else {
            warn!("Lock surface created for unknown output");
            return;
        };

        info!("New lock surface for output: {}", output.name());

        // Configure lock surface to cover the entire output
        configure_lock_surface(&surface, &output);

        // Store in per-output state
        if let Some(state) = self.ewm.output_state.get_mut(&output) {
            state.lock_surface = Some(surface);
        }

        self.ewm.queue_redraw(&output);
    }
}

/// Configure a lock surface to cover the full output
pub(crate) fn configure_lock_surface(surface: &LockSurface, output: &Output) {
    surface.with_pending_state(|states| {
        let size = crate::utils::output_size(output);
        states.size = Some(size.to_i32_round());
    });

    let scale = output.current_scale();
    let transform = output.current_transform();
    let wl_surface = surface.wl_surface();

    with_states(wl_surface, |data| {
        crate::utils::send_scale_transform(wl_surface, data, scale, transform);
    });

    surface.send_configure();
}
