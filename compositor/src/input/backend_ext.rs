//! Input backend extensions.
//!
//! Ported from niri's `input/backend_ext.rs`.

use smithay::output::Output;
use smithay::reexports::input as libinput;

/// Input backends whose devices may be bound to one output.
pub trait EwmInputDevice {
    fn output(&self) -> Option<&Output> {
        None
    }

    fn natural_scroll(&self) -> bool {
        false
    }
}

impl EwmInputDevice for libinput::Device {
    fn natural_scroll(&self) -> bool {
        self.config_scroll_natural_scroll_enabled()
    }
}
