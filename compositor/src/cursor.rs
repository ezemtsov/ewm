//! Cursor rendering support
//!
//! This module provides a simple fallback cursor for rendering when
//! running on the DRM backend where no host compositor provides a cursor.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::gles::{GlesError, GlesRenderer};
use smithay::utils::{Physical, Point, Transform};

/// Fallback cursor image data (64x64 RGBA)
/// This is a simple left_ptr style cursor.
static FALLBACK_CURSOR_DATA: &[u8] = include_bytes!("../resources/cursor.rgba");

/// Cursor hotspot (where the click point is relative to top-left)
pub const CURSOR_HOTSPOT: (i32, i32) = (1, 1);

/// Cursor width
const CURSOR_WIDTH: i32 = 64;
/// Cursor height
const CURSOR_HEIGHT: i32 = 64;

/// Wrapper around MemoryRenderBuffer for cursor rendering
pub struct CursorBuffer {
    buffer: MemoryRenderBuffer,
}

impl CursorBuffer {
    /// Create a new cursor buffer with the fallback cursor image
    pub fn new() -> Self {
        let buffer = MemoryRenderBuffer::from_slice(
            FALLBACK_CURSOR_DATA,
            Fourcc::Abgr8888,
            (CURSOR_WIDTH, CURSOR_HEIGHT),
            1, // scale
            Transform::Normal,
            None,
        );
        Self { buffer }
    }

    /// Create a render element for the cursor at the given position
    pub fn render_element(
        &self,
        renderer: &mut GlesRenderer,
        position: Point<i32, Physical>,
    ) -> Result<MemoryRenderBufferRenderElement<GlesRenderer>, GlesError>
    {
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            position.to_f64(),
            &self.buffer,
            None,
            None,
            None,
            smithay::backend::renderer::element::Kind::Cursor,
        )
    }
}

impl Default for CursorBuffer {
    fn default() -> Self {
        Self::new()
    }
}
