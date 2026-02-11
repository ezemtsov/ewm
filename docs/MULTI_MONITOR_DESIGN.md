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
Following niri's pattern, there is no static "primary" output. Instead, the **active output** is dynamic:
- The output containing the cursor, or
- The output with the focused Emacs frame

New non-Emacs surfaces appear on the active output. This is intuitive: windows open where you're currently working.

### Explicit Frame Creation
Emacs frames are created with an explicit target output. No "show then reassign" logic needed for frames. The compositor assigns the frame to the requested output immediately.

## IPC Protocol

### Compositor → Emacs Events

```rust
enum IpcEvent {
    // Output discovery
    OutputDetected {
        name: String,           // e.g., "HDMI-A-1"
        make: String,           // manufacturer
        model: String,          // model name
        width_mm: i32,          // physical dimensions
        height_mm: i32,
        modes: Vec<Mode>,       // available video modes
    },
    OutputDisconnected { name: String },

    // Surface events (for non-Emacs clients)
    New { id: u32, app: String, output: String },
}

struct Mode {
    width: i32,
    height: i32,
    refresh: i32,  // mHz
    preferred: bool,
}
```

### Emacs → Compositor Commands

```rust
enum Command {
    // Output configuration
    ConfigureOutput {
        name: String,
        x: i32,
        y: i32,
        mode_width: i32,
        mode_height: i32,
        mode_refresh: i32,
        scale: f64,
        enabled: bool,
    },

    // Create frame on specific output
    CreateFrame { output: String },

    // Surface-to-output assignment (for non-Emacs clients)
    AssignOutput { id: u32, output: String },
}
```

## Emacs Frame Lifecycle

```
1. Compositor sends OutputDetected { name: "HDMI-A-1", ... }
2. Emacs receives event, consults ewm-output-config
3. Emacs sends CreateFrame { output: "HDMI-A-1" }
4. Emacs calls (make-frame) which creates a Wayland surface
5. Compositor assigns the new surface to the requested output
6. Frame is visible on the correct output immediately
```

## Non-Emacs Surface Lifecycle

```
1. External client (Firefox, terminal, etc.) creates surface
2. Compositor assigns to active output (cursor/focus based)
3. Compositor sends: New { id, app, output }
4. Emacs receives, checks app-output-rules
5. If different output desired: Emacs sends AssignOutput { id, output }
6. Compositor moves surface to requested output
```

## Startup Flow

```
1. ewm starts
2. Compositor discovers outputs via DRM
3. Compositor positions outputs (auto-horizontal initially)
4. Compositor spawns: emacs --fg-daemon
5. Emacs daemon starts (no frames exist yet)
6. Emacs runs ewm-connect (early in startup)
7. IPC connects, compositor sends OutputDetected for each output
8. For each output, Emacs:
   a. Consults ewm-output-config for positioning/scale
   b. Sends ConfigureOutput to set output properties
   c. Sends CreateFrame to request a frame on that output
   d. Calls (make-frame) to create the actual frame
9. All outputs now have frames, system is ready
```

## Hotplug Support

The compositor uses UdevBackend to detect monitor connect/disconnect events at runtime. Hotplug uses the exact same codepath as startup:

- **Connect**: Compositor sends `OutputDetected` → Emacs creates frame for it
- **Disconnect**: Compositor sends `OutputDisconnected` → Emacs closes the frame, moves windows to remaining frames

This uniformity means no special cases for "first output" vs "hotplugged output".

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Broken Emacs config | Frames created with default positioning |
| IPC never connects | No frames created, compositor shows fallback |
| CreateFrame for unknown output | Compositor ignores, logs warning |
| Emacs crashes | Existing surfaces remain visible |
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

;; Assign non-Emacs surface to output
(ewm-assign-output 42 "DP-1")
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

This follows niri's pattern: windows open where you're working, not on a static "primary".

## Troubleshooting

**Only one monitor shows content:**
```bash
# Check if outputs are being discovered
RUST_LOG=debug ./target/release/ewm emacs 2>&1 | grep -i connector

# Verify DRM sees all outputs
cat /sys/class/drm/card*-*/status
```

**IPC not receiving events:**
```bash
# Verify socket exists
ls -la $XDG_RUNTIME_DIR/ewm.sock

# Test socket manually
echo '{"command":"screenshot"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/ewm.sock
```

**Frames not created for outputs:**
```elisp
;; Check if outputs were received
ewm--outputs

;; Manually trigger frame creation for all outputs
(ewm--setup-outputs)

;; Create frame for specific output
(ewm--create-frame-on-output "HDMI-A-1")
```

**Hotplug not detected:**
```bash
# Check udev events
udevadm monitor --property | grep -i drm
```

**No frames on startup:**
```bash
# Ensure Emacs is started as fg-daemon
# Check ewm spawn command includes --fg-daemon
```
