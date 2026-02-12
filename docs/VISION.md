# EWM - Emacs Wayland Manager

A Wayland compositor designed to provide an EXWM-like experience while preserving
the entire Emacs ecosystem and solving Emacs's single-threaded limitations.

## Development Philosophy

**Keep it tiny. Keep it simple. Validate everything.**

1. **Minimal code** - Every line must earn its place
2. **Lean development** - Start with the most fundamental feature, validate, iterate
3. **No premature optimization** - Make it work first, measure before optimizing
4. **Minimal dependencies** - Each dependency is a liability
5. **Elegant simplicity** - Take solutions that simplify overall architecture
6. **Validate assumptions** - Before building, ask: "How do we know this is needed?"

## The Problem

EXWM provides a unique workflow where X11 windows appear as Emacs buffers.
However, EXWM has fundamental limitations:

1. **X11 only** - Wayland is the future
2. **Single-threaded** - Emacs's event loop blocks everything
3. **UI freezes** - Long-running Elisp freezes all windows

## The Vision

**Wayland surfaces as first-class Emacs buffers**, with:
- Full Emacs ecosystem preserved (magit, org-mode, consult, vertico, etc.)
- Responsive compositor (never freezes, even during Elisp execution)
- EXWM-like keybindings and workflow

## Architecture

### Current: Dynamic Module Architecture

The compositor runs as an Emacs dynamic module (`ewm-core.so`):

```
┌─ Emacs Process ────────────────────────────────────────────┐
│                                                             │
│  Main Thread: Elisp execution                              │
│       ↑↓ shared memory, mutex-protected                    │
│  Compositor Thread: Smithay (Rust dynamic module)          │
│       ↑↓                                                    │
│  Render Thread: DRM/GPU                                    │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

Benefits:
- Single source of truth for state (no sync bugs)
- Low latency (no IPC round-trips)
- Synchronous API calls eliminate race conditions

### Startup Flow

```
1. emacs --fg-daemon (headless, no display needed)
2. (require 'ewm) loads ewm-core.so module
3. Module initializes DRM, creates Wayland display
4. Output detected → Emacs creates frame on that output
```

## Goals

### The Core Goal

**Use Emacs + Wayland apps together seamlessly, with compositor that doesn't freeze.**

### What "Done" Looks Like

- Run `foot` (terminal) and Firefox alongside Emacs
- Switch between them with `C-x b`
- Tile them with `C-x 2`, `C-x 3`
- Type in them, click in them
- Run `(sleep-for 10)` in Emacs and apps still respond

### What We're NOT Building

- A general-purpose compositor (use Sway for that)
- X11 support (Wayland only)
- A modified Emacs (works with stock pgtk Emacs)
- Features we haven't needed yet

## Completed Features

| Feature | Status |
|---------|--------|
| Keyboard/mouse input | ✓ |
| Bidirectional focus sync | ✓ |
| Multi-monitor with hotplug | ✓ |
| DRM and Winit backends | ✓ |
| Input method support | ✓ |
| Dynamic module integration | ✓ |
| Screen sharing (PipeWire) | ✓ |

## User Experience

```bash
$ emacs --fg-daemon      # Starts Emacs + compositor via module
```

Then in Emacs:
```
C-x b foot RET           # Switch to terminal
C-x b *scratch* RET      # Switch back to Emacs buffer
C-x 3                    # Split
C-x b foot RET           # Terminal on the side
```

## Relationship to EXWM

**Guiding principle**: Inspired by EXWM's patterns, not adapting its codebase.

We analyzed EXWM's ~9,400 lines and found deep X11 coupling (1,239 xcb calls).
EWM reimplements the workflow with ~200 lines of Elisp wrapping a Rust compositor.

| EXWM Pattern | EWM Implementation |
|--------------|-------------------|
| Buffer-local window tracking | `ewm-surface-id`, `ewm--surfaces` hash table |
| Layout refresh on changes | `window-configuration-change-hook` |
| Kill buffer → close window | `kill-buffer-query-functions` hook |

## Open Questions

We'll answer these when we need to:
- XWayland? (for legacy X11 apps)
- Popups? (menu positioning, tooltips)
