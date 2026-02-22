# EWM Protocol Roadmap

This document outlines Wayland protocols to implement for broader application compatibility.

## Currently Implemented

| Protocol | Notes |
|----------|-------|
| `wl-compositor` | Core Wayland |
| `xdg-shell` | Window management (including fullscreen) |
| `xdg-decoration` | Server-side decorations |
| `wlr-layer-shell` | Panels, notifications, wallpapers |
| `wlr-screencopy` | Screenshot/recording |
| `zwp-text-input-v3` | Input method support |
| `zwp-input-method-v2` | Emacs as input method |
| `linux-dmabuf` | Efficient buffer sharing |
| `xdg-output` | Multi-monitor info |
| `xdg-activation-v1` | Focus requests from apps |
| `wlr-foreign-toplevel-v1` | Exposes windows to external tools |
| `ext-session-lock-v1` | Secure screen locking (swaylock) |
| `ext-idle-notify-v1` | Idle detection (swayidle) |
| `ext-workspace-v1` | Workspace state for panels (tab-bar integration) |
| `wlr-data-control-v1` | Clipboard access for external tools (wl-copy, cliphist) |
| `zwp-primary-selection-v1` | Primary (middle-click) selection |
| `wp-fractional-scale-v1` | Fractional output scaling (1.25x, 1.5x, etc.) |
| `wp-viewporter` | Buffer scaling/cropping |

## Priority 1: Application Compatibility

### pointer-constraints-unstable-v1

**Purpose**: Confine or lock pointer to a surface

**Enables**:
- Games (FPS mouse capture)
- 3D modeling apps
- VMs (mouse capture)

**Complexity**: Medium - Track constraints, handle edge cases

### relative-pointer-unstable-v1

**Purpose**: Relative pointer motion events (deltas, not absolute)

**Enables**:
- Games (mouse look)
- 3D apps (orbit controls)

**Status**: Partially done (events sent in input handler)

**Complexity**: Low - Already sending relative motion in DRM backend

### keyboard-shortcuts-inhibit-unstable-v1

**Purpose**: Allow clients to capture compositor shortcuts

**Enables**:
- VMs capturing all keys
- Games with conflicting shortcuts
- Remote desktop clients

**Complexity**: Low - Flag to bypass compositor key handling

### idle-inhibit-unstable-v1

**Purpose**: Prevent idle/screensaver activation

**Enables**:
- Video players preventing screen blank
- Presentation software
- Games

**Complexity**: Low - Track inhibitors, disable idle timeout when active

### ext-data-control-v1

**Purpose**: Standardized clipboard access (replaces wlr-data-control)

**Enables**:
- Future-proof clipboard manager support
- Broader compositor compatibility

**Complexity**: Low - Same pattern as wlr-data-control, already implemented

## Priority 2: Enhanced Features

### cursor-shape-v1

**Purpose**: Standard cursor shapes without client-side cursors

**Enables**:
- Consistent cursor theming
- Reduced bandwidth

**Complexity**: Low - Map shape enum to cursor images

### content-type-hint-v1

**Purpose**: Clients hint content type (video, game, etc.)

**Enables**:
- Optimized rendering paths
- VRR/adaptive sync decisions

**Complexity**: Low - Store hint, use in rendering decisions

### wlr-output-management-unstable-v1

**Purpose**: External display configuration

**Enables**:
- wdisplays, kanshi
- GUI display configuration

**Complexity**: Medium - Must coordinate with Emacs display management

**Note**: May conflict with EWM's Emacs-driven output configuration

### wlr-virtual-pointer-unstable-v1

**Purpose**: Create virtual pointer devices

**Enables**:
- Remote desktop input
- Automation tools
- Accessibility

**Complexity**: Low

## Implementation Notes

### Adding a New Protocol

1. Check if Smithay has built-in support (delegate macros)
2. Study niri's implementation as reference
3. Add state to `Ewm` struct
4. Implement handler trait
5. Add delegate macro
6. Test with relevant client

### Testing Tools

| Protocol | Test With | Status |
|----------|-----------|--------|
| foreign-toplevel | `wlrctl`, DankMaterialShell | Done |
| idle-notify | `swayidle` | Done |
| session-lock | `swaylock` | Done |
| activation | Launch apps from terminal | Done |
| data-control | `wl-copy`, `wl-paste`, `cliphist` | Done |
| workspace | waybar, DankMaterialShell | Done |
| pointer-constraints | `pointer-constraints-demo` | TODO |
| idle-inhibit | Video player, `wayland-info` | TODO |

## References

- [Wayland Protocol Registry](https://wayland.app/protocols/)
- [wlroots protocols](https://gitlab.freedesktop.org/wlroots/wlr-protocols)
- [niri source](https://github.com/YaLTeR/niri) - Reference implementation
- [Smithay protocols](https://github.com/Smithay/smithay)
