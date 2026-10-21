//! Testing infrastructure for EWM compositor
//!
//! Provides a headless test fixture for integration testing
//! the compositor without requiring real hardware.

mod client;
mod fixture;
mod input;
pub mod lisp;

#[cfg(feature = "testing-layer-shell")]
pub use client::LayerSurface;
pub use client::{
    ClientEvent, ClientState as TestClientState, Gesture, Popup, Subsurface, TestClient, Toplevel,
};
pub use fixture::Fixture;
use smithay::utils::{Rectangle, Size};

use crate::layout::strip;
use crate::{Layout, LayoutEntry, LayoutEntryId, animation};

/// Two wl_fixed steps of 1/256 px.
pub const WL_FIXED_TOLERANCE: f64 = 1.0 / 128.0;

/// Record that `id` was the active frame when a close began.
///
/// Integration tests use this to simulate the pgtk race where focus moves to
/// another frame before `xdg_toplevel.destroy` reaches the compositor.
pub fn mark_active_frame_close(id: u64) {
    crate::module::mark_pending_active_frame_close(id);
}

/// Queue Emacs frame SEQ tiled on OUTPUT, as `ewm-prepare-frame` does.
pub fn prepare_frame(output: &str, seq: u64, after: Option<u64>) {
    crate::module::prepare_frame(crate::module::PendingFrame {
        output: output.to_owned(),
        placement: crate::layout::Placement::Tiled { after },
        seq,
    });
}

/// A bare output called `name`, for layouts without a backend.
pub fn output(name: &str) -> smithay::output::Output {
    smithay::output::Output::new(
        name.to_string(),
        smithay::output::PhysicalProperties {
            size: (0, 0).into(),
            subpixel: smithay::output::Subpixel::Unknown,
            make: String::new(),
            model: String::new(),
            serial_number: String::new(),
        },
    )
}

/// A layout with one mapped output per strip, without a compositor around it.
pub fn layout_with(strips: Vec<(&str, strip::Strip)>) -> Layout {
    let mut layout = Layout::new(animation::Clock::default());
    for (name, strip) in strips {
        let output = output(name);
        layout.add_output(
            output.clone(),
            Rectangle::from_size(Size::from((1600, 900))),
        );
        layout.install_strip(&output, strip);
    }
    layout
}

/// Drop every queued frame. The queue is a process-wide static.
pub fn clear_pending_frames() {
    while crate::module::take_pending_frame().is_some() {}
}

// Layout fixtures shared by the unit tests in lib.rs and layout/.

pub const HEAD_A: &str = "HEAD-A";
pub const HEAD_B: &str = "HEAD-B";
pub const HEAD_C: &str = "HEAD-C";
pub const HEAD_D: &str = "HEAD-D";
pub const PROP_HEADS: [&str; 4] = [HEAD_A, HEAD_B, HEAD_C, HEAD_D];

pub fn nz(raw: u64) -> LayoutEntryId {
    std::num::NonZeroU64::new(raw).unwrap()
}

pub fn frame_with_entry(
    frame_surface_id: u64,
    entry_id: LayoutEntryId,
    surface_id: u64,
) -> strip::Frame {
    let mut frame = strip::Frame::new(frame_surface_id);
    frame.width = 400.0;
    frame.selected_entry_id = Some(entry_id);
    frame.entries.push(LayoutEntry::surface(
        entry_id,
        "entry".to_string(),
        surface_id,
        0,
        0,
        400,
        400,
    ));
    frame
}

pub fn frame_with_width(surface_id: u64, width: f64) -> strip::Frame {
    let mut frame = strip::Frame::new(surface_id);
    frame.width = width;
    frame
}

pub fn add_prop_outputs(fix: &mut Fixture) {
    for name in PROP_HEADS {
        fix.add_output(name, 1600, 900);
    }
}

pub fn order_from_keys(keys: &[u8; 4]) -> Vec<usize> {
    let mut order = (0..PROP_HEADS.len()).collect::<Vec<_>>();
    order.sort_by_key(|&idx| (keys[idx], idx));
    order
}

pub fn set_sorted_output_order(fix: &mut Fixture, order: &[usize]) {
    fix.ewm().sorted_outputs.sort_by_key(|output| {
        order
            .iter()
            .position(|&idx| PROP_HEADS[idx] == output.name())
            .unwrap_or(usize::MAX)
    });
    let order = fix.ewm().sorted_outputs.clone();
    fix.ewm().layout.sort_monitors(&order);
}

pub fn strip_with_frame_ids(ids: Vec<u64>, active_idx: usize) -> strip::Strip {
    let mut strip = strip::Strip::new(animation::Clock::default());
    strip.append_frames(ids.into_iter().map(strip::Frame::new));
    if !strip.frames().is_empty() {
        strip.set_active(active_idx % strip.frames().len());
    }
    strip
}
