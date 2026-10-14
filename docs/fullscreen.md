# Fullscreen

Fullscreen is one bit per surface, held on its record and set only by a client
request or an Emacs command. Layouts, output changes and migrations decide where
it shows but never change it (`tests/fullscreen_proptest.rs`). Emacs keeps its
windows as they are and the compositor draws the view over the whole output.

## Requests

- Client: `xdg_toplevel.set_fullscreen` and the foreign-toplevel request set the
  bit at once, with no Emacs round trip; the requested output is ignored. Every
  request is answered with a configure, a repeated one too. A request made
  before Emacs has shown a view configures the client at fullscreen size and
  takes effect on the first view the layout brings
  (`tests/fullscreen_integration.rs`).
- Emacs: `ewm-toggle-fullscreen` (`s-f`) toggles the selected window's view
  through `ewm-toggle-fullscreen-module`, `ewm-set-fullscreen-module` sets it.
  Emacs is not told about client requests, nothing in its window layout changes.

## Where it shows

- The bit drives the configure. What a view shows and where a frame goes follow
  the client: after every layout merge and commit that takes a surface in or out
  of fullscreen, `Layout::reconcile_fullscreen` marks the view each strip frame
  shows fullscreen, its selected entry of a surface committed fullscreen, else
  its first. Parked frames mark none. The marks are what the
  renderer, hit testing, the top-layer deferral and the idle heuristic read, so
  the screen changes once, when the client's buffer of the new size arrives,
  not when it is asked for.
- A strip frame showing a fullscreen view spans the output in the strip, not
  the working area. A float stays a float: its view covers the output in place,
  above the strip, its own frame hidden under it, and it is pinned to the strip
  frame that was active when it went fullscreen (`Frame::pinned_to`). It rides
  in that frame's slot, sliding out and back in with it, and focusing it brings
  the frame to the front. Off screen, it leaves without a fade. It is a plain
  float again, in place and untouched in size, once it shows a view that is not
  fullscreen, while one left with no view at all stays pinned. One view is
  fullscreen per strip frame: a request puts the one before it, the frame's own
  view or a float pinned to the frame, back (`Layout::fullscreen_rivals`).
- The bit outlives the view: a buffer switched away and back is fullscreen
  again, and every view of a fullscreen surface shows it fullscreen when its
  frame is active.
- Floats hide under their strip frame's fullscreen view, for the renderer and
  the pointer alike, unless one of them has focus (`Ewm::float_visible`).

## Presentation

- Configure: the primary view is configured at the output's logical size with
  `XdgToplevelState::Fullscreen` (`configure_primary_entries`).
- Render: `ViewPlace::fullscreen` places a primary view at native size centered
  on the output and a non-primary view filling it, over a black backdrop. The
  Top layer is deferred behind it (`render_above_top_layer`). Every change of
  what a view shows fades on one spring curve (`resize_fade.rs`): the buffer a
  layout asked for, once it comes, over the old contents where they were, at
  1:1, the view holding its fit until then, and a layout refitting a view the
  same way. A fullscreen flip is such a fade over the whole slot, the backdrop
  fading along, and the frame and its other views hide only under an opaque
  backdrop. Views of a frame under a resize grab or on an output whose working
  area changed jump.
- Input: hit-testing reads the same drawn rects as the renderer
  (`Ewm::entry_rect_at`), so pointer positions map to the fullscreen view rather
  than the hosting frame. Intercepted keys without
  `allow_fullscreen` are forwarded to the fullscreen surface.
- Focus meant for a strip frame goes to the fullscreen view pinned over it, so
  keys never land under it (`Ewm::commit_focus`).
- Foreign toplevel state follows the committed state, so a fullscreen surface
  whose view is hidden still reports it.

## Not implemented

- Fullscreen for Emacs buffers, a distraction-free mode, is a separate design:
  the minibuffer stands in the way.
