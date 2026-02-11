# Render Architecture

This document describes EWM's rendering architecture, focusing on state management, the redraw state machine, and VBlank synchronization.

## State Ownership

The compositor uses direct state ownership through the event loop's generic parameter. All handlers receive `&mut State` automatically, enabling compile-time borrow checking without runtime overhead.

### State Structure

```rust
pub struct State {
    pub backend: DrmBackendState,                    // DRM backend (directly owned)
    pub ewm: Ewm,                                    // Compositor state
    pub emacs: Option<UnixStream>,                   // Emacs IPC connection
    pub ipc_stream_token: Option<RegistrationToken>, // IPC stream cleanup token
    pub client_process: Option<Child>,               // Spawned client (standalone mode)
}
```

Note: The Wayland `Display` is owned by the event loop (via `Generic` source), not by `State`. This ensures the display outlives all handlers.

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
         +-----------+-----------+
         |           |           |
         v           v           v
     Session      Input      DRM VBlank
     Handler      Handler    Handler

     All receive &mut State automatically
```

The `refresh_and_flush_clients()` callback is called after each event dispatch:

```rust
impl State {
    pub fn refresh_and_flush_clients(&mut self) {
        // Check stop conditions (module request, client exit)
        // Process pending early imports
        self.backend.redraw_queued_outputs(&mut self.ewm);
        // Process IM relay events
        self.flush_events();
        self.ewm.display_handle.flush_clients().unwrap();
    }
}
```

This pattern avoids `Rc<RefCell<>>` wrapping, providing:
- No Rc clone overhead
- No runtime borrow checking (compile-time safety)
- Clear ownership and proper Drop ordering
- Direct mutable references to state
- Backend is never `None` - no Option unwrapping needed

## Per-Output Rendering

Render elements are collected per-output, not globally. Each output only receives elements that intersect with its geometry.

### OutputState

Each output has its own state stored in `Ewm::output_state: HashMap<Output, OutputState>`:

```rust
pub struct OutputState {
    pub redraw_state: RedrawState,
    pub refresh_interval_us: u64,  // For estimated VBlank timer
}
```

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

The `queue_redraw()` method is idempotent - calling it multiple times won't queue duplicate redraws.

## Render Loop

The main render loop processes outputs with queued redraws:

```rust
// In main loop callback
for (output, output_state) in &mut state.ewm.output_state {
    match &output_state.redraw_state {
        RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
            // Render this output
            render_output(backend, &mut state.ewm, output);
        }
        _ => continue,
    }
}
```

### Render Flow

```
1. Collect render elements for output
   └─► Windows, cursors, popups intersecting output geometry

2. Render to DRM compositor
   └─► GbmDrmCompositor::render_frame()

3. Queue frame to KMS
   ├─► Success: transition to WaitingForVBlank
   └─► No damage: queue estimated VBlank timer

4. VBlank event received
   ├─► Send presentation feedback to clients
   ├─► If redraw_needed: transition to Queued
   └─► Else: transition to Idle
```

## VBlank Synchronization

### Real VBlank

When a frame is successfully queued to DRM/KMS, the compositor waits for the actual VBlank interrupt:

```rust
// After queue_frame() succeeds
output_state.redraw_state = RedrawState::WaitingForVBlank { redraw_needed: false };

// In VBlank handler
match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
    RedrawState::WaitingForVBlank { redraw_needed: true } => {
        // Another redraw was requested while waiting
        output_state.redraw_state = RedrawState::Queued;
    }
    RedrawState::WaitingForVBlank { redraw_needed: false } => {
        // Output is now idle
    }
    _ => unreachable!(),
}
```

### Estimated VBlank

When rendering produces no damage (content unchanged), the compositor uses a timer instead of submitting to KMS:

```rust
fn queue_estimated_vblank_timer(state: &mut State, output: &Output) {
    let refresh_interval = output_state.refresh_interval_us;
    let timer = Timer::from_duration(Duration::from_micros(refresh_interval));

    let token = event_loop.insert_source(timer, |_, _, state| {
        // Timer fired - can send frame callbacks now
        on_estimated_vblank(state, output);
        TimeoutAction::Drop
    });

    output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
}
```

This avoids busy-waiting when there's no visual change while still maintaining proper frame pacing for client frame callbacks.

## Damage Tracking

The compositor uses `OutputDamageTracker` to compare element commit counters between frames:

- **No damage**: Skip frame submission, use estimated VBlank timer
- **Damage detected**: Submit frame to DRM, wait for real VBlank

This optimization reduces CPU/GPU usage when content is static.

## Screen Sharing Integration

Screen sharing (via PipeWire/xdg-desktop-portal) integrates with the render loop:

1. When a screen cast is active, render elements are collected for the shared output
2. Frames are rendered to a DMA-BUF and sent to PipeWire
3. Frame rate limiting prevents excessive CPU usage (~30fps cap)
4. Damage tracking allows skipping frames when content hasn't changed

## Shutdown and Cleanup

The state ownership model ensures proper cleanup ordering:

```
Kill Combo / Quit Request / Client Exit
    │
    ▼
ewm.stop() -> LoopSignal::stop()
    │
    ▼
event_loop.run() returns
    │
    ▼
State dropped (in order):
    ├─► DrmBackendState::drop() runs
    │     ├─► Session notifier removed from event loop
    │     ├─► DRM devices dropped (DRM master released)
    │     └─► LibSeatSession dropped (VT released)
    └─► Other resources dropped
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
        // Fields then drop: device → libinput → session
    }
}
```

This ensures clean shutdown in both standalone and embedded modes.
