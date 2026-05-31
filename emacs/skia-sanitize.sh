#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"

emacs_tree="${1:-${EMACS_TREE:-$HOME/git/emacs}}"
build_dir="${2:-${EWM_SKIA_SANITIZE_BUILD_DIR:-/tmp/ewm-skia-asan-emacs}}"
jobs="${EWM_SKIA_JOBS:-8}"
smoke_groups="${EWM_SKIA_SANITIZE_SMOKE_GROUPS:-frames,bell,recovery,images,text,copy,export}"
sanitize_flags="${EWM_SKIA_SANITIZE_FLAGS:--O1 -g -fsanitize=address,undefined -fno-sanitize=nonnull-attribute -fno-omit-frame-pointer}"
detect_leaks="${EWM_SKIA_SANITIZE_LEAKS:-0}"
lsan_suppressions="${EWM_SKIA_LSAN_SUPPRESSIONS:-$script_dir/skia-lsan.supp}"
# Emacs' conservative stack scanner needs the real stack, not ASan's fake
# stack used for use-after-return detection.
asan_options="${ASAN_OPTIONS:-detect_leaks=$detect_leaks:abort_on_error=1:strict_string_checks=1:detect_stack_use_after_return=0}"
lsan_options="${LSAN_OPTIONS:-suppressions=$lsan_suppressions:print_suppressions=0}"
ubsan_options="${UBSAN_OPTIONS:-halt_on_error=1:print_stacktrace=1}"
configure_args=(
    --with-pgtk
    --with-skia
    --without-native-compilation
    --without-imagemagick
    --without-tree-sitter
)

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

config_stamp="$build_dir/.ewm-skia-sanitize-config"
config_signature="$(
    printf 'emacs_tree=%s\n' "$emacs_tree"
    printf 'CFLAGS=%s\n' "$sanitize_flags"
    printf 'CXXFLAGS=%s\n' "$sanitize_flags"
    printf 'LDFLAGS=%s\n' "-fsanitize=address,undefined"
    printf 'arg=%s\n' "${configure_args[@]}"
)"

if [[ ! -x "$build_dir/config.status" ]] \
    || [[ "$(cat "$config_stamp" 2>/dev/null || true)" != "$config_signature" ]]; then
    (
        cd "$build_dir"
        CFLAGS="$sanitize_flags" \
        CXXFLAGS="$sanitize_flags" \
        LDFLAGS="-fsanitize=address,undefined" \
        "$emacs_tree/configure" "${configure_args[@]}"
    )
    printf '%s\n' "$config_signature" > "$config_stamp"
else
    (cd "$build_dir" && ./config.status >/dev/null)
fi

if [[ -z "${EWM_SKIA_SANITIZE_SKIP_BUILD:-}" ]]; then
    ASAN_OPTIONS="$asan_options" \
    LSAN_OPTIONS="$lsan_options" \
    UBSAN_OPTIONS="$ubsan_options" \
        make -C "$build_dir/src" -j"$jobs" all
fi

if [[ ! -x "$build_dir/src/emacs" ]]; then
    echo "error: sanitized Emacs executable not found: $build_dir/src/emacs" >&2
    exit 1
fi

ASAN_OPTIONS="$asan_options" \
LSAN_OPTIONS="$lsan_options" \
UBSAN_OPTIONS="$ubsan_options" \
EWM_SKIA_SMOKE_GROUPS="$smoke_groups" \
    "$script_dir/skia-smoke.sh" "$build_dir/src/emacs"
