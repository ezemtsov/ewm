#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
init_args=()

if [[ $# -gt 1 ]]; then
    echo "usage: $0 [INIT_DIRECTORY]" >&2
    exit 2
fi

if [[ $# -eq 1 ]]; then
    init_args=("--init-directory=$1")
fi

cargo build --release --features screencast
EWM_MODULE_PATH="$script_dir/target/release/libewm_core.so" \
    exec emacs --fg-daemon=vt2 \
    -L "$script_dir/../lisp" \
    --eval '(setq load-prefer-newer t)' \
    -l ewm \
    -f ewm-start-module \
    "${init_args[@]}"
