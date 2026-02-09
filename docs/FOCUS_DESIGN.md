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

### Why Input-to-Focus?

Previous design had keyboard focus only change on click. This caused issues:
- User scrolls Firefox on external monitor
- User presses M-x
- M-x would route to primary monitor (last clicked) instead of external

With input-to-focus:
- Scroll Firefox → Firefox has focus, external monitor is "active"
- Press M-x → routes to Emacs frame on external monitor

## IPC Events

| Event | When Sent | Purpose |
|-------|-----------|---------|
| `focus` | External surface clicked | Tell Emacs to show surface buffer |
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
- `ewm-input--on-buffer-change`: Sync focus when buffer changes

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
