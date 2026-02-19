# Clipboard Integration — Emacs as Clipboard Manager

## Problem

Standard Wayland clipboard has three limitations for EWM:

1. **Clipboard dies when source app closes** — the app owns the data, no persistence
2. **Emacs is isolated** — it's the compositor host, not a Wayland client, so it can't
   participate in clipboard exchange via normal protocols
3. **No `wlr-data-control`** — clipboard manager tools (`wl-copy`, `wl-paste`, `cliphist`)
   don't work

## Solution

Emacs acts as the central clipboard hub. When a Wayland client copies text, it flows
to Emacs's kill ring. When Emacs kills text, Wayland clients can paste it. Clipboard
persists because Emacs holds it. The `wlr-data-control` protocol is also enabled for
external tools.

## Architecture

### Data Flow: Wayland Client → Emacs

```
Client copies text
  → Smithay calls SelectionHandler::new_selection()
  → Compositor creates UnixStream pair, asks client to write via
    request_data_device_client_selection()
  → Background thread reads from socket, pushes SelectionChanged event
  → Emacs receives event via SIGUSR1 → ewm--handle-selection-changed
  → Text pushed to kill ring via kill-new
```

### Data Flow: Emacs → Wayland Client

```
Emacs kills/copies text
  → interprogram-cut-function calls ewm-set-selection-module
  → ModuleCommand::SetSelection pushed to command queue
  → Compositor calls set_data_device_selection() with Arc<[u8]> user data
  → Client pastes → Smithay calls SelectionHandler::send_selection()
  → Background thread writes user data to client's fd
```

### Echo Prevention

`ewm--last-selection` tracks the most recent clipboard text to prevent infinite
loops: Emacs copy → compositor → SelectionChanged → kill-new → interprogram-cut →
compositor... The guard checks `(not (equal text ewm--last-selection))` on both sides.

## Implementation

### Compositor (`compositor/src/lib.rs`)

**SelectionHandler** — `SelectionUserData` is `Arc<[u8]>` (reference-counted byte
slice), matching niri's pattern. Two methods:

- `new_selection()` — called when any client (including via `wlr-data-control`) sets
  a clipboard selection. Filters for text mime types, then calls
  `read_client_selection_to_emacs()`.
- `send_selection()` — called when a client requests paste. Clears `O_NONBLOCK` on the
  fd (otherwise `write_all` stops halfway), then writes the `Arc<[u8]>` data in a
  background thread.

**read_client_selection_to_emacs()** — creates a `UnixStream::pair()` (used instead of
`rustix::pipe` because the pipe feature isn't enabled in Smithay's rustix re-export),
passes the write end to `request_data_device_client_selection()`, reads from the read
end in a background thread, and pushes a `SelectionChanged` event to Emacs.

**DataControlHandler** — enables `wlr-data-control-unstable-v1` protocol so external
tools (`wl-copy`, `wl-paste`, `cliphist`) can access the clipboard. Initialized with
primary selection support and no client filtering (`|_| true`).

**SetSelection command** — converts text to `Arc<[u8]>` and calls
`set_data_device_selection()` with three mime types: `text/plain;charset=utf-8`,
`text/plain`, and `UTF8_STRING`.

### Emacs (`lisp/ewm.el`)

- `ewm--handle-selection-changed` — pushes received text to kill ring, deduplicating
  against current `car kill-ring`
- `ewm--interprogram-cut-function` — set as `interprogram-cut-function` when EWM mode
  is active; sends kill ring additions to compositor
- No `interprogram-paste-function` needed — clipboard is kept in sync via push-based
  `SelectionChanged` events rather than polling

## Files

| File | What |
|------|------|
| `compositor/src/lib.rs` | `SelectionHandler` (`Arc<[u8]>`, `new_selection`, `send_selection`), `DataControlHandler`, `read_client_selection_to_emacs`, `SetSelection` handling |
| `compositor/src/module.rs` | `SetSelection` command variant, `set_selection_module` defun, `SelectionChanged` event conversion |
| `compositor/src/event.rs` | `SelectionChanged { text }` event variant |
| `lisp/ewm.el` | Event handler, `interprogram-cut-function`, echo prevention |

## Smithay APIs

| API | Purpose |
|-----|---------|
| `set_data_device_selection()` | Compositor sets clipboard (Emacs → clients) |
| `request_data_device_client_selection()` | Read client's clipboard into an fd |
| `DataControlState::new()` | Enable `wlr-data-control` protocol |
| `SelectionHandler::new_selection()` | Intercept clipboard changes |
| `SelectionHandler::send_selection()` | Serve clipboard data to requesting clients |

## Testing

**Automated**: `cargo test` — all existing tests pass (clipboard doesn't affect headless tests).

**Manual**:
1. Emacs → client: `M-w` in Emacs, `Ctrl+Shift+V` in foot
2. Client → Emacs: `Ctrl+Shift+C` in foot, `C-y` in Emacs
3. Persistence: copy in app, close app, paste in another app
4. wl-clipboard: `echo test | wl-copy` then `C-y`; copy in Emacs then `wl-paste`
