# EWM Screen Sharing Design

## Overview

EWM implements screen sharing through two complementary protocols:

1. **wlr-screencopy** — Wayland-native protocol for tools like `grim` and `wf-recorder`
2. **PipeWire ScreenCast** — D-Bus interface for xdg-desktop-portal (Firefox, OBS, etc.)

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

## wlr-screencopy

### Protocol (`compositor/src/protocols/screencopy.rs`)

Implements `zwlr_screencopy_manager_v1` and `zwlr_screencopy_frame_v1`:
- `capture_output(output)` — full output capture
- `capture_output_region(output, x, y, w, h)` — region capture
- Supports both SHM and DMA-BUF buffers
- Requires `xdg-output-manager` for tools like `grim` to detect output layout

### Request Handling

The `ScreencopyHandler::frame()` handler splits requests by type:

- **`Copy`** — rendered immediately via `Backend::with_renderer()`. Screenshot
  tools like `grim` get a response without waiting for the next VBlank.
- **`CopyWithDamage`** — queued for processing during the output redraw cycle,
  where per-queue damage tracking can skip no-change frames.

### Per-Queue Damage Tracking (`compositor/src/render.rs`)

Each screencopy manager (client) gets its own `OutputDamageTracker` inside
`ScreencopyQueue`. This tracks what changed since that specific client's last
capture:

- `process_screencopies_for_output()` iterates queues, computes damage via
  `damage_tracker.damage_output()`, and skips rendering when `CopyWithDamage`
  sees no damage — the request stays in the queue until the next redraw.
- Damage rectangles are converted from Physical to Buffer coordinates before
  being sent to the client.
- On render failure, the damage tracker resets to `(0,0)` so the next attempt
  reports full damage.
- Both paths (immediate and queued) update the damage tracker, keeping
  subsequent `CopyWithDamage` calls accurate.

Element collection uses `OnceCell` so multiple queues for the same output
share a single element collection pass. The screencopy state is temporarily
taken via `mem::take` to avoid borrow conflicts with element collection.

## PipeWire ScreenCast

### PipeWire Integration (`compositor/src/pipewire/`)

**`mod.rs`** — PipeWire initialization:
- MainLoop/Context/Core setup
- Event loop integration with calloop via fd polling
- Fatal error detection (EPIPE on connection loss)

**`stream.rs`** — `Cast` struct for video streaming:
- DMA-BUF buffer allocation via GBM device
- Format negotiation with SPA pod builder (DONT_FIXATE + dual modifier offer)
- 3-state machine: `ResizePending` → `ConfirmationPending` → `Ready`
- Non-blocking GPU sync via fence FD export + calloop source
- Damage-based frame skipping via `OutputDamageTracker`
- Timer-based scheduled redraws stored on Cast (cancellable)
- Refresh rate renegotiation via `set_refresh()`
- Linear modifier fallback when tiled exports produce multi-FD buffers

### D-Bus Interfaces (`compositor/src/dbus/`)

**`screen_cast.rs`** — `org.gnome.Mutter.ScreenCast` (version 4):
- `ScreenCast` — main interface with `CreateSession()`
- `Session` — per-session with `Start()`, `Stop()`, `RecordMonitor()`
- `Stream` — per-stream with `parameters` property, `PipeWireStreamAdded` signal

**`display_config.rs`** — `org.gnome.Mutter.DisplayConfig`:
- Required by xdg-desktop-portal-gnome for monitor enumeration
- Provides `GetCurrentState()` with monitor info

**`service_channel.rs`** — `org.gnome.Mutter.ServiceChannel`:
- Provides Wayland connection to xdg-desktop-portal-gnome
- Creates Unix socket pair, inserts portal as Wayland client

Each D-Bus interface uses its own blocking connection to avoid deadlocks
between interfaces. Names registered with `AllowReplacement | ReplaceExisting`
so the active session always takes over.

### Session Lifecycle

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

### Cast State Machine

```
ResizePending ──→ ConfirmationPending ──→ Ready
    ↑                                       │
    └───────── output resize ───────────────┘
```

- **ResizePending**: Initial state and after output resize. Waiting for
  PipeWire to negotiate format at the requested size.
- **ConfirmationPending**: Modifier fixated (DONT_FIXATE was set, multiple
  modifiers offered). Waiting for PipeWire to confirm the chosen format.
  During fixation, both the fixated format and original all-modifiers format
  are offered as fallback.
- **Ready**: Format confirmed, streaming. Holds the `OutputDamageTracker`.

### Non-Blocking GPU Sync

After rendering to a PipeWire DMA-BUF, `queue_after_sync()` avoids blocking
the compositor thread:

1. Export a fence FD from the `SyncPoint`
2. Register a calloop `Generic` source that triggers when the GPU is done
3. `queue_completed_buffers()` drains finished buffers in order

If fence export fails (pre-signalled or export error), the buffer is queued
immediately with a signalled SyncPoint to avoid getting stuck.

`rendering_buffers` (shared via `Rc<RefCell>`) tracks in-flight buffers.
The `remove_buffer` PipeWire callback cleans entries when PipeWire reclaims
a buffer.

### Rendering

Screencast rendering runs in `DrmBackendState::post_render()`, called from
`Ewm::redraw()` after the main frame is submitted. For each active cast on
the output:

1. `check_time_and_schedule()` — skip if too soon, schedule timer redraw
2. Element collection — lazy, shared across all casts for the same output
3. Render to PipeWire DMA-BUF buffer via `dequeue_buffer_and_render()`
4. Non-blocking queue via `queue_after_sync()`
5. Update `last_frame_time`

## DMA-BUF Feedback

`SurfaceDmabufFeedback { render, scanout }` is built per output surface at
connect time from DRM plane format information. After each render,
`Ewm::send_dmabuf_feedbacks()` tells clients which formats the compositor can
scanout directly vs. which require GPU composition. Clients that allocate in
scanout-compatible formats skip the GPU copy entirely.

The `select_dmabuf_feedback` utility chooses between render and scanout
feedback based on whether the surface is currently in the direct-scanout
state (tracked by `update_primary_scanout_output`).

## Performance

### Damage-Based Frame Skipping

Both screencopy and screencast use `OutputDamageTracker` to detect when
content hasn't changed:
- Screencopy: per-queue tracker, `CopyWithDamage` requests skip when idle
- Screencast: per-cast tracker inside `Cast::dequeue_buffer_and_render()`
- No damage → no render → reduced CPU/GPU usage

### Per-Output Redraw Tracking

- `queue_redraw(&output)` queues redraw only for the affected output
- `output_layouts` / `surface_outputs` determine which surfaces appear on which outputs
- Surface commits only trigger redraws on relevant outputs

### DMA-BUF Zero-Copy

Screencast buffers allocated via GBM device from DRM backend. Frames rendered
directly to PipeWire DMA-BUF buffers without memory copies.

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

### Modifier Fallback

Intel tiled modifiers (e.g., I915_FORMAT_MOD_Y_TILED) can produce multi-FD
GBM buffers that Smithay's `export()` doesn't support. `find_preferred_modifier()`
falls back to `Modifier::Linear` when the initial export fails, which always
produces single-FD buffers.

### Clean Shutdown

`Drop` impl for `Cast` cancels any scheduled redraw timer and explicitly
disconnects the PipeWire stream, ensuring clean disconnection even if `stop()`
is not called.

## Output Naming

EWM uses full DRM connector names matching Smithay's `connector.interface()`:
- `DisplayPort-1` (not `DP-1`)
- `EmbeddedDisplayPort-1` (not `eDP-1`)
- `HDMI-A-1`

## Portal Configuration

EWM implements `org.gnome.Mutter.ScreenCast` (the GNOME D-Bus interface), so
the portal backend must be `xdg-desktop-portal-gnome` — not `xdg-desktop-portal-wlr`
which uses `wlr-screencopy`.

The NixOS module (`nix/service.nix`) configures:
- `xdg.portal.config.ewm` routes ScreenCast to `gnome`
- `extraPortals` includes `xdg-desktop-portal-gnome` and `xdg-desktop-portal-gtk`

### Startup Race Condition

`xdg-desktop-portal-gnome` needs `org.gnome.Mutter.ServiceChannel` to be
registered on D-Bus when it starts. If the portal is D-Bus-activated before
the compositor registers its names (e.g., triggered by
`dbus-update-activation-environment` in the session script), the portal falls
back to "Non-compatible display server, exposing settings only" and ScreenCast
is unavailable.

EWM uses `Type=notify` with `sd_notify(READY=1)` sent after D-Bus registration,
so systemd-ordered services wait. But D-Bus activation bypasses systemd ordering.
If the portal starts too early, restart it: `systemctl --user restart
xdg-desktop-portal-gnome.service`.

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
