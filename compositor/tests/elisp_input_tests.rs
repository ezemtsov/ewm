//! Runs the elisp ERT input suite under `cargo test`.

mod common;

#[test]
fn ewm_input_ert_suite() {
    common::run_ert_suite("ewm-input-tests.el");
}
