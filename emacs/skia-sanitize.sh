#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"

emacs_tree="${1:-${EMACS_TREE:-$HOME/git/emacs}}"
build_dir="${2:-${EWM_SKIA_SANITIZE_BUILD_DIR:-/tmp/ewm-skia-asan-emacs}}"
jobs="${EWM_SKIA_JOBS:-8}"
smoke_groups="${EWM_SKIA_SANITIZE_SMOKE_GROUPS:-frames,bell,recovery,images}"
sanitize_flags="${EWM_SKIA_SANITIZE_FLAGS:--O1 -g -fsanitize=address,undefined -fno-omit-frame-pointer}"
asan_options="${ASAN_OPTIONS:-detect_leaks=0:abort_on_error=1:strict_string_checks=1}"
ubsan_options="${UBSAN_OPTIONS:-halt_on_error=1:print_stacktrace=1}"

if [[ $# -gt 2 ]]; then
    echo "usage: $0 [EMACS_TREE] [BUILD_DIR]" >&2
    exit 2
fi

if [[ ! -d "$emacs_tree/.git" ]]; then
    echo "error: not an Emacs git tree: $emacs_tree" >&2
    exit 2
fi

if [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
    echo "error: WAYLAND_DISPLAY is not set; run from an EWM/Wayland session" >&2
    exit 1
fi

mkdir -p "$build_dir"

if [[ ! -x "$build_dir/config.status" ]]; then
    (
        cd "$build_dir"
        CFLAGS="$sanitize_flags" \
        CXXFLAGS="$sanitize_flags" \
        LDFLAGS="-fsanitize=address,undefined" \
        "$emacs_tree/configure" \
            --with-pgtk \
            --with-skia \
            --without-native-compilation \
            --without-imagemagick
    )
fi

if [[ -z "${EWM_SKIA_SANITIZE_SKIP_BUILD:-}" ]]; then
    make -C "$build_dir/src" -j"$jobs" all
fi

if [[ ! -x "$build_dir/src/emacs" ]]; then
    echo "error: sanitized Emacs executable not found: $build_dir/src/emacs" >&2
    exit 1
fi

ASAN_OPTIONS="$asan_options" \
UBSAN_OPTIONS="$ubsan_options" \
EWM_SKIA_SMOKE_GROUPS="$smoke_groups" \
    "$script_dir/skia-smoke.sh" "$build_dir/src/emacs"
