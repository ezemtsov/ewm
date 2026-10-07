# ext-workspace-v1

EWM exposes its per-output frame strip via `ext-workspace-v1` so that
external panels (waybar, DankMaterialShell, noctalia, etc.) can render
the strip and request switches.

The wiring is intentionally thin: the protocol is a *mirror* of the
strip, with one workspace handle per Emacs frame. Frame placement lives
in the compositor. Lisp declares frame contents and asks for moves, and
the compositor keeps clients consistent with its strips.

## Source of truth

| Side  | Owns                                | Reads from        |
|-------|-------------------------------------|-------------------|
| rust  | `output_strips` (`HashMap<String, Strip>`)      | itself. Lisp asks via `ewm-prepare-frame` and `ewm-swap-frame` |
| lisp  | frame contents (`ewm-layout`)                    | rust via `ewm-get-strip-frame-ids` and `ewm-get-active-frame-id` |
| protocol mirror | per-client ext-workspace handles        | rust strips |

Every other source of "frame order" is wrong. In particular:
`frame-list` (Emacs creation order), `HashMap` iteration of the strips,
and any client's own `WindowManager.windowsets` append order are *not*
sources of truth.

## Mapping to ext-workspace-v1

- **Workspace group**: one per output.
- **Workspace handle**: one per Emacs frame. Bound to the frame's
  `surface_id` for the frame's lifetime. The handle is *not*
  recreated on reorder.
- **`coordinates`**: `[0, k]` where `k` is the frame's current strip
  slot. Reissued on every reorder.
- **`name`**: `(k + 1)` as a decimal string. Reissued on every
  reorder. Reflects current slot, not the frame's identity.
- **`state`**: `Active` on the workspace whose frame is at
  `strip.active_idx`. No other state bits are emitted yet.
- **`id` event**: sent as the decimal `surface_id`. The protocol
  reserves `id` for values stable across sessions; EWM's surface ids
  are runtime-only, so this is a minor spec deviation. Clients
  should not persist preferences keyed by it.
- **Capabilities**: `Activate` only. `Assign` (cross-output move) is
  TODO.

## Invariants

In the table below, `O` ranges over outputs, `k` over slot indices.
"Mirror" means the per-client protocol state.

| #  | Invariant                                                                                          |
|----|----------------------------------------------------------------------------------------------------|
| I1 | The strip is the only copy of frame order. Lisp reads it through `ewm-get-strip-frame-ids`.        |
| I2 | A layout command changes neither a strip's frame set nor its order.                                 |
| I3 | The mirror has exactly one workspace handle per `(O, frame.surface_id)` in `output_strips`.        |
| I4 | A mirror workspace's `coordinates[1]` equals the slot index of its frame in `output_strips[O]`.    |
| I5 | A mirror workspace's `name` equals `(coordinates[1] + 1)` as decimal.                              |
| I6 | Exactly one workspace per non-empty group has `Active` state, at slot `strip.active_idx`.          |
| I7 | The active workspace's frame's `surface_id` equals `focused_frame_id` (when focus is on a frame).  |
| I8 | After `frame-new`: a frame appended right of the previously active one; new frame is active.       |
| I9 | Closing the active frame at slot `K` of strip length `N` (where `N ≥ 2`, enforced by `ewm--prevent-frame-deletion`): new active becomes `K` if `K < N-1` (right neighbour shifts into the closed slot), otherwise `K-1` (left neighbour, since the closed slot was the rightmost). |
| I10 | After close of a non-active frame: active follows its frame (slot may shift left by one if the closed slot was left of active). |
| I11 | After move-left/right: only the two affected frames swap slots; `active_idx` follows the moved frame if it was active, otherwise unchanged. |
| I12 | No `workspace_leave` is implicit. When a workspace leaves a group it gets an explicit `workspace_leave`. Groups send `workspace_leave` for every member before `removed`. |

## Ownership of decisions

When more than one party could pick, exactly one of them does:

| Decision                         | Owner   | Mechanism                          |
|----------------------------------|---------|------------------------------------|
| Frame creation                   | lisp    | `ewm-frame-new` → `make-frame`     |
| Frame placement                  | rust    | `ewm-prepare-frame` names the output and the frame to insert right of. `new_toplevel` inserts |
| Strip reorder                    | rust    | `ewm-frame-move-left/right` → `ewm-swap-frame` → `Strip::move_frame` |
| Active frame on reorder          | rust    | the active slot follows its frame  |
| Active frame on close (I9, I10)  | rust    | lisp passes an explicit frame to `delete-frame`; Rust applies active-close or inactive-close strip rules when that frame's toplevel is destroyed |
| Workspace handle lifecycle       | rust    | `protocols::workspace::refresh`    |
| Coordinate / name / state events | rust    | same                               |
| Activate request (client click)  | rust→lisp | `Event::ActivateWorkspace { id }` names the frame's surface id. Lisp selects that frame |

Close-time focus is compositor-owned because Emacs can change selection
and send layout refreshes before `xdg_toplevel.destroy` reaches the
compositor. Lisp records active-close intent synchronously before
calling `delete-frame`; if a later layout omits that closing frame, the
compositor keeps it anchored at its old slot until destroy. The
compositor then removes the destroyed slot with active-close semantics
(right neighbour, else left) or ordinary inactive-close semantics
(preserve active frame).

## Refresh model

`protocols::workspace::refresh()` runs every event-loop iteration. It
diffs the protocol mirror against `output_strips` and emits only the
changed events (`name`, `coordinates`, `state`, plus `workspace_enter` /
`workspace_leave` on cross-output moves). A single `done()` is emitted
at the end of each iteration that produced any change.

Refresh runs after `bind`, so freshly-bound clients always see the
current state on their first `done`.

## Client expectations

Clients are expected to read `coordinates` (or `name`) to render
strip order. Clients that display workspaces in advertisement order
without consulting `coordinates` will display them in the order
they were originally created, not the current strip order. EWM does
not work around that on the server side; it is fixed in the client.
