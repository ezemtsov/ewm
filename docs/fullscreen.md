# Fullscreen

Fullscreen is compositor-owned presentation state, not layout input. Emacs
keeps its windows as they are and the compositor draws the selected view over
the whole output. Layout refreshes, migrations and output changes may keep or
clear a selection but never create one (`tests/fullscreen_proptest.rs`).

## Requests

- Client: `xdg_toplevel.set_fullscreen` and the foreign-toplevel request reach
  Emacs as `fullscreen_request` and `unfullscreen_request` events. The
  compositor changes nothing on its own, and the requested output is ignored
  (`tests/fullscreen_request_integration.rs`).
- Emacs: `ewm--fullscreen` selects the buffer's entry through
  `ewm-set-fullscreen-module`. `ewm-toggle-fullscreen` (`s-f`) toggles a tiled
  view through `ewm-toggle-fullscreen-module`.
- Only tiled frames host a selection. A floating buffer is grounded first: it
  takes over the active frame's selected window and the entry is selected.
  Leaving fullscreen floats it back to its saved geometry and restores the
  window's previous buffer, dropping the selection.

## Presentation

- Configure: the primary view is configured at the output's logical size with
  `XdgToplevelState::Fullscreen` (`configure_primary`).
- Render: `push_fullscreen_view` draws a primary view at native size centered
  on the output and a non-primary view stretched to fill it, over a black
  backdrop. The Top layer is deferred behind it (`render_above_top_layer`).
- Input: hit-testing uses `layout_entry_rect`, so pointer positions map to
  the fullscreen view rather than the hosting frame. Intercepted keys without
  `allow_fullscreen` are forwarded to the fullscreen surface.
- Foreign toplevel state follows `is_surface_fullscreen`.

## Not implemented

- A request made before the surface has a window is dropped, so a client that
  asks for fullscreen before mapping is placed as usual.
- Fullscreen for Emacs buffers, a distraction-free mode, is a separate design:
  the minibuffer and frame-output parity both stand in the way.
