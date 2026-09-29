//! Properties for the whole-field replace gate used by `ewm-edit`.

use ewm_core::im::text_input::{DrainedReplaces, ReplaceResult, Replacement, TextInputCommitState};
use proptest::prelude::*;

const SURFACE_IDS: std::ops::RangeInclusive<u64> = 1..=4;
const MAX_OPS: usize = 40;

#[derive(Debug, Clone)]
enum Op {
    Activate(Option<u64>),
    Deactivate,
    SetSurrounding {
        text_len: u32,
        cursor: u32,
        anchor: u32,
    },
    Replace {
        surface_id: u64,
        text: String,
    },
    Drain,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        prop::option::of(SURFACE_IDS).prop_map(Op::Activate),
        Just(Op::Deactivate),
        (0u32..8, 0u32..8, 0u32..8).prop_map(|(text_len, cursor, anchor)| Op::SetSurrounding {
            text_len,
            cursor,
            anchor
        }),
        (SURFACE_IDS, "[a-z]{1,3}").prop_map(|(surface_id, text)| Op::Replace { surface_id, text }),
        Just(Op::Drain),
    ]
}

/// Reference model of the gate's intended semantics.
#[derive(Default)]
struct Model {
    /// (surface_id, len, sel_start, sel_end).
    surrounding: Option<(u64, u32, u32, u32)>,
    pending: Vec<(u64, String)>,
}

impl Model {
    fn delete_lengths(&self, surface_id: u64) -> Option<(u32, u32)> {
        match self.surrounding {
            Some((id, len, ss, se)) if id == surface_id => Some((ss, len.saturating_sub(se))),
            _ => None,
        }
    }

    fn drain(&mut self, active_surface_id: Option<u64>) -> DrainedReplaces {
        let mut out = DrainedReplaces::default();
        let Some(active) = active_surface_id else {
            return out;
        };
        let Some((before, after)) = self.delete_lengths(active) else {
            return out;
        };
        let mut text = None;
        for (surface_id, chunk) in self.pending.drain(..) {
            if surface_id == active {
                text = Some(chunk);
            } else {
                out.dropped.push(surface_id);
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
}

proptest! {
    #[test]
    fn replace_gate_matches_model(
        ops in prop::collection::vec(op_strategy(), 1..MAX_OPS),
    ) {
        let mut state = TextInputCommitState::default();
        let mut model = Model::default();
        let mut active_surface_id = None;

        for op in ops {
            match op {
                Op::Activate(surface_id) => {
                    active_surface_id = surface_id;
                    state.activate();
                }
                Op::Deactivate => {
                    active_surface_id = None;
                    state.deactivate();
                    model.surrounding = None;
                }
                Op::SetSurrounding { text_len, cursor, anchor } => {
                    state.set_surrounding(active_surface_id, text_len, cursor, anchor);
                    model.surrounding = active_surface_id.map(|sid| {
                        (
                            sid,
                            text_len,
                            cursor.min(anchor).min(text_len),
                            cursor.max(anchor).min(text_len),
                        )
                    });
                }
                Op::Replace { surface_id, text } => {
                    let result = state.replace(active_surface_id, surface_id, text.clone());
                    let expected = match active_surface_id {
                        Some(active) if active == surface_id => match model.delete_lengths(surface_id) {
                            Some((before, after)) => {
                                ReplaceResult::Deliver(Replacement { before, after, text: text.clone() })
                            }
                            None => {
                                model.pending.push((surface_id, text.clone()));
                                ReplaceResult::Queued
                            }
                        },
                        Some(_) => ReplaceResult::DroppedStale,
                        None => {
                            model.pending.push((surface_id, text.clone()));
                            ReplaceResult::Queued
                        }
                    };
                    prop_assert_eq!(result, expected);
                }
                Op::Drain => {
                    let result = state.drain_pending_replace(active_surface_id);
                    prop_assert_eq!(result, model.drain(active_surface_id));
                }
            }
        }
    }
}

#[test]
fn replace_deletes_everything_before_end_cursor() {
    let mut state = TextInputCommitState::default();
    state.activate();
    // "hello world" (11 bytes), cursor at end, no selection.
    state.set_surrounding(Some(1), 11, 11, 11);
    assert_eq!(
        state.replace(Some(1), 1, "bye".to_string()),
        ReplaceResult::Deliver(Replacement {
            before: 11,
            after: 0,
            text: "bye".to_string()
        })
    );
}

#[test]
fn replace_deletes_outside_the_selection() {
    let mut state = TextInputCommitState::default();
    state.activate();
    // 11 bytes, selection [2,7).
    state.set_surrounding(Some(1), 11, 7, 2);
    assert_eq!(
        state.replace(Some(1), 1, "X".to_string()),
        ReplaceResult::Deliver(Replacement {
            before: 2,
            after: 4,
            text: "X".to_string()
        })
    );
}

#[test]
fn replace_queues_until_fresh_surrounding_then_drains() {
    let mut state = TextInputCommitState::default();
    state.activate();
    // Field re-active after refocus but surrounding not re-sent yet: queue.
    assert_eq!(
        state.replace(Some(1), 1, "x".to_string()),
        ReplaceResult::Queued
    );
    // Fresh surrounding arrives.
    state.set_surrounding(Some(1), 4, 4, 4);
    assert_eq!(
        state.drain_pending_replace(Some(1)),
        DrainedReplaces {
            deliver: Some((
                1,
                Replacement {
                    before: 4,
                    after: 0,
                    text: "x".to_string()
                }
            )),
            dropped: vec![],
        }
    );
}

#[test]
fn drain_reports_superseded_surface_as_dropped() {
    let mut state = TextInputCommitState::default();
    assert_eq!(
        state.replace(None, 1, "a".to_string()),
        ReplaceResult::Queued
    );
    state.set_surrounding(Some(2), 3, 3, 3);
    assert_eq!(
        state.drain_pending_replace(Some(2)),
        DrainedReplaces {
            deliver: None,
            dropped: vec![1]
        }
    );
}

#[test]
fn deactivate_clears_surrounding() {
    let mut state = TextInputCommitState::default();
    state.activate();
    state.set_surrounding(Some(1), 4, 4, 4);
    state.deactivate();
    assert_eq!(
        state.replace(Some(1), 1, "x".to_string()),
        ReplaceResult::Queued
    );
}
