# EWM - Emacs Wayland Manager

> **DISCLAIMER**: This project was vibe-coded together with Claude Code over a weekend as an experiment to see how far I could progress without getting drawn into the details. As a long-time EXWM user, the idea of having a modern Wayland-based EXWM implementation intrigued me a lot, but not enough to spend months implementing everything by hand. With that in mind, I assume a large portion of this code likely requires a proper rewrite by a developer who actually understands Wayland.

## Video Demo

https://github.com/user-attachments/assets/d1c19772-532e-4b99-8c21-0ecba6e598c5

## What is EWM?

EWM brings EXWM-like workflows to Wayland. Wayland applications appear as Emacs buffers, letting you switch between code and apps with `C-x b`, organize windows with your familiar Emacs commands, and keep everything responsive even when Emacs is busy evaluating.

The key difference from EXWM: the compositor runs as a separate thread within Emacs (via a dynamic module), so applications never freeze waiting for Elisp evaluation.

```
┌─ Emacs Process ────────────────────────────────────────────┐
│  Main Thread: Elisp execution                              │
│       ↑↓ shared memory, mutex-protected                    │
│  Compositor Thread: Smithay (Rust dynamic module)          │
│       ↑↓                                                   │
│  Render Thread: DRM/GPU                                    │
└────────────────────────────────────────────────────────────┘
```

## Inspirations

- **[EXWM](https://github.com/ch11ng/exwm)** (Emacs side): Buffer-per-window model, prefix key interception, line/char mode switching, automatic focus management
- **[niri](https://github.com/YaLTeR/niri)** (Compositor side): Backend architecture, Smithay patterns, DRM abstraction

## Building

```bash
cd compositor
cargo build  # builds to compositor/target/debug/libewm_core.so
```

## Running

From a TTY (not inside an existing Wayland/X11 session):

```bash
emacs --fg-daemon
# Then in Emacs: M-x ewm-start-module
```

Or start apps in the compositor:
```bash
WAYLAND_DISPLAY=wayland-ewm foot
```

**Kill combo**: `Super+Shift+E` exits the compositor.

## NixOS Setup

EWM provides a NixOS module for easy deployment. Import the module and configure it:

```nix
# configuration.nix
{ pkgs, ... }:

{
  imports = [ /path/to/ewm/nix/service.nix ];

  programs.ewm = {
    enable = true;
    emacsPackage = pkgs.emacs30-pgtk;
    initDirectory = /etc/nixos/dotfiles/emacs;
  };
}
```

The module registers an `ewm` session with your display manager (e.g., ly, gdm).
Select "EWM" at login to start the compositor.

Module options:
- `emacsPackage`: Your Emacs package (default: `pkgs.emacs`)
- `initDirectory`: Path to your Emacs config directory
- `screencast.enable`: Enable screen sharing via PipeWire (default: true)

## Emacs Setup

Load `ewm.el` in your Emacs:

```elisp
(use-package ewm
  :load-path "~/git/ewm/lisp"
  :custom
  ;; Optional: configure output modes
  (ewm-output-config '(("DP-1" :width 2560 :height 1440)))
  :bind
  ;; Super-key bindings are auto-detected by EWM
  ("s-d" . consult-buffer)
  ("s-<left>" . windmove-left)
  ("s-<right>" . windmove-right))
```

Then start the compositor with `M-x ewm-start-module`.

Unlike EXWM's `exwm-input-global-keys`, you don't need separate configuration.
Just use normal `:bind` or `global-set-key` - EWM scans your keymaps and
automatically intercepts keys with the super modifier.

When the compositor starts, Wayland surfaces appear as special buffers. Use standard Emacs commands:
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
- DRM backend with multi-monitor support (hotplug, per-output Emacs frames)
- Layer-shell protocol (waybar, notifications, etc.)
- Screen sharing via xdg-desktop-portal (PipeWire DMA-BUF)
- Input method support (type in any script via Emacs input methods)

## Known Limitations

- No screen locking (ext-session-lock-v1)
- GPU selection is automatic (no override)
- Must run from TTY (no nested mode)

## License

GPL-3.0
