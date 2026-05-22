//! Property tests for the strip mutation rules from
//! `docs/ext-workspace-v1.md` §Invariants.
//!
//! The protocol mirror is a pure function of the strip, so pinning the
//! strip's behaviour under random reorders / closures / activations is
//! the foundation for ext-workspace-v1 correctness. Protocol-mirror
//! invariants (I3-I6) are covered separately; here we cover I8-I11.
//!
//! Op semantics intentionally mirror the user-facing lisp commands:
//! - `InsertAfterActive` ↔ `ewm-frame-new` (I8)
//! - `CloseAt` ↔ `ewm-frame-close` + `strip.remove_frame_at` (I9 / I10)
//! - `MoveSelected(±1)` ↔ `ewm-frame-move-right` / `ewm-frame-move-left` (I11)
//! - `Focus` ↔ `ewm-activate-frame`

use std::collections::HashSet;

use ewm_core::strip::{AnimationsClock, Frame, Strip};
use proptest::prelude::*;

const SID_RANGE: std::ops::RangeInclusive<u64> = 1..=8;
const MAX_OPS: usize = 30;

#[derive(Debug, Clone)]
enum Op {
    /// Insert `sid` at slot `active_idx + 1` and make it active.
    /// Skipped if `sid` is already in the strip.
    InsertAfterActive(u64),
    /// Close the frame at `slot`. No-op if `slot >= strip.len()`.
    CloseAt(usize),
    /// Move the currently active frame by `delta` slots (±1). No-op if
    /// the target slot is out of range or the strip is empty.
    MoveSelected(i32),
    /// Activate `slot`. No-op if `slot >= strip.len()`.
    Focus(usize),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => SID_RANGE.prop_map(Op::InsertAfterActive),
        2 => (0..8usize).prop_map(Op::CloseAt),
        1 => prop_oneof![Just(-1), Just(1)].prop_map(Op::MoveSelected),
        2 => (0..8usize).prop_map(Op::Focus),
    ]
}

fn apply_op(strip: &mut Strip, op: &Op) {
    match op {
        Op::InsertAfterActive(sid) => {
            if strip.frames.iter().any(|f| f.surface_id == *sid) {
                return;
            }
            let prev_len = strip.frames.len();
            let prev_active_idx = strip.active_idx();
            let pos = if prev_len == 0 {
                0
            } else {
                prev_active_idx + 1
            };
            strip.frames.insert(pos, Frame::new(*sid));
            strip.set_active(pos);

            assert_eq!(strip.active_idx(), pos, "I8: new frame must be active");
            assert_eq!(strip.frames[pos].surface_id, *sid, "I8: new frame at pos");
            assert_eq!(strip.frames.len(), prev_len + 1);
            if prev_len > 0 {
                assert_eq!(
                    pos,
                    prev_active_idx + 1,
                    "I8: insert right of previous active"
                );
            }
        }

        Op::CloseAt(slot) => {
            if *slot >= strip.frames.len() {
                return;
            }
            let prev_len = strip.frames.len();
            let prev_active_idx = strip.active_idx();
            let prev_active_sid = strip.active_frame().map(|f| f.surface_id);
            let was_active = *slot == prev_active_idx;
            let expected_new_active_sid = if !was_active {
                prev_active_sid
            } else if *slot + 1 < prev_len {
                Some(strip.frames[*slot + 1].surface_id)
            } else if *slot > 0 {
                Some(strip.frames[*slot - 1].surface_id)
            } else {
                None
            };

            strip.remove_frame_at(*slot);

            assert_eq!(strip.frames.len(), prev_len - 1);
            assert_eq!(
                strip.active_frame().map(|f| f.surface_id),
                expected_new_active_sid,
                "I9/I10: close slot {} (was_active={}, prev_active_idx={})",
                slot,
                was_active,
                prev_active_idx,
            );
        }

        Op::MoveSelected(delta) => {
            if strip.frames.is_empty() {
                return;
            }
            let i = strip.active_idx();
            let j = match i.checked_add_signed(*delta as isize) {
                Some(j) if j < strip.frames.len() => j,
                _ => return,
            };
            let prev_active_sid = strip.frames[i].surface_id;
            let prev_neighbour_sid = strip.frames[j].surface_id;
            strip.frames.swap(i, j);
            strip.set_active(j);

            assert_eq!(
                strip.active_frame().map(|f| f.surface_id),
                Some(prev_active_sid),
                "I11: active must follow on move (delta={})",
                delta,
            );
            assert_eq!(
                strip.frames[i].surface_id, prev_neighbour_sid,
                "I11: previous neighbour must be at slot {} (delta={})",
                i, delta,
            );
        }

        Op::Focus(slot) => {
            if *slot >= strip.frames.len() {
                return;
            }
            let target_sid = strip.frames[*slot].surface_id;
            strip.set_active(*slot);
            assert_eq!(
                strip.active_frame().map(|f| f.surface_id),
                Some(target_sid),
                "Focus: active must point at the requested slot",
            );
        }
    }
}

/// Structural invariants that must hold between any two ops.
fn assert_structural(strip: &Strip) {
    if strip.frames.is_empty() {
        return;
    }
    assert!(
        strip.active_idx() < strip.frames.len(),
        "active_idx={} out of bounds for {} frames",
        strip.active_idx(),
        strip.frames.len(),
    );
    let mut seen = HashSet::new();
    for f in &strip.frames {
        assert!(
            seen.insert(f.surface_id),
            "duplicate surface_id {} in strip",
            f.surface_id,
        );
    }
}

proptest! {
    #[test]
    fn workspace_invariants_hold(ops in prop::collection::vec(op_strategy(), 1..MAX_OPS)) {
        let mut strip = Strip::new(AnimationsClock::default());
        assert_structural(&strip);
        for op in &ops {
            apply_op(&mut strip, op);
            assert_structural(&strip);
        }
    }

    #[test]
    fn active_close_uses_destroyed_slot_not_current_active_idx(
        len in 1usize..=8,
        raw_destroy_idx in 0usize..8,
        raw_active_idx in 0usize..8,
    ) {
        let destroy_idx = raw_destroy_idx % len;
        let active_idx = raw_active_idx % len;
        let ids = (0..len).map(|i| 10 + i as u64).collect::<Vec<_>>();
        let mut strip = Strip::new(AnimationsClock::default());
        strip.frames = ids.iter().copied().map(Frame::new).collect();
        strip.set_active(active_idx);

        let expected_ids = ids
            .iter()
            .copied()
            .filter(|&id| id != ids[destroy_idx])
            .collect::<Vec<_>>();
        let expected_active = if len == 1 {
            None
        } else if destroy_idx + 1 < len {
            Some(ids[destroy_idx + 1])
        } else {
            Some(ids[destroy_idx - 1])
        };

        strip.remove_active_frame_at(destroy_idx);

        prop_assert_eq!(
            strip.frames.iter().map(|f| f.surface_id).collect::<Vec<_>>(),
            expected_ids,
        );
        prop_assert_eq!(strip.active_frame().map(|f| f.surface_id), expected_active);
        if destroy_idx < strip.frames.len() {
            prop_assert!(
                strip.frames[destroy_idx].move_anim.is_some(),
                "right neighbor should slide into destroyed active slot",
            );
        }
    }
}
