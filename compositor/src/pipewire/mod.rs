//! PipeWire integration for screen sharing
//!
//! This module provides PipeWire support for screen casting via the
//! org.gnome.Mutter.ScreenCast D-Bus interface.

pub mod stream;

use std::mem;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use pipewire::context::ContextRc;
use pipewire::core::{CoreRc, PW_ID_CORE};
use pipewire::main_loop::MainLoopRc;
use smithay::reexports::calloop::channel::{self, Channel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use tracing::{error, info, warn};

/// PipeWire state
pub struct PipeWire {
    _context: ContextRc,
    pub core: CoreRc,
    pub token: RegistrationToken,
    /// Channel to receive fatal error notifications (Option so it can be taken)
    pub fatal_error_rx: Option<Channel<()>>,
    /// Flag to track if PipeWire has encountered a fatal error
    pub had_fatal_error: Arc<AtomicBool>,
}

impl PipeWire {
    /// Initialize PipeWire and integrate with the calloop event loop
    pub fn new<D: 'static>(
        event_loop: &LoopHandle<'static, D>,
        on_error: impl Fn() + 'static,
    ) -> anyhow::Result<Self> {
        info!("Initializing PipeWire");

        let main_loop = MainLoopRc::new(None).context("error creating PipeWire MainLoop")?;
        let context = ContextRc::new(&main_loop, None).context("error creating PipeWire Context")?;
        let core = context
            .connect_rc(None)
            .context("error connecting to PipeWire")?;

        // Create channel for fatal error notifications
        let (fatal_error_tx, fatal_error_rx) = channel::channel::<()>();
        let had_fatal_error = Arc::new(AtomicBool::new(false));
        let had_fatal_error_clone = had_fatal_error.clone();

        // Listen for PipeWire errors
        let listener = core
            .add_listener_local()
            .error(move |id, seq, res, message| {
                warn!(id, seq, res, message, "PipeWire error");

                // Detect connection lost error (id=0, res=-32 is EPIPE)
                if id == PW_ID_CORE && res == -32 {
                    error!("PipeWire connection lost");
                    had_fatal_error_clone.store(true, Ordering::SeqCst);
                    // Notify compositor via channel (ignore send error if receiver dropped)
                    let _ = fatal_error_tx.send(());
                    on_error();
                }
            })
            .register();
        // Keep the listener alive for the lifetime of the core
        mem::forget(listener);

        // Wrapper to get the fd from MainLoop
        struct MainLoopFd(MainLoopRc);
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
            fatal_error_rx: Some(fatal_error_rx),
            had_fatal_error,
        })
    }

    /// Check if PipeWire has encountered a fatal error
    pub fn has_fatal_error(&self) -> bool {
        self.had_fatal_error.load(Ordering::SeqCst)
    }
}
