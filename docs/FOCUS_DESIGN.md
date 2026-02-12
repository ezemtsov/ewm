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

## Module Architecture: Signal-Based Event Sync

### The Fundamental Problem

The compositor runs as a thread within Emacs. Two execution contexts must sync:

```
┌─────────────────────────────────────────────────────────┐
│                    Emacs Process                        │
│  ┌─────────────────┐         ┌─────────────────────┐   │
│  │  Main Thread    │  SIGUSR1│  Compositor Thread  │   │
│  │  (Lisp, UI)     │◄────────│  (Wayland, Input)   │   │
│  └─────────────────┘         └─────────────────────┘   │
│          │                              │              │
│          └──────── Event Queue ─────────┘              │
└─────────────────────────────────────────────────────────┘
```

Emacs Lisp is single-threaded and cannot be called from the compositor thread.
The solution: compositor pushes events to a shared queue and signals Emacs
via SIGUSR1. Emacs handles the signal and drains the queue.

### Solution: SIGUSR1 + Shared Queue

```
Compositor Thread              │  Emacs Main Thread
                               │
push_event(Event::Focus{...})  │
  └─ queue.push(event)         │
  └─ raise(SIGUSR1) ──────────►│  [sigusr1] event received
                               │    └─ ewm--sigusr1-handler
                               │    └─ ewm--process-pending-events
                               │    └─ while (event = pop_event())
                               │         ewm--handle-module-event(event)
```

### Implementation

#### Rust Side (15 lines)

```rust
static EVENT_QUEUE: OnceLock<Mutex<Vec<Event>>> = OnceLock::new();

pub fn push_event(event: Event) {
    let mut queue = event_queue().lock().unwrap();
    queue.push(event);
    drop(queue);  // Release lock before signaling
    unsafe { libc::raise(libc::SIGUSR1); }
}
```

#### Emacs Side (10 lines)

```elisp
(defun ewm--sigusr1-handler ()
  "Handle SIGUSR1 signal from compositor."
  (interactive)
  (ewm--process-pending-events))

(defun ewm--enable-signal-handler ()
  (define-key special-event-map [sigusr1] #'ewm--sigusr1-handler))

(defun ewm--disable-signal-handler ()
  (define-key special-event-map [sigusr1] nil))
```

### Why SIGUSR1 Works

Emacs has built-in support for Unix signals via `special-event-map`. When
SIGUSR1 arrives, Emacs queues a `[sigusr1]` event that runs our handler
at the next safe point in the event loop.

Signal coalescing is fine—multiple rapid events result in one signal, but
the handler drains the entire queue each time.

### Startup Sequence

The compositor sends a `ready` event after initialization. Emacs waits for
this event instead of using arbitrary sleep delays:

```elisp
(defun ewm-start-module ()
  (ewm-start)                      ; Start compositor thread
  (setq ewm--module-mode t)
  (ewm-mode 1)                     ; Enable BEFORE processing events
  (ewm--enable-signal-handler)
  ;; Wait for ready event
  (let ((timeout 50))
    (while (and (> timeout 0) (not ewm--compositor-ready))
      (sleep-for 0.1)
      (ewm--process-pending-events)
      (cl-decf timeout)))
  ...)
```

**Critical**: `ewm-mode` must be enabled before the wait loop so that
`output_detected` events properly register frames as pending.

### Events

| Event | Rust | Purpose |
|-------|------|---------|
| `ready` | `Event::Ready` | Compositor initialized |
| `new` | `Event::New{id,app,output}` | Surface created |
| `close` | `Event::Close{id}` | Surface destroyed |
| `focus` | `Event::Focus{id}` | External surface focused |
| `title` | `Event::Title{id,app,title}` | Surface title changed |
| `output_detected` | `Event::OutputDetected(info)` | Monitor connected |
| `output_disconnected` | `Event::OutputDisconnected{name}` | Monitor removed |
| `outputs_complete` | `Event::OutputsComplete` | All outputs sent |
| `key` | `Event::Key{keysym,utf8}` | Intercepted key |

### Timer Usage (Minimal)

| Timer | Purpose | Status |
|-------|---------|--------|
| ~~60Hz polling~~ | Event sync | Removed (SIGUSR1) |
| ~~Minibuffer 50ms~~ | Layout settle | Removed (sync redisplay) |
| ~~Startup sleep~~ | Wait for init | Removed (ready event) |
| Shutdown polling | Wait for thread exit | Kept |
| Frame deletion | Defer during creation | Kept (one-shot) |

### Minibuffer Handling

No timers needed. Synchronous `redisplay t` ensures geometry is current:

```elisp
(defun ewm--on-minibuffer-setup ()
  (when-let ((state ewm--input-state))
    (setq ewm--pre-minibuffer-surface-id
          (ewm-input-state-last-focused-id state))
    (when-let ((frame-surface-id (frame-parameter (selected-frame) 'ewm-surface-id)))
      (ewm-focus frame-surface-id)))
  (redisplay t)
  (ewm-layout--refresh))

(defun ewm--on-minibuffer-exit ()
  (when ewm--pre-minibuffer-surface-id
    (ewm-focus ewm--pre-minibuffer-surface-id)
    (setq ewm--pre-minibuffer-surface-id nil))
  (redisplay t)
  (ewm-layout--refresh))
```

### Benefits

1. **Simple**: ~25 lines total (Rust + Elisp)
2. **Zero latency**: Signal delivered immediately
3. **No polling**: No timers for event sync
4. **No resources**: No sockets, files, or processes
5. **Reliable**: Built-in Emacs signal handling

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
