//! Shared helpers for integration tests.

use std::path::PathBuf;
use std::process::Command;

/// Spawn `emacs -Q -batch` to run an ERT suite from the repo's `tests/`
/// directory. Panics with the captured stdout/stderr on failure.
pub fn run_ert_suite(test_file_name: &str) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.parent().expect("compositor/ has a parent");
    let lisp_dir = repo_root.join("lisp");
    let test_file = repo_root.join("tests").join(test_file_name);

    let output = Command::new("emacs")
        .args([
            "-Q",
            "-batch",
            "-L",
            lisp_dir.to_str().unwrap(),
            "-l",
            test_file.to_str().unwrap(),
            "-f",
            "ert-run-tests-batch-and-exit",
        ])
        .output()
        .expect("failed to spawn `emacs`; ensure it is on PATH");

    if !output.status.success() {
        panic!(
            "ERT suite {} failed (exit={:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            test_file_name,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
