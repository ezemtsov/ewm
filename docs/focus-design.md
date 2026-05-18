# Focus, in EWM

This document is about how keyboard focus works in EWM, and why it is shaped
the way it is.  It is meant to be read by someone wandering into the focus
code for the first time, not as a reference for the symbols.

## What EWM is, and what makes focus hard

EWM is a Wayland compositor that runs as a thread inside Emacs.  It tries
to make Wayland applications -- terminals, browsers, anything that speaks
the Wayland protocol -- feel native inside Emacs's window layout, so that
managing them happens through the same commands and conventions you already
use for buffers.

This is the source of the design tension that drives almost everything in
the focus code: **two parties both want to own focus.**

Emacs has `selected-window` and `selected-frame`.  Commands dispatch through
them.  Display logic, the minibuffer, every interactive function in lisp
assumes these are authoritative.  This is not something we can take away.

Wayland has `keyboard.set_focus(surface)`.  Whichever surface holds it
receives every keystroke, end of story.  This is not something we can fake
around.

These two notions of focus are *the same notion* from the user's
perspective -- "what am I typing into?" -- but they live in different
worlds, on different threads, with different concepts of what a "window"
even is.  The job of the focus subsystem is to keep them agreeing without
either side losing its own coherence.

## The mapping problem

Emacs thinks in **frames, windows, and buffers**.  A frame is an OS-level
window.  Windows are panes inside a frame.  Each window displays a buffer.

Wayland thinks in **surfaces**.  A surface is whatever a client draws into.
An Emacs frame is a surface (the frame's own).  An external application
is also a surface.

To make external applications feel like Emacs windows, EWM puts each
external surface inside an Emacs window.  Lisp wraps the surface in a
buffer; Emacs displays that buffer in a window; the compositor positions
the surface to match the window's screen rect.

This means an external application looks, to Emacs, like a buffer.  And it
means a single surface can be displayed in multiple Emacs windows
simultaneously -- the same way the same buffer can be shown in two windows.
We call each such display of a surface a **view**.

So: an Emacs frame is one surface (the frame itself).  An external application
is one surface that might appear as zero, one, or many views inside Emacs
frames.

## The layout authority and the focus authority

Given the mapping, who decides where everything goes?

Lisp decides layout.  Emacs is, after all, a window manager: `split-window`,
`display-buffer-alist`, `tab-bar-mode`, every layout-shaping mechanism
already exists there and is configured to a fault by every user.  Trying
to take that away would mean reimplementing window management twice.  So
lisp ships a description of the layout to the compositor: per output, an
ordered list of Emacs frames; per frame, the views inside it with their
geometry.

The compositor decides keyboard focus.  This is forced: Wayland's keyboard
state lives in the compositor, and intercepted keys (like `C-x`) need to
be re-routed *atomically with the key forwarding* -- there's no time to
defer it.  So the compositor holds two ids: which Emacs frame is focused,
and which view inside that frame is the keyboard target.

These authorities are not symmetric.  Layout is data that flows mostly
one way (lisp -> compositor).  Focus is interactive and flows both ways:
lisp can request a focus change ("focus this view"), but the compositor
makes the call and tells lisp what happened.

```
 ┌──────────────────────────┐  module commands  ┌──────────────────────────┐
 │           Emacs          │ ─────────────────► │        Compositor        │
 │     (lisp; Emacs main    │   OutputLayout     │      (Rust; thread       │
 │         thread)          │   FocusView        │      inside Emacs)       │
 │                          │   FocusOutputDir   │                          │
 │   layout authority       │                    │    focus authority       │
 │   frame / window tree    │                    │    Wayland keyboard      │
 │   buffer ↔ view ↔        │      pipe          │    stored focus pair     │
 │   surface mapping        │ ◄───────────────── │    + overlay state       │
 │                          │   Focus, New,      │                          │
 │                          │   Close, Title, …  │                          │
 └──────────────────────────┘                    └──────────────────────────┘
```

## The strip

The layout has a particular shape.  Each output hosts a **strip**: a
left-to-right sequence of Emacs frames.  Only one frame is "on-screen" at
a time per output, and a keybind slides between them.  Frames in the
strip are workspaces, more or less.

Inside a frame, Emacs's own window splits position the views.  The
compositor renders each view at the screen rect of the Emacs window
that displays it.

So layout has three nested layers:

```
output -> strip (ordered Emacs frames) -> frame (views in window splits)
```

The compositor doesn't manage any of this -- it receives it as a
description.  But it does have to be able to *reason about* it: "which
frame is the user looking at?", "which view should keys go to?", "what
moved between layout updates?".

Strip-active transitions also gate Emacs's own paint cost: every change
to a strip's active slot emits `FrameShown` / `FrameHidden` events that
lisp forwards into `gdk_window_freeze_updates` on the frame's GtkWindow,
so hidden frames don't redraw on every redisplay tick.  See
`render-architecture.md` "Hidden Frame Paint Suppression" for the
mechanism.

## Identity: ids and view ids

To talk about these objects in the lisp <-> compositor protocol, we need
ids.

**Surface ids** identify wl_surfaces.  The compositor allocates them
monotonically at surface creation; lisp learns them through compositor
events.

**View ids** identify *occurrences of a surface in the layout*.  A surface
shown in two Emacs windows has two distinct view ids -- they're per-view,
not per-surface.  This is what lets Emacs send keys to "the Firefox window
on my external monitor" rather than just "Firefox".

For most views (entries inside frames), view ids are random and lisp
generates them.  Lisp has to: it owns the Emacs window concept, and only
lisp knows how many windows show a surface.

For one special case -- a frame's own view -- the view id is **derived
deterministically from the surface id**: bit 32 of the u64 is set, and
the low bits hold the surface id.  Entry view ids live in
`[1, 2^32 - 1]` with bit 32 unset; frame view ids live above with bit
32 set.  Disjoint ranges, no collisions, both sides compute the same
id from the same surface id without ever needing to coordinate.  This
sounds like a small detail; it removes a whole class of races (see the
async section below).

## The focus state, as the compositor sees it

The compositor's stored focus is a pair: a focused frame's surface id, and
optionally a focused view id inside that frame.  At resolve time, two
override modes sit on top of this pair:

- **Lock**: when a session lock surface is present, keyboard focus is
  forced to it.  The underlying pair is preserved for restore on unlock.
- **Layer shell**: panels that request exclusive or on-demand keyboard
  interactivity steal focus while present.  Again, the underlying pair is
  preserved.

Collapsed to discrete states, this gives:

```
                ┌──────────┐
                │   None   │   no Emacs frame mapped yet
                └────┬─────┘
                     │ first Emacs frame mapped
                     ▼
                ┌──────────┐  ◄──── prefix redirect parks focus here
                │  Frame   │        for the duration of C-x, M-x, …
                │          │        (focused frame's own surface)
                └────┬─────┘
                     │ focused frame's selected window
                     │ shows an external surface
                     ▼
                ┌──────────┐
                │   View   │   that surface is the keyboard target
                └──────────┘

       ─── overlays sit on top of any of the above at resolve time ───

                ┌────────────┐
                │ LayerShell │   exclusive/on-demand panel surface
                └────────────┘   present; restores on dismissal

                ┌────────────┐
                │   Locked   │   session lock surface present;
                └────────────┘   restores prior state on unlock
```

Transitions between them are driven by clicks, gestures, lisp-initiated
focus commands, layout refreshes that change the active frame's selected
view, prefix-key interception (which parks on Frame temporarily), and
unlock/relock.

The state machine itself is small.  What makes the focus subsystem look
complicated is not the states but the synchronization across the
lisp/compositor boundary -- the next section is about why.

## Invariants

`compositor/tests/focus_proptest.rs` is the executable spec.  Six
invariants always hold; two more describe what specific operations
must leave behind.

Always:

1. **Some frame is focused if and only if some frame exists.**  No
   orphaned focus when there are no frames; no missing focus when
   there is at least one.
2. **A focused view, if any, lives on the focused frame.**  The
   compositor never points at a view that belongs to a different
   frame than the focused one.
3. **If no frame is focused, no view is focused either.**  The
   "nothing is focused" state is consistent at both levels.
4. **Each frame lives on exactly one output's strip.**  Cross-output
   migration moves a frame; it does not duplicate it.
5. **The focused frame is the active frame of its strip.**  Strip
   navigation and focus stay in lockstep.
6. **A frame's selected view, when set, is one of that frame's own
   entries.**  Lisp can name a view as selected only if the view
   belongs to the frame it's naming it on.

After specific operations:

7. **Prefix-redirect parks focus on the frame.**  After
   `enter_prefix_redirect(id)` on a frame that exists in some strip,
   the focused frame is that frame and no view is focused.
8. **Applying a layout never removes a frame.**  The set of frame
   surface_ids across all strips can grow or shuffle, but it never
   strictly shrinks via layout.  Frames go away only through
   `handle_toplevel_destroyed`.

The pure `resolve_keyboard_focus_target` helper (lock > layer >
toplevel priority) is covered by a second set of properties in the
same file.

## Why two threads make this hard

The compositor runs on a thread inside Emacs but does not share the lisp
heap.  Communication is through two channels:

- **Module commands**: lisp -> compositor, a queue of typed messages
  (apply this layout; focus this view; configure this output).
- **Pipe events**: compositor -> lisp, JSON over a pipe (focus changed;
  surface created; output disconnected).

Both channels are asynchronous and ordering between them is not
guaranteed.  Lisp can send a message, the compositor can send an event
right after, and they can be processed in either order on the other side.

This is the source of essentially every "focus is in a weird state"
bug in this codebase.  Most of the architecture is shaped by making the
async fine.

Three concrete examples:

1. When Emacs creates a new frame, the Wayland surface is born *before*
   lisp has finished registering the frame in its own data structures.
   The compositor sees a new surface, and a layout-refresh from lisp may
   arrive in either order.  An early refresh won't include the new
   frame yet (lisp hasn't told itself about it).  If apply-layout were
   "replace the strip with what lisp says", the new frame would be wiped
   the moment it tried to exist.

2. When the user clicks on a surface, the compositor needs to make
   keyboard focus follow the click and tell lisp about it.  Lisp will
   then update `selected-window` accordingly.  But until lisp has
   processed the event, lisp's `selected-window` and the compositor's
   focus disagree -- by design, briefly.

3. When the user toggles a fullscreen surface via an intercepted key, the
   keystroke reaches Emacs through a prefix-redirect (focus is parked on
   Emacs for the duration of the key sequence).  Emacs runs the toggle;
   lisp re-ships the layout; the compositor sees fullscreen now applies;
   the user wants keys to go back to the surface immediately, not to
   Emacs.

The architecture's job is to make each of these self-healing rather than
brittle.

## How the architecture handles the async

A few small decisions buy a lot of robustness:

**The compositor eagerly inserts new Emacs frames into the strip.**  When
the Wayland surface is born, the compositor doesn't wait for lisp's first
layout-refresh -- it constructs a placeholder frame in the target output's
strip immediately, using the deterministic view id derivation described
above.  This means "is this an Emacs frame?" is answerable from strip
membership alone, without a separate registry.  No dual state, no
synchronization rule between two data structures.

**Layouts merge into strips, they don't replace them.**  When lisp ships a
layout, the compositor updates fields of existing frames and adds any new
frames.  Frames in the strip that aren't in the layout are kept.  They go
away only when their wl_surface is destroyed -- a separate, authoritative
signal.  This lets eager-inserted placeholders survive a layout refresh
that races ahead of lisp's own bookkeeping, and it lets lisp's layout be
"advisory" without each transient miss costing the user a frame.

**Per-frame selected-view-id replaces a per-entry focused flag.**  An
older design carried a "focused" flag on each layout entry, recomputed
from `selected-window` every layout refresh.  Process-filter activity in
lisp would jostle `selected-window` between commands, the flag would
flicker, and the compositor would track the flicker.  Now lisp ships only
"for each frame, which view (if any) is its selected window".  The
compositor derives its focused view from the focused frame's selected
view.  One signal, one direction, no flicker.

**Lisp-initiated focus uses a dedicated command, not xdg_activation.**
PGTK (the Emacs Wayland backend) wants to route focus requests through
xdg_activation.  If we let that through, every Emacs-side focus change
would round-trip through xdg_activation, which the compositor would
honour and echo back as a focus event, which lisp would handle by
issuing another focus change -- echo loop.  EWM overrides Emacs's
focus-frame primitive to bypass `x-focus-frame` and use a direct module
command instead.

**Destruction is authoritative; layout is advisory.**  A frame in the
strip but not in the latest layout is preserved.  A frame whose
wl_surface is destroyed is removed everywhere, immediately.  This neatly
separates "lisp may not have caught up" from "the thing is actually
gone".

## Modal input states

The state machine above assumes the user wants keystrokes to go to
whatever the focused frame's selected window holds.  While Emacs is
reading keys for itself -- minibuffer, transient, isearch, an
in-flight `universal-argument` -- the embedded surface is not the
keyboard target; Emacs is.  The layout sender closes this gap by
suppressing `:selected-view-id` for every frame whenever
`ewm-layout--menu-active-p` says so, collapsing the compositor's focus
to the focused frame's own surface.

The predicate is a hook (`ewm-layout-menu-predicates`).  Its defaults
catch the two Emacs-internal mechanisms without naming any third-party
package:

- `active-minibuffer-window` -- an active minibuffer read.
- `ewm-layout--reading-key-p` -- a non-nil
  `overriding-terminal-local-map` or `overriding-local-map`.
  Transient, isearch, hydra, and `set-transient-map` install one
  directly; `universal-argument` reaches it via `set-transient-map`.

`post-command-hook` runs `ewm-layout--on-post-command`, which refreshes
the layout only when the predicate's boolean result flips since the
last command.

The same predicate gates the receive side: `ewm--handle-focus` drops
compositor-originated focus events while modal, so a stray pointer
motion or click cannot yank selection out of an active read.

## Why focus needs to be acquired atomically sometimes

Two operations need keyboard focus moved *atomically* with the event that
triggers them, rather than deferred to a later tick:

- **Intercepted keys.**  When a prefix key like `C-x` is pressed while an
  external app has focus, the compositor must (a) route the key to Emacs
  and (b) keep Emacs focused for the rest of the sequence.  Both have to
  happen synchronously in the same key handler, because the next key is
  already coming.  Wayland has no keyboard-grab equivalent of X11's
  `XGrabKeyboard`, so the compositor tracks a prefix-sequence flag
  itself.  When the lisp command completes, `post-command-hook` clears
  the flag and lisp ships a layout refresh that restores the prior view
  focus.

- **Fullscreen surfaces.**  After an intercepted key toggles a surface
  fullscreen, keyboard focus has to land back on the now-fullscreen
  surface immediately, not on the Emacs frame the key was redirected to.
  A small redirect in the input handler ensures this: if the focused
  frame has a fullscreen view and the keyboard isn't on it, snap it
  back.

Everything else -- clicks, gestures, layout-driven focus changes -- can
use the slower, deferred path: set the stored focus pair, sync the
Wayland keyboard state on the next tick.  Only the two cases above need
the fast path.

## Multi-monitor and multi-view

Each output has its own strip.  Cross-output navigation (directional
keybinds, clicks on the other monitor) updates the focused frame to the
destination output's active frame, and the focused view follows from
that frame's selected view.  The compositor doesn't need a special
"current monitor" concept -- it falls out of "which frame is focused".

Multi-view -- one surface in multiple Emacs windows -- works because
focus events carry view ids, not surface ids.  When the user clicks the
left-monitor copy of Firefox, the compositor sends a focus event with
that copy's view id; lisp selects the Emacs window with that view id.
The right-monitor copy isn't affected.  The same surface is "focused" in
the Wayland sense (`keyboard.set_focus(firefox_surface)`), but the
question "which Emacs window is the user editing?" is resolved
independently by view id.

## Why we ended up here

Earlier iterations of EWM had a single Emacs frame per output and a flat
list of surfaces.  As the design grew to support multiple workspaces per
output (strips with multiple frames), the focus subsystem grew with it
-- mostly by accretion, occasionally by retraction.  The two most recent
waves of changes:

The **layout-as-strip** transition moved frames from a flat list into
per-output ordered columns.  This is what made workspaces work, but it
also created more places where focus state could be "between layouts".

The **single-signal cleanup** removed redundancies that had accumulated:
a per-entry focused flag that duplicated per-frame "selected window", a
separate `emacs_frame_ids` set that duplicated strip membership,
multiple focus mutator paths that bypassed each other.  Each of these
had a reason at the time it was added.  Each became wrong as the
surrounding code grew up around it.

The current architecture is what's left after those cleanups.  The
remaining complexity is, as best we can tell, intrinsic to the problem:
two threads, two authorities, two world-views that have to keep
agreeing.
