#!/usr/bin/env bash
cargo build --release --features screencast && EWM_MODULE_PATH="$(dirname "$0")/target/release/libewm_core.so" emacs --fg-daemon=vt2 -L "$(dirname "$0")/../lisp" -l ewm -f ewm-start-module
