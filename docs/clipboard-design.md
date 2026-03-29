# Clipboard Integration via wl-copy/wl-paste

## Problem

Emacs is the compositor host, not a Wayland client, so it cannot participate
in `wl_data_device` clipboard exchange directly. We need a bridge for both
text (kill-ring) and binary data (images for `yank-media`).

## Solution

The compositor enables `wlr-data-control-unstable-v1`, which allows
`wl-copy`/`wl-paste` (from `wl-clipboard`) to access the Wayland clipboard.
Emacs shells out to these tools for all clipboard operations. No custom Rust
clipboard logic is needed — Smithay's default `SelectionHandler` handles
client-to-client clipboard natively.

## Architecture

### Data Flow: Emacs → Wayland Client

```
Emacs kills/copies text
  → interprogram-cut-function calls ewm--interprogram-cut-function
  → start-process "wl-copy" with text on stdin
  → wl-copy sets clipboard via wlr-data-control protocol
  → Client pastes normally via wl_data_device
```

### Data Flow: Wayland Client → Emacs

```
Client copies (text or image)
  → Data available on Wayland clipboard
  → Emacs yanks (C-y or yank-media)
  → interprogram-paste-function calls ewm--interprogram-paste-function
  → call-process "wl-paste" to read clipboard
  → Text returned to kill-ring / binary data to yank-media handler
```

### Image Support (yank-media)

`gui-get-selection` is advised to call `wl-paste` for CLIPBOARD requests:

- `TARGETS` → `wl-paste --list-types` → returns available mime types
- `image/png` (etc.) → `wl-paste --no-newline --type image/png` → raw binary

This allows `yank-media` (Emacs 29+) to paste images into modes that
register handlers (e.g., Org mode, telega.el).

## Implementation

### Compositor

Minimal — just enables the protocol:

- `DataControlState::new()` enables `wlr-data-control` for `wl-copy`/`wl-paste`
- `SelectionHandler` uses default `type SelectionUserData = ()`
- No custom `new_selection()` or `send_selection()` overrides

### Emacs (`lisp/ewm.el`)

- `ewm--wl-copy` / `ewm--wl-paste` — cached `executable-find` results
- `ewm--interprogram-cut-function` — async `start-process` to `wl-copy`
- `ewm--interprogram-paste-function` — sync `call-process` to `wl-paste`
- `ewm--gui-get-selection` — `:around` advice on `gui-get-selection` for
  binary clipboard data (images, etc.)
- `ewm--clipboard-enable` / `ewm--clipboard-disable` — called from mode
  enable/disable; no-op if `wl-copy`/`wl-paste` are not installed

## Dependencies

`wl-clipboard` package must be installed for clipboard to work. If absent,
clipboard functions silently no-op.

## Files

| File | What |
|------|------|
| `compositor/src/lib.rs` | `DataControlHandler`, minimal `SelectionHandler` |
| `lisp/ewm.el` | `wl-copy`/`wl-paste` bridge, `gui-get-selection` advice |

## Testing

**Automated**: `cargo test` — clipboard doesn't affect headless tests.

**Manual**:
1. Emacs → client: `M-w` in Emacs, `Ctrl+Shift+V` in foot
2. Client → Emacs: `Ctrl+Shift+C` in foot, `C-y` in Emacs
3. Image: screenshot → `yank-media` in telega/org buffer
4. wl-clipboard: `echo test | wl-copy` then `C-y`; copy in Emacs then `wl-paste`
5. No wl-clipboard: verify no errors when `wl-copy`/`wl-paste` are absent
