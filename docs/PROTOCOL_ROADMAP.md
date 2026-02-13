# EWM Protocol Roadmap

This document outlines Wayland protocols to implement for broader application compatibility.

## Currently Implemented

| Protocol | Status | Notes |
|----------|--------|-------|
| `wl-compositor` | Done | Core Wayland |
| `xdg-shell` | Done | Window management |
| `xdg-decoration` | Done | Server-side decorations |
| `wlr-layer-shell` | Done | Panels, notifications, wallpapers |
| `wlr-screencopy` | Done | Screenshot/recording |
| `zwp-text-input-v3` | Done | Input method support |
| `zwp-input-method-v2` | Done | Emacs as input method |
| `linux-dmabuf` | Done | Efficient buffer sharing |
| `xdg-output` | Done | Multi-monitor info |
| `xdg-activation-v1` | Done | Focus requests from apps |
| `wlr-foreign-toplevel-v1` | Done | Exposes windows to external tools |
| `ext-session-lock-v1` | Done | Secure screen locking (swaylock) |
| `ext-idle-notify-v1` | Done | Idle detection (swayidle) |

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

## Priority 2: Enhanced Features

### fractional-scale-v1

**Purpose**: Non-integer scale factors (1.25x, 1.5x, etc.)

**Enables**:
- Better HiDPI support
- Per-monitor scaling

**Complexity**: Medium - Requires careful coordinate handling

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

## Priority 3: Workspace Protocols

These are lower priority since EWM uses Emacs for workspace management.

### ext-workspace-v1 (proposed)

**Purpose**: Standard workspace protocol

**Status**: Not yet standardized, compositor-specific alternatives exist

**Note**: EWM's workspace model is fundamentally different (Emacs buffers/windows)

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
| foreign-toplevel | `wlrctl`, DankMaterialShell | ✓ |
| idle-notify | `swayidle` | ✓ |
| session-lock | `swaylock` | ✓ |
| activation | Launch apps from terminal | ✓ |
| pointer-constraints | `pointer-constraints-demo` | TODO |
| idle-inhibit | Video player, `wayland-info` | TODO |

## References

- [Wayland Protocol Registry](https://wayland.app/protocols/)
- [wlroots protocols](https://gitlab.freedesktop.org/wlroots/wlr-protocols)
- [niri source](https://github.com/YaLTeR/niri) - Excellent reference implementation
- [Smithay protocols](https://github.com/Smithay/smithay)
