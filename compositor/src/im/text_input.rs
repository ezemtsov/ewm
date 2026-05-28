//! Text-input focus helpers and surface-aware commit gating.

use smithay::input::{Seat, SeatHandler};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::text_input::TextInputSeat;

#[derive(Debug, PartialEq, Eq)]
pub struct PendingCommit {
    pub surface_id: u64,
    pub text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommitResult {
    Deliver(String),
    Queued,
    DroppedStale,
}

/// Surface-aware commit gate for text-input commits produced by Emacs.
///
/// Commits may arrive while a client is cycling text-input disable/enable.
/// Queue only while no active text-input surface is known; once a surface is
/// active, text for any other surface is stale and must not be delivered later.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TextInputCommitState {
    active: bool,
    pending_commits: Vec<PendingCommit>,
}

impl TextInputCommitState {
    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn pending_commits(&self) -> &[PendingCommit] {
        &self.pending_commits
    }

    pub fn activate(&mut self) -> bool {
        !std::mem::replace(&mut self.active, true)
    }

    pub fn deactivate(&mut self) -> bool {
        std::mem::take(&mut self.active)
    }

    pub fn commit(
        &mut self,
        active_surface_id: Option<u64>,
        target_surface_id: u64,
        text: String,
    ) -> CommitResult {
        match active_surface_id {
            Some(active) if active == target_surface_id => CommitResult::Deliver(text),
            Some(_) => CommitResult::DroppedStale,
            None => {
                self.pending_commits.push(PendingCommit {
                    surface_id: target_surface_id,
                    text,
                });
                CommitResult::Queued
            }
        }
    }

    /// Drain queued text for `active_surface_id`, dropping text for any other
    /// surface because it is stale once a different surface is active.
    pub fn drain_pending_for_active_surface(
        &mut self,
        active_surface_id: Option<u64>,
    ) -> Option<String> {
        let active_surface_id = active_surface_id?;
        let mut text = String::new();
        for commit in self.pending_commits.drain(..) {
            if commit.surface_id == active_surface_id {
                text.push_str(&commit.text);
            }
        }
        (!text.is_empty()).then_some(text)
    }
}

pub fn update_focus<D: SeatHandler + 'static>(
    seat: &Seat<D>,
    surface: Option<&WlSurface>,
    is_emacs: bool,
) {
    let text_input = seat.text_input();

    if is_emacs || surface.is_none() {
        text_input.leave();
        text_input.set_focus(None);
    } else if let Some(surface) = surface {
        text_input.set_focus(Some(surface.clone()));
        text_input.enter();
    }
}

pub fn active_surface_id<D, F>(seat: &Seat<D>, mut surface_id: F) -> Option<u64>
where
    D: SeatHandler + 'static,
    F: FnMut(&WlSurface) -> Option<u64>,
{
    let text_input = seat.text_input();
    let mut active_surface_id = None;
    text_input.with_active_text_input(|_, surface| {
        active_surface_id = surface_id(surface);
    });
    active_surface_id
}

pub fn commit_active<D: SeatHandler + 'static>(seat: &Seat<D>, text: String) {
    let text_input = seat.text_input();
    let mut text = Some(text);
    let mut delivered = false;
    text_input.with_active_text_input(|ti, _surface| {
        ti.commit_string(text.take());
        delivered = true;
    });
    if delivered {
        text_input.done(false);
    }
}
