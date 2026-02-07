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

### NixOS / Nix

```bash
nix-shell
cargo build --release -p ewm
```

### Other Linux distributions

Install dependencies (names vary by distro):
- `libxkbcommon`, `libGL`, `wayland`
- `libX11`, `libXcursor`, `libXrandr`, `libXi` (for nested mode)
- `libseat`, `libinput`, `libudev`, `libdrm`, `libgbm` (for DRM mode)
- Rust toolchain

Then:
```bash
cargo build --release -p ewm
```

## Testing

### Nested Mode (inside existing Wayland/X11 session)

The easiest way to test. Run from your current desktop:

```bash
# Basic test - runs Emacs inside EWM window
cargo run -p ewm -- emacs

# With custom Emacs config
cargo run -p ewm -- emacs -Q -l /path/to/ewm.el

# Test with a simple Wayland client
cargo run -p ewm -- foot
```

EWM auto-detects nested mode when `WAYLAND_DISPLAY` or `DISPLAY` is set.

### DRM Mode (standalone TTY session)

For running EWM as your actual Wayland compositor:

1. Switch to a TTY (`Ctrl+Alt+F2`)
2. Ensure you're in the `video` and `input` groups
3. Start a seat daemon if not running (`seatd` or via systemd-logind)
4. Run:

```bash
# Make sure WAYLAND_DISPLAY and DISPLAY are unset
unset WAYLAND_DISPLAY DISPLAY

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
