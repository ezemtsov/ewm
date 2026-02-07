# EWM - Emacs Wayland Manager

> **DISCLAIMER**: This project was vibe-coded together with Claude Code over a weekend as an experiment to see how far I could progress without getting drawn into the details. As a long-time EXWM user, the idea of having a modern Wayland-based EXWM implementation intrigued me a lot, but not enough to spend months implementing everything by hand. With that in mind, I assume a large portion of this code likely requires a proper rewrite by a developer who actually understands Wayland.

## What is EWM?

EWM brings EXWM-like workflows to Wayland. Wayland applications appear as Emacs buffers, letting you switch between code and apps with `C-x b`, organize windows with your familiar Emacs commands, and keep everything responsive even when Emacs is busy evaluating.

The key difference from EXWM: the compositor runs as a separate process, so applications never freeze waiting for Emacs.

```
┌─────────────────────────────────────────────────────┐
│  Compositor (Rust)  ◄───Unix Socket───►  Emacs      │
│  - Renders surfaces                    - Controls   │
│  - Handles input                         layout     │
│  - Manages Wayland                     - Buffer     │
│    protocol                              per app    │
└─────────────────────────────────────────────────────┘
```

## Inspirations

- **[EXWM](https://github.com/ch11ng/exwm)** (Emacs side): Buffer-per-window model, prefix key interception, line/char mode switching, automatic focus management
- **[niri](https://github.com/YaLTeR/niri)** (Compositor side): Backend architecture, Smithay patterns, DRM/Winit abstraction

## Building

```bash
cd compositor
cargo build --release
```

## Running

```bash
# Nested mode (inside existing Wayland/X11)
./target/release/ewm emacs

# DRM mode (from TTY, unset WAYLAND_DISPLAY first)
./target/release/ewm emacs
```

**Kill combo**: `Super+Ctrl+Backspace` exits the compositor.

## Emacs Setup

Load `ewm.el` in your Emacs:

```elisp
(load "/path/to/ewm/ewm.el")
```

When Emacs connects to the compositor, Wayland surfaces appear as special buffers. Use standard Emacs commands:
- `C-x b` - switch between apps and regular buffers
- `C-x 2`, `C-x 3` - split windows (surfaces follow)
- `C-x 0`, `C-x 1` - close/maximize windows

## Current Features

- Wayland surfaces as Emacs buffers
- Automatic layout synchronization
- Multi-view rendering (same surface in multiple windows)
- Prefix key interception (compositor forwards to Emacs)
- Line/char mode (like EXWM)
- Client-side decoration auto-disable
- Both nested (Winit) and standalone (DRM) backends

## Known Limitations

- No multi-monitor support yet
- No layer-shell protocol (waybar, etc.)
- No screen locking
- Input method support is basic
- GPU selection is automatic (no override)

## License

GPL-3.0
