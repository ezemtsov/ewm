//! Runs the elisp ERT fullscreen-policy suite under `cargo test`.

mod common;

#[test]
fn ewm_fullscreen_ert_suite() {
    common::run_ert_suite("ewm-fullscreen-tests.el");
}
