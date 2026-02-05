# EWM - Emacs Wayland Manager

EXWM-like workflow for Wayland. Compositor that doesn't freeze.

**Status**: Design phase

## What

- Wayland apps appear as Emacs buffers
- `C-x b` switches between code and apps
- Apps stay responsive even when Emacs is busy

## How

Separate compositor process talks to Emacs via socket.
Emacs controls layout. Compositor handles rendering.

```
Compositor (Rust) ◄──socket──► Emacs (unmodified)
```

## Philosophy

Keep it tiny. Keep it simple. Validate everything.

See [VISION.md](VISION.md) for details.

## License

GPL-3.0
