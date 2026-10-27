//! Runs the elisp ERT suites under `cargo test`.

mod common;

#[test]
fn ewm_focus_ert_suite() {
    common::run_ert_suite("ewm-focus-tests.el");
}

#[test]
fn ewm_input_ert_suite() {
    common::run_ert_suite("ewm-input-tests.el");
}

#[test]
fn ewm_compat_ert_suite() {
    common::run_ert_suite("ewm-compat-tests.el");
}

#[test]
fn ewm_text_input_ert_suite() {
    common::run_ert_suite("ewm-text-input-tests.el");
}

#[test]
fn ewm_event_ert_suite() {
    common::run_ert_suite("ewm-event-tests.el");
}

#[test]
fn ewm_cursor_ert_suite() {
    common::run_ert_suite("ewm-cursor-tests.el");
}
