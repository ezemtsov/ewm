//! Runs the elisp ERT focus suite under `cargo test`.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn ewm_focus_ert_suite() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.parent().expect("compositor/ has a parent");
    let lisp_dir = repo_root.join("lisp");
    let test_file = repo_root.join("tests").join("ewm-focus-tests.el");

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
            "ERT suite failed (exit={:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
