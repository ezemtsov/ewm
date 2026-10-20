//! Idle timeouts, the unattended tint and cursor hiding.

use std::time::Duration;

use smithay::desktop::utils::surface_primary_scanout_output;
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::calloop::timer::Timer;
use smithay::reexports::wayland_server::Resource;
use tracing::{info, warn};

use crate::{Ewm, State, animation, event, tracy_span};

pub(crate) const SEPIA_FADE_DURATION: Duration = Duration::from_millis(600);

/// Action to perform when the native idle timeout fires.
#[derive(Debug, Clone)]
pub enum IdleAction {
    /// Turn off monitors (deactivate_monitors)
    DeactivateMonitors,
    /// Run a shell command (e.g., screensaver)
    RunCommand(String),
}

/// State for the native idle timeout feature.
///
/// Uses a last-activity timestamp pattern to avoid timer thrashing:
/// input events just update `last_activity` (free), and when the timer
/// fires it checks elapsed time, rescheduling if activity occurred.
pub struct IdleTimeoutState {
    pub timeout: Option<Duration>,
    pub action: IdleAction,
    pub timer_token: Option<RegistrationToken>,
    pub child_process: Option<std::process::Child>,
    pub is_idle: bool,
    pub last_activity: std::time::Instant,
}

/// State for cursor auto-hide on inactivity and hide-on-typing.
///
/// Kept separate from `cursor_manager` so client cursor requests are
/// preserved across hide/show transitions.
pub struct CursorHideState {
    pub timeout: Option<Duration>,
    pub timer_token: Option<RegistrationToken>,
    pub is_hidden: bool,
    pub last_activity: std::time::Instant,
    /// Hide the cursor immediately on key press. Pointer motion restores it.
    pub hide_when_typing: bool,
}

impl Ewm {
    /// Monitors off. Redraws keep settling, the backend is skipped.
    pub fn deactivate_monitors(&mut self) {
        if self.monitors_active {
            info!("Monitors deactivated");
            self.monitors_active = false;
        }
    }

    /// Monitors on. The next queued frame re-enables each CRTC.
    pub fn activate_monitors(&mut self) {
        // Dark until the lid opens or an external display appears.
        if self.unattended {
            return;
        }
        if !self.monitors_active {
            info!("Monitors activated");
            self.monitors_active = true;
            self.queue_redraw_all();
        }
    }

    /// Fade the sepia tint in or out.
    pub fn set_sepia(&mut self, sepia: bool) {
        let to = if sepia { 1. } else { 0. };
        if self.sepia.to() == to {
            return;
        }
        let from = self.sepia.value();
        self.sepia = animation::Animation::ease(
            self.animations_clock.clone(),
            from,
            to,
            SEPIA_FADE_DURATION,
        );
        self.queue_redraw_all();
    }

    /// How far toward sepia the outputs are, 0 to 1.
    pub fn sepia_amount(&self) -> f64 {
        self.sepia.value()
    }

    /// Kept awake behind a closed lid, nothing shows any window.
    pub fn set_unattended(&mut self, unattended: bool) {
        if self.unattended != unattended {
            info!("Unattended: {unattended}");
            self.unattended = unattended;
        }
    }

    /// Configure the native idle timeout.
    /// Passing `None` for timeout disables it.
    pub fn configure_idle(&mut self, timeout: Option<Duration>, action: IdleAction) {
        // If currently idle, wake without restarting the old timer
        if self.idle_timeout.is_idle {
            self.idle_timeout.is_idle = false;
            self.kill_idle_child();
            self.activate_monitors();
            self.queue_event(event::Event::IdleStateChanged { idle: false });
        }

        self.cancel_idle_timer();
        self.idle_timeout.timeout = timeout;
        self.idle_timeout.action = action;
        self.idle_timeout.last_activity = std::time::Instant::now();

        if let Some(duration) = timeout {
            self.start_idle_timer(duration);
            info!("Idle timeout configured: {:?}", duration);
        } else {
            info!("Idle timeout disabled");
        }
    }

    fn start_idle_timer(&mut self, duration: Duration) {
        self.cancel_idle_timer();
        let timer = Timer::from_duration(duration);
        match self.loop_handle.insert_source(timer, |_, _, state| {
            let went_idle = state.ewm.on_idle_timer_fired();
            if went_idle && !state.ewm.monitors_active {
                // Blank the DRM surfaces (DPMS off)
                state.backend.clear_all_surfaces();
            }
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        }) {
            Ok(token) => {
                self.idle_timeout.timer_token = Some(token);
            }
            Err(err) => {
                warn!("Failed to insert idle timer: {:?}", err);
            }
        }
    }

    pub(crate) fn cancel_idle_timer(&mut self) {
        if let Some(token) = self.idle_timeout.timer_token.take() {
            self.loop_handle.remove(token);
        }
    }

    /// Called when the calloop timer fires. Checks whether enough idle time
    /// has actually elapsed (activity may have occurred since the timer was set).
    /// If not, reschedules for the remaining time instead of firing.
    /// Returns `true` if the idle action was actually fired.
    fn on_idle_timer_fired(&mut self) -> bool {
        self.idle_timeout.timer_token = None;

        // Must refresh here: calloop may have been sleeping when D-Bus
        // inhibit arrived, so the per-iteration refresh hasn't run yet.
        self.refresh_idle_inhibit();
        if self.idle_is_inhibited {
            info!("Idle timer fired while inhibited, skipping idle action");
            return false;
        }

        let Some(timeout) = self.idle_timeout.timeout else {
            return false;
        };

        let elapsed = self.idle_timeout.last_activity.elapsed();
        if elapsed < timeout {
            // Activity happened since timer was set. Reschedule for remaining time
            let remaining = timeout - elapsed;
            self.start_idle_timer(remaining);
            return false;
        }

        // Actually fire the idle action
        self.idle_timeout.is_idle = true;
        info!("Idle timeout fired");

        match &self.idle_timeout.action {
            IdleAction::DeactivateMonitors => {
                self.deactivate_monitors();
            }
            // A second locker would be refused, and spawning it reaps the first.
            IdleAction::RunCommand(_) if self.lock_client_is_live() => self.deactivate_monitors(),
            IdleAction::RunCommand(cmd) => {
                match std::process::Command::new("sh").arg("-c").arg(cmd).spawn() {
                    Ok(child) => {
                        info!("Idle command spawned: {}", cmd);
                        // Reap the child the lock guard spared.
                        if let Some(mut previous) = self.idle_timeout.child_process.replace(child) {
                            let _ = previous.kill();
                            let _ = previous.wait();
                        }
                    }
                    Err(err) => {
                        warn!("Failed to spawn idle command: {:?}", err);
                    }
                }
            }
        }

        self.queue_event(event::Event::IdleStateChanged { idle: true });
        true
    }

    /// Wake from idle state: kill child process, reactivate monitors, restart timer.
    pub fn wake_from_idle(&mut self) {
        if !self.idle_timeout.is_idle {
            return;
        }
        info!("Waking from idle");
        self.idle_timeout.is_idle = false;

        self.kill_idle_child();
        self.activate_monitors();

        // Restart the timer for the next idle cycle
        self.idle_timeout.last_activity = std::time::Instant::now();
        if let Some(duration) = self.idle_timeout.timeout {
            self.start_idle_timer(duration);
        }

        self.queue_event(event::Event::IdleStateChanged { idle: false });
    }

    /// Record user activity. If idle, wakes immediately.
    /// Otherwise just updates the timestamp. The timer callback
    /// will check elapsed time and reschedule if needed.
    pub fn reset_idle_timer(&mut self) {
        if self.idle_timeout.is_idle {
            self.wake_from_idle();
        } else {
            self.idle_timeout.last_activity = std::time::Instant::now();
        }
    }

    pub fn notify_activity(&mut self) {
        if self.notified_activity_this_iteration {
            return;
        }

        tracy_span!("Ewm::notify_activity");

        self.idle_notifier_state.notify_activity(&self.seat);

        self.notified_activity_this_iteration = true;
    }

    pub fn configure_cursor_hide(&mut self, timeout: Option<Duration>, hide_when_typing: bool) {
        if self.cursor_hide.is_hidden {
            self.cursor_hide.is_hidden = false;
            self.queue_redraw_for_pointer();
        }

        self.cancel_cursor_hide_timer();
        self.cursor_hide.timeout = timeout;
        self.cursor_hide.hide_when_typing = hide_when_typing;
        self.cursor_hide.last_activity = std::time::Instant::now();

        if let Some(duration) = timeout {
            self.start_cursor_hide_timer(duration);
        }
        info!(
            "Cursor auto-hide configured: timeout={:?}, hide_when_typing={}",
            timeout, hide_when_typing
        );
    }

    pub fn hide_cursor_for_typing(&mut self) {
        // If the cursor is already invisible, don't redraw on every keystroke.
        if self.cursor_hide.is_hidden {
            return;
        }

        if !self.cursor_hide.hide_when_typing {
            return;
        }

        // A pen in use moves constantly. Hiding would just flicker.
        if self.tablet_cursor_location.is_some() {
            return;
        }

        self.cursor_hide.is_hidden = true;
        self.queue_redraw_for_pointer();
    }

    fn start_cursor_hide_timer(&mut self, duration: Duration) {
        self.cancel_cursor_hide_timer();
        let timer = Timer::from_duration(duration);
        match self.loop_handle.insert_source(timer, |_, _, state| {
            state.ewm.on_cursor_hide_timer_fired();
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        }) {
            Ok(token) => {
                self.cursor_hide.timer_token = Some(token);
            }
            Err(err) => {
                warn!("Failed to insert cursor-hide timer: {:?}", err);
            }
        }
    }

    pub(crate) fn cancel_cursor_hide_timer(&mut self) {
        if let Some(token) = self.cursor_hide.timer_token.take() {
            self.loop_handle.remove(token);
        }
    }

    fn on_cursor_hide_timer_fired(&mut self) {
        self.cursor_hide.timer_token = None;

        let Some(timeout) = self.cursor_hide.timeout else {
            return;
        };

        let elapsed = self.cursor_hide.last_activity.elapsed();
        if elapsed < timeout {
            self.start_cursor_hide_timer(timeout - elapsed);
            return;
        }

        if !self.cursor_hide.is_hidden {
            self.cursor_hide.is_hidden = true;
            self.queue_redraw_for_pointer();
        }
    }

    pub fn reset_cursor_hide_timer(&mut self) {
        if self.cursor_hide.timeout.is_none() && !self.cursor_hide.hide_when_typing {
            return;
        }
        self.cursor_hide.last_activity = std::time::Instant::now();
        if self.cursor_hide.is_hidden {
            self.cursor_hide.is_hidden = false;
            self.queue_redraw_for_pointer();
            if let Some(duration) = self.cursor_hide.timeout {
                self.start_cursor_hide_timer(duration);
            }
        }
    }

    /// Refresh the idle inhibition state.
    ///
    /// Combines four sources of idle inhibition:
    /// 1. **Manual toggle**: `ewm-idle-inhibit-mode` in Emacs.
    /// 2. **FDO ScreenSaver D-Bus**: Firefox, Chrome, and other apps use
    ///    `org.freedesktop.ScreenSaver.Inhibit()` to prevent screensaver.
    /// 3. **Wayland idle-inhibit protocol**: Video players create `zwp_idle_inhibitor_v1` on their
    ///    surface.
    /// 4. **Fullscreen heuristic**: A fullscreen surface that is currently focused is treated as an
    ///    idle inhibitor (covers video players and games that don't implement the protocol).
    ///
    /// For Wayland protocol inhibitors, only visible surfaces count
    /// (checked via `surface_primary_scanout_output`). This prevents a
    /// minimized/hidden app from keeping the screen awake.
    ///
    /// While the session is locked only the lock surface is visible, so only its
    /// own protocol inhibitor counts and the other sources wait for the unlock.
    ///
    /// The result is propagated to both the ext-idle-notify protocol (for
    /// swayidle) and the native idle timeout (monitor blanking/command).
    pub fn refresh_idle_inhibit(&mut self) {
        use smithay::wayland::compositor::with_states;

        // Clean up dead surfaces
        self.idle_inhibiting_surfaces.retain(|s| s.is_alive());

        let is_inhibited = (!self.is_locked()
            && (self.manual_idle_inhibited
                || self
                    .is_fdo_idle_inhibited
                    .load(std::sync::atomic::Ordering::SeqCst)))
            || self.fullscreen_surface_inhibits_idle()
            || self.idle_inhibiting_surfaces.iter().any(|surface| {
                with_states(surface, |states| {
                    surface_primary_scanout_output(surface, states).is_some()
                })
            });

        if is_inhibited != self.idle_is_inhibited {
            info!(
                "Idle inhibition changed: {} -> {}",
                self.idle_is_inhibited, is_inhibited
            );
            self.idle_is_inhibited = is_inhibited;

            // Propagate to ext-idle-notify protocol (for swayidle)
            self.idle_notifier_state.set_is_inhibited(is_inhibited);

            // Propagate to native idle timeout
            if is_inhibited {
                // Cancel the idle timer to prevent monitor blanking
                self.cancel_idle_timer();
                // If already idle, wake up
                if self.idle_timeout.is_idle {
                    self.wake_from_idle();
                }
            } else {
                // Restart the idle timer if configured
                if let Some(duration) = self.idle_timeout.timeout {
                    self.start_idle_timer(duration);
                }
            }
        }
    }

    /// A visible fullscreen surface on any output inhibits idle.
    /// Catches apps that don't implement zwp-idle-inhibit-v1 or D-Bus ScreenSaver.
    fn fullscreen_surface_inhibits_idle(&self) -> bool {
        !self.is_locked() && !self.frames_hidden() && self.layout.any_active_fullscreen()
    }

    /// Kill idle child process (e.g., on lid close).
    pub fn kill_idle_child(&mut self) {
        // Killing the lock client leaves the session blanked for good.
        if self.lock_client_is_live() {
            return;
        }
        if let Some(mut child) = self.idle_timeout.child_process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl State {
    /// Apply the lid and sepia settings, the lid policy again when it changed.
    pub(crate) fn configure_power(&mut self, closed_lid_suspends: bool, sepia: bool) {
        let lid_changed = std::mem::replace(&mut self.ewm.closed_lid_suspends, closed_lid_suspends)
            != closed_lid_suspends;
        self.ewm.set_sepia(sepia);
        if lid_changed && let Some(drm) = self.backend.as_drm_mut() {
            drm.reapply_lid_policy(&mut self.ewm, crate::backend::drm::LidPolicyReason::Config);
        }
    }
}
