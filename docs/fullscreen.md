# Per-Window Fullscreen (Wayland Surfaces)

## Overview

Fullscreen is a per-window declarative layout property. Emacs is the authority —
client fullscreen requests become events that Emacs handles by setting
`ewm-fullscreen` on the window parameter. The compositor renders fullscreen
surfaces at full output size with a black backdrop, defers the Top layer behind
them, and expands the input hit area to the full output.

Currently only Wayland surfaces support fullscreen (`ewm-toggle-fullscreen`
requires `ewm-surface-id`). Emacs buffer fullscreen (distraction-free mode for
non-surface buffers) is not yet implemented.

## Data Flow

```
Client (e.g. Firefox)
  → xdg_toplevel.set_fullscreen
    → Compositor pushes FullscreenRequest event to Emacs
      → Emacs sets ewm-fullscreen window parameter
        → ewm-layout--refresh sends LayoutEntry with :fullscreen t
          → Compositor apply_output_layout configures surface at full output size
            → Renderer draws backdrop + centered surface, defers Top layer
```

Unfullscreen follows the reverse path. The user can also toggle with `s-f`
(`ewm-toggle-fullscreen`).

## Compositor

### LayoutEntry

`fullscreen: bool` field on `LayoutEntry` (serde default = false). When true:

- **apply_output_layout**: Primary fullscreen entries get configured at full
  output logical size with `XdgToplevelState::Fullscreen` (not `Maximized`).
  Primary computation uses output size (not entry dimensions) for area
  calculation.

- **Rendering** (`render.rs`): Fullscreen entries get a black
  `SolidColorBuffer` backdrop at output origin. Primary entries render at
  native size, centered via `fullscreen_center_offset()`. The Top layer is
  deferred behind fullscreen surfaces (waybar disappears).

- **Input**: `layout_surface_under()` expands the hit area to the full output.
  Pointer mapping is 1:1 for primary entries (with centering offset).

- **Foreign toplevel**: `WindowInfo.is_fullscreen` drives
  `zwlr_foreign_toplevel_handle_v1::State::Fullscreen`.

### XDG Shell Handlers

- `fullscreen_request()`: Pushes `Event::FullscreenRequest { id, output }` to
  Emacs. Does NOT immediately configure — waits for Emacs layout update.
- `unfullscreen_request()`: Pushes `Event::UnfullscreenRequest { id }`.

### Intercepted Keys

`InterceptedKey` has `allow_fullscreen: bool`. The compositor uses this to
gate fullscreen keys through to Emacs even during fullscreen.

## Emacs

### Event Handlers (`ewm.el`)

- `ewm--handle-fullscreen-request`: Sets `ewm-fullscreen` window parameter on
  all windows showing the buffer on the target frame, refreshes layout.
- `ewm--handle-unfullscreen-request`: Clears `ewm-fullscreen` on the focused
  frame only (other outputs stay fullscreen), refreshes layout.

### Toggle Command

`ewm-toggle-fullscreen` (`s-f`): Toggles the `ewm-fullscreen` window parameter
and refreshes layout. Guarded by `ewm-surface-id` — only works for Wayland
surfaces.

### Layout (`ewm-layout.el`)

`ewm-layout--make-output-view` reads the `ewm-fullscreen` window parameter and
includes `:fullscreen t` or `:fullscreen :false` in the layout entry plist sent
to the compositor.

## Future: Emacs Buffer Fullscreen

Extending `s-f` to work on regular Emacs buffers (non-surface) would provide a
universal distraction-free mode. Key challenges:

- The Emacs frame must cover the full output (compositor side — straightforward)
- The minibuffer must be hidden or inaccessible (Emacs architectural constraint)
- Window configuration must be restored exactly on exit
- Frame-output parity enforcement must accommodate auxiliary frames

Possible approaches include creating an auxiliary frame with `(minibuffer . nil)`
or using compositor-level cropping to hide the minibuffer line.
