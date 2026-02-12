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
| `focus` | External surface clicked/scrolled | Tell Emacs to show surface buffer |
| `new` | Surface created | Register new surface with Emacs |
| `close` | Surface destroyed | Clean up surface buffer |

## Functions

### Compositor (Rust)

- `set_focus(id)`: Set compositor focus, notify Emacs for external surfaces
- `get_emacs_surface_for_focused_output()`: Find Emacs frame on same output as focused surface

### Emacs (ewm.el)

- `ewm-focus(id)`: Request compositor to focus a surface
- `ewm--handle-focus`: Handle focus event from compositor
- `ewm-input--focus-debounced`: Debounced focus changes to prevent loops
- `ewm-input--on-window-buffer-change`: Sync focus when window's buffer changes
- `ewm-input--on-window-selection-change`: Sync focus when selected window changes

Note: Focus sync uses `window-buffer-change-functions` and
`window-selection-change-functions` instead of `buffer-list-update-hook`.
This avoids spurious focus events from buffer renames (e.g., vterm title updates).

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

### Single Source of Truth

Layout updates (Views/Hide commands) use a single-cache architecture where the
compositor is the source of truth:

```
Emacs (stateless)              │  Compositor (cache)
                               │
ewm-layout--refresh            │
  └─ compute views for surface │
  └─ ewm-views(id, views) ────►│  if views == cached[id]:
                               │    skip (unchanged)
                               │  else:
                               │    cached[id] = views
                               │    apply layout
```

### Why Single Cache?

The dynamic module runs in-process, so IPC cost is negligible (just copying
a few integers). The expensive part is Emacs's `redisplay` and window
traversal, which happens regardless of caching.

Alternative approaches and why they were rejected:

1. **Dual caching** (Emacs + compositor): Risk of cache divergence, more
   complex code, no meaningful performance benefit.

2. **Emacs-only cache**: Compositor becomes stateless, but if Emacs cache
   corrupts, compositor has no way to detect or recover.

3. **Compositor-only cache** (current): Single source of truth, impossible
   to desync, simple Emacs code.

### Deduplication Stats

In practice, ~70% of Views calls are filtered as duplicates. This happens
because Emacs triggers `window-configuration-change-hook` frequently (on
every keystroke in some modes), but actual geometry changes are rare.

### Commands with Deduplication

| Command | Cache Key | Skip Condition |
|---------|-----------|----------------|
| `Views` | surface ID | `views == cached_views[id]` |
| `Hide` | surface ID | `!cached_views.contains(id)` |
| `Focus` | global | `id == focused_surface_id` |
| `TextInputIntercept` | global | `state == cached_state` |
