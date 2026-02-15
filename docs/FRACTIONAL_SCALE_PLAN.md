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
- Session lock surfaces
- Input method popups
- After `apply_output_config` changes scale/transform (iterate all surfaces on output)

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

## Emacs Integration (DONE)

Output configuration accepts scale and transform:

```elisp
;; ewm-output-config defcustom:
(setq ewm-output-config
      '(("DP-1" :width 2560 :height 1440 :scale 1.5)
        ("eDP-1" :width 1920 :height 1200 :x 0 :y 0 :transform 0)))

;; Programmatic:
(ewm-configure-output "DP-1" :scale 1.5 :transform 0)
```

### What was done (Output Config Architecture)

Separated **desired output configuration** from **runtime output state**, following
niri's pattern. Config is stored per output name and re-applied on hot-plug.

- `OutputConfig` struct in `lib.rs` stores mode, position, scale, transform, enabled
- `Ewm.output_config: HashMap<String, OutputConfig>` persists across connect/disconnect
- `Backend::apply_output_config()` dispatches to backend-specific methods
- `DrmBackendState::apply_output_config()` - one concrete function: resolves mode,
  applies everything in one `change_current_state` call, updates all bookkeeping
  (OutputInfo, D-Bus, refresh interval, working areas)
- `HeadlessBackend::apply_output_config()` - headless equivalent (scale/transform/position)
- `ConfigureOutput` command handler stores config then delegates to `apply_output_config()`
- `connect_output()` (DRM) and `add_output()` (headless) look up config on connect
- `initialize_drm()` reuses `connect_output()` for initial scan (no code duplication)
- `resolve_drm_mode()` / `preferred_drm_mode()` shared helpers for mode matching
- `OutputInfo` event includes `scale` and `transform` so Emacs knows applied state
- `configure_output_module` defun accepts `scale` (f64) and `transform` (i32) params
- Scale applied via `output.change_current_state(..., Some(Scale::Fractional(scale)), ...)`
- Transform integer mapping: 0=Normal, 1=90, 2=180, 3=270, 4=Flipped, ...

## Divergences from Niri (TODO)

Comparison done against `~/git/niri/`. These are the remaining gaps:

### 1. No fractional scale protocol (steps 1-4)
Niri initializes `FractionalScaleManagerState` and has `send_scale_transform()` which
sends **both** integer scale (via `send_surface_state`) and fractional scale (via
`wp_fractional_scale_v1.set_preferred_scale`). EWM has none of this — clients cannot
discover or use fractional scaling.

### 2. No surface notification after scale/transform changes
When niri's `reload_output_config()` changes scale or transform, it calls
`output_resized()` which iterates all layer surfaces, windows, lock surfaces, popups
on that output and sends updated preferred scale/transform via `send_scale_transform`.
EWM's `apply_output_config` changes output state but never tells existing surfaces.

### 3. No N/120 scale rounding (step 10)
Niri rounds every scale to `closest_representable_scale()` before applying. EWM stores
the raw f64 from config. Scale 1.5 happens to be representable (180/120), but arbitrary
values like 1.3333 would not round-trip through the protocol correctly.

### 4. Working area and Emacs frames not recalculated on reconfig
`connect_output` calculates and sends working areas, but `apply_output_config` does not.
Both mode changes (resolution) and scale changes affect the logical output size — e.g.
2560x1440 at scale 1.5 = 1707x960 logical. The working area shrinks, but layer shell
exclusive zones are never re-evaluated and Emacs frames are never resized. The machinery
exists (`check_working_area_change` → `update_frames_for_working_area`) but is not called
from `apply_output_config`. This is EWM-specific: niri has `output_resized()` for layout
recalculation but doesn't manage Emacs frames.

### 5. No "config applied" event to Emacs
`connect_output` sends `OutputDetected` to Emacs, but `apply_output_config` sends nothing.
If the requested mode was unavailable and a fallback was used, Emacs cannot know the
actual applied state. Niri sends output management protocol events after config changes.

### 6. `recalculate_output_size` inconsistency
`DrmBackendState::recalculate_output_size` computes `(max_right_edge, max_height)`,
ignoring y-offsets. `Ewm::recalculate_output_size` computes `(max_right_edge,
max_bottom_edge)`. `connect_output` calls the DRM version; `apply_output_config` calls
the Ewm version — different results for multi-output setups with vertical offsets.

### 7. OutputInfo stale after mode change
`apply_output_config` patches scale/transform/position in `OutputInfo` but not mode
dimensions. After a DRM mode change, the `OutputInfo` sent to Emacs would have the
old width/height.

### 8. `configure_lock_surface` discards fractional scale
`configure_lock_surface` calls `with_fractional_scale` but does `let _ = fractional_scale`,
discarding the value. Should send it to the lock surface once the protocol is set up.

## Implementation Steps

### Phase 1: Fix existing gaps (no protocol changes)

1. [x] Extend output config to accept scale parameter
2. [x] Update Elisp API to pass scale to compositor
3. [x] Store output config and re-apply on hot-plug (output config architecture)

4. [x] Fix recalculate_output_size inconsistency (use Ewm version everywhere)
   - **Validate**: integration test — two outputs, second at y-offset 500;
     `ewm.output_size` height accounts for the offset regardless of whether outputs
     were added via `connect_output` or `apply_output_config`

5. [x] Update OutputInfo mode dimensions after DRM mode change
   - **Validate**: `(ewm-configure-output "DP-1" :width 1920 :height 1080)`, then
     `M-x ewm-show-state`; verify `OutputInfo` shows 1920x1080, not the old dimensions

6. [x] Recalculate working areas in apply_output_config after mode or scale changes
   - Both mode changes (resolution) and scale changes affect the logical output size,
     which changes `non_exclusive_zone()`. Call `check_working_area_change()` after
     applying config — this cascades to `update_frames_for_working_area()` which
     resizes Emacs frames to fit the new logical dimensions and sends `WorkingArea`
     event to Emacs. This is EWM-specific; niri doesn't manage Emacs frames but has
     analogous layout recalculation in `output_resized()`.
   - **Validate**: integration test — create fixture with output at scale 1.0 (1920x1080),
     call `apply_output_config` with scale 2.0, assert `WorkingArea` event is queued
     with logical dimensions 960x540; also test with mode change

7. [x] Send OutputConfigChanged event to Emacs after apply_output_config
   - **Validate**: `(ewm-configure-output "DP-1" :scale 1.5)`, then check
     `ewm--last-output-event` (or `*ewm-events*` buffer) shows the applied config
     including effective scale/transform/mode

8. [x] Round scale to nearest representable value (N/120)
   - **Validate**: unit test — `closest_representable_scale(1.0) == 1.0`,
     `closest_representable_scale(1.25) == 1.25` (150/120),
     `closest_representable_scale(1.77) == 1.766667` (212/120);
     integration test — configure scale 1.3333, read back from output state,
     verify it equals 160/120

9. [x] Add coordinate helper functions + retype `output_size` to `Size<i32, Logical>`
   - **Validate**: unit tests —
     `to_physical_precise_round(1.5, 101) == 152`,
     `round_logical_in_physical(1.5, 10.3) == 10.333...`,
     `output_size()` returns correct logical size for a fractionally-scaled output

### Phase 2: Fractional scale protocol

10. [x] Add `FractionalScaleManagerState` initialization
    - **Validate**: `wayland-info | grep fractional` shows `wp_fractional_scale_manager_v1`

11. [x] Implement `FractionalScaleHandler` delegation
    - **Validate**: builds; GTK4 app launched with `GDK_DEBUG=gl` doesn't log
      "fractional scale not supported" warnings

12. [x] Add `send_scale_transform()` utility function
    - **Validate**: unit test — call with `Scale::Fractional(1.5)`, mock surface data,
      assert `set_preferred_scale` was called with `1.5` and `send_surface_state` with
      integer scale `2`

13. [x] Update all surface notification sites to use it
    - **Validate**: `WAYLAND_DEBUG=1 foot` (or any Wayland terminal), observe
      `wp_fractional_scale_v1.preferred_scale(180)` in protocol trace for scale 1.5

14. [x] Notify existing surfaces after apply_output_config changes scale/transform
    - **Validate**: run `foot` at scale 1.0, then `(ewm-configure-output "DP-1" :scale 1.5)`;
      `WAYLAND_DEBUG=1` shows new `preferred_scale` sent to foot's surface; foot
      re-renders at the new scale without needing to restart

15. [x] Fix configure_lock_surface to send fractional scale
    - **Validate**: `WAYLAND_DEBUG=1` during session lock (e.g. `swaylock`), verify
      `wp_fractional_scale_v1.preferred_scale` is sent to the lock surface

16. [x] Create render buffer wrappers (or adapt existing)
    - **Validate**: existing `cargo test --test render` passes; visual check that
      cursor and solid-color backgrounds render without seams at scale 1.5
    - No custom wrappers needed — Smithay's built-in buffers work for EWM's simple
      use case (solid color lock background + fallback cursor). Fixed:
      - Cursor position: `to_physical_precise_round(scale)` instead of cast-to-i32
      - Lock buffer: sized with logical output dimensions, resized on config change
      - Scale notification: uses target output (not first output) in handle_new_toplevel
      - FractionalScaleHandler emptied (like niri) — scale sent from lifecycle handlers

17. [x] Test with applications that support fractional scaling
    - **Validate**: see Testing section below

## Testing

Manual testing checklist at scale 1.5:

- [x] `wayland-info` shows `wp_fractional_scale_manager_v1` global
- [x] `foot` — text renders sharp, no blurriness at fractional scale
- [ ] GTK4 app (e.g. `nautilus`) — renders at correct scale, no scaling artifacts
- [ ] Firefox — `about:support` shows correct device pixel ratio (1.5)
- [ ] Qt app — renders correctly with `QT_WAYLAND_FORCE_DPI` or native
- [x] Runtime scale change via `ewm-configure-output` — apps re-render without restart
- [ ] Hot-unplug + re-plug — output comes back at configured scale
- [ ] `weston-simple-shm` — basic rendering works (integer scale fallback)
