#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
init_args=()
emacs_path="emacs"

if [[ $# -gt 2 ]]; then
    echo "usage: $0 [INIT_DIRECTORY] [EMACS_PATH]" >&2
    exit 2
fi

if [[ $# -ge 1 && -n "$1" ]]; then
    init_args=("--init-directory=$1")
fi

if [[ $# -eq 2 ]]; then
    emacs_path="$2"
fi

if [[ -d "$emacs_path" ]]; then
    emacs_path="$emacs_path/src/emacs"
fi

if [[ "$emacs_path" == */* && ! -x "$emacs_path" ]]; then
    echo "error: Emacs executable not found: $emacs_path" >&2
    exit 1
fi

cargo build --release --features screencast
export RUST_LOG="${RUST_LOG:-ewm=debug,smithay=warn}"
EWM_MODULE_PATH="$script_dir/target/release/libewm_core.so" \
    exec "$emacs_path" --fg-daemon=vt2 \
    -L "$script_dir/../lisp" \
    --eval '(setq load-prefer-newer t)' \
    -l ewm \
    -f ewm-start-module \
    "${init_args[@]}"
