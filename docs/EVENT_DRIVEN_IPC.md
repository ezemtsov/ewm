# Event-Driven IPC Refactoring

This document describes the refactoring done to make the compositor fully event-driven, eliminating unnecessary polling delays.

## Problem

The compositor had several time-based synchronizations that added latency:

1. **16ms dispatch timeout** - The event loop used `dispatch(Some(Duration::from_millis(16)))` which caused periodic wake-ups even when no events were pending. This was required because IPC commands from Emacs were polled rather than event-driven.

2. **2-second client spawn delay** - An unnecessary delay before spawning the client process.

## Solution

Inspired by [niri](https://github.com/YaLTeR/niri)'s approach, the IPC handling was refactored to be fully event-driven.

### Key Changes

#### 1. State Fields Added to `Ewm` (main.rs)

```rust
struct Ewm {
    // ...
    keyboard_focus: Option<WlSurface>,  // Actual keyboard focus surface
    pending_screenshot: Option<String>, // Screenshot request (set by IPC, consumed by render loop)
    // ...
}
```

#### 2. Event-Driven IPC Stream Registration

When Emacs connects to the IPC socket, the stream is now registered as a calloop event source:

```rust
// When connection is accepted:
let token = loop_handle.insert_source(
    Generic::new(stream, Interest::READ, CalloopMode::Level),
    |_, source, data: &mut LoopData| {
        let stream = unsafe { source.get_mut() };
        data.process_commands_from_stream(stream);
        Ok(PostAction::Continue)
    },
).expect("Failed to register IPC stream");
```

This means:
- Commands are processed immediately when data arrives on the socket
- No polling is needed
- The event loop only wakes when there's actual work to do

#### 3. Dispatch Timeout Removed

Changed from:
```rust
event_loop.dispatch(Some(Duration::from_millis(16)), &mut data)?;
```

To:
```rust
event_loop.dispatch(None, &mut data)?;
```

The compositor now blocks indefinitely waiting for events, just like niri does.

#### 4. Command Handler Refactored

The `handle_command` method no longer takes `keyboard_focus` as a parameter - it uses the state field directly:

```rust
fn handle_command(&mut self, cmd: Command) {
    // ...
    Command::Focus { id } => {
        self.state.keyboard_focus = Some(focus_surface.clone());
        // ...
    }
    Command::Screenshot { path } => {
        self.state.pending_screenshot = Some(target);
    }
    // ...
}
```

## Files Modified

- `src/main.rs` - Winit backend IPC and main loop
- `src/drm_backend.rs` - DRM backend IPC and main loop

## Comparison with Niri

Niri uses async IO (`calloop::io::Async` with `futures_util`) for IPC handling. Our approach is simpler - we register the stream directly as a `Generic` event source and process commands synchronously in the callback. This achieves the same event-driven behavior without the complexity of async.

## Testing

The compositor should now:
1. Start immediately (no 2-second delay)
2. Respond to IPC commands with minimal latency
3. Use less CPU when idle (no 60Hz polling)
