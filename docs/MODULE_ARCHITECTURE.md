# Module Architecture

EWM runs as an Emacs dynamic module with the Wayland compositor in a background
thread. This document describes the architecture and how the two execution
contexts communicate.

## Overview

```
┌─ Emacs Process ───────────────────────────────────────────────┐
│                                                                │
│  ┌─ ewm-core.so (Rust) ─────────────────────────────────────┐ │
│  │                                                           │ │
│  │  Compositor Thread          Shared State (Mutex)          │ │
│  │  ┌─────────────┐           ┌───────────────────┐         │ │
│  │  │  calloop    │──────────▶│ focused_surface   │         │ │
│  │  │  Wayland    │           │ surfaces          │         │ │
│  │  └──────┬──────┘           │ outputs           │         │ │
│  │         │                  └─────────▲─────────┘         │ │
│  │    SIGUSR1                           │                   │ │
│  │         │                  ┌─────────┴─────────┐         │ │
│  │         ▼                  │    Elisp API      │         │ │
│  │  ┌─────────────┐           │ ewm-focus-module  │         │ │
│  │  │ Event Queue │──────────▶│ ewm-pop-event     │         │ │
│  │  └─────────────┘           └───────────────────┘         │ │
│  └───────────────────────────────────────────────────────────┘ │
│                                                                │
│  ┌─ ewm.el ────────────────────────────────────────────────────┐
│  │  Buffer-surface mapping, focus callback, user commands      │
│  └─────────────────────────────────────────────────────────────┘
└────────────────────────────────────────────────────────────────┘
```

## Threading Model

1. **Compositor Thread**: Runs calloop event loop, processes Wayland events
2. **Emacs Main Thread**: Runs Elisp, calls into module functions

Emacs Lisp is single-threaded and cannot be called from the compositor thread.
Communication uses shared queues protected by `Mutex`, with locks released
before crossing thread boundaries.

## Event Delivery: Compositor → Emacs

The compositor pushes events to a shared queue and signals Emacs via SIGUSR1:

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

### Rust Side

```rust
static EVENT_QUEUE: OnceLock<Mutex<Vec<Event>>> = OnceLock::new();

pub fn push_event(event: Event) {
    let mut queue = event_queue().lock().unwrap();
    queue.push(event);
    drop(queue);  // Release lock before signaling
    unsafe { libc::raise(libc::SIGUSR1); }
}
```

### Emacs Side

```elisp
(defun ewm--sigusr1-handler ()
  "Handle SIGUSR1 signal from compositor."
  (interactive)
  (ewm--process-pending-events))

(defun ewm--enable-signal-handler ()
  (define-key special-event-map [sigusr1] #'ewm--sigusr1-handler))
```

### Why SIGUSR1 Works

Emacs has built-in support for Unix signals via `special-event-map`. When
SIGUSR1 arrives, Emacs queues a `[sigusr1]` event that runs our handler
at the next safe point in the event loop.

Signal coalescing is fine—multiple rapid events result in one signal, but
the handler drains the entire queue each time.

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

## Command Delivery: Emacs → Compositor

Commands flow in the opposite direction via `COMMAND_QUEUE`:

```
Emacs Main Thread              │  Compositor Thread
                               │
ewm-focus-module(id)           │
  └─ queue.push(Focus{id})     │
  └─ wake LOOP_SIGNAL ────────►│  calloop wakes
                               │    └─ drain_commands()
                               │    └─ handle Focus{id}
```

All `ewm-*-module` functions push to the command queue and wake the
compositor's event loop via `LOOP_SIGNAL`.

## Startup Sequence

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

## API Reference

### Lifecycle

```elisp
(ewm-start)      ; Start compositor thread, returns t/nil
(ewm-stop)       ; Request graceful shutdown
(ewm-running)    ; Check if compositor running
(ewm-socket)     ; Get Wayland socket name
```

### Events

```elisp
(ewm-event-fd)      ; Get pipe fd for select()
(ewm-pop-event)     ; Pop next event as alist, nil if empty
(ewm-drain-events)  ; Clear notification pipe after processing
```

### Commands

All `*-module` functions push to command queue:

```elisp
(ewm-layout-module id x y w h)
(ewm-focus-module id)
(ewm-hide-module id)
(ewm-close-module id)
(ewm-views-module id views-vector)
(ewm-warp-pointer-module x y)
(ewm-screenshot-module &optional path)
(ewm-assign-output-module id output-name)
(ewm-configure-output-module name &key x y width height refresh enabled)
(ewm-intercept-keys-module keys-vector)
(ewm-im-commit-module text)
(ewm-configure-xkb-module layouts &optional options)
(ewm-switch-layout-module layout-name)
(ewm-get-layouts-module)
```

## Timer Usage

| Timer | Purpose | Status |
|-------|---------|--------|
| ~~60Hz polling~~ | Event sync | Removed (SIGUSR1) |
| ~~Minibuffer 50ms~~ | Layout settle | Removed (sync redisplay) |
| ~~Startup sleep~~ | Wait for init | Removed (ready event) |
| Focus debounce | Coalesce rapid changes | 10ms, for UX only |
| Shutdown polling | Wait for thread exit | Kept |
| Frame deletion | Defer during creation | Kept (one-shot) |

The focus debounce timer is for user experience (prevents flicker), not
correctness. All state is synchronous via the module.

## Minibuffer Handling

No timers needed for minibuffer. Synchronous `redisplay t` ensures geometry
is current before layout refresh:

```elisp
(defun ewm--on-minibuffer-setup ()
  (setq ewm--pre-minibuffer-surface-id (ewm-get-focused-id))
  (when-let ((frame-surface-id (frame-parameter (selected-frame) 'ewm-surface-id)))
    (ewm-focus frame-surface-id))
  (redisplay t)
  (ewm-layout--refresh))

(defun ewm--on-minibuffer-exit ()
  (when ewm--pre-minibuffer-surface-id
    (ewm-focus ewm--pre-minibuffer-surface-id)
    (setq ewm--pre-minibuffer-surface-id nil))
  (redisplay t)
  (ewm-layout--refresh))
```

## Benefits

| Metric | Value |
|--------|-------|
| Focus latency | <2ms |
| Race window | 0ms |
| Event sync code | ~25 lines |
| Polling timers | 0 |

## Risk Mitigations

| Risk | Mitigation |
|------|------------|
| Module crash = Emacs crash | `catch_unwind` at thread boundary |
| Threading deadlocks | Single mutex per queue, release before Elisp |
| Development iteration | Debug builds, `ewm-show-state` for inspection |

## Files

- `compositor/src/module.rs` - Dynamic module FFI, queues, defuns
- `compositor/src/lib.rs` - Compositor core
- `compositor/src/event.rs` - Event enum shared with module
- `lisp/ewm.el` - Elisp integration, event handling

## References

- [How to Write Fast(er) Emacs Lisp](https://nullprogram.com/blog/2017/02/14/) - Chris Wellons' article on Emacs dynamic modules, inspiration for this architecture

## Related Documents

- [FOCUS_DESIGN.md](FOCUS_DESIGN.md) - Focus handling and prefix key sequences
