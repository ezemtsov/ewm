# Fractional Scale Implementation Plan

> **Note to self**: Follow niri's implementation as closely as possible. Reference files:
> - `~/git/niri/src/niri.rs` - FractionalScaleManagerState initialization, output scale setting
> - `~/git/niri/src/utils/mod.rs` - `send_scale_transform()`, coordinate helpers
> - `~/git/niri/src/utils/scale.rs` - scale rounding, auto-detection
> - `~/git/niri/src/render_helpers/` - custom buffer wrappers for `Scale<f64>`
> - `~/git/niri/src/handlers/mod.rs` - FractionalScaleHandler delegation

## Overview

Add optional scale support to EWM's output configuration, allowing fractional scales (1.25x, 1.5x, etc.) for HiDPI displays.

## Protocol Setup

Smithay handles the wire protocol. Minimal code needed:

```rust
impl FractionalScaleHandler for Ewm {}
delegate_fractional_scale!(Ewm);

// In initialization:
let fractional_scale_manager = FractionalScaleManagerState::new::<Ewm>(&display_handle);
```

## Scale Representation

- Store as `output::Scale::Fractional(f64)` on outputs
- Protocol precision is N/120, use helper to round:
  ```rust
  pub fn closest_representable_scale(scale: f64) -> f64 {
      const FRACTIONAL_SCALE_DENOM: f64 = 120.;
      (scale * FRACTIONAL_SCALE_DENOM).round() / FRACTIONAL_SCALE_DENOM
  }
  ```
- Default to 1.0 if not specified (backward compatible)

## Surface Notification

Surfaces must receive both integer (legacy) and fractional scales:

```rust
pub fn send_scale_transform(
    surface: &WlSurface,
    data: &SurfaceData,
    scale: output::Scale,
    transform: Transform,
) {
    send_surface_state(surface, data, scale.integer_scale(), transform);
    with_fractional_scale(data, |fractional| {
        fractional.set_preferred_scale(scale.fractional_scale());
    });
}
```

Call this in:
- New subsurface creation (compositor handler)
- Toplevel/popup window configuration (xdg_shell handler)
- Layer shell surface output assignment/resize
- Session lock surfaces (if implemented)
- Input method popups

## Render Buffer Wrappers

Niri wraps Smithay buffers to support `Scale<f64>`. Copy these patterns:
- `TextureBuffer` with `scale: Scale<f64>`
- `SolidColorBuffer` with `Size<f64, Logical>`
- `MemoryBuffer` with `scale: Scale<f64>`

## Coordinate Helpers

```rust
pub fn to_physical_precise_round<N: Coordinate>(scale: f64, logical: impl Coordinate) -> N {
    N::from_f64((logical.to_f64() * scale).round())
}

pub fn round_logical_in_physical(scale: f64, logical: f64) -> f64 {
    (logical * scale).round() / scale
}

pub fn output_size(output: &Output) -> Size<f64, Logical> {
    let scale = output.current_scale().fractional_scale();
    let transform = output.current_transform();
    let mode = output.current_mode().unwrap();
    transform.transform_size(mode.size.to_f64().to_logical(scale))
}
```

## Emacs Integration

Extend output configuration to accept optional scale:

```elisp
;; Current (assumed):
(ewm-configure-output "DP-1" :mode "2560x1440@144")

;; Extended:
(ewm-configure-output "DP-1" :mode "2560x1440@144" :scale 1.5)
```

The Rust side should:
1. Accept scale in the output config message
2. Default to 1.0 if not provided
3. Round to nearest representable scale
4. Apply via `output.change_current_state(..., Some(output::Scale::Fractional(scale)), ...)`

## Implementation Steps

1. [ ] Add `FractionalScaleManagerState` initialization
2. [ ] Implement `FractionalScaleHandler` delegation
3. [ ] Add `send_scale_transform()` utility function
4. [ ] Update all surface notification sites to use it
5. [ ] Add coordinate helper functions
6. [ ] Create render buffer wrappers (or adapt existing)
7. [ ] Extend output config to accept scale parameter
8. [ ] Update Elisp API to pass scale to compositor
9. [ ] Test with applications that support fractional scaling

## Testing

- Firefox/Chrome with `GDK_SCALE` or native Wayland scaling
- GTK4 apps (native fractional scale support)
- Qt apps with `QT_WAYLAND_FORCE_DPI` or native support
- `weston-simple-shm` for basic verification
