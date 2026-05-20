//! Runs the elisp ERT focus suite under `cargo test`.

mod common;

#[test]
fn ewm_focus_ert_suite() {
    common::run_ert_suite("ewm-focus-tests.el");
}
