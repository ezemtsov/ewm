# EWM Multi-Monitor Design

## Overview

EWM supports multiple monitors with automatic frame-per-output management, hotplug detection, and Emacs-controlled configuration. Emacs runs as a foreground daemon (`--fg-daemon`), creating frames explicitly for each discovered output.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                Compositor (ewm) - Backend                   │
│  - Discovers physical outputs via DRM                       │
│  - Reports hardware info to Emacs                           │
│  - Executes positioning/assignment commands                 │
│  - Renders all outputs independently                        │
│  - Tracks active output (cursor/focus based)                │
└─────────────────────────────────────────────────────────────┘
                    │ events                   ▲ commands
                    ▼                          │
┌─────────────────────────────────────────────────────────────┐
│                Emacs (ewm.el) - Controller                  │
│  - User configuration for outputs                           │
│  - Decides output arrangement/positioning                   │
│  - Creates one frame per output on startup                  │
│  - All policy decisions live here                           │
└─────────────────────────────────────────────────────────────┘
```

## Key Design Decisions

### Foreground Daemon Mode
Emacs starts with `--fg-daemon`, meaning no frames exist initially. Frames are created explicitly when outputs are discovered, ensuring uniform handling for all outputs.

### Single Frame Per Output
Each physical output maps to one Emacs frame. Emacs manages all windows within frames; the compositor handles output discovery and rendering.

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
via shared state protected by mutexes, with SIGUSR1 signals for event notification.

### Compositor → Emacs Events

Events are pushed to a shared queue and Emacs is notified via SIGUSR1:

| Event | Fields | Purpose |
|-------|--------|---------|
| `output_detected` | name, make, model, width_mm, height_mm, modes | Monitor connected |
| `output_disconnected` | name | Monitor removed |
| `new` | id, app, output | Surface created |

### Emacs → Compositor Functions

Emacs calls module functions directly (no serialization needed):

```elisp
(ewm-configure-output name &key x y width height refresh enabled)
(ewm-prepare-frame output)     ; Register pending frame for output
(ewm-output-layout output surfaces)  ; Declare per-output surface layout
```

## Emacs Frame Lifecycle

```
1. Compositor pushes output_detected event, signals Emacs via SIGUSR1
2. Emacs receives event, consults ewm-output-config
3. Emacs calls (ewm-prepare-frame "HDMI-A-1") to register pending frame
4. Emacs calls (make-frame) which creates a Wayland surface
5. Compositor matches the new surface to the pending frame, assigns to output
6. Frame is visible on the correct output immediately
```

## Non-Emacs Surface Lifecycle

```
1. External client (Firefox, terminal, etc.) creates surface
2. Compositor assigns to active output (cursor/focus based)
3. Compositor pushes new event with { id, app, output }
4. Emacs receives via SIGUSR1, creates buffer for surface
5. Emacs includes surface in next OutputLayout declaration for the target output
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
7. Emacs receives events, for each output:
   a. Consults ewm-output-config for positioning/scale
   b. Calls ewm-configure-output to set properties
   c. Calls ewm-prepare-frame to register pending frame
   d. Calls (make-frame) to create the actual frame
8. All outputs now have frames, system is ready
```

## Hotplug Support

The compositor uses UdevBackend to detect monitor connect/disconnect events at runtime. Hotplug uses the exact same codepath as startup:

- **Connect**: Compositor sends `OutputDetected` → Emacs creates frame for it
- **Disconnect**: Compositor sends `OutputDisconnected` → Emacs closes the frame, moves windows to remaining frames

This uniformity means no special cases for "first output" vs "hotplugged output".

## Lid Switch and Session Resume

### Lid Close

When the laptop lid is closed, the compositor receives a libinput `SwitchToggle`
event. Behavior depends on whether an external monitor is connected:

- **With external display**: The laptop panel (identified by `eDP` prefix) is
  disconnected — its output is removed and Emacs closes the frame, moving windows
  to remaining outputs. The external display continues working normally.
- **Without external display**: All monitors are deactivated (blanked). The
  session remains running and will resume when the lid opens.

### Lid Open

Re-scans DRM connectors to discover outputs. If the laptop panel reappears, the
standard `OutputDetected` → frame creation path runs.

### Session Suspend/Resume

`PauseSession`/`ActivateSession` events from logind handle system suspend:

- **Resume**: Activates monitors, re-scans connectors (handles displays
  added/removed while suspended), redraws all outputs, sends `OutputsComplete`
  to Emacs.
- `OutputsComplete` is the universal "output topology settled" signal — it fires
  on startup, hotplug, and resume. Emacs uses it to apply output config, enforce
  frame parity, refresh layouts, and sync focus.

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Broken Emacs config | Frames created with default positioning |
| Module fails to load | Error message, compositor not started |
| PrepareFrame for unknown output | Compositor ignores, logs warning |
| Compositor thread panics | Emacs continues running (catch_unwind) |
| No outputs connected | Emacs daemon running, ready for hotplug |

## User Configuration

```elisp
;; Output positioning and properties
(setq ewm-output-config
      '(("HDMI-A-1" :position (0 . 0) :scale 1.0)
        ("DP-1" :position (1920 . 0) :scale 1.25)
        ("eDP-1" :position (0 . 1080) :scale 2.0)))

;; Policy for new outputs not in config (hotplug)
(setq ewm-default-output-position 'right)  ; 'right, 'left, 'above, 'below

;; Per-app output rules (for non-Emacs clients)
(setq ewm-app-output-rules
      '(("firefox" . "DP-1")
        ("slack" . follow-focus)))  ; open on active output
```

## Emacs Commands

```elisp
ewm--outputs                        ; list of detected outputs
(ewm-active-output)                 ; output with cursor/focus
(frame-parameter nil 'ewm-output)   ; output of current frame

;; Reposition an output
(ewm-configure-output "DP-1" :x 1920 :y 0)

;; Declare surface layout for an output
(ewm-output-layout "DP-1" surfaces-vector)
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
- Workspace management
- Window-to-monitor assignment logic
- Output configuration parsing

## Active Output Tracking

The compositor tracks the "active output" for placing new non-Emacs surfaces:

```rust
fn active_output(&self) -> &Output {
    // Priority: cursor position > focused surface's output
    self.output_under_cursor()
        .or_else(|| self.focused_output())
        .unwrap_or_else(|| self.outputs.first())
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
;; Check if SIGUSR1 handler is installed
(lookup-key special-event-map [sigusr1])

;; Check if compositor is running
(ewm-running)

;; Manually drain event queue
(ewm--process-pending-events)
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
