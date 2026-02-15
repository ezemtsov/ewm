# Known Bugs

## AMD PSR display freeze with continuous-commit clients

**Status**: Workaround applied (kernel parameter)
**Affected**: AMD laptops with eDP panels and PSR-capable displays
**Trigger**: Layer-shell clients that commit on every frame callback (e.g. dms-shell)

### Symptom

Display freezes shortly after session start. The compositor continues working
internally — input is processed and text typed during the freeze appears after
VT switch recovery (Ctrl+Alt+F2, then back). Freezes again after a few seconds.

### Cause

AMD's Panel Self Refresh (PSR) fails to exit when the compositor is continuously
flipping buffers. dms-shell commits a new buffer on every frame callback even
when its content hasn't changed, creating a 60fps render loop. PSR gets stuck
in self-refresh, showing a stale cached frame.

This is a kernel driver bug — confirmed to affect niri identically on the same
hardware.

### Workaround

Disable PSR via kernel parameter:

```
amdgpu.dcdebugmask=0x10
```

Downside: higher power consumption on battery during idle (PSR saves power by
letting the display controller stop reading from the GPU framebuffer when content
is static).

### Debugging

Runtime PSR toggle (no reboot):
```sh
# Disable PSR
echo 1 | sudo tee /sys/kernel/debug/dri/0000:c3:00.0/eDP-1/disallow_edp_enter_psr

# Check PSR state
cat /sys/kernel/debug/dri/0000:c3:00.0/eDP-1/psr_state
cat /sys/kernel/debug/dri/0000:c3:00.0/eDP-1/psr_capability
```
