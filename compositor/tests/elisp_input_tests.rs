//! Runs the elisp ERT input suite under `cargo test`.

mod common;

#[test]
fn ewm_input_ert_suite() {
    common::run_ert_suite("ewm-input-tests.el");
}

#[test]
fn ewm_text_input_ert_suite() {
    common::run_ert_suite("ewm-text-input-tests.el");
}

#[test]
fn ewm_event_ert_suite() {
    common::run_ert_suite("ewm-event-tests.el");
}
