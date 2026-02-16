# EWM Focus Design

## Overview

EWM has a unique focus model that bridges Wayland compositor focus with Emacs frame/window focus. The compositor manages keyboard focus for surfaces while coordinating with Emacs to ensure a consistent user experience.

## Key Concepts

### Surface Types

1. **Emacs surfaces** (`emacs_surfaces`): Frames belonging to the Emacs process. Identified by matching PID.
2. **External surfaces**: Windows from other applications (Firefox, terminals, etc.)

### Focus States

- **Compositor focus** (`focused_surface_id`): Which surface has keyboard focus at the Wayland level
- **Emacs focus**: Which frame/window is selected in Emacs
- **Pointer location**: Current mouse position (tracked separately from focus)

## Focus Behaviors

### Input-to-Focus

Any input action (click OR scroll) focuses the surface under the pointer:
1. Compositor sets `focused_surface_id` to the surface under pointer
2. If it's an external surface, sends `Focus { id }` event to Emacs
3. Emacs shows the surface's buffer and selects its window

This unified model means:
- Scrolling a surface focuses it (keyboard focus follows)
- Clicking a surface focuses it
- Mere hover without input does NOT change focus

### Intercepted Keys (Super-key bindings)

When a Super-key binding is pressed while focus is on an external surface:
1. Compositor intercepts the key (doesn't forward to surface)
2. Finds the Emacs frame on the **same output as the focused surface**
3. Switches keyboard focus to that Emacs frame
4. Forwards the key to Emacs

Because scroll updates focus, the focused surface is always where the user last interacted, ensuring intercepted keys route to the correct Emacs frame.

### Prefix Key Sequences

When a prefix key (C-x, C-h, M-x) is intercepted from an external app, focus
must stay on Emacs until the key sequence completes. Without this, popups like
which-key would appear but the user couldn't interact with them.

#### The Problem

EXWM solves this with X11 keyboard grabbing - during a prefix sequence, ALL keys
go to Emacs regardless of focus. Wayland doesn't have an equivalent mechanism.

Without special handling:
1. User presses C-x in Firefox
2. Compositor intercepts, redirects to Emacs
3. which-key popup appears (triggers `window-configuration-change-hook`)
4. Hook calls `ewm-layout--refresh`, which syncs focus back to Firefox
5. User's next keypress goes to Firefox instead of completing the sequence

#### Solution: Compositor-side Prefix Tracking

During initialization, Emacs tells the compositor which intercepted keys are
prefix keys (bound to keymaps). The compositor uses this to track state:

```
Initialization:
  Emacs scans keymaps → sends intercept specs with :is-prefix flag

Runtime:
  Prefix key intercepted → compositor sets IN_PREFIX_SEQUENCE=true
  Other sync paths check ewm--focus-locked-p → see flag=true → skip sync
  Debounced sync timer fires → clears flag → refreshes layout
```

Key insight: Compositor only SETS the flag true on prefix keys, never clears it.
The debounced `ewm-input--sync-focus` always clears the flag first (avoiding
circular dependency), then checks other conditions before refreshing.

#### Implementation

**Rust side:**
- `InterceptedKey.is_prefix` - marks prefix keys
- `IN_PREFIX_SEQUENCE: AtomicBool` - the tracking flag
- `ewm-in-prefix-sequence-p` - Emacs queries the flag
- `ewm-clear-prefix-sequence` - Emacs clears the flag

**Elisp side:**
- `ewm--event-to-intercept-spec` adds `:is-prefix` based on `(keymapp (key-binding ...))`
- `ewm--focus-locked-p` - centralized check for all focus sync paths:

```elisp
(defun ewm--focus-locked-p ()
  "Return non-nil if focus should not be synced to surfaces."
  (or (active-minibuffer-window)
      (> (minibuffer-depth) 0)
      prefix-arg
      (ewm-in-prefix-sequence-p)
      (and overriding-terminal-local-map
           (keymapp overriding-terminal-local-map))))
```

#### Critical Design Decisions

1. **Don't clear flag on non-prefix intercepts**: If user presses C-x then s-left
   (an intercepted non-prefix key), the flag must stay true. Only the debounced
   sync timer clears it.

2. **Centralized focus lock check**: Multiple code paths call `ewm-layout--refresh`:
   - `window-configuration-change-hook` (which-key popup)
   - `window-size-change-functions`
   - `minibuffer-setup-hook` / `minibuffer-exit-hook`

   All these paths check `ewm--focus-locked-p` (which includes prefix sequence
   check) before syncing focus.

3. **Debounced sync clears flag first**: `ewm-input--sync-focus` always clears
   the prefix sequence flag before checking other conditions. This avoids a
   circular dependency where the flag could never be cleared because the check
   itself prevented clearing.

### Mouse-Follows-Focus

When `ewm-mouse-follows-focus` is enabled, the pointer warps to the center of a
window when it gains focus via keyboard (e.g., `C-x o`, windmove). This ensures
the pointer is always in the active window for subsequent mouse interactions.

The implementation includes a pointer-in-window check inspired by
[exwm-mff](https://codeberg.org/emacs-weirdware/exwm-mff): if the pointer is
already inside the target window, no warp occurs. This prevents unnecessary
pointer jumps when keyboard-switching to a window the mouse happens to be over.

Key functions:
- `ewm-input--pointer-in-window-p`: Checks if pointer is within window bounds
- `ewm-input--warp-pointer-to-window`: Warps pointer to window center (if needed)
- `ewm-get-pointer-location`: Queries compositor for current pointer position

### Why Input-to-Focus?

Previous design had keyboard focus only change on click. This caused issues:
- User scrolls Firefox on external monitor
- User presses M-x
- M-x would route to primary monitor (last clicked) instead of external

With input-to-focus:
- Scroll Firefox → Firefox has focus, external monitor is "active"
- Press M-x → routes to Emacs frame on external monitor

## Module Events

Events are pushed to a shared queue and delivered to Emacs via SIGUSR1:

| Event | When Sent | Purpose |
|-------|-----------|---------|
| `ready` | Compositor fully initialized | Signal Emacs to begin setup |
| `new` | Surface created | Register new surface with Emacs |
| `close` | Surface destroyed | Clean up surface buffer |
| `title` | Surface title changes | Update buffer name |
| `focus` | External surface clicked/scrolled | Tell Emacs to show surface buffer |
| `output_detected` | Output connected | Create Emacs frame for output |
| `output_disconnected` | Output disconnected | Clean up frame |
| `outputs_complete` | All outputs reported | Trigger initial layout |
| `text-input-activated` | Text field focused in surface | Enable input method bridge |
| `text-input-deactivated` | Text field unfocused | Disable input method bridge |
| `key` | Intercepted key with UTF-8 | Forward key event to Emacs |
| `state` | Debug state requested | Populate `*ewm-state*` buffer |
| `working_area` | Layer-shell exclusive zone changes | Adjust frame geometry |

## Functions

### Compositor (Rust)

- `set_focus(id)`: Set logical focus, mark keyboard dirty, notify Emacs for external surfaces
- `focus_surface(id, notify_emacs)`: Wrapper around `focus_surface_with_source`
- `focus_surface_with_source(id, notify_emacs, source, context)`: Set logical focus with debug tracking
- `sync_keyboard_focus()`: Resolve `focused_surface_id` → `WlSurface`, call `keyboard.set_focus()` if dirty
- `get_emacs_surface_for_focused_output()`: Find Emacs frame on same output as focused surface

### Emacs (ewm.el)

- `ewm-focus(id)`: Request compositor to focus a surface
- `ewm--handle-focus`: Handle focus event from compositor
- `ewm-input--on-post-command`: `post-command-hook` handler, schedules debounced focus sync
- `ewm-input--sync-focus`: Clears prefix sequence flag, syncs focus if not locked
- `ewm--focus-locked-p`: Centralized check — returns non-nil during minibuffer, prefix args, prefix key sequences

Note: Focus sync uses `post-command-hook` with a debounced timer (0.01s).
This lets Emacs "settle" after commands before syncing focus to the compositor,
avoiding spurious focus events during transient states.

## Multi-Monitor Behavior

Each output typically has one Emacs frame. When the pointer moves between outputs:
- Scroll/hover work on surfaces under the pointer
- Intercepted keys route to the Emacs frame on that output
- Click focus updates `focused_surface_id`

## Design Rationale

The input-to-focus model was chosen over alternatives:

1. **Click-only focus + pointer-based key routing**: More complex, requires tracking both focus and pointer location for routing decisions.

2. **Full focus-follows-mouse (hover = focus)**: Too aggressive, causes focus changes during casual mouse movement.

3. **Input-to-focus (current)**: Simple unified model where any interaction (click or scroll) activates that location. Matches user intent: "I'm interacting here, so this is active."

## Layout Synchronization

### Per-Output Declarative Layout

Emacs sends one `OutputLayout` command per output — a complete declaration of
what's visible on that output with frame-relative coordinates. The compositor
materializes it via `apply_output_layout`, which is the single entry point for
positioning, output association, and scale notification.

```
Emacs                          │  Compositor
                               │
ewm-layout--refresh            │
  └─ compute layout per output │
  └─ ewm-output-layout ───────►│  apply_output_layout(output, entries)
                               │    ├─ diff old vs new surface sets
                               │    ├─ output.enter/leave as needed
                               │    ├─ send scale/transform
                               │    └─ queue_redraw(&output)
```

Each `OutputLayout` replaces the previous layout for that output. The compositor
diffs surface ID sets to send `wl_surface.enter`/`leave` incrementally.

### Deduplication

`Focus` and `TextInputIntercept` commands still deduplicate:

| Command | Cache Key | Skip Condition |
|---------|-----------|----------------|
| `Focus` | global | `id == focused_surface_id` |
| `TextInputIntercept` | global | `state == cached_state` |

## Keyboard Focus Synchronization

### The Two Levels of Focus

EWM tracks focus at two levels that must stay in sync:

1. **Logical focus** (`focused_surface_id`): The compositor's idea of which surface
   should have input. Updated by clicks, scrolls, xdg_activation, Emacs commands.
2. **Wayland keyboard focus** (`keyboard.set_focus()`): The actual Wayland protocol
   state that determines which surface receives key events.

A bug where these diverge is invisible — the surface appears focused (renders with
focus decorations, Emacs shows its buffer) but keyboard input goes elsewhere.

### Comparison with Niri

[Niri](https://github.com/YaLTeR/niri) uses a fully deferred focus model:

```
Any focus trigger              refresh() (main loop)
  activate_window() ──────►  update_keyboard_focus()
  layout state changes           │
  queue_redraw_all()             ├─ compute focus from layout state
                                 ├─ compare with current
                                 └─ keyboard.set_focus() ← ONLY CALL SITE
```

- `keyboard.set_focus()` is called in exactly **1 function** (`update_keyboard_focus`)
- That function is called from exactly **1 place** (`refresh()` in the main loop)
- All focus triggers (clicks, activation, keybinds) just update layout state
- Keyboard focus syncs on the next refresh cycle

This is robust: impossible to forget a `keyboard.set_focus()` call, and focus
naturally settles after all state changes complete (e.g., surface replacement).

### Why EWM Cannot Fully Defer

EWM's `intercept_redirect` path requires **atomic focus + key forwarding**:

1. User presses `C-x` while Firefox has focus
2. Compositor intercepts the key (not forwarded to Firefox)
3. Must set Wayland keyboard focus to Emacs **immediately**
4. Must re-send the key to Emacs **in the same handler**

If step 3 were deferred to a refresh cycle, the key in step 4 would be sent to
whatever surface currently has Wayland keyboard focus (still Firefox), not Emacs.

Niri doesn't have this constraint because its intercepted keybinds execute
compositor-internal actions. EWM forwards intercepted keys to a client (Emacs),
which requires the focus change to happen first.

A second constraint is **layout ownership**. Niri's `update_keyboard_focus()`
recomputes focus from scratch each cycle because the compositor owns the full
layout state. In EWM, layout lives in Emacs — the compositor only knows
`focused_surface_id`. It cannot derive "who should have focus" from layout state;
it must track focus incrementally.

### Desired Architecture: Hybrid Model

```
                            ┌──────────────────────────────┐
                            │    focused_surface_id        │
                            │    (single source of truth)  │
                            └──────────┬───────────────────┘
                                       │
              ┌────────────────────────┼─────────────────────────┐
              │ DEFERRED PATH          │  IMMEDIATE PATH         │
              │                        │                         │
              │  xdg_activation        │  intercept_redirect     │
              │  ModuleCommand::Focus  │   (must set focus +     │
              │  click / scroll        │    re-send key in same  │
              │  toplevel_destroyed    │    handler)             │
              │                        │                         │
              │  Set focused_surface_id│  Set focused_surface_id │
              │  Set keyboard_focus    │  Set keyboard_focus     │
              │  Set dirty flag ───┐   │  keyboard.set_focus()   │
              │                    │   │  Clear dirty flag       │
              │                    ▼   │                         │
              │         ┌──────────────┴──┐                      │
              │         │sync_kbd_focus() │                      │
              │         │                 │                      │
              │         │ if dirty:       │                      │
              │         │   resolve id    │                      │
              │         │   → WlSurface   │                      │
              │         │   kbd.set_focus │                      │
              │         │   dirty = false │                      │
              │         └─────────────────┘                      │
              │            Called from:                           │
              │            • handle_keyboard_event (top)         │
              │            • after ModuleCommand batch           │
              │            • main loop tick                      │
              └──────────────────────────────────────────────────┘
```

**Deferred path**: Most focus changes set `focused_surface_id` + dirty flag.
The `sync_keyboard_focus()` function resolves the ID to a `WlSurface` and calls
`keyboard.set_focus()`. This catches missed syncs and handles surface replacement
(e.g., Firefox surface 2 → 3) naturally.

**Immediate path**: Only `intercept_redirect` calls `keyboard.set_focus()`
directly, because it must forward the key in the same handler. It also clears the
dirty flag to prevent a redundant sync.

### Benefits

1. **Eliminates "forgot to sync" bugs**: New focus-changing code paths only need
   to set `focused_surface_id` and mark dirty. The sync function handles the rest.
2. **Handles surface replacement**: If a surface is replaced between focus change
   and sync, the sync resolves the current `focused_surface_id` (which Emacs has
   already updated) to the correct `WlSurface`.
3. **Auditable**: Only 2 call sites for `keyboard.set_focus()` (sync function +
   intercept_redirect), down from 13.

### Why Emacs Must Own Layout

Niri can recompute focus from layout state because the compositor owns the full
layout: workspace assignments, column ordering, window stacking. Could EWM move
layout ownership to the compositor for the same robustness?

No — Emacs IS the layout engine:

1. **Window tree**: Emacs's window tree (splits, sizing, ordering) is the layout.
   Reimplementing `split-window`, `balance-windows`, `display-buffer-alist` in
   the compositor would duplicate Emacs's window manager without gaining anything.

2. **Buffer ↔ surface mapping**: Emacs decides which buffer shows in which window
   via `display-buffer-alist`, `pop-to-buffer`, dedicated windows, etc. This
   mapping is deeply integrated with user configuration.

3. **Tabs**: `tab-bar-mode` multiplexes window configurations. The compositor
   cannot know which surfaces belong to which tab without Emacs telling it.

4. **User customization**: Users customize layout via Elisp (hooks, advices,
   `display-buffer-alist` rules). Moving layout to the compositor would require
   a new configuration language for something Elisp already handles well.

The current model — Emacs owns layout, compositor owns rendering and input —
matches the natural boundary. The hybrid keyboard focus sync (deferred + dirty
flag) provides robustness without requiring the compositor to understand layout.

## Session Lock (ext-session-lock-v1)

### Design Principle: Lock as a Mode

Session lock is a **global mode** that overrides normal compositor behavior. It is
NOT a special surface type — it's a state that changes how every subsystem operates.
Every subsystem must check lock state at its entry point and branch early, rather
than sprinkling lock checks throughout normal logic.

Reference: [niri](https://github.com/YaLTeR/niri) treats lock as a priority branch
in `update_keyboard_focus()`, `commit()`, rendering, and input — the same pattern
EWM follows.

### State Machine

```
LockState::Unlocked ──lock()──► LockState::Locking(SessionLocker)
                                       │
                          all outputs render locked frame
                                       │
                                       ▼
                                LockState::Locked(ExtSessionLockV1)
                                       │
                                   unlock()
                                       │
                                       ▼
                                LockState::Unlocked
```

`is_locked()` returns true for both `Locking` and `Locked`.

### Subsystems Affected

Lock touches **6 subsystems**. When adding lock-related features, audit all of them:

| Subsystem | Entry point | Lock behavior |
|-----------|-------------|---------------|
| **Focus resolution** | `sync_keyboard_focus()` | Priority branch: resolve to `lock_surface_focus()` |
| **Keyboard input** | `handle_keyboard_event()` | Block all keys except VT switch; forward to lock surface |
| **Pointer input** | `handle_pointer_motion/button/scroll` | Route to lock surface; skip click/scroll-to-focus |
| **Rendering** | `collect_render_elements()` | Early return: render only lock surface + background |
| **Surface commits** | `CompositorHandler::commit()` | Queue redraw for lock surface commits (not in `space.elements()`) |
| **Frame completion** | DRM `queue_frame` | Track `LockRenderState` per-output; confirm lock when all rendered |

### Focus During Lock

Lock surface focus follows the **deferred path** via `sync_keyboard_focus()`:

```
new_surface() stores lock surface
  → sets keyboard_focus_dirty = true
  → sync_keyboard_focus() runs on next tick
  → is_locked() = true → resolve to lock_surface_focus()
  → keyboard.set_focus(lock_surface)
```

The input handler (`input.rs`) also sets focus directly on each key event as a
**redundant immediate path** — this handles the race where keys arrive before the
first sync tick. Both paths set the same target (lock surface), so they don't
conflict.

### Unlock Focus Restoration

On unlock:
1. `keyboard_focus` is cleared to `None` (invalidates stale tracking)
2. `pre_lock_focus` is restored via `focus_surface_with_source()`
3. `sync_keyboard_focus()` resolves the restored ID to a `WlSurface`

Step 1 is critical: during lock, the input handler's direct `keyboard.set_focus()`
calls don't update the tracked `keyboard_focus` field. Without clearing it, the
sync function would see `keyboard_focus == restored_focus` (stale value from
pre-lock) and skip the actual `keyboard.set_focus()` call, leaving Smithay's
internal keyboard state pointing at the destroyed lock surface.

### Per-Output State

Each output tracks independently:
- `lock_surface: Option<LockSurface>` — the lock surface for this output
- `lock_render_state: LockRenderState` — whether a locked frame has been rendered

Lock is confirmed (`Locking` → `Locked`) only when ALL outputs have
`lock_render_state == Locked`. This prevents briefly showing unlocked content
on a slow output.

### Commit Handler: Surface Type Dispatch

`CompositorHandler::commit()` dispatches by surface type in this order:
1. **Layer surfaces** — `handle_layer_surface_commit()` (early return)
2. **Popups** — `popups.commit()` + initial configure
3. **Windows** — `space.elements()` lookup → `on_commit()` + redraw
4. **Lock surfaces** — `is_locked()` + output scan → redraw

If adding a new surface type, it must be added to this dispatch chain.
Lock surfaces need special handling because they aren't tracked in
`space.elements()` — they're stored per-output in `OutputState`.
