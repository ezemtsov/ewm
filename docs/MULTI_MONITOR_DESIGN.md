# EWM Multi-Monitor Design

## Overview

EWM supports multiple monitors with automatic frame-per-output management, hotplug detection, and Emacs-controlled configuration.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                Compositor (ewm) - Backend                   │
│  - Discovers physical outputs via DRM                       │
│  - Reports hardware info to Emacs                           │
│  - Executes positioning/assignment commands                 │
│  - Renders all outputs independently                        │
│  - Default: show new surfaces on primary output             │
└─────────────────────────────────────────────────────────────┘
                    │ events                   ▲ commands
                    ▼                          │
┌─────────────────────────────────────────────────────────────┐
│                Emacs (ewm.el) - Controller                  │
│  - User configuration for outputs                           │
│  - Decides output arrangement/positioning                   │
│  - Creates frames, requests output assignment               │
│  - All policy decisions live here                           │
└─────────────────────────────────────────────────────────────┘
```

## Key Design Decisions

### Single Frame Per Monitor
Each physical output maps to one Emacs frame. Emacs manages all windows within frames; the compositor handles output discovery and rendering.

### Emacs-Controlled Configuration
All output configuration (positioning, scale, primary selection) is controlled via `ewm.el`, not hardcoded in the compositor. This allows user-friendly configuration through Emacs customization.

### Compositor-Controlled Frame Assignment
Unlike X11 where clients control positioning, the Wayland compositor controls where surfaces appear. Emacs requests assignment via IPC, compositor executes.

### Show Immediately, Reassign Later
All new surfaces appear immediately on the primary output. Emacs can reassign to other outputs via IPC. This ensures:
- Something is always visible, even with broken config
- No timeouts or hidden states
- Uniform mechanism for all surfaces (including first frame)

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

    // Surface events include output info
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

    // Surface-to-output assignment
    AssignOutput { id: u32, output: String },
}
```

## Surface Lifecycle

```
1. Surface created (Emacs frame, or other Wayland client)
2. Compositor assigns to primary output immediately (always visible)
3. Compositor sends: New { id, app, output: "primary-output-name" }
4. Emacs receives, checks config/policy
5. If different output desired: Emacs sends AssignOutput { id, output }
6. Compositor moves surface to requested output
```

## Startup Flow

```
1. ewm starts
2. Compositor discovers outputs via DRM
3. Compositor positions outputs (auto-horizontal initially)
4. Compositor spawns Emacs
5. Emacs creates initial frame → toplevel surface
6. Compositor shows frame on primary output immediately
7. Compositor sends New event (queued until IPC connects)
8. Emacs runs ewm-connect (early in startup)
9. IPC connects, compositor sends:
   - OutputDetected for each output
   - Queued New events
10. Emacs consults ewm-output-config
11. Emacs sends ConfigureOutput for each output (repositions them)
12. Emacs sends AssignOutput for surfaces if needed
13. System is in user-configured state
```

## Hotplug Support

The compositor uses UdevBackend to detect monitor connect/disconnect events at runtime:

- **Connect**: Sends `OutputDetected` event, Emacs creates a new frame
- **Disconnect**: Sends `OutputDisconnected` event, Emacs closes the frame and moves windows to remaining frames

## Failure Modes

| Failure | Behavior |
|---------|----------|
| Broken Emacs config | All surfaces on primary, user can fix |
| IPC never connects | Single-monitor mode, fully functional |
| AssignOutput bug | Surfaces stay on primary |
| Emacs crashes | Existing surfaces remain visible |

## User Configuration

```elisp
;; Output positioning and properties
(setq ewm-output-config
      '(("HDMI-A-1" :position (0 . 0) :scale 1.0 :primary t)
        ("DP-1" :position (1920 . 0) :scale 1.25)
        ("eDP-1" :position (0 . 1080) :scale 2.0)))

;; Policy for new outputs not in config
(setq ewm-default-output-position 'right)  ; 'right, 'left, 'above, 'below

;; Per-app output rules
(setq ewm-app-output-rules
      '(("firefox" . "DP-1")
        ("emacs" . follow-focus)))
```

## Emacs Commands

```elisp
ewm--outputs                        ; list of detected outputs
(length (frame-list))               ; number of frames
(frame-parameter nil 'ewm-output)   ; output of current frame

;; Reposition an output
(ewm-configure-output "DisplayPort-1" :x 1920 :y 0)

;; Assign surface to output
(ewm-assign-output 1 "DisplayPort-1")
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

;; Manually trigger frame creation
(ewm--setup-outputs)
```

**Hotplug not detected:**
```bash
# Check udev events
udevadm monitor --property | grep -i drm
```
