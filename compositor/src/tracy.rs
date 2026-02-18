//! Tracy profiling integration
//!
//! This module provides Tracy profiling macros that compile to no-ops when
//! the `profile-with-tracy` feature is disabled.
//!
//! # Why Tracy?
//!
//! Tracy is a real-time frame profiler with nanosecond precision and low
//! overhead (~20ns per span). It's ideal for compositor profiling because:
//!
//! 1. **Frame-oriented**: Built for graphics applications with frame markers
//! 2. **Low overhead**: Spans can be left in production builds
//! 3. **Visual timeline**: Shows exactly where time is spent per-frame
//! 4. **Remote capture**: Connect to running compositor from another machine
//!
//! # Usage
//!
//! Use the `tracy_span!` macro at the start of functions to profile:
//!
//! ```ignore
//! fn render_output() {
//!     tracy_span!("render_output");
//!     // ... rendering code
//! }
//! ```
//!
//! For frame markers (VBlank tracking), use `tracy_frame_mark!`:
//!
//! ```ignore
//! fn on_vblank() {
//!     tracy_frame_mark!("vblank");
//!     // ... VBlank handling
//! }
//! ```

/// Create a Tracy span for the current scope.
/// Compiles to no-op when tracy feature is disabled.
#[macro_export]
#[cfg(feature = "profile-with-tracy")]
macro_rules! tracy_span {
    ($name:expr) => {
        let _span = tracy_client::span!($name);
    };
}

#[macro_export]
#[cfg(not(feature = "profile-with-tracy"))]
macro_rules! tracy_span {
    ($name:expr) => {};
}

/// Mark a frame boundary for Tracy's frame view.
/// Useful for VBlank tracking.
#[macro_export]
#[cfg(feature = "profile-with-tracy")]
macro_rules! tracy_frame_mark {
    ($name:expr) => {
        tracy_client::Client::running().map(|c| c.frame_mark());
    };
    () => {
        tracy_client::Client::running().map(|c| c.frame_mark());
    };
}

#[macro_export]
#[cfg(not(feature = "profile-with-tracy"))]
macro_rules! tracy_frame_mark {
    ($name:expr) => {};
    () => {};
}

/// Create a Tracy plot value for tracking metrics over time.
#[macro_export]
#[cfg(feature = "profile-with-tracy")]
macro_rules! tracy_plot {
    ($name:expr, $value:expr) => {{
        static PLOT: std::sync::OnceLock<tracy_client::PlotName> = std::sync::OnceLock::new();
        let name = PLOT.get_or_init(|| tracy_client::plot_name!($name));
        tracy_client::Client::running().map(|c| c.plot(*name, $value as f64));
    }};
}

#[macro_export]
#[cfg(not(feature = "profile-with-tracy"))]
macro_rules! tracy_plot {
    ($name:expr, $value:expr) => {};
}

/// VBlank frame tracking for per-output profiling.
///
/// Tracy supports "non-continuous frames" which are perfect for tracking
/// VBlank-to-VBlank intervals per output. Each output gets its own frame
/// series with a unique name.
///
/// Usage:
/// ```ignore
/// // Create tracker for an output
/// let mut tracker = VBlankFrameTracker::new("Virtual-1");
///
/// // When starting to wait for VBlank
/// tracker.begin_frame();
///
/// // When VBlank arrives
/// tracker.end_frame();
/// ```
#[cfg(feature = "profile-with-tracy")]
pub struct VBlankFrameTracker {
    frame: Option<tracy_client::Frame>,
    frame_name: tracy_client::FrameName,
}

#[cfg(feature = "profile-with-tracy")]
impl VBlankFrameTracker {
    /// Create a new VBlank frame tracker for the given output name.
    pub fn new(output_name: &str) -> Self {
        // Create a unique frame name for this output
        // Note: tracy_client::frame_name! requires a string literal, so we use
        // the lower-level API for dynamic names
        let name_string = format!("vblank-{}", output_name);
        let frame_name = tracy_client::FrameName::new_leak(name_string);
        Self {
            frame: None,
            frame_name,
        }
    }

    /// Start tracking a frame (call when queueing redraw/waiting for VBlank).
    pub fn begin_frame(&mut self) {
        if let Some(client) = tracy_client::Client::running() {
            // End any existing frame first
            self.frame.take();
            // Start new non-continuous frame
            self.frame = Some(client.non_continuous_frame(self.frame_name));
        }
    }

    /// End the current frame (call on VBlank).
    pub fn end_frame(&mut self) {
        // Dropping the Frame marks frame end in Tracy
        self.frame.take();
    }

    /// Check if a frame is currently being tracked.
    pub fn is_tracking(&self) -> bool {
        self.frame.is_some()
    }
}

/// No-op implementation when Tracy is disabled.
#[cfg(not(feature = "profile-with-tracy"))]
pub struct VBlankFrameTracker;

#[cfg(not(feature = "profile-with-tracy"))]
impl VBlankFrameTracker {
    #[inline(always)]
    pub fn new(_output_name: &str) -> Self {
        Self
    }

    #[inline(always)]
    pub fn begin_frame(&mut self) {}

    #[inline(always)]
    pub fn end_frame(&mut self) {}

    #[inline(always)]
    pub fn is_tracking(&self) -> bool {
        false
    }
}
