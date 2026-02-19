# EWM - Emacs Wayland Manager

> **Disclaimer**: This project was vibe-coded together with Claude Code over a week as an experiment to see how far I could progress without getting drawn into the details. As a long-time EXWM user, the idea of having a modern Wayland-based EXWM implementation intrigued me a lot, but not enough to spend months implementing everything by hand. With that in mind, please expect possible issues, the project is in active development.

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

## Requirements

- **Emacs with pgtk**: Must be built with the pure GTK (pgtk) backend for native Wayland support. On NixOS use `emacs-pgtk` or `emacs30-pgtk`.
- **Mesa/EGL**: Graphics drivers providing `libEGL.so.1`. On NixOS, enable `hardware.graphics.enable = true`.
- **Run from TTY**: Must launch from a virtual terminal, not inside an existing Wayland/X11 session.

## Building

```bash
cd compositor
cargo build  # builds to compositor/target/debug/libewm_core.so
```

## Running

From a TTY (not inside an existing Wayland/X11 session):

```bash
emacs --fg-daemon -Q -L ~/git/ewm/lisp -l ewm --eval "(ewm-start-module)"
```

The `--fg-daemon` flag is required because EWM creates frames dynamically as
outputs are discovered. Starting Emacs without initial frames ensures uniform
handling for all monitors and proper multi-monitor support.

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

  programs.ewm.enable = true;
}
```

The module registers an `ewm` session with your display manager (e.g., ly, gdm).
Select "EWM" at login to start the compositor. Emacs loads your normal init
directory (`~/.emacs.d` or `~/.config/emacs`).

To use a custom init directory, different Emacs version, or add extra packages:

```nix
programs.ewm = {
  enable = true;
  extraEmacsArgs = "--init-directory /etc/nixos/dotfiles/emacs";
  emacsPackage = pkgs.emacs30-pgtk.pkgs.withPackages (epkgs: [
    config.programs.ewm.ewmPackage
    epkgs.consult
  ]);
};
```

Module options:
- `emacsPackage`: Emacs with EWM included (default: `pkgs.emacs-pgtk` with EWM package)
- `extraEmacsArgs`: Additional Emacs CLI arguments (e.g., `"--no-site-lisp"`)
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

## Configuration

### Output

Configure display modes and positions via `ewm-output-config`:

```elisp
(setq ewm-output-config
      '(("DP-1" :width 2560 :height 1440 :scale 1.0)
        ("eDP-1" :width 1920 :height 1200 :scale 1.25 :x 0 :y 0)))
```

| Property     | Type    | Description                                            |
|--------------|---------|--------------------------------------------------------|
| `:width`     | integer | Horizontal resolution in pixels                        |
| `:height`    | integer | Vertical resolution in pixels                          |
| `:refresh`   | integer | Refresh rate in Hz                                     |
| `:x`         | integer | Horizontal position in global coordinate space         |
| `:y`         | integer | Vertical position in global coordinate space           |
| `:scale`     | float   | Fractional scale (e.g. 1.25, 1.5, 2.0)                |
| `:transform` | integer | 0=Normal 1=90 2=180 3=270 4=Flipped 5-7=Flipped+rot   |
| `:enabled`   | boolean | Whether the output is enabled (default t)              |

### Touchpad

Configure via `ewm-touchpad-config`. All properties are optional; omitted
properties use the device default.

```elisp
(setq ewm-touchpad-config
      '(:natural-scroll t :tap t :dwt t))
```

| Property            | Type   | Description                                         |
|---------------------|--------|-----------------------------------------------------|
| `:natural-scroll`   | bool   | Invert scroll direction (content follows fingers)   |
| `:tap`              | bool   | Tap-to-click                                        |
| `:dwt`              | bool   | Disable touchpad while typing                       |
| `:accel-speed`      | float  | Pointer acceleration, -1.0 (slowest) to 1.0 (fastest) |
| `:accel-profile`    | string | `"flat"` (linear) or `"adaptive"` (acceleration curve) |
| `:click-method`     | string | `"button-areas"` or `"clickfinger"`                 |
| `:scroll-method`    | string | `"two-finger"`, `"edge"`, `"on-button-down"`, `"no-scroll"` |
| `:left-handed`      | bool   | Swap left/right buttons                             |
| `:middle-emulation` | bool   | Emulate middle button from simultaneous L+R click   |
| `:tap-button-map`   | string | `"left-right-middle"` or `"left-middle-right"`      |

### Mouse

Configure via `ewm-mouse-config`. Same defaults-if-omitted behavior.

```elisp
(setq ewm-mouse-config
      '(:accel-profile "flat"))
```

| Property            | Type   | Description                                         |
|---------------------|--------|-----------------------------------------------------|
| `:natural-scroll`   | bool   | Invert scroll direction                             |
| `:accel-speed`      | float  | Pointer acceleration, -1.0 to 1.0                   |
| `:accel-profile`    | string | `"flat"` or `"adaptive"`                            |
| `:scroll-method`    | string | `"two-finger"`, `"edge"`, `"on-button-down"`, `"no-scroll"` |
| `:left-handed`      | bool   | Swap left/right buttons                             |
| `:middle-emulation` | bool   | Emulate middle button from simultaneous L+R click   |

All input settings take effect immediately when set (via `customize-variable`
or `setq` + `ewm--send-touchpad-config` / `ewm--send-mouse-config`). New
devices receive the current configuration on hotplug.

## Current Features

- Wayland surfaces as Emacs buffers
- Automatic layout synchronization
- Per-output declarative layout (surfaces can span multiple outputs)
- Prefix key interception (compositor forwards to Emacs)
- Line/char mode (like EXWM)
- Client-side decoration auto-disable
- DRM backend with multi-monitor support (hotplug, per-output Emacs frames)
- Layer-shell protocol (waybar, notifications, etc.)
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

## License

GPL-3.0
