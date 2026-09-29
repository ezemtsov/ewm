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

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct Surrounding {
    len: u32,
    sel_start: u32,
    sel_end: u32,
}

#[derive(Debug, PartialEq, Eq)]
struct PendingReplace {
    surface_id: u64,
    text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Replacement {
    pub before: u32,
    pub after: u32,
    pub text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplaceResult {
    Deliver(Replacement),
    Queued,
    DroppedStale,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainedReplaces {
    pub deliver: Option<(u64, Replacement)>,
    pub dropped: Vec<u64>,
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
    surrounding: Option<(u64, Surrounding)>,
    pending_replace: Vec<PendingReplace>,
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
        self.surrounding = None;
        std::mem::take(&mut self.active)
    }

    pub fn set_surrounding(
        &mut self,
        active_surface_id: Option<u64>,
        text_len: u32,
        cursor: u32,
        anchor: u32,
    ) {
        self.surrounding = active_surface_id.map(|surface_id| {
            (
                surface_id,
                Surrounding {
                    len: text_len,
                    sel_start: cursor.min(anchor).min(text_len),
                    sel_end: cursor.max(anchor).min(text_len),
                },
            )
        });
    }

    /// `None` until this surface has reported surrounding text.
    fn delete_lengths(&self, surface_id: u64) -> Option<(u32, u32)> {
        match self.surrounding {
            Some((id, s)) if id == surface_id => {
                Some((s.sel_start, s.len.saturating_sub(s.sel_end)))
            }
            _ => None,
        }
    }

    /// Queued until `target_surface_id` is active with fresh surrounding.
    pub fn replace(
        &mut self,
        active_surface_id: Option<u64>,
        target_surface_id: u64,
        text: String,
    ) -> ReplaceResult {
        match active_surface_id {
            Some(active) if active == target_surface_id => {
                if let Some((before, after)) = self.delete_lengths(active) {
                    return ReplaceResult::Deliver(Replacement {
                        before,
                        after,
                        text,
                    });
                }
            }
            Some(_) => return ReplaceResult::DroppedStale,
            None => {}
        }
        // Not re-active yet, or active without fresh surrounding: queue.
        self.pending_replace.push(PendingReplace {
            surface_id: target_surface_id,
            text,
        });
        ReplaceResult::Queued
    }

    /// Last replace queued for the active surface wins; the rest drop as stale.
    pub fn drain_pending_replace(&mut self, active_surface_id: Option<u64>) -> DrainedReplaces {
        let mut out = DrainedReplaces::default();
        let Some(active) = active_surface_id else {
            return out;
        };
        let Some((before, after)) = self.delete_lengths(active) else {
            return out;
        };
        let mut text = None;
        for pr in self.pending_replace.drain(..) {
            if pr.surface_id == active {
                text = Some(pr.text);
            } else {
                out.dropped.push(pr.surface_id);
            }
        }
        out.deliver = text.map(|text| {
            (
                active,
                Replacement {
                    before,
                    after,
                    text,
                },
            )
        });
        out
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

pub fn replace_active<D: SeatHandler + 'static>(
    seat: &Seat<D>,
    before: u32,
    after: u32,
    text: String,
) -> bool {
    let text_input = seat.text_input();
    let mut payload = Some((before, after, text));
    let mut delivered = false;
    text_input.with_active_text_input(|ti, _surface| {
        if let Some((before, after, text)) = payload.take() {
            ti.delete_surrounding_text(before, after);
            ti.commit_string(Some(text));
        }
        delivered = true;
    });
    if delivered {
        text_input.done(false);
    }
    delivered
}
