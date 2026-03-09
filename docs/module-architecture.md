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
│  │    pipe fd                           │                   │ │
│  │   (open_channel)           ┌─────────┴─────────┐         │ │
│  │         │                  │    Elisp API      │         │ │
│  │         ▼                  │ ewm-focus-module  │         │ │
│  │  ┌─────────────┐           │                   │         │ │
│  │  │ Event Pipe  │──────────▶│ ewm--event-filter │         │ │
│  │  │ (JSON/fd)   │           └───────────────────┘         │ │
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
before crossing thread boundaries. Events flow from compositor to Emacs via
a pipe fd (newline-delimited JSON).

## Event Delivery: Compositor → Emacs

The compositor serializes events to JSON and writes them to a pipe fd created
via `emacs-module-rs`'s `open_channel`. Emacs receives them through a process
filter on the pipe process:

```
Compositor Thread              │  Emacs Main Thread
                               │
push_event(Event::Focus{...})  │
  └─ serde_json::to_vec(event) │
  └─ write to pipe fd ────────►│  ewm--event-filter receives data
                               │    └─ split on newlines
                               │    └─ json-parse-string → alist
                               │    └─ ewm--handle-event(alist)
```

### Rust Side

```rust
static EVENT_WRITER: Mutex<Option<Box<dyn Write + Send>>> = Mutex::new(None);

pub fn push_event(event: Event) {
    let mut guard = EVENT_WRITER.lock().unwrap();
    if let Some(ref mut w) = *guard {
        let mut buf = serde_json::to_vec(&event).unwrap_or_default();
        buf.push(b'\n');
        let _ = w.write_all(&buf);  // Broken pipe = Emacs closed
    }
}
```

### Emacs Side

```elisp
(defun ewm--event-filter (_proc output)
  "Process newline-delimited JSON events from the compositor pipe."
  (setq ewm--event-buffer (concat ewm--event-buffer output))
  (while (string-match "\n" ewm--event-buffer)
    (let ((line (substring ewm--event-buffer 0 (match-beginning 0))))
      (setq ewm--event-buffer (substring ewm--event-buffer (match-end 0)))
      (when (> (length line) 0)
        (let ((event (json-parse-string line :object-type 'alist :false-object nil)))
          (ewm--handle-event event))))))
```

### Why Pipe fd Works

`emacs-module-rs`'s `open_channel` creates a pipe whose read end is registered
with Emacs's event loop as a process. Data written to the write end triggers
the process filter at the next safe point — same integration as network or
subprocess output. Unlike SIGUSR1, pipe writes are never coalesced: each
JSON line is delivered exactly once. Serde serialization replaces the manual
`event_to_lisp` converter, keeping the Event enum as the single source of truth.

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
  (ewm--init-event-channel)         ; Set up pipe before starting compositor
  (ewm-start)                       ; Start compositor thread
  (setq ewm--module-mode t)
  (ewm-mode 1)                      ; Enable BEFORE processing events
  ;; Wait for ready event
  (let ((timeout 50))
    (while (and (> timeout 0) (not ewm--compositor-ready))
      (accept-process-output ewm--event-process 0.1)
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

### Event Channel

```elisp
(ewm-init-event-channel process)  ; Register pipe process for compositor events
```

### Commands

All `*-module` functions push to command queue:

```elisp
(ewm-focus-module id)
(ewm-close-module id)
(ewm-output-layout-module output surfaces-vector)
(ewm-warp-pointer-module x y)
(ewm-screenshot-module &optional path)
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
| ~~60Hz polling~~ | Event sync | Removed (pipe fd) |
| ~~Minibuffer 50ms~~ | Layout settle | Removed (sync redisplay) |
| ~~Startup sleep~~ | Wait for init | Removed (ready event) |
| Focus debounce | Coalesce rapid changes | 10ms, for UX only |
| Shutdown polling | Wait for thread exit | Kept |
| Frame deletion | Defer during creation | Kept (one-shot) |

The focus debounce timer is for user experience (prevents flicker), not
correctness. All state is synchronous via the module.

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

- `compositor/src/module.rs` - Dynamic module FFI, queues, pipe event channel, defuns
- `compositor/src/lib.rs` - Compositor core
- `compositor/src/event.rs` - Event enum shared with module
- `lisp/ewm.el` - Main module initialization, event dispatch
- `lisp/ewm-focus.el` - Bidirectional focus synchronization
- `lisp/ewm-input.el` - Input handling, intercepted keys, prefix sequences
- `lisp/ewm-layout.el` - Per-output layout declaration and refresh
- `lisp/ewm-surface.el` - Surface ↔ buffer mapping
- `lisp/ewm-text-input.el` - Input method bridge
- `lisp/ewm-transient.el` - Transient menus

## References

- [How to Write Fast(er) Emacs Lisp](https://nullprogram.com/blog/2017/02/14/) - Chris Wellons' article on Emacs dynamic modules, inspiration for this architecture

## Related Documents

- [focus-design.md](focus-design.md) - Focus handling and prefix key sequences
