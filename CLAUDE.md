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
- IPC uses JSON over Unix socket at `/tmp/ewm.sock`

### Emacs Lisp
- Use character literals for keys: `?\C-x` not `"C-x"`
- Prefer Emacs built-ins (e.g., `key-parse`) over custom parsing
- Keep ewm.el compatible with both EWM and regular Emacs sessions

## Key Design Decisions
- Emacs sends pre-parsed keysyms to compositor (not string notation)
- Super-key bindings are auto-detected from Emacs keymaps
- `ewm-connect` is safe to call unconditionally (warns if socket missing)
