//! Runs the elisp ERT cursor suite under `cargo test`.

mod common;

#[test]
fn ewm_cursor_ert_suite() {
    common::run_ert_suite("ewm-cursor-tests.el");
}
