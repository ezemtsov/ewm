//! PipeWire integration for screen sharing
//!
//! This module provides PipeWire support for screen casting via the
//! org.gnome.Mutter.ScreenCast D-Bus interface.

pub mod stream;

use std::mem;
use std::os::fd::{AsFd, BorrowedFd};
use std::time::Duration;

use anyhow::Context as _;
use pipewire::context::Context;
use pipewire::core::{Core, PW_ID_CORE};
use pipewire::main_loop::MainLoop;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use tracing::{info, warn};

/// Messages sent from PipeWire thread to main compositor
pub enum PwToCompositor {
    /// A fatal error occurred, PipeWire needs to be reset
    FatalError,
}

/// PipeWire state
pub struct PipeWire {
    _context: Context,
    pub core: Core,
    pub token: RegistrationToken,
}

impl PipeWire {
    /// Initialize PipeWire and integrate with the calloop event loop
    pub fn new<D: 'static>(
        event_loop: &LoopHandle<'static, D>,
        on_error: impl Fn() + 'static,
    ) -> anyhow::Result<Self> {
        info!("Initializing PipeWire");

        let main_loop = MainLoop::new(None).context("error creating PipeWire MainLoop")?;
        let context = Context::new(&main_loop).context("error creating PipeWire Context")?;
        let core = context.connect(None).context("error connecting to PipeWire")?;

        // Listen for PipeWire errors
        let listener = core
            .add_listener_local()
            .error(move |id, seq, res, message| {
                warn!(id, seq, res, message, "PipeWire error");

                // Reset PipeWire on connection errors
                if id == PW_ID_CORE && res == -32 {
                    on_error();
                }
            })
            .register();
        // Keep the listener alive for the lifetime of the core
        mem::forget(listener);

        // Wrapper to get the fd from MainLoop
        struct MainLoopFd(MainLoop);
        impl AsFd for MainLoopFd {
            fn as_fd(&self) -> BorrowedFd<'_> {
                self.0.loop_().fd()
            }
        }

        // Integrate PipeWire event loop with calloop
        let generic = Generic::new(MainLoopFd(main_loop), Interest::READ, Mode::Level);
        let token = event_loop
            .insert_source(generic, move |_, wrapper, _| {
                wrapper.0.loop_().iterate(Duration::ZERO);
                Ok(PostAction::Continue)
            })
            .map_err(|e| anyhow::anyhow!("error inserting PipeWire source: {}", e))?;

        info!("PipeWire initialized successfully");

        Ok(Self {
            _context: context,
            core,
            token,
        })
    }
}
