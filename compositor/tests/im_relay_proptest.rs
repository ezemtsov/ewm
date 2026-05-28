//! Properties for the text-input commit gate.
//!
//! The commit gate must never deliver a commit to whichever text-input surface
//! is active by accident. Commits are either delivered to their target surface,
//! queued while no surface is active, or dropped once another surface is known
//! to be active.

use ewm_core::im::relay::ImEvent;
use ewm_core::im::text_input::{CommitResult, TextInputCommitState};
use proptest::prelude::*;

const SURFACE_IDS: std::ops::RangeInclusive<u64> = 1..=4;
const MAX_OPS: usize = 40;

#[derive(Debug, Clone)]
enum Op {
    Activate(Option<u64>),
    Deactivate,
    Commit { surface_id: u64, text: String },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        prop::option::of(SURFACE_IDS).prop_map(Op::Activate),
        Just(Op::Deactivate),
        (SURFACE_IDS, "[a-z]{1,3}").prop_map(|(surface_id, text)| Op::Commit { surface_id, text }),
    ]
}

fn pending_tuples(commit_state: &TextInputCommitState) -> Vec<(u64, String)> {
    commit_state
        .pending_commits()
        .iter()
        .map(|commit| (commit.surface_id, commit.text.clone()))
        .collect()
}

fn drain_expected(
    pending: &mut Vec<(u64, String)>,
    active_surface_id: Option<u64>,
) -> Option<String> {
    let active_surface_id = active_surface_id?;
    let mut text = String::new();
    for (surface_id, chunk) in pending.drain(..) {
        if surface_id == active_surface_id {
            text.push_str(&chunk);
        }
    }
    (!text.is_empty()).then_some(text)
}

proptest! {
    #[test]
    fn text_input_commit_state_matches_surface_aware_model(
        ops in prop::collection::vec(op_strategy(), 1..MAX_OPS),
    ) {
        let mut commit_state = TextInputCommitState::default();
        let mut active_surface_id = None;
        let mut events = Vec::new();
        let mut delivered = Vec::new();

        let mut expected_active = false;
        let mut expected_pending = Vec::new();
        let mut expected_events = Vec::new();
        let mut expected_delivered = Vec::new();

        for op in ops {
            match op {
                Op::Activate(surface_id) => {
                    active_surface_id = surface_id;

                    if commit_state.activate() {
                        events.push(ImEvent::Activated);
                    }
                    if let Some(text) = commit_state.drain_pending_for_active_surface(active_surface_id) {
                        delivered.push((active_surface_id.unwrap(), text));
                    }

                    if !expected_active {
                        expected_active = true;
                        expected_events.push(ImEvent::Activated);
                    }
                    if let Some(text) = drain_expected(&mut expected_pending, surface_id) {
                        expected_delivered.push((surface_id.unwrap(), text));
                    }
                }
                Op::Deactivate => {
                    active_surface_id = None;

                    if commit_state.deactivate() {
                        events.push(ImEvent::Deactivated);
                    }

                    if expected_active {
                        expected_active = false;
                        expected_events.push(ImEvent::Deactivated);
                    }
                }
                Op::Commit { surface_id, text } => {
                    let result = commit_state.commit(active_surface_id, surface_id, text.clone());

                    match active_surface_id {
                        Some(active) if active == surface_id => {
                            prop_assert_eq!(result, CommitResult::Deliver(text.clone()));
                            delivered.push((surface_id, text.clone()));
                            expected_delivered.push((surface_id, text));
                        }
                        Some(_) => {
                            prop_assert_eq!(result, CommitResult::DroppedStale);
                        }
                        None => {
                            prop_assert_eq!(result, CommitResult::Queued);
                            expected_pending.push((surface_id, text));
                        }
                    }
                }
            }

            prop_assert_eq!(commit_state.is_active(), expected_active);
            let actual_pending = pending_tuples(&commit_state);
            prop_assert_eq!(&actual_pending, &expected_pending);
            prop_assert_eq!(&events, &expected_events);
            prop_assert_eq!(&delivered, &expected_delivered);
        }
    }
}
