//! Per-output workspace transition animation state.
//!
//! When the active tab changes on an output, a snapshot of the departing
//! workspace is captured and slid off while the new layout renders
//! underneath. This module holds the animation state; the capture, render
//! integration, and trigger live elsewhere.

use std::time::Duration;

use crate::utils::get_monotonic_time;

/// In-flight transition for a single output.
///
/// Currently a timing scaffold; texture and direction will land in later
/// commits alongside render integration.
pub struct WorkspaceTransition {
    start: Duration,
    duration: Duration,
}

impl WorkspaceTransition {
    pub fn new(duration: Duration) -> Self {
        Self {
            start: get_monotonic_time(),
            duration,
        }
    }

    pub fn is_done(&self) -> bool {
        get_monotonic_time().saturating_sub(self.start) >= self.duration
    }
}
