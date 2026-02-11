//! EWM binary entry point
//!
//! Standalone compositor for debugging. Production use is via the Emacs dynamic module.

use ewm_core::backend;
use tracing::{error, info};

fn main() {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        eprintln!("Usage: ewm <PROGRAM> [ARGS...]");
        eprintln!("Examples:");
        eprintln!("  ewm emacs              # Start Emacs");
        eprintln!("  ewm foot               # Start foot terminal");
        eprintln!();
        eprintln!("Note: Must be run from a TTY (not inside another compositor).");
        std::process::exit(1);
    }

    let program = args[0].clone();
    let program_args: Vec<String> = args[1..].to_vec();

    info!("Starting EWM with DRM backend");
    info!("Will spawn: {} {:?}", program, program_args);

    if let Err(e) = backend::drm::run_drm(Some((program, program_args))) {
        error!("Fatal error: {}", e);
        std::process::exit(1);
    }
}
