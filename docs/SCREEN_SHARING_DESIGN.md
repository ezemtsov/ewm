# EWM Screen Sharing Design

## Overview

EWM implements screen sharing through two complementary protocols:

1. **wlr-screencopy** - Wayland-native protocol for tools like `grim` and `wf-recorder`
2. **PipeWire ScreenCast** - D-Bus interface for xdg-desktop-portal (Firefox, OBS, etc.)

## Architecture

```
Applications (OBS, Firefox, etc.)
        ↓
xdg-desktop-portal-gnome
        ↓ D-Bus
org.gnome.Mutter.ScreenCast interface (EWM)
        ↓
PipeWire stream (DMA-BUF zero-copy)
        ↓
EWM compositor rendering
```

## Components

### wlr-screencopy (`compositor/src/protocols/screencopy.rs`)

Implements `zwlr_screencopy_manager_v1` and `zwlr_screencopy_frame_v1` interfaces:
- `capture_output(output)` - Full output capture
- `capture_output_region(output, x, y, w, h)` - Region capture
- Supports both SHM and DMA-BUF buffers

**Note**: Requires `xdg-output-manager` protocol for tools like `grim` to detect output layout.

### PipeWire Integration (`compositor/src/pipewire/`)

**`mod.rs`** - PipeWire initialization:
- MainLoop/Context/Core setup
- Event loop integration with calloop via fd polling
- Fatal error detection (EPIPE on connection loss)

**`stream.rs`** - Cast struct for video streaming:
- DMA-BUF buffer allocation via GBM device
- Format negotiation with SPA pod builder
- Damage-based frame skipping via `OutputDamageTracker`
- Frame rate limiting (~30fps cap)

### D-Bus Interfaces (`compositor/src/dbus/`)

**`screen_cast.rs`** - `org.gnome.Mutter.ScreenCast` (version 4):
- `ScreenCast` - Main interface with `CreateSession()`
- `Session` - Per-session with `Start()`, `Stop()`, `RecordMonitor()`
- `Stream` - Per-stream with `parameters` property, `PipeWireStreamAdded` signal

**`display_config.rs`** - `org.gnome.Mutter.DisplayConfig`:
- Required by xdg-desktop-portal-gnome for monitor enumeration
- Provides `GetCurrentState()` with monitor info

**`service_channel.rs`** - `org.gnome.Mutter.ServiceChannel`:
- Portal compatibility interface

## Data Flow

### Screen Cast Session Lifecycle

```
D-Bus Thread                          Compositor (calloop)
     │                                       │
     │ CreateSession()                       │
     │ RecordMonitor(connector)              │
     │ Start()                               │
     │    ├──── StartCast ──────────────────>│ Create PipeWire stream
     │    │                                  │ Store in screen_casts HashMap
     │    │<─── node_id (via signal_ctx) ────│
     │    │                                  │
     │ PipeWireStreamAdded(node_id)          │
     │    │                                  │
     │    │     [frames rendered each vblank]│
     │    │                                  │
     │ Stop() or output disconnect           │
     │    ├──── StopCast ───────────────────>│ Remove from screen_casts
     │    │<─── Session::stop() ─────────────│ Emit Closed signal
     │    │                                  │ Disconnect PipeWire stream
```

### Per-Output Element Collection

Elements are collected per-output for efficient rendering:

```rust
collect_render_elements_for_output(
    ewm,
    renderer,
    output_scale,
    cursor_buffer,
    output_pos,
    output_size,
    include_cursor: true,  // Cursor rendered into frame
)
```

Only elements intersecting the output geometry are included, preventing false damage from other outputs.

## Performance Optimizations

### Damage-Based Frame Skipping

`OutputDamageTracker` compares element commit counters between frames:
- No damage → no render → reduced CPU/GPU usage
- Idle outputs emit zero PipeWire frames

**Measured overhead**: ~2% CPU when sharing static content.

### Per-Output Redraw Tracking

- `queue_redraw(&output)` queues redraw only for the affected output
- `output_layouts` / `surface_outputs` determine which surfaces appear on which outputs
- Surface commits only trigger redraws on relevant outputs

### DMA-BUF Zero-Copy

Buffers allocated via GBM device from DRM backend. Frames rendered directly to PipeWire DMA-BUF buffers without memory copies.

## Robustness

### Output Hotplug

When an output disconnects during screen sharing:
1. `stop_cast()` called for all sessions on that output
2. PipeWire stream explicitly disconnected
3. D-Bus `Closed` signal emitted (clients see stream ended, not frozen)
4. Session removed from D-Bus object server

### PipeWire Fatal Errors

Core error listener detects connection loss (EPIPE):
1. `had_fatal_error` flag set
2. Fatal error channel notifies compositor
3. All screencasts cleared

### Clean Shutdown

`Drop` impl for `Cast` explicitly disconnects PipeWire stream, ensuring clean disconnection even if `stop()` not called.

## Output Naming

EWM uses full DRM connector names matching Smithay's `connector.interface()`:
- `DisplayPort-1` (not `DP-1`)
- `EmbeddedDisplayPort-1` (not `eDP-1`)
- `HDMI-A-1`

## Feature Flag

Screen sharing is optional:

```toml
[features]
screencast = ["pipewire", "zbus", "async-io"]
```

Build with: `cargo build --features screencast`

## Testing

```bash
# Screenshot (wlr-screencopy)
grim /tmp/screenshot.png

# Screen recording (wlr-screencopy)
wf-recorder -f /tmp/recording.mp4

# WebRTC screen share (PipeWire)
firefox https://meet.jit.si/test

# OBS capture (PipeWire)
obs  # Add source → Screen Capture (PipeWire)

# Verify D-Bus interface
busctl --user introspect org.gnome.Mutter.ScreenCast /org/gnome/Mutter/ScreenCast

# Monitor PipeWire
pw-cli list-objects | grep ewm
```

## Reference

Based on niri's implementation:
- `niri/src/pw_utils.rs` - PipeWire integration
- `niri/src/dbus/mutter_screen_cast.rs` - D-Bus interface
