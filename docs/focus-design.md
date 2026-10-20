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

## The contents authority and the placement authority

Given the mapping, who decides where everything goes?

Lisp decides contents.  Emacs is, after all, a window manager: `split-window`,
`display-buffer-alist`, `tab-bar-mode`, every layout-shaping mechanism
already exists there and is configured to a fault by every user.  Trying
to take that away would mean reimplementing window management twice.  So
lisp ships a description of what each frame shows to the compositor: per
frame, the views inside it with their geometry.

The compositor decides placement.  Which output a frame is on, its slot in
the strip and where a float sits are compositor state.  Lisp sends intents
(`ewm-prepare-frame`, `ewm-move-frame-module`) and reads placement back through
snapshot queries (`ewm-get-strip-frame-ids`, `ewm-get-active-frame-id`,
`ewm-get-frame-output`).

The compositor decides keyboard focus.  This is forced: Wayland's keyboard
state lives in the compositor, and intercepted keys (like `C-x`) need to
be re-routed *atomically with the key forwarding* -- there's no time to
defer it.  The compositor stores no focus of its own: the focused frame is
the active frame of the active output, on its strip or its floating space,
and the keyboard target is that frame's selected view.

These authorities are not symmetric.  Contents flow one way (lisp ->
compositor).  Placement and focus are interactive and flow both ways:
lisp can request a change ("focus this view", "move this frame"), but the
compositor makes the call and tells lisp what happened.

```
 ┌──────────────────────────┐  module commands  ┌──────────────────────────┐
 │           Emacs          │ ─────────────────► │        Compositor        │
 │     (lisp; Emacs main    │   Layout           │      (Rust; thread       │
 │         thread)          │   FocusTarget      │      inside Emacs)       │
 │                          │   FocusOutput      │                          │
 │   contents authority     │                    │    focus authority       │
 │   frame / window tree    │                    │    Wayland keyboard      │
 │   buffer ↔ view ↔        │      pipe          │    focus from layout     │
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

The compositor owns the strip.  Frames enter it at `new_toplevel` where
lisp asked, move along it with `ewm-move-frame-module`, and move with their
output on hotplug.  Inside a frame, Emacs's own window splits position
the views, and that is the description lisp sends.  The compositor
renders each view at the screen rect of the Emacs window that displays it.

So layout has three nested layers:

```
output -> strip (ordered Emacs frames) -> frame (views in window splits)
```

The compositor has to *reason about* all three: "which frame is the user
looking at?", "which view should keys go to?", "what moved between layout
updates?".

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

The compositor's window focus is read off the layout: the active output,
whether its strip or its floating space holds the keyboard, that
container's active frame, and that frame's selected view.  Focusing a
frame activates it in its container and makes its output the active one,
so the focused frame is on screen by construction.  At keyboard-focus
resolve time, override modes sit on top of this:

- **Lock**: when a session lock surface is present, keyboard focus is
  forced to it.  The window focus underneath returns on unlock.
- **Layer shell**: panels that request exclusive or on-demand keyboard
  interactivity steal focus while present.  Again, the window focus
  underneath is untouched.
- **Emacs keyboard capture**: incomplete key sequences, minibuffers,
  transient maps, and read-key/read-event commands hold tokened leases that
  route Wayland keyboard focus to Emacs without changing the window focus.
  Stale token cleanup cannot clear another live capture.

Collapsed to discrete states, this gives:

```
                ┌──────────┐
                │   None   │   no Emacs frame mapped yet
                └────┬─────┘
                     │ first Emacs frame mapped
                     ▼
                ┌──────────┐
                │  Frame   │
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
                │   Emacs    │   keyboard capture;
                └────────────┘   window focus underneath is unchanged

                ┌────────────┐
                │ LayerShell │   exclusive/on-demand panel surface
                └────────────┘   present; restores on dismissal

                ┌────────────┐
                │   Locked   │   session lock surface present;
                └────────────┘   restores prior state on unlock
```

Transitions between them are driven by clicks, gestures, lisp-initiated
focus commands, layout refreshes that change the active frame's selected
view, fullscreen correction, and unlock/relock.  Keyboard capture is an
overlay on top of those states, not a transition of the window focus.

The state machine itself is small.  What makes the focus subsystem look
complicated is not the states but the synchronization across the
lisp/compositor boundary -- the next section is about why.

## Invariants

`compositor/tests/focus_proptest.rs` is the executable spec.  Some
invariants always hold; others describe what specific operations must
leave behind.

Always:

1. **Some frame is focused if and only if some frame exists.**  No
   orphaned focus when there are no frames; no missing focus when
   there is at least one.  Kept by the layout, which moves its active
   output to the first monitor in output order with a frame whenever it
   loses one, and checked by `Layout::verify_invariants`.
2. **A focused view, if any, lives on the focused frame.**  The
   compositor never points at a view that belongs to a different
   frame than the focused one.  Structural: the focused view is the
   focused frame's selected view.
3. **If no frame is focused, no view is focused either.**  The
   "nothing is focused" state is consistent at both levels.  Structural,
   from 2.
4. **Each frame sits in exactly one place.**  One output's strip or
   floating space, or the parked pool.  A move takes the frame along, it
   does not duplicate it (`Layout::verify_invariants`).
5. **The focused frame is the active frame of its strip.**  Strip
   navigation and focus stay in lockstep.  Structural: the focused frame is
   defined as the active frame of the active output's strip or floating
   space, and a parked frame is on no output.
6. **A frame's selected view, when set, is one of that frame's own
   entries.**  Lisp can name a view as selected only if the view
   belongs to the frame it's naming it on.
7. **Every live view id identifies one layout occurrence.**  A view id
   is not a surface id and not a reusable focus token; it names one
   occurrence of a surface in Emacs's window tree.

After specific operations:

8. **Keyboard capture itself does not change window focus.**  When Lisp
   asks the compositor to capture keyboard focus for a key sequence,
   minibuffer, transient, or read-key state, the Wayland keyboard target
   may move to Emacs but the capture command leaves the window focus
   alone.
9. **Applying a layout never removes a frame.**  Frames go away only
   through `handle_toplevel_destroyed_by_id`.
10. **Detaching an output moves its frames whole.**  After
   `detach_output`, the output has no monitor, its frames sit on the
   next output in order or in the parked pool, and focus has hopped to
   a surviving frame if any exist.
11. **A layout changes no frame's placement.**  `apply_layout(frames)`
    leaves every strip's frame set and order as they were. It replaces
    the contents of the frames it names and empties the frames it
    leaves out. Frames enter a strip at `new_toplevel` and move only
    through compositor operations.
12. **Destruction is authoritative.**  After
    `handle_toplevel_destroyed_by_id`, the frame's surface_id is gone from
    every strip.
13. **Activating a multi-view surface stays in the focused frame.**  The
    focused view wins when it shows the surface, else a view in the
    focused frame, else the first view in output order.
14. **Keys for Emacs follow the Focus they were typed after.**  While a
    Focus event waits unread, keys bound for an Emacs surface queue in the
    compositor and replay in order once Emacs drains the queue, into the
    frame Emacs then selects.  Actions never wait, a VT switch never waits,
    and the queue is handed over before keyboard focus leaves Emacs.
    Modifier-only keys join an existing queue but never start one.
15. **A compositor action completes without Emacs.**  Strip, output,
    overview, hide, fullscreen and spawn actions change the compositor's
    state on the key press or module command and queue events for Lisp
    at most.  Nothing waits for a reply, so they work while Emacs is busy.
16. **A selection made from the event filter ends in a command boundary.**
    `current-buffer` reverts when the filter returns, so `ewm--handle-focus`
    queues an `ewm-focus-change` event whose no-op command runs before the
    next typed key, and `post-command-hook` sees the new buffer first.  The
    event is of the `switch-frame` kind, so a key sequence in progress
    finishes first.  A running command gets the event once it ends.  A macro
    being defined gets none, since Emacs records events a filter unreads.
    The boundary leaves keyboard capture alone.  PGTK's own `switch-frame`
    events stay ignored, they may name a stale frame.
17. **A frame sent to another output stays focused.**  The move lands it
    after the destination's active frame, makes it that output's active
    frame and keeps keyboard focus on it, and repeated moves carry it round
    the outputs.
18. **A strip switch leaves the keyboard on a float it keeps on screen.**
    Next, nth and swiped frame changes show the target frame and take the
    keyboard only from a strip frame or from a float the switch hides, a
    pinned one whose frame slides away.  `ewm-float-focus-toggle` moves it
    between the layers on purpose.
19. **The floats hold the keyboard only while their active one is shown,
    and take it from an empty strip.**  Closing the focused float hands the
    keyboard to the next shown float, else to the strip, and an output whose
    strip emptied gives it to its topmost float.  Checked by
    `Layout::verify_invariants`.

## Why two threads make this hard

The compositor runs on a thread inside Emacs but does not share the lisp
heap.  Communication is through two channels:

- **Module commands**: lisp -> compositor, a queue of typed messages
  (apply this layout; focus this view; configure this output).
- **Queued events**: compositor -> lisp, Lisp data in a queue the module drains on a
  wakeup byte (focus changed, surface created, output disconnected).

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

3. When a key runs a compositor action (a strip switch, a fullscreen
   toggle), the compositor changes focus or the fullscreen bit on the
   press and tells lisp over the pipe, as for a click or a swipe.  Until
   lisp drains the event, `selected-frame` lags -- by design, briefly.

The architecture's job is to make each of these self-healing rather than
brittle.

## How the architecture handles the async

A few small decisions buy a lot of robustness:

**Frames enter the layout once, at `new_toplevel`.**  When the Wayland
surface is born, the compositor places the frame where `ewm-prepare-frame`
said, before lisp's first layout arrives.  This means "is this an Emacs
frame?" is answerable from the layout alone, without a separate registry.
No dual state, no synchronization rule between two data structures.

**A layout refills contents, it never places.**  When lisp ships a layout,
the compositor refills the frames it names and empties the frames it
leaves out.  It adds, moves and removes nothing.  Frames go away only when
their wl_surface is destroyed, a separate, authoritative signal.

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

**Destruction is authoritative.**  A frame the latest layout leaves out
keeps its place, emptied, until its wl_surface is destroyed, and then it
is removed everywhere, immediately.  This neatly separates "lisp has not
caught up" from "the thing is actually gone".

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
catch the three Emacs-internal mechanisms without naming any third-party
package:

- `active-minibuffer-window` -- an active minibuffer read.
- `ewm-layout--reading-key-p` -- a non-nil
  `overriding-terminal-local-map` or `overriding-local-map`.
  Transient, isearch, hydra, and `set-transient-map` install one
  directly; `universal-argument` reaches it via `set-transient-map`.
- `ewm-layout--command-popup-p` -- a popup menu shown by a command.  A
  click's own popup is left out because dismissing it runs no command
  that a hook could see.

`post-command-hook` runs `ewm-layout--on-post-command`, which refreshes
the layout only when the predicate's boolean result flips since the
last command.

The same predicate gates the receive side: `ewm--handle-focus` drops
compositor-originated focus events while modal, so a stray pointer
motion or click cannot yank selection out of an active read.

## Why focus needs to be acquired atomically sometimes

Two operations need keyboard focus moved *atomically* with the event that
triggers them, rather than deferred to a later tick:

- **Keyboard-capturing input.**  When a real keymap prefix like `C-x` is
  pressed while an external app has window focus, the compositor must
  route Wayland keyboard focus to Emacs and keep it there for the rest of
  the sequence without changing the window focus.  This
  has to happen synchronously in the same key handler, because the next
  key is already coming.  Ordinary command bindings are delivered to Lisp
  without moving Wayland focus; if they open a minibuffer, transient, or
  read-key state, Lisp asks the compositor to capture keyboard focus at
  that point.  Bindings of compositor actions (`Action`) never reach Lisp.

- **Fullscreen surfaces.**  After a key or an Emacs command makes a
  surface fullscreen, keyboard focus has to land on the now-fullscreen
  surface immediately, not on the Emacs frame.
  A small redirect in the input handler ensures this: if the focused
  frame has a fullscreen view and the keyboard isn't on it, snap it
  back.

Everything else -- clicks, gestures, layout-driven focus changes -- can
use the slower, deferred path: activate the frame, sync the Wayland
keyboard state on the next tick.  Only the two cases above need
the fast path.

## Multi-monitor and multi-view

Each output has its own strip.  Cross-output navigation (directional
keybinds, clicks on the other monitor) makes the destination the active
output, which gets the keyboard back in the layer that last held it
there, a float or the strip, and the focused view follows from that
frame's selected view.  The active output is layout state, and the
focused frame falls out of it.

Multi-view -- one surface in multiple Emacs windows -- works because
focus events carry view ids, not surface ids.  When the user clicks the
left-monitor copy of Firefox, the compositor sends a focus event with
that copy's view id; lisp selects the Emacs window with that view id.
The right-monitor copy isn't affected.  The same surface is "focused" in
the Wayland sense (`keyboard.set_focus(firefox_surface)`), but the
question "which Emacs window is the user editing?" is resolved
independently by view id.

Activation goes the other way: a token or foreign-toplevel request names
a surface, and the compositor must pick a view.  A view in the focused
frame wins (invariant 13), so a link opened from the terminal lands on
the Firefox copy next to it instead of sliding another output's strip.

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

The **derived-focus** step removed the stored focused frame and view pair.
Focus is a position in the layout, the active output and the active frame
of the container holding the keyboard there, so a moving frame keeps the
keyboard by being activated where it lands, invariants 2, 3 and 5 hold by
construction, and invariant 1 is one rule the layout checks.

The current architecture is what's left after those cleanups.  The
remaining complexity is, as best we can tell, intrinsic to the problem:
two threads, two authorities, two world-views that have to keep
agreeing.
