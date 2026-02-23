# EWM - Emacs Wayland Manager

![EWM Screenshot](docs/ewm-showcase.png)

> **Disclaimer**: Writing a Wayland compositor from scratch is a staggering amount of work. I wanted to switch from EXWM as soon as possible, so the initial bootstrap was done with Claude, which helped a lot in reverse-engineering the brilliant [niri](https://github.com/YaLTeR/niri) codebase and surgically extracting the pieces relevant to EWM. Inherently, this means the codebase still needs validation and cleanup, which is the current priority.

## What is EWM?

EWM is a Wayland compositor that runs inside Emacs. Wayland applications appear as Emacs buffers, letting you switch between code and apps with `C-x b`, organize windows with your familiar Emacs commands, and keep everything responsive even when Emacs is busy evaluating.

The compositor runs as a separate thread within Emacs (via a dynamic module), so applications never freeze waiting for Elisp evaluation.

```
┌─ Emacs Process ────────────────────────────────────────────┐
│  Main Thread: Elisp execution                              │
│       ↑↓ shared memory, mutex-protected                    │
│  Compositor Thread: Smithay (Rust dynamic module)          │
│       ↑↓                                                   │
│  Render Thread: DRM/GPU                                    │
└────────────────────────────────────────────────────────────┘
```

## Requirements

- **Emacs with pgtk**: Must be built with the pure GTK (pgtk) backend for native Wayland support. On NixOS use `emacs-pgtk` or `emacs30-pgtk`.
- **Mesa/EGL**: Graphics drivers providing `libEGL.so.1`. On NixOS, enable `hardware.graphics.enable = true`.
- **Run from TTY**: Must launch from a virtual terminal, not inside an existing Wayland/X11 session.

## Running from Source

Build the compositor module and launch Emacs from a TTY (not inside an existing
Wayland/X11 session):

```bash
cd compositor
cargo build --features=screencast
EWM_MODULE_PATH=$(pwd)/target/debug/libewm_core.so \
  emacs --fg-daemon -L ../lisp -l ewm -f ewm-start-module
```

`--fg-daemon` is required — EWM creates frames dynamically as outputs are
discovered, so Emacs must start without initial frames. Add `--init-directory`
to point at your config if it's not in the default location.

Launch applications with `s-d` (`ewm-launch-app`). **Kill combo**: `Super+Shift+E`.

## NixOS Setup

See the [wiki](https://codeberg.org/ezemtsov/ewm/wiki/NixOS) for NixOS module configuration.

## Emacs Setup

Load `ewm.el` in your Emacs:

```elisp
(use-package ewm
  :custom
  ;; Optional: configure output modes
  (ewm-output-config '(("DP-1" :width 2560 :height 1440)))
  :bind (:map ewm-mode-map
         ;; Override defaults with your preferred commands
         ("s-d" . consult-buffer)))
```

Then start the compositor with `M-x ewm-start-module`.

All bindings in `ewm-mode-map` are automatically intercepted by the compositor —
no separate keybinding configuration is needed. See the [keyboard wiki page](https://codeberg.org/ezemtsov/ewm/wiki/Keyboard) for default bindings and customization.

When the compositor starts, Wayland surfaces appear as special buffers. Use standard Emacs commands:
- `C-x b` — switch between apps and regular buffers
- `C-x 2`, `C-x 3` — split windows (surfaces follow)
- `C-x 0`, `C-x 1` — close/maximize windows

## Configuration

See the [wiki](https://codeberg.org/ezemtsov/ewm/wiki) for configuration options.

## Current Features

- Wayland surfaces as Emacs buffers
- Automatic layout synchronization
- Per-output declarative layout (surfaces can span multiple outputs)
- Per-window fullscreen (XDG fullscreen protocol, `s-f` toggle)
- Prefix key interception (compositor forwards to Emacs)
- Client-side decoration auto-disable
- DRM backend with multi-monitor support (hotplug, per-output Emacs frames)
- Lid close/open handling (laptop panel off when external monitor connected)
- Layer-shell protocol (waybar, notifications, etc.)
- Workspace protocol (ext-workspace-v1, tab-bar integration)
- Screen sharing via xdg-desktop-portal (PipeWire DMA-BUF)
- Input method support (type in any script via Emacs input methods)
- Clipboard integration with Emacs kill-ring as central hub
- Screen locking via ext-session-lock-v1 (swaylock)
- Idle notification via ext-idle-notify-v1 (swayidle)
- XDG activation (focus requests from apps)
- Fractional output scaling (1.25x, 1.5x, etc.)

## Known Limitations

- GPU selection is automatic (no override)
- Must run from TTY (no nested mode)

## Related Projects

- **[ewm-consult](https://github.com/Kirth/ewm-consult)** - Consult integration for EWM

## Inspirations

- **[EXWM](https://github.com/ch11ng/exwm)** (Emacs side): Buffer-per-window model, prefix key interception, automatic focus management
- **[niri](https://github.com/YaLTeR/niri)** (Compositor side): Backend architecture, Smithay patterns, DRM abstraction

## License

GPL-3.0
