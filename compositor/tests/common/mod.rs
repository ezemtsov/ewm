//! Shared helpers for integration tests.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

/// Run `emacs -Q -batch` with `lisp/` on the load path and return its stdout.
pub fn emacs(args: &[&str]) -> String {
    let lisp_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).with_file_name("lisp");
    let output = Command::new("emacs")
        .args(["-Q", "-batch", "-L", lisp_dir.to_str().unwrap()])
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

/// Run an ERT suite from the repo's `tests/` directory.
pub fn run_ert_suite(test_file_name: &str) {
    let test_file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .with_file_name("tests")
        .join(test_file_name);
    emacs(&[
        "-l",
        test_file.to_str().unwrap(),
        "-f",
        "ert-run-tests-batch-and-exit",
    ]);
}

/// Compare the generated file at repo-relative `path` with `expected`, rewriting it under
/// `EWM_UPDATE_FIXTURES`.
pub fn assert_generated(path: &str, expected: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(path);
    if std::env::var_os("EWM_UPDATE_FIXTURES").is_some() {
        std::fs::write(&path, expected).unwrap();
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        actual == expected,
        "{} is stale; rerun with EWM_UPDATE_FIXTURES=1",
        path.display()
    );
}
