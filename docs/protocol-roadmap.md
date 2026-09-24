# EWM Protocol Roadmap

This document outlines Wayland protocols to implement for broader application compatibility.

## Currently Implemented

| Protocol | Notes |
|----------|-------|
| `wl-compositor` | Core Wayland |
| `xdg-shell` | Window management (including fullscreen) |
| `xdg-decoration` | Server-side decorations |
| `kde-server-decoration` | Server-side decorations for GTK clients |
| `wlr-layer-shell` | Panels, notifications, wallpapers |
| `wlr-screencopy` | Screenshot/recording |
| `zwp-text-input-v3` | Input method support |
| `zwp-input-method-v2` | Emacs as input method |
| `linux-dmabuf` | Efficient buffer sharing |
| `xdg-output` | Multi-monitor info |
| `xdg-activation-v1` | Focus requests from apps |
| `xdg-system-bell-v1` | System bell, forwarded to Emacs via `ewm-bell-function` |
| `wlr-foreign-toplevel-manager-v1` | Allows external tools to manage windows |
| `ext-foreign-toplevel-list-v1` | Allows external tools to list windows |
| `ext-session-lock-v1` | Secure screen locking (swaylock) |
| `ext-idle-notify-v1` | Idle detection (swayidle) |
| `zwp-idle-inhibit-v1` | Idle inhibition (video players, games) |
| `ext-workspace-v1` | Workspace state for panels (tab-bar integration) |
| `wlr-data-control-v1` | Clipboard access for external tools (wl-copy, cliphist) |
| `zwp-primary-selection-v1` | Primary (middle-click) selection |
| `wp-fractional-scale-v1` | Fractional output scaling (1.25x, 1.5x, etc.) |
| `wp-viewporter` | Buffer scaling/cropping |
| `wlr-gamma-control-unstable-v1` | Manage gamma tables of outputs |
| `wlr-output-management-unstable-v1` | External display configuration (wlr-randr, wdisplays, DankMaterialShell) |
| `ext-background-effect-v1` | Blur behind surfaces that request it |
| `wl_data_device` | Drag-and-drop (icon rendering, data transfer between clients) |

## Priority 1: Application Compatibility

### xdg-toplevel-drag-v1

**Purpose**: Associate a toplevel window with an ongoing DnD drag operation

**Enables**:
- Browser tab detach-and-reattach (Chromium supports this, Firefox planned)
- Detachable panels, sidebars, and tool windows
- Distinguishing "new surface from drag" vs "new surface from launch":
  Emacs can split the window for dragged surfaces while replacing the
  buffer for explicitly launched apps

**Why we need it**: Without this protocol, browsers detach tabs over
plain `wl_data_device`: Firefox opens a new window when the drop is
rejected, Chromium (133+) keeps the torn-off window hidden during the
drag and shows it when the drag ends. Both work today: Emacs rejects the
tab's mime types, and a drop focuses the Emacs window under the pointer
like a click, so the new window lands there. What is missing is the live
window following the cursor and an explicit association between the drag
and the new toplevel.

**Status**: Chromium supports it client-side; Firefox does not (GTK3,
Bugzilla 1955820). Smithay has no compositor-side handler yet (tracked
in Smithay#781). Implementation requires building the handler against
raw `wayland-protocols` bindings, similar to our `screencopy` and
`output-management` custom protocols.

**Complexity**: Medium. Intercept DnD grab, bind toplevel to drag
session, position toplevel at pointer during drag, handle drop/cancel

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

**Status**: Implemented

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
| wlr-foreign-toplevel-manager | `wlrctl`, DankMaterialShell | Done |
| ext-foreign-toplevel-list | `lswt` | Done |
| idle-notify | `swayidle` | Done |
| session-lock | `swaylock` | Done |
| activation | Launch apps from terminal | Done |
| system-bell | `printf '\a'` in foot or kitty | Done |
| data-control | `wl-copy`, `wl-paste`, `cliphist` | Done |
| workspace | waybar, DankMaterialShell | Done |
| output-management | `wlr-randr`, DankMaterialShell | Done |
| data-device (DnD) | Nemo file drag, Firefox tab drag | Done |
| pointer-constraints | `pointer-constraints-demo` | TODO |
| idle-inhibit | Video player, `wayland-info` | Done |
| xdg-toplevel-drag | Chromium live tab drag | TODO |

## References

- [Wayland Protocol Registry](https://wayland.app/protocols/)
- [wlroots protocols](https://gitlab.freedesktop.org/wlroots/wlr-protocols)
- [niri source](https://github.com/YaLTeR/niri) - Reference implementation
- [Smithay protocols](https://github.com/Smithay/smithay)
