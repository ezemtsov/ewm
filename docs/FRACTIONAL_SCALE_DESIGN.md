# Fractional Scale Design

## Overview

EWM supports fractional output scaling (1.25x, 1.5x, etc.) for HiDPI displays via
the `wp-fractional-scale-v1` Wayland protocol. The implementation follows
[niri](https://github.com/YaLTeR/niri)'s patterns: explicit output association
managed by the layout system, scale notification at surface lifecycle points, and
an empty `FractionalScaleHandler` with scale pushed from lifecycle handlers.

## Architecture

### Output Configuration

Output config separates **desired configuration** from **runtime state**, following
niri's pattern. Config is stored per output name and re-applied on hot-plug.

```elisp
;; Declarative (applied on connect/reconnect):
(setq ewm-output-config
      '(("DP-1" :width 2560 :height 1440 :scale 1.5)
        ("eDP-1" :width 1920 :height 1200 :x 0 :y 0 :transform 0)))

;; Programmatic (applied immediately):
(ewm-configure-output "DP-1" :scale 1.5 :transform 0)
```

Key types and flow:

```
Emacs ─── ewm-configure-output ───► ConfigureOutput command
                                         │
                                         ▼
                                    output_config.insert(name, config)
                                         │
                                         ▼
                                    Backend::apply_output_config()
                                     ├─► DrmBackendState::apply_output_config()
                                     └─► HeadlessBackend::apply_output_config()
                                         │
                                         ▼
                                    output.change_current_state(mode, transform, scale, position)
                                         │
                                    ┌────┴────┐
                                    ▼         ▼
                              Notify        Notify
                              surfaces      Emacs
```

- `OutputConfig` struct stores mode, position, scale, transform, enabled
- `Ewm.output_config: HashMap<String, OutputConfig>` persists across connect/disconnect
- `connect_output()` (DRM) and `add_output()` (headless) look up stored config on connect
- `OutputInfo` event includes `scale` and `transform` so Emacs knows the applied state

### Scale Precision

The `wp-fractional-scale-v1` protocol represents scale as N/120. All configured scales
are rounded to this precision before applying:

```rust
pub fn closest_representable_scale(scale: f64) -> f64 {
    const FRACTIONAL_SCALE_DENOM: f64 = 120.0;
    (scale * FRACTIONAL_SCALE_DENOM).round() / FRACTIONAL_SCALE_DENOM
}
```

Common values: 1.0 = 120/120, 1.25 = 150/120, 1.5 = 180/120, 2.0 = 240/120.
Arbitrary values like 1.3333 become 160/120 = 1.33333... which round-trips correctly.

### Coordinate Helpers

Fractional scales require precise coordinate conversion to avoid pixel gaps:

```rust
// Logical → physical with proper rounding (avoids truncation artifacts)
pub fn to_physical_precise_round<N: Coordinate>(scale: f64, logical: impl Coordinate) -> N {
    N::from_f64((logical.to_f64() * scale).round())
}

// Snap a logical value to the nearest physical pixel boundary
pub fn round_logical_in_physical(scale: f64, logical: f64) -> f64 {
    (logical * scale).round() / scale
}

// Logical output dimensions accounting for fractional scale and transform
pub fn output_size(output: &Output) -> Size<f64, Logical> {
    let scale = output.current_scale().fractional_scale();
    let transform = output.current_transform();
    let mode = output.current_mode().unwrap();
    transform.transform_size(mode.size.to_f64().to_logical(scale))
}
```

Used in cursor positioning (replaces cast-to-i32), lock buffer sizing, and working
area calculations. Without `to_physical_precise_round`, the cursor renders with
sub-pixel misalignment at non-integer scales.

## Scale Notification

### The Two-Protocol Problem

Wayland has two scale mechanisms that must both be sent:

1. **`wl_surface.preferred_buffer_scale`** (wl_compositor v6+): Integer scale.
   Clients use this for `wl_surface.set_buffer_scale`. Legacy clients that don't
   support fractional scaling rely solely on this.

2. **`wp_fractional_scale_v1.preferred_scale`**: Fractional scale as N/120.
   Modern clients (Firefox, GTK4, Qt6) use this for CSS DPR and precise rendering.

Both are sent via `send_scale_transform()`:

```rust
pub fn send_scale_transform(
    surface: &WlSurface,
    data: &SurfaceData,
    scale: output::Scale,
    transform: Transform,
) {
    // Integer scale (wl_compositor v6 preferred_buffer_scale)
    send_surface_state(surface, data, scale.integer_scale(), transform);
    // Fractional scale (wp-fractional-scale-v1 preferred_scale)
    with_fractional_scale(data, |fractional| {
        fractional.set_preferred_scale(scale.fractional_scale());
    });
}
```

**Important**: `CompositorState::new_v6` (not `new`) is required to advertise
wl_compositor version 6, which enables `preferred_buffer_scale` events. Without
this, Emacs (which uses `wl_surface.preferred_buffer_scale` for its own scaling)
renders blurry at fractional scales.

### Notification Sites

Scale is sent at every surface lifecycle point:

| Lifecycle Event | Function | What Happens |
|----------------|----------|-------------|
| Toplevel created | `handle_new_toplevel()` | `send_scale_transform` + direct `output.enter()` |
| Subsurface created | `new_subsurface()` | `propagate_preferred_scale` from root surface |
| Popup initial commit | `commit()` | `output_for_popup` → `send_scale_transform` |
| Layer surface created | `new_layer_surface()` | `send_scale_transform` with output's scale |
| Lock surface created | `configure_lock_surface()` | `send_scale_transform` with output's scale |
| Output config changed | `apply_output_config()` | `send_scale_transform_to_output_surfaces()` |
| Window repositioned | Layout command | `enter_output_for_position()` (output enter/leave) |

### FractionalScaleHandler

The handler is intentionally empty (matching niri):

```rust
impl FractionalScaleHandler for State {}
delegate_fractional_scale!(State);
```

Scale is not sent in `new_fractional_scale` because that callback fires when the
client binds `get_fractional_scale` on a surface — at that point, Smithay
automatically sends any `preferred_scale` already stored in the surface's data map.
The lifecycle handlers above ensure the value is stored before the client binds.

## Output Association

### Why Not `space.refresh()`

Smithay's `Space::refresh()` iterates ALL elements against ALL outputs, calling
`SpaceElement::output_leave` for non-overlapping elements. This is the call chain:

```
space.refresh()
  → Window::refresh()           (SpaceElement trait)
    → output_update(output, overlap, surface)
      → if overlap.is_none():
          output.leave(wl_surface)     ← sends wl_surface.leave
```

This is destructive for EWM because Emacs manages layout: windows start at
(-10000, -10000) before Emacs positions them, and hidden windows return there.
`space.refresh()` would call `output.leave()` for every off-screen window, removing
its output association and preventing fractional scale from working.

**Niri's approach**: Windows are NOT in the global space. Output association is
managed explicitly by the workspace/column layout system. `global_space.refresh()`
only tracks outputs (monitors), not windows.

**EWM's approach**: Windows stay in `Space` for hit-testing and rendering, but output
association is managed explicitly. `space.refresh()` is replaced with
`cleanup_dead_windows()` which only removes dead elements:

```rust
pub fn cleanup_dead_windows(&mut self) {
    let dead: Vec<Window> = self.space.elements()
        .filter(|w| !w.alive()).cloned().collect();
    for w in dead {
        self.space.unmap_elem(&w);
    }
}
```

### Explicit Output Enter/Leave

Output association happens at two points:

**1. Window creation** (`handle_new_toplevel`): Direct `Output::enter` before the
client's first commit. Cannot use `SpaceElement::output_enter` because that calls
`output_update` which sends `output.leave()` for uncommitted surfaces.

```rust
if let Some(output) = scale_output {
    let scale = output.current_scale();
    let transform = output.current_transform();
    window.with_surfaces(|surface, data| {
        send_scale_transform(surface, data, scale, transform);
    });
    if let Some(surface) = window.wl_surface() {
        output.enter(&surface);
    }
}
```

**2. Layout positioning** (Layout command): When Emacs positions a window,
`enter_output_for_position` sends enter/leave based on the window's coordinates:

```rust
pub fn enter_output_for_position(&self, window: &Window, pos: (i32, i32)) {
    let Some(surface) = window.wl_surface() else { return; };
    let point = Point::from(pos);
    for output in self.space.outputs() {
        let geo = self.space.output_geometry(output).unwrap_or_default();
        if geo.contains(point) {
            output.enter(&surface);
        } else {
            output.leave(&surface);
        }
    }
}
```

## Child Surface Scale Propagation

Subsurfaces and xdg_popups are separate `wl_surface` objects that don't automatically
inherit scale from their parent. Each needs explicit scale notification.

### Subsurfaces: `propagate_preferred_scale`

Firefox creates `wp_fractional_scale_v1` on a **subsurface**, not the toplevel.
Without `preferred_scale` on the subsurface, Firefox's CSS DPR defaults to 1.

The `CompositorHandler::new_subsurface` callback and popup handling both use
`propagate_preferred_scale()` — a shared helper that walks from parent to root
surface and copies the stored `preferred_scale`:

```rust
pub fn propagate_preferred_scale(surface: &WlSurface, parent: &WlSurface) {
    let mut root = parent.clone();
    while let Some(p) = get_parent(&root) {
        root = p;
    }
    let root_scale = with_states(&root, |data| {
        with_fractional_scale(data, |state| state.preferred_scale())
    });
    if let Some(scale) = root_scale {
        with_states(surface, |data| {
            with_fractional_scale(data, |state| {
                state.set_preferred_scale(scale);
            });
        });
    }
}
```

This stores the value in Smithay's per-surface data map. When the client later
calls `get_fractional_scale`, Smithay finds the stored value and sends it.

### Popups: `output_for_popup` + `send_scale_transform`

Unlike subsurfaces, xdg_popups also need the full `send_scale_transform` call
(integer `preferred_buffer_scale` + fractional `preferred_scale`). GTK4 apps
like nautilus have blurry popups without this.

Scale is sent in `commit()` before the initial configure, matching niri's pattern:

```rust
// In CompositorHandler::commit()
if let PopupKind::Xdg(ref xdg_popup) = popup {
    if !xdg_popup.is_initial_configure_sent() {
        if let Some(output) = self.ewm.output_for_popup(&popup).cloned() {
            let scale = output.current_scale();
            let transform = output.current_transform();
            with_states(surface, |data| {
                send_scale_transform(surface, data, scale, transform);
            });
        }
        xdg_popup.send_configure().expect("initial configure failed");
    }
}
```

`output_for_popup` finds the popup's output by resolving root surface → window →
position → output geometry. It also checks layer surfaces as popup parents:

```rust
pub fn output_for_popup(&self, popup: &PopupKind) -> Option<&Output> {
    let root = find_popup_root_surface(popup).ok()?;
    // Check windows, then layer surfaces
    ...
}
```

### Debugging Child Surface Issues

If a client shows wrong DPR despite correct output scale:

1. Check `WAYLAND_DEBUG=1` for `wp_fractional_scale_v1.preferred_scale(N)` events.
   At 1.5x, N should be 180 (180/120 = 1.5).
2. Check which surface the client creates `wp_fractional_scale_v1` on — it may be
   a subsurface, not the toplevel.
3. Verify `wl_surface.enter` is sent before the client binds fractional scale.
   Without output enter, there's no integer scale either.
4. For popups: verify `preferred_buffer_scale` is sent before initial configure.
   Some toolkits (GTK4) rely on this rather than `wp_fractional_scale_v1`.

## Runtime Scale Changes

When `apply_output_config` changes scale or transform:

```
apply_output_config()
  │
  ├─► output.change_current_state(mode, transform, scale, position)
  │
  ├─► send_scale_transform_to_output_surfaces(&output)
  │     ├─► iterate space.elements() → send_scale_transform per window
  │     └─► iterate layer_map.layers() → send_scale_transform per layer
  │
  ├─► resize lock buffer (if locked)
  ├─► reconfigure lock surface (if locked)
  │
  ├─► check_working_area_change() → update_frames_for_working_area()
  │     └─► sends WorkingArea event to Emacs with new logical dimensions
  │
  └─► send OutputConfigChanged event to Emacs
```

This ensures:
- All existing surfaces learn the new scale (re-render without restart)
- Lock surfaces resize to match new logical output size
- Emacs frames resize to fit the new working area
- Emacs knows the actual applied configuration

## Rendering at Fractional Scales

No custom buffer wrappers are needed. Smithay's built-in buffers work for EWM's
simple use case (solid color lock background + fallback cursor):

- **Cursor**: positioned using `to_physical_precise_round(scale)` instead of
  integer cast, preventing sub-pixel misalignment
- **Lock buffer**: sized with logical output dimensions from `output_size()`,
  resized when config changes
- **Window/layer rendering**: Smithay handles buffer-to-output scaling internally
  via the `Scale<f64>` parameter passed to `render_elements`

Niri needs custom wrappers (`TextureBuffer`, `SolidColorBuffer`, `MemoryBuffer`
with `Scale<f64>`) because it has more complex render elements (workspace animations,
window shadows, resize previews). EWM delegates all complex rendering to clients.

## Debugging Reference

### Useful Commands

```sh
# Check fractional scale protocol is advertised
WAYLAND_DISPLAY=wayland-ewm-vt2 wayland-info | grep fractional

# Trace protocol messages for a specific app
WAYLAND_DISPLAY=wayland-ewm-vt2 WAYLAND_DEBUG=1 firefox 2>&1 | grep -E 'preferred_scale|buffer_scale|enter|leave'

# Check Firefox DPR (in Firefox devtools console)
window.devicePixelRatio

# Runtime scale change
emacsclient --socket-name=vt2 -e '(ewm-configure-output "DP-1" :scale 1.5)'
```

### Common Issues

| Symptom | Cause | Fix |
|---------|-------|-----|
| Emacs blurry at fractional scale | `CompositorState::new` instead of `new_v6` | Use `CompositorState::new_v6` to advertise wl_compositor v6 |
| Firefox DPR=1 at 1.5x | No `preferred_scale` on subsurface | `new_subsurface` callback copies scale from root |
| App ignores scale change | `send_scale_transform_to_output_surfaces` not called | Ensure `apply_output_config` notifies surfaces |
| Cursor offset at fractional scale | Integer truncation in cursor position | Use `to_physical_precise_round(scale)` |
| Window loses output after hide/show | `space.refresh()` calls `output.leave()` for off-screen windows | Use `cleanup_dead_windows()` instead |
| GTK4 popup blurry | No `send_scale_transform` before initial configure | `output_for_popup` → `send_scale_transform` in `commit()` |
| Working area wrong after scale change | `check_working_area_change` not called | Ensure `apply_output_config` calls it after state change |

### Key Code Locations

| Component | File | Function |
|-----------|------|----------|
| Scale notification | `utils.rs` | `send_scale_transform()` |
| Coordinate helpers | `utils.rs` | `to_physical_precise_round`, `round_logical_in_physical`, `output_size` |
| Scale rounding | `backend/mod.rs` | `closest_representable_scale()` |
| Output association | `lib.rs` | `enter_output_for_position()`, `cleanup_dead_windows()` |
| Scale propagation | `utils.rs` | `propagate_preferred_scale()` |
| Subsurface handler | `lib.rs` | `CompositorHandler::new_subsurface()` |
| Popup output lookup | `lib.rs` | `output_for_popup()` |
| Toplevel setup | `lib.rs` | `handle_new_toplevel()` |
| Bulk notification | `lib.rs` | `send_scale_transform_to_output_surfaces()` |
| Lock surface | `lib.rs` | `configure_lock_surface()` |
| DRM config | `backend/drm.rs` | `apply_output_config()` |
| Headless config | `backend/headless.rs` | `apply_output_config()` |
| Protocol init | `lib.rs` | `FractionalScaleManagerState::new::<State>()` |

### Niri Reference

The implementation follows niri's patterns. Key reference files in `~/git/niri/`:

| Niri File | EWM Equivalent | Purpose |
|-----------|---------------|---------|
| `src/niri.rs` | `lib.rs` | `FractionalScaleManagerState` init, output scale |
| `src/utils/mod.rs` | `utils.rs` | `send_scale_transform()`, coordinate helpers |
| `src/utils/scale.rs` | `backend/mod.rs` | `closest_representable_scale()` |
| `src/handlers/mod.rs` | `lib.rs` | Empty `FractionalScaleHandler`, `new_subsurface` |

Key divergence from niri: niri's windows are not in a global space, so
`space.refresh()` is safe. EWM keeps windows in `Space` for hit-testing but manages
output association explicitly via `enter_output_for_position` and direct
`output.enter()` calls, with `cleanup_dead_windows()` replacing `space.refresh()`.
