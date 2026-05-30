#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd "$script_dir/.." && pwd -P)"

input="${1:-${EMACS_TREE:-$HOME/git/emacs}}"
base_ref="${EWM_SKIA_BASE_REF:-emacs-30.2}"
jobs="${EWM_SKIA_JOBS:-8}"

if [[ -d "$input" ]]; then
    emacs_tree="$(cd "$input" && pwd -P)"
    emacs_bin="$emacs_tree/src/emacs"
else
    emacs_bin="$(cd "$(dirname "$input")" && pwd -P)/$(basename "$input")"
    emacs_tree="$(cd "$(dirname "$emacs_bin")/.." && pwd -P)"
fi

if [[ ! -d "$emacs_tree/.git" ]]; then
    echo "usage: $0 [EMACS_TREE|EMACS_BINARY]" >&2
    echo "error: not an Emacs git tree: $emacs_tree" >&2
    exit 2
fi

quote() {
    printf "%q" "$1"
}

run() {
    printf '==> %s\n' "$*"
    "$@"
}

nix_run() {
    local command="$1"
    printf '==> nix-shell %s --run %s\n' "$script_dir/shell.nix" "$command"
    nix-shell "$script_dir/shell.nix" --run "$command"
}

tmp_dir="$(mktemp -d -t ewm-skia-check.XXXXXX)"
cleanup() {
    rm -rf "$tmp_dir"
}
trap cleanup EXIT

new_files_patch="$script_dir/emacs-30.2-pgtk-skia-sources.patch"
production_patch="$script_dir/emacs-30.2-pgtk-skia-integration.patch"
test_hooks_patch="$script_dir/emacs-30.2-pgtk-skia-test-hooks.patch"

if [[ -z "${EWM_SKIA_CHECK_SKIP_BUILD:-}" ]]; then
    nix_run "make -C $(quote "$emacs_tree") -j$jobs"
fi

run git -C "$emacs_tree" diff --check -- \
    src/pgtkterm.c src/skia/emacs_skia.cpp \
    src/skia/emacs_skia.h src/pgtkterm.h
run git -C "$repo_root" diff --check -- \
    emacs/skia-smoke.sh emacs/skia-profile.sh nix/service.nix

if rg -n "pgtk-skia-test-|Spgtk_skia_test" "$production_patch"; then
    echo "error: production patch contains local Skia test hooks" >&2
    exit 1
fi

run git -C "$emacs_tree" apply --check --reverse "$new_files_patch"
run git -C "$emacs_tree" apply --check --reverse "$test_hooks_patch"

patch_tmp="$(mktemp -d -p "$tmp_dir" split-patchcheck.XXXXXX)"
git -C "$emacs_tree" archive "$base_ref" | tar -x -C "$patch_tmp"
test ! -e "$patch_tmp/src/skia"
run git -C "$patch_tmp" apply --check "$new_files_patch"
run git -C "$patch_tmp" apply "$new_files_patch"
test -e "$patch_tmp/src/skia/emacs_skia.cpp"
test -e "$patch_tmp/src/skia/emacs_skia.h"
test -e "$patch_tmp/src/skiafont.c"
run git -C "$patch_tmp" apply --check "$production_patch"
run git -C "$patch_tmp" apply "$production_patch"
run git -C "$patch_tmp" apply --check "$test_hooks_patch"

run nix-instantiate --parse "$repo_root/nix/service.nix" >/dev/null

if [[ -z "${EWM_SKIA_CHECK_SKIP_SVG_SYNTAX:-}" ]]; then
    svg_check="$tmp_dir/svg-syntax-check.sh"
    cat > "$svg_check" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

cd "$1/src"
compile_cmd="$(
    make -n -W skia/emacs_skia.cpp \
        -o ../config.status -o ../Makefile -o Makefile \
        skia/emacs_skia.o |
    sed -n '/^g++ -c /p' |
    tail -n 1
)"
if [[ -z "$compile_cmd" ]]; then
    echo "error: could not extract emacs_skia.cpp compile command" >&2
    exit 1
fi

compile_cmd="${compile_cmd#*g++ -c }"
compile_cmd="$(
    printf '%s\n' "$compile_cmd" |
    sed 's/-MMD -MF deps\/skia\/emacs_skia\.d -MP//g; s/-o skia\/emacs_skia\.o/-o \/tmp\/emacs_skia_svg.o/'
)"

# shellcheck disable=SC2086
g++ -DSK_SVG=1 -fsyntax-only -c $compile_cmd
EOF
    chmod +x "$svg_check"
    nix_run "$(quote "$svg_check") $(quote "$emacs_tree")"
fi

if [[ -z "${EWM_SKIA_CHECK_SKIP_SMOKE:-}" ]]; then
    if [[ ! -x "$emacs_bin" ]]; then
        echo "error: Emacs executable not found: $emacs_bin" >&2
        exit 1
    fi
    nix_run "$(quote "$script_dir/skia-smoke.sh") $(quote "$emacs_bin")"
fi

echo "ok: Skia patch stack checks passed"
