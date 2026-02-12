# EWM Project Guidelines

## Overview
EWM (Emacs Wayland Manager) is a Wayland compositor designed specifically for Emacs, providing EXWM-like functionality on Wayland.

## Architecture
- `compositor/` - Rust compositor using Smithay framework
- `ewm.el` - Emacs integration package

## Commit Style
Use conventional commits without Co-Authored-By lines:
- `feat(scope):` for new features
- `fix(scope):` for bug fixes
- `refactor(scope):` for refactoring
- `docs:` for documentation
- `chore:` for maintenance

## Code Style

### Rust
- Follow standard Rust conventions
- Keep named key → keysym mapping in `KeyId::to_keysym()`

### Emacs Lisp
- Use character literals for keys: `?\C-x` not `"C-x"`
- Prefer Emacs built-ins (e.g., `key-parse`) over custom parsing

## Key Design Decisions
- Module-only mode: compositor runs as thread within Emacs
- Emacs sends pre-parsed keysyms to compositor (not string notation)
- Super-key bindings are auto-detected from Emacs keymaps

## Compositor Design Principles

### Per-Output Rendering
Render elements are collected per-output, not globally. Each output only receives
elements that intersect with its geometry. This is critical for:
- **Efficient rendering**: Don't process elements that won't be visible
- **Accurate damage tracking**: Elements from other outputs don't trigger false damage
- **Screen sharing**: Only elements on the shared output affect the stream

### Damage-Based Frame Skipping
Screen sharing uses damage tracking to skip frames when content hasn't changed:
- `OutputDamageTracker` compares element commit counters between frames
- No damage = no render = reduced CPU/GPU usage
- Frame rate limiting provides a fallback (~30fps cap)

### VBlank Synchronization
The redraw state machine ensures proper frame pacing:
- `RedrawState::Idle` → `Queued` → `WaitingForVBlank` → `Idle`
- Redraw flag cleared after VBlank, not after queue_frame
- Estimated VBlank timer used when no damage (avoids busy-waiting)

### D-Bus Integration
Each D-Bus interface (ScreenCast, DisplayConfig, ServiceChannel) gets its own
blocking connection to avoid deadlocks between interfaces.

## Module Development Workflow

### Building
```sh
cargo build  # builds to compositor/target/debug/libewm_core.so
```

**Critical**: Emacs cannot hot-reload dynamic modules. Once loaded, the module
stays in memory until Emacs fully restarts. If you rebuild the module, you MUST
restart Emacs to load the new version.

### Module Loading
The module is loaded from `compositor/target/debug/libewm_core.so` relative to
`ewm.el`. Set `EWM_MODULE_PATH` environment variable to override.

### Viewing Logs
Compositor logs go to journald with identifier `ewm`:
```sh
journalctl --user -t ewm -f      # follow logs
journalctl --user -t ewm -n 100  # last 100 lines
```
Filter controlled by `RUST_LOG` env var (default: `ewm=debug,smithay=warn`).

## Reference Implementation
The compositor's DRM backend, screen sharing, and D-Bus integration follow
patterns from [niri](https://github.com/YaLTeR/niri), a Wayland compositor
with excellent documentation and clean architecture.
