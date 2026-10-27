//! A batch Emacs with the real Lisp loaded, standing in for the session the compositor serves.
//!
//! Every module function is a no-op that keeps what Emacs sends. A test runs a config through it
//! and takes the intercept list, the last `ewm-send-key` and the options the compositor would have
//! received, once per distinct config.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::{Deserialize, Deserializer};

use crate::InterceptedKey;
use crate::config::Config;
use crate::module::{self, ChordSpec, KeySpec};

/// Run `emacs -Q -batch` with `lisp/` and `tests/` on the load path and return its stdout.
pub fn emacs(args: &[&str]) -> String {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let lisp_dir = repo.with_file_name("lisp");
    let tests_dir = repo.with_file_name("tests");
    let output = Command::new("emacs")
        .args([
            "-Q",
            "-batch",
            "-L",
            lisp_dir.to_str().unwrap(),
            "-L",
            tests_dir.to_str().unwrap(),
        ])
        .args(args)
        .output()
        .expect("failed to spawn `emacs`; ensure it is on PATH");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "emacs {args:?} failed (exit={:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    stdout
}

/// Module functions keep what they are sent, then the real Lisp loads in module mode.
const PRELUDE: &str = r#"(require 'ewm-test-stubs)
  (setenv "XCURSOR_SIZE" nil)
  (defvar keys nil)
  (defvar sent nil)
  (defvar capture nil)
  (defun ewm-intercept-keys-module (k) (setq keys k))
  (defun ewm-send-key-module (id k) (cl-assert (not capture)) (setq sent (vector id k)))
  (defun ewm-keyboard-redirect-capture-clear-module () (setq capture nil))
  (defun ewm-keyboard-capture-active-module () capture)
  (require 'ewm)
  (setq ewm--module-mode t)"#;

/// What Emacs sent once a config had run, printed as `(:keys KEYS :sent SENT :config CONFIG)`.
#[derive(Deserialize)]
pub struct Session {
    #[serde(rename = "keys", deserialize_with = "resolve_keys")]
    pub intercepts: Vec<InterceptedKey>,
    /// The last `ewm-send-key` call, `[surface-id keys]`.
    pub sent: Option<(Option<u64>, Vec<ChordSpec>)>,
    pub config: Config,
}

fn resolve_keys<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<InterceptedKey>, D::Error> {
    Vec::<KeySpec>::deserialize(d).map(module::resolve_keys)
}

impl Session {
    fn run(config: &str) -> Self {
        let form = format!(
            "(progn {PRELUDE} {config} (ewm--send-intercept-keys)
  (prin1 (list :keys keys :sent sent :config (ewm--config))))"
        );
        module::from_lisp(&emacs(&["--eval", &form]))
    }
}

/// A session's slot, filled once by whoever asks first.
type Slot = Arc<OnceLock<Arc<Session>>>;

/// The session `config` leaves behind, run once per distinct config. The map is held only to
/// find the slot, two configs run their Emacs side by side.
pub fn session(config: &str) -> Arc<Session> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    let slot = SESSIONS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(config.to_owned())
        .or_default()
        .clone();
    slot.get_or_init(|| Arc::new(Session::run(config))).clone()
}
