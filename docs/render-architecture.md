# Render Architecture

This document describes EWM's rendering architecture, focusing on state management, the redraw state machine, and VBlank synchronization.

## State Ownership

The compositor uses direct state ownership through the event loop's generic parameter. All handlers receive `&mut State` automatically, enabling compile-time borrow checking without runtime overhead.

### State Structure

```rust
pub struct State {
    pub backend: Backend,  // Backend enum (DRM or Headless)
    pub ewm: Ewm,         // Compositor state
}

pub enum Backend {
    Drm(DrmBackendState),
    Headless(HeadlessBackend),
}
```

The `Backend` enum dispatches to the active backend. Backend-specific operations (render, post-render, early-import) are methods on `Backend` that delegate to the inner variant. Backend-agnostic orchestration (redraw loop, state transitions, frame callbacks) lives on `Ewm`.

Note: The Wayland `Display` is owned by the event loop (via `Generic` source), not by `State`. This ensures the display outlives all handlers. Communication with Emacs happens via shared state (mutexes) and SIGUSR1 signals — no IPC socket needed.

### Event Loop Integration

```
                EVENT LOOP
                     |
                     v
    EventLoop::try_new() -> EventLoop<State>
                     |
                     v
    event_loop.run(None, &mut state, |state| {
        state.refresh_and_flush_clients()
    })
                     |
         +-----------+-----------+-----------+-----------+
         |           |           |           |           |
         v           v           v           v           v
     Session      Input      DRM VBlank   UDev        Module
     Handler      Handler    Handler      Handler     Commands
     (pause/      (libinput) (frame       (hotplug)   (DRM init
      resume)                 pacing)                  channel)
```

All handlers receive `&mut State` automatically.

The `refresh_and_flush_clients()` callback runs after each event dispatch:

```rust
impl State {
    pub fn refresh_and_flush_clients(&mut self) {
        // 1. Check if stop was requested from Emacs module (ewm-stop)
        // 2. Process module commands (from Emacs via dynamic module)
        // 3. Sync keyboard focus after processing commands
        self.sync_keyboard_focus();

        // 4. Process pending early buffer imports
        for surface in self.ewm.pending_early_imports.drain(..) {
            self.backend.early_import(&surface);
        }

        // 5. Render any outputs with queued redraws
        self.ewm.redraw_queued_outputs(&mut self.backend);

        // 6. Process IM relay events and send to Emacs
        self.process_im_events();

        // 7. Clean up dead windows from space
        self.ewm.cleanup_dead_windows();

        // 8. Flush Wayland clients
        if let Err(e) = self.ewm.display_handle.flush_clients() {
            warn!("Failed to flush Wayland clients: {e}");
        }
    }
}
```

This pattern avoids `Rc<RefCell<>>` wrapping, providing:
- No Rc clone overhead
- No runtime borrow checking (compile-time safety)
- Clear ownership and proper Drop ordering
- Direct mutable references to state
- Backend is never `None` — no Option unwrapping needed

## Per-Output Rendering

Render elements are collected per-output, not globally. Each output only receives elements that intersect with its geometry.

### OutputState

Each output has its own state stored in `Ewm::output_state: HashMap<Output, OutputState>`:

```rust
pub struct OutputState {
    pub redraw_state: RedrawState,
    pub frame_clock: frame_clock::FrameClock,
    pub unfinished_animations_remain: bool,
    pub vblank_tracker: VBlankFrameTracker,
    pub lock_surface: Option<LockSurface>,
    pub lock_render_state: LockRenderState,
    pub lock_color_buffer: SolidColorBuffer,
    pub frame_callback_sequence: u32,
}
```

Key fields:
- `redraw_state`: Tracks the output's position in the redraw state machine
- `frame_clock`: Tracks last presentation time and refresh interval, predicts next VBlank via `next_presentation_time()`
- `unfinished_animations_remain`: When true, VBlank and estimated VBlank handlers queue another redraw even if `redraw_needed` is false, keeping animations pumping without explicit `queue_redraw()` calls from animation code
- `frame_callback_sequence`: Monotonically increasing counter incremented each VBlank cycle, used to prevent sending duplicate frame callbacks within the same refresh cycle
- `lock_surface` / `lock_render_state` / `lock_color_buffer`: Session lock per-output state (see `FOCUS_DESIGN.md`)

### Redraw State Machine

The `RedrawState` enum tracks each output's rendering state:

```rust
pub enum RedrawState {
    Idle,
    Queued,
    WaitingForVBlank { redraw_needed: bool },
    WaitingForEstimatedVBlank(RegistrationToken),
    WaitingForEstimatedVBlankAndQueued(RegistrationToken),
}
```

State transitions:

```
                    queue_redraw()
        Idle ─────────────────────────► Queued
         ▲                                 │
         │                                 │ render submitted to DRM
         │                                 ▼
         │                    WaitingForVBlank { redraw_needed: false }
         │                                 │
         │ VBlank received                 │ queue_redraw() while waiting
         │ (redraw_needed = false)         ▼
         │                    WaitingForVBlank { redraw_needed: true }
         │                                 │
         └─────────────────────────────────┘
                   VBlank received (redraw_needed = true)
                   transitions back to Queued
```

When a frame produces no damage:

```
    Queued ──────────────────────► WaitingForEstimatedVBlank(token)
           no damage, start timer              │
                                               │ queue_redraw()
                                               ▼
                              WaitingForEstimatedVBlankAndQueued(token)
                                               │
                                               │ timer fires
                                               ▼
                                            Queued
```

### Queue Redraw Methods

```rust
impl Ewm {
    /// Queue a redraw for all outputs
    pub fn queue_redraw_all(&mut self) {
        for state in self.output_state.values_mut() {
            state.redraw_state = mem::take(&mut state.redraw_state).queue_redraw();
        }
    }

    /// Queue a redraw for a specific output
    pub fn queue_redraw(&mut self, output: &Output) {
        if let Some(state) = self.output_state.get_mut(output) {
            state.redraw_state = mem::take(&mut state.redraw_state).queue_redraw();
        }
    }
}
```

The `queue_redraw()` method is idempotent — calling it multiple times won't queue duplicate redraws.

## Render Loop

The redraw loop lives on `Ewm` and iterates `output_state` using `while let .find()` to pick up outputs that get queued during rendering (e.g., by VBlank handlers):

```rust
impl Ewm {
    pub fn redraw_queued_outputs(&mut self, backend: &mut Backend) {
        while let Some(output) = self
            .output_state
            .iter()
            .find(|(_, state)| matches!(
                state.redraw_state,
                RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
            ))
            .map(|(output, _)| output.clone())
        {
            self.redraw(backend, &output);
        }
    }
}
```

### Render Flow

`Ewm::redraw()` orchestrates a single output redraw. The backend only handles the GPU/DRM render and returns a `RenderResult`:

```rust
pub enum RenderResult {
    Submitted,  // Frame submitted for presentation
    NoDamage,   // Render succeeded but no damage
    Skipped,    // Frame not rendered (error or paused)
}
```

Orchestration:

```
1. Get target_presentation_time from FrameClock
   └─► frame_clock.next_presentation_time()

2. Render via backend
   └─► backend.render(ewm, output, target_presentation_time) → RenderResult

3. Handle state transitions based on RenderResult
   ├─► Submitted: DRM backend transitions to WaitingForVBlank internally
   ├─► NoDamage: DRM backend queues estimated VBlank timer internally
   └─► Skipped: preserve existing timer or go Idle

4. Update lock render state if session is locked

5. Send frame callbacks to clients
   └─► send_frame_callbacks() — throttled by frame_callback_sequence

6. Process screencopy and screencast via backend
   └─► backend.post_render(ewm, output)
```

The DRM backend's `render()` method handles:
- Collecting render elements for the output
- `GbmDrmCompositor::render_frame()` with GPU sync
- Updating primary scanout output tracking
- Collecting presentation feedback for `wp_presentation_time`
- `queue_frame()` with feedback data, or queueing estimated VBlank timer on no-damage
- Incrementing `frame_callback_sequence`

## VBlank Synchronization

### FrameClock

Each output has a `FrameClock` that tracks presentation timing:

```rust
pub struct FrameClock {
    last_presentation_time: Option<Duration>,
    refresh_interval_ns: Option<NonZeroU64>,
}
```

- `new(refresh_interval)`: Constructor; refresh interval is set from DRM mode refresh rate at output creation
- `presented(time)`: Records the last VBlank presentation time
- `next_presentation_time()`: Predicts when the next VBlank will occur, used for both the render path (`target_presentation_time`) and estimated VBlank timer duration

### VBlankThrottle

Some buggy drivers deliver VBlank events too early (< 50% of the expected refresh interval). `VBlankThrottle` detects this and defers processing with a timer:

```rust
impl VBlankThrottle {
    pub fn throttle<F>(&mut self, ..., callback: F) -> bool
    // Returns true if throttled (caller should NOT proceed — deferred via timer)
    // Returns false if not throttled (caller should proceed normally)
}
```

Each DRM output surface has its own `VBlankThrottle`. The VBlank handler calls `throttle()` before processing — if the VBlank arrived too early, processing is deferred to a timer that fires at the expected time.

### Real VBlank

When a frame is successfully queued to DRM/KMS, the compositor waits for the actual VBlank interrupt. The VBlank handler extracts `DrmEventMetadata` (presentation time, sequence number) and delegates to `process_vblank()`:

```rust
fn process_vblank(&mut self, crtc, meta: DrmEventMetadata, ewm) {
    // 1. Extract presentation time from DRM metadata (fallback to monotonic clock)
    let presentation_time = match meta.time {
        DrmEventTime::Monotonic(time) if !time.is_zero() => time,
        _ => get_monotonic_time(),
    };

    // 2. Process frame_submitted() and send presentation feedback
    match surface.compositor.frame_submitted() {
        Ok(Some((feedback, _target))) => {
            feedback.presented(presentation_time, refresh, seq, flags);
        }
        ...
    }

    // 3. Record presentation time in FrameClock
    output_state.frame_clock.presented(presentation_time);

    // 4. Transition state and handle redraw_needed / animations
    let old_state = mem::replace(&mut output_state.redraw_state, Idle);
    let redraw_needed = match &old_state {
        WaitingForVBlank { redraw_needed } => *redraw_needed,
        _ => true, // force redraw to recover from unexpected state
    };

    if redraw_needed || output_state.unfinished_animations_remain {
        ewm.queue_redraw(&output);
    } else {
        ewm.send_frame_callbacks(&output);
    }
}
```

Presentation feedback flags include `Vsync | HwCompletion`, plus `HwClock` when the DRM timestamp is a real hardware timestamp (not zero).

### Estimated VBlank

When rendering produces no damage (content unchanged), the compositor uses a timer instead of submitting to KMS. The timer duration is computed from `FrameClock`:

```rust
fn queue_estimated_vblank_timer(&mut self, output, ewm, target_presentation_time) {
    let now = get_monotonic_time();
    let mut duration = target_presentation_time.saturating_sub(now);

    // Don't set a zero timer — frame callbacks are sent right after render anyway
    if duration.is_zero() {
        duration = frame_clock.refresh_interval().unwrap_or(Duration::from_micros(16_667));
    }

    let token = handle.insert_source(Timer::from_duration(duration), |_, _, state| {
        drm.on_estimated_vblank_timer(crtc, &mut state.ewm);
        TimeoutAction::Drop
    });
    output_state.redraw_state = WaitingForEstimatedVBlank(token);
}
```

The estimated VBlank handler increments `frame_callback_sequence` and either transitions to `Queued` (if a redraw was requested or animations remain) or to `Idle` (sending frame callbacks to clients).

## Frame Callback Throttling

Each output maintains a `frame_callback_sequence` counter that increments on every VBlank (real or estimated). The `send_frame_callbacks()` method uses this to avoid duplicate callbacks:

```rust
pub fn send_frame_callbacks(&self, output: &Output) {
    let sequence = self.output_state.get(output).map(|s| s.frame_callback_sequence);

    // For each surface: check if primary scanout output matches,
    // then check if already sent at this sequence number.
    // Skip if (last_output, last_sequence) matches current — prevents
    // double-sending within the same refresh cycle.
}
```

This is especially important because frame callbacks are sent from both the VBlank handler (for `redraw_needed: false`) and the render path (after every `redraw`).

## Damage Tracking

The `DrmCompositor` compares element commit counters between frames via `render_frame()`:

- **No damage** (`result.is_empty`): Skip frame submission, use estimated VBlank timer
- **Damage detected**: Submit frame to DRM via `queue_frame()`, wait for real VBlank

This optimization reduces CPU/GPU usage when content is static.

## Presentation Feedback

The `wp_presentation_time` protocol provides clients with accurate frame timing information:

1. Before `queue_frame()`, `take_presentation_feedbacks()` collects `OutputPresentationFeedback` from all visible surfaces (windows, layer surfaces, lock surfaces) using their render element states
2. The feedback and `target_presentation_time` are passed as frame data through `queue_frame()`
3. On VBlank, `frame_submitted()` returns the feedback, which is then sent to clients via `feedback.presented()` with the actual DRM presentation time, refresh rate, and hardware flags

## Screen Sharing Integration

Screen sharing (via PipeWire/xdg-desktop-portal) integrates with the render loop:

1. After the main DRM frame is submitted, active screen casts for the output are checked
2. Elements are lazily collected (shared across multiple casts on the same output)
3. Each cast calls `dequeue_buffer_and_render()` to render into a DMA-BUF PipeWire buffer
4. Frame rate limiting prevents excessive CPU usage (~30fps cap via `should_skip_frame`)
5. Damage tracking within the cast allows skipping unchanged frames
6. Orphaned casts (output disconnected) are detected and skipped

Screen sharing orchestration happens in `Backend::post_render()`, called by `Ewm::redraw()` after a successful render.

## Session Pause/Resume

VT switching triggers session pause/resume:

```
VT switch away → SessionEvent::PauseSession
    ├─► Suspend libinput
    ├─► Pause DRM device
    ├─► Cancel all estimated VBlank timers
    └─► Set all output states to Idle, set paused = true

VT switch back → SessionEvent::ActivateSession
    ├─► Resume libinput
    ├─► Activate DRM device (acquire DRM master)
    ├─► Reset DRM compositor state on all surfaces
    │     ├─► reset_state() — re-read hardware state
    │     └─► reset_buffers() — clear stale buffer references
    └─► Queue redraws for all outputs
```

The DRM compositor reset on resume is critical: without it, stale buffer references from before the pause can cause rendering artifacts.

The `render()` method checks `paused` and `drm.is_active()` before rendering, preventing attempts to submit frames while another session holds DRM master.

## Shutdown and Cleanup

The state ownership model ensures proper cleanup ordering:

```
Kill Combo / ewm-stop / Client Exit
    │
    ▼
ewm.stop() -> LoopSignal::stop()
    │
    ▼
event_loop.run() returns
    │
    ▼
State dropped (in order):
    ├─► Backend::Drm(DrmBackendState)::drop() runs
    │     ├─► Session notifier removed from event loop
    │     ├─► DRM devices dropped (DRM master released)
    │     └─► LibSeatSession dropped (VT released)
    └─► Ewm dropped (Wayland globals cleaned up)
    │
    ▼
Clean exit to TTY
```

### Session Notifier Cleanup

The `DrmBackendState` implements `Drop` to ensure the session notifier is removed
from the event loop BEFORE the `LibSeatSession` is dropped. This is critical because:

1. The `SessionNotifier` holds references to session internals
2. In embedded mode (dynamic module), the process doesn't exit to force cleanup
3. If the session drops while the notifier is still registered, libseat cleanup fails

```rust
impl Drop for DrmBackendState {
    fn drop(&mut self) {
        // Remove notifier BEFORE session drops
        if let (Some(handle), Some(token)) = (&self.loop_handle, self.session_notifier_token.take()) {
            handle.remove(token);
        }
        // Fields then drop in declaration order: device → libinput → session
    }
}
```

This ensures clean shutdown when running as a dynamic module within Emacs.

## Remaining Gaps vs Niri

Comparison performed against niri commit `ae14fa12`. Items are ordered by
impact on correctness. Animations are excluded (not yet implemented).

### 1. DMA-BUF Feedback

**Gap**: After `update_primary_scanout_output`, niri calls
`send_dmabuf_feedbacks()` which tells clients which DRM device and format
modifiers to use for their next buffer allocation. EWM skips this entirely.

**Impact**: GPU-accelerated clients (Firefox, mpv, chromium) allocate DMA-BUFs
without knowing whether the compositor can scanout their buffer directly. This
forces the compositor to copy through the GPU rather than promoting the buffer
to a hardware plane, increasing power consumption and latency.

**Plan**:
- Niri stores `SurfaceDmabufFeedback { render, scanout }` per output surface,
  created from `DrmCompositor::format_feedback()` at output connect time
  (`tty.rs:1109`).
- `send_dmabuf_feedbacks()` (`niri.rs:4611`) iterates windows and layer
  surfaces, calling `send_dmabuf_feedback()` with a closure that selects
  `scanout` feedback for surfaces in direct-scanout state, `render` feedback
  otherwise (via `select_dmabuf_feedback`).
- Add `dmabuf_feedback: Option<SurfaceDmabufFeedback>` to `OutputSurface` in
  `drm.rs`. Populate it from `surface.compositor.format_feedback()` in
  `connect_output()`.
- Add `Ewm::send_dmabuf_feedbacks()` to `lib.rs`, iterating `id_windows` and
  layer surfaces.
- Call it from `DrmBackendState::render()` after `update_primary_scanout_output`.
- Files: `backend/drm.rs`, `lib.rs`

### 2. Lock Render State — Bidirectional Updates

**Gap**: `Ewm::redraw()` only sets `lock_render_state = Locked` when the
session is locked and a lock surface exists. It never sets `Unlocked` on a
successful non-locked render. Niri sets `Locked` or `Unlocked` on every
non-Skipped render (`niri.rs:4381-4385`).

**Impact**: If the lock state transitions from locked to unlocked, the
`lock_render_state` stays `Locked` until explicitly cleared by `unlock()` or
`abort_lock_on_render_failure()`. This is currently safe because those paths
do fire, but it's fragile — any new code path that checks `lock_render_state`
could see stale `Locked` state.

**Plan**:
- In `Ewm::redraw()`, replace the current conditional-only-set-Locked with:
  ```rust
  if res != RenderResult::Skipped {
      state.lock_render_state = if is_locked {
          LockRenderState::Locked
      } else {
          LockRenderState::Unlocked
      };
  }
  ```
- This matches niri's pattern exactly.
- Files: `lib.rs`

### 3. Cursor and DnD Frame Callbacks & Presentation Feedback

**Gap**: `send_frame_callbacks()` and `take_presentation_feedbacks()` cover
windows, layer surfaces, and lock surfaces — but not the cursor surface or
DnD icon surface. Niri sends to all five (`niri.rs:4770-4788`, `4867-4887`).

**Impact**: Cursor surface animations (e.g. loading spinner) won't animate
because they never receive frame callbacks. DnD icon surfaces have the same
issue. Most clients don't notice, but it's a protocol correctness gap.

**Plan**:
- In `send_frame_callbacks()`: after the lock surface block, add blocks for
  `self.cursor_manager.cursor_image()` (check for `CursorImageStatus::Surface`)
  and `self.dnd_icon` if they exist.
- In `take_presentation_feedbacks()`: similarly add
  `take_presentation_feedback_surface_tree` calls for cursor and DnD surfaces.
- Requires access to cursor manager and DnD icon state from `Ewm`. Check
  whether these are already stored there.
- Files: `lib.rs`

### 4. Screencopy Damage Tracking

**Gap**: EWM's screencopy always reports full damage (the `process_screencopies`
function in `render.rs` sends the entire output region as damage). Niri uses
per-queue damage trackers (`render_for_screencopy_with_damage` at
`niri.rs:4923`) to only report actual damaged regions.

**Impact**: Screencopy clients (e.g. `grim`, `wf-recorder`) receive the full
output as damage every frame, preventing them from optimizing for partial
updates. For screenshot tools this is irrelevant, but for continuous capture
tools it wastes bandwidth.

**Plan**:
- Add a per-output `OutputDamageTracker` for screencopy (separate from the
  DRM compositor's internal tracker).
- In the screencopy render path, use this tracker to compute actual damage
  between frames.
- Pass the damage rectangles to the screencopy submission instead of the
  full output region.
- Files: `render.rs`, possibly `lib.rs` (OutputState)

### 5. Fallback Frame Callback Timer

**Gap**: Niri registers a 1-second recurring timer (`niri.rs:2366-2374`) that
calls `send_frame_callbacks_on_fallback_timer()` — a safety net that sends
frame callbacks to ALL surfaces unconditionally (bypassing output matching).
EWM has no equivalent.

**Impact**: If a surface somehow gets stuck without receiving frame callbacks
(e.g. it was invisible but became visible between VBlanks, or its primary
scanout output tracking is stale), it will remain frozen until the next
redraw cycle touches it. The fallback timer prevents this class of bugs.

**Plan**:
- Add a 1-second recurring `Timer` source in compositor initialization.
- The callback calls a new `Ewm::send_frame_callbacks_on_fallback_timer()`
  that iterates all windows, layer surfaces, cursor, and DnD, sending frame
  callbacks with `|_, _| None` (no output check — just the throttle timer
  prevents busy-looping).
- Files: `lib.rs`

### 6. Session Resume Robustness

**Gap**: EWM's `resume()` does: resume libinput, activate DRM, reset DRM
compositor state, queue redraws. Niri's resume (`tty.rs:593-718`) additionally
handles: devices removed during VT switch, devices added during VT switch,
connector topology changes via `device_changed()`, gamma restoration, DRM
lease resumption, deferred output config changes.

**Impact**: If a monitor is plugged in or unplugged while on another VT, EWM
won't notice until the next udev event (which may not fire if the topology
change happened on the other VT). Gamma settings will be lost. These are edge
cases but affect real multi-monitor setups.

**Plan**:
- After `device.drm.activate(true)`, call the existing `on_device_changed()`
  to re-scan connectors. This catches monitors added/removed during VT switch.
- Add `deferred_output_config: bool` flag. When `apply_output_config()` is
  called while paused, set the flag. On resume, apply deferred config.
- Gamma restoration and DRM lease support can be deferred to when those
  features are actually needed.
- Files: `backend/drm.rs`

### 7. Screencast in Compositor vs Backend

**Gap**: EWM runs screencopy and screencast in `Backend::post_render()` (called
from `Ewm::redraw()`). Niri runs them directly inside `Niri::redraw()` using
`backend.with_primary_renderer()` to get renderer access.

**Impact**: No functional impact — both produce the same result. The difference
is architectural: screen sharing is a compositor-level concern (it needs to
know about outputs, surfaces, and damage) that happens to need a renderer.
Placing it in the backend couples it to DRM details unnecessarily.

**Plan**:
- Move screencopy and screencast rendering from `DrmBackendState::post_render()`
  to `Ewm::redraw()`.
- Add `Backend::with_renderer()` (already exists for crossfade snapshots) to
  provide renderer access from the compositor level.
- The `post_render()` method on `Backend` can then be removed.
- This is a refactor with no behavioral change — low priority.
- Files: `lib.rs`, `backend/drm.rs`, `backend/mod.rs`
