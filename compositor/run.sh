#!/usr/bin/env bash
# Run EWM compositor with debug logging

export RUST_LOG=info

exec ./target/release/ewm "$@" 2>&1 | tee output.log
