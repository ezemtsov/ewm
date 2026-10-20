//! What an Emacs frame shows, as Lisp declares it: entries, the surfaces in them, and ids.

use serde::{Deserialize, Serialize};
use smithay::utils::{Logical, Rectangle, Size};

/// An entry in a per-output declarative layout.
/// Coordinates are relative to the output's working area (frame-relative).
pub type LayoutEntryId = std::num::NonZeroU64;
pub type FocusId = std::num::NonZeroU64;

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct LayoutEntry {
    /// Entry identity assigned by lisp. Names one Emacs window in a frame.
    pub id: LayoutEntryId,
    /// Buffer name shown in this Emacs window.
    #[serde(default)]
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Surface-specific attachment when this Emacs window displays a Wayland surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<LayoutEntrySurface>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct LayoutEntrySurface {
    /// Which Wayland surface this entry displays (analogous to an Emacs buffer).
    pub surface_id: u64,
    /// Largest view per surface, computed by compositor from entry dimensions.
    /// Drives send_configure() size + scale and native-size rendering.
    pub primary: bool,
    /// The view its frame shows fullscreen, marked by `Layout::reconcile_fullscreen`.
    #[serde(default)]
    pub fullscreen: bool,
}

impl LayoutEntry {
    pub fn emacs_window(id: LayoutEntryId, name: String, x: i32, y: i32, w: u32, h: u32) -> Self {
        Self {
            id,
            name,
            x,
            y,
            w,
            h,
            surface: None,
        }
    }

    pub fn surface(
        id: LayoutEntryId,
        name: String,
        surface_id: u64,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Self {
        Self {
            id,
            name,
            x,
            y,
            w,
            h,
            surface: Some(LayoutEntrySurface {
                surface_id,
                primary: false,
                fullscreen: false,
            }),
        }
    }

    pub fn surface_id(&self) -> Option<u64> {
        self.surface.as_ref().map(|surface| surface.surface_id)
    }

    pub fn rect(&self) -> Rectangle<i32, Logical> {
        Rectangle::new(
            (self.x, self.y).into(),
            (self.w as i32, self.h as i32).into(),
        )
    }

    pub fn primary(&self) -> bool {
        self.surface.as_ref().is_some_and(|surface| surface.primary)
    }

    pub fn set_primary(&mut self, primary: bool) {
        if let Some(surface) = &mut self.surface {
            surface.primary = primary;
        }
    }

    pub fn fullscreen(&self) -> bool {
        self.surface
            .as_ref()
            .is_some_and(|surface| surface.fullscreen)
    }

    pub fn set_fullscreen(&mut self, fullscreen: bool) {
        if let Some(surface) = &mut self.surface {
            surface.fullscreen = fullscreen;
        }
    }
}

/// Centering offset for a fullscreen surface within an output.
/// Returns (0, 0) when the surface is at least as large as the output.
pub fn fullscreen_center_offset(
    window_size: Size<i32, Logical>,
    output_size: Size<i32, Logical>,
) -> (i32, i32) {
    (
        if window_size.w < output_size.w {
            (output_size.w - window_size.w) / 2
        } else {
            0
        },
        if window_size.h < output_size.h {
            (output_size.h - window_size.h) / 2
        } else {
            0
        },
    )
}
