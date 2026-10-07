# EWM Multi-Monitor Design

## Overview

EWM supports multiple monitors with a frame per output to start, hotplug detection, and Emacs-controlled configuration. Emacs runs as a foreground daemon (`--fg-daemon`), creating frames explicitly for each discovered output.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                Compositor (ewm) - Backend                   │
│  - Discovers physical outputs via DRM                       │
│  - Reports hardware info to Emacs                           │
│  - Owns frame placement: output, strip slot, float position │
│  - Renders all outputs independently                        │
│  - Tracks active output (focus/cursor based)                │
└─────────────────────────────────────────────────────────────┘
                    │ events                   ▲ commands
                    ▼                          │
┌─────────────────────────────────────────────────────────────┐
│                Emacs (ewm.el) - Controller                  │
│  - User configuration for outputs                           │
│  - Decides output arrangement/positioning                   │
│  - Creates one frame per output on startup                  │
│  - Owns what frames exist and what each one shows           │
└─────────────────────────────────────────────────────────────┘
```

## Key Design Decisions

### Foreground Daemon Mode
Emacs starts with `--fg-daemon`, meaning no frames exist initially. Frames are created explicitly when outputs are discovered, ensuring uniform handling for all outputs.

### A Frame Per Output To Start
Each output starts with one Emacs frame and the user adds more with `ewm-frame-new`. Emacs manages the windows inside a frame. The compositor owns where every frame sits.

### Emacs-Controlled Configuration
All output configuration (positioning, scale) is controlled via `ewm.el`, not hardcoded in the compositor. This allows user-friendly configuration through Emacs customization.

### Active Output (Not Primary)
There is no static "primary" output. Instead, the **active output** is dynamic:
- The output containing the cursor, or
- The output with the focused Emacs frame

New non-Emacs surfaces appear on the active output. Windows open where you're currently working.

### Explicit Frame Creation
Emacs frames are created with an explicit target output. No "show then reassign" logic needed for frames. The compositor assigns the frame to the requested output immediately.

## Module Interface

The compositor runs as a dynamic module thread within Emacs. Communication happens
via shared state protected by mutexes, with events delivered over a pipe fd
(newline-delimited JSON via `open_channel`).

### Compositor -> Emacs Events

Events are serialized to JSON and written to a pipe fd:

| Event | Fields | Purpose |
|-------|--------|---------|
| `output_detected` | name, make, model, width_mm, height_mm, modes | Monitor connected |
| `output_disconnected` | name | Monitor removed |
| `new` | id, app, seq, pid | Surface created. `seq` names the Emacs frame it belongs to |

### Emacs -> Compositor Functions

Emacs calls module functions directly (no serialization needed):

```elisp
(ewm-configure-output name &key x y width height refresh enabled)
(ewm-prepare-frame output seq &optional after)  ; Frame SEQ goes on output
(ewm-layout frames)            ; Declare every frame's contents
```

## Emacs Frame Lifecycle

```
1. Compositor pushes output_detected, then outputs_complete, via pipe fd
2. Emacs consults ewm-output-config and finds the output has no frame
3. Emacs calls (ewm-prepare-frame "HDMI-A-1" seq) to register pending frame
4. Emacs calls (make-frame) which creates a Wayland surface
5. Compositor places the new surface on the output and echoes seq in its new event
6. Frame is visible on the correct output immediately
```

## Non-Emacs Surface Lifecycle

```
1. External client (Firefox, terminal, etc.) creates surface
2. Compositor places it on the active output (focused frame's, else under the cursor)
3. Compositor pushes new event with { id, app, pid } (`seq` is sent for Emacs frames only)
4. Emacs receives via pipe fd, creates buffer for surface
5. Emacs includes the surface in its next layout declaration
6. Compositor positions and displays the surface
```

## Startup Flow

```
1. Emacs starts with --fg-daemon (no frames exist yet)
2. User runs M-x ewm-start-module
3. Module loads, compositor thread starts
4. Compositor discovers outputs via DRM
5. For each output, compositor pushes output_detected event
6. After all outputs discovered, compositor pushes outputs_complete event
7. Emacs receives outputs_complete, for each output without a frame:
   a. Consults ewm-output-config for positioning/scale
   b. Calls ewm-configure-output to set properties
   c. Calls ewm-prepare-frame to register pending frame
   d. Calls (make-frame) to create the actual frame
8. All outputs now have frames, system is ready
```

## Hotplug Support

The compositor uses UdevBackend to detect monitor connect/disconnect events at runtime. Frames never leave the compositor's hands:

- **Disconnect**: the output's frames move to the next output in output order, or wait in the parked pool when it was the last one. The compositor remembers which display each frame came from and which frame that display showed, then sends `OutputDisconnected`.
- **Connect**: the new output adopts any parked frames and reclaims the frames that came from its display, matched by make, model and serial rather than connector, and shows the frame it showed before. `OutputDetected` tells Emacs, which creates a frame only when the output ends up empty.

Emacs observes all of this through `frame_shown`, `frame_hidden` and the placement queries. Hotplug uses the same path as startup.

A frame's home display changes only when the user moves it on purpose. Dragging a floating frame onto another output rehomes it there, so a later replug leaves it where the user put it.

## Lid Switch and Session Resume

### Lid Close

When the laptop lid is closed, the compositor receives a libinput `SwitchToggle`
event. Behavior depends on whether an external monitor is connected:

- **With external display**: The laptop panel (identified by `eDP` prefix) is
  detached and its frames move to the external display, which continues working
  normally.
- **Without external display**: All monitors are deactivated (blanked). The
  session remains running and will resume when the lid opens.

### Lid Open

Re-scans DRM connectors to discover outputs. If the laptop panel reappears, the
standard `OutputDetected` -> frame creation path runs.

### Session Suspend/Resume

`PauseSession`/`ActivateSession` events from logind handle system suspend:

- **Resume**: Activates monitors, re-scans connectors (handles displays
  added/removed while suspended), redraws all outputs, sends `OutputsComplete`
  to Emacs.
- `OutputsComplete` is the universal "output topology settled" signal. It fires
  on startup, hotplug, and resume. Emacs uses it to apply output config, give
  empty outputs a frame, refresh layouts, and sync focus.

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Broken Emacs config | Frames created with default positioning |
| Module fails to load | Error message, compositor not started |
| Frame prepared for an unknown output | Compositor places it on the active output |
| Compositor thread panics | Emacs continues running (catch_unwind) |
| No outputs connected | Emacs daemon running, ready for hotplug |

## User Configuration

```elisp
;; Output positioning and properties
(setq ewm-output-config
      '(("HDMI-A-1" :x 0 :y 0 :scale 1.0)
        ("DP-1" :x 1920 :y 0 :scale 1.25)
        ("eDP-1" :x 0 :y 1080 :scale 2.0)))
```

## Emacs Commands

```elisp
ewm--output-info                    ; detected outputs by name
(ewm-get-active-outputs)            ; mapped outputs in order
(ewm--frame-output (selected-frame)) ; output of current frame

;; Reposition an output
(ewm-configure-output "DP-1" :x 1920 :y 0)

;; Declare the contents of every frame
(ewm-layout frames-vector)
```

## Per-Output Rendering

Each output renders elements relative to its own (0,0) origin. Global coordinates are offset by the output's position. For example, DisplayPort-1 at position (1920,0) needs elements shifted left by 1920px.

Key implementation details:
- Per-output `Surface` with independent `GbmDrmCompositor`
- `HashMap<crtc::Handle, OutputSurface>` for multi-output tracking
- Output position offset applied in `collect_render_elements_for_output()`

## Reference

Based on patterns from [niri](https://github.com/YaLTeR/niri):
- `src/backend/tty.rs`: DRM/output discovery, hotplug
- `src/niri.rs`: Output positioning

Key patterns NOT adopted (Emacs handles instead):
- Output configuration parsing

## Active Output Tracking

The compositor tracks the "active output" for placing new non-Emacs surfaces:

```rust
/// Output for surface placement and keyboard nav: focused, else under the cursor, else first.
pub fn active_output(&self) -> Option<&Output> {
    self.focused_output()
        .or_else(|| self.output_under_cursor())
        .or_else(|| self.first_output())
}
```

Windows open where you're working, not on a static "primary" output.

## Troubleshooting

**Only one monitor shows content:**
```bash
# Check compositor logs for output discovery
journalctl --user -t ewm | grep -i connector

# Verify DRM sees all outputs
cat /sys/class/drm/card*-*/status
```

**Events not reaching Emacs:**
```elisp
;; Check if event pipe process is alive
(process-live-p ewm--event-process)

;; Check if compositor is running
(ewm-running)
```

**Frames not created for outputs:**
```elisp
;; Dump compositor state to see detected outputs
M-x ewm-show-state

;; Create frame for specific output manually
(ewm--create-frame-for-output "HDMI-A-1")
```

**Hotplug not detected:**
```bash
# Check udev events
udevadm monitor --property | grep -i drm
```

**No frames on startup:**
```bash
# Ensure Emacs is started as fg-daemon from TTY
emacs --fg-daemon
# Then run M-x ewm-start-module
```
