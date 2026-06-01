#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd "$script_dir/.." && pwd -P)"

input="${1:-${EMACS_TREE:-$HOME/git/emacs}}"
jobs="${EWM_SKIA_JOBS:-8}"
build_mode="${EWM_SKIA_CHECK_BUILD_MODE:-full}"
object_targets="${EWM_SKIA_CHECK_OBJECT_TARGETS:-pgtkterm.o pgtkfns.o image.o gtkutil.o skia/emacs_skia.o skiafont.o}"
smoke_groups="${EWM_SKIA_CHECK_SMOKE_GROUPS:-${EWM_SKIA_SMOKE_GROUPS:-}}"

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

if [[ -z "${EWM_SKIA_CHECK_SKIP_BUILD:-}" ]]; then
    case "$build_mode" in
        full)
            nix_run "make -C $(quote "$emacs_tree") -j$jobs"
            ;;
        src)
            nix_run "make -C $(quote "$emacs_tree/src") -j$jobs all"
            ;;
        objects)
            nix_run "make -C $(quote "$emacs_tree/src") -j$jobs $object_targets"
            ;;
        *)
            echo "error: unknown EWM_SKIA_CHECK_BUILD_MODE: $build_mode" >&2
            echo "expected one of: full, src, objects" >&2
            exit 2
            ;;
    esac
fi

test -e "$emacs_tree/src/skia/emacs_skia.cpp"
test -e "$emacs_tree/src/skia/emacs_skia.h"
test -e "$emacs_tree/src/skiafont.c"

run git -C "$emacs_tree" diff --check HEAD -- \
    src/pgtkterm.c src/pgtkfns.c src/skia/emacs_skia.cpp \
    src/skia/emacs_skia.h src/pgtkterm.h src/image.c
run git -C "$repo_root" diff --check HEAD -- \
    emacs/check-skia.sh emacs/skia-smoke.sh emacs/skia-profile.sh \
    emacs/skia-monitor.el emacs/skia-sanitize.sh emacs/skia-lsan.supp \
    nix/service.nix

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
    if rg -q "pgtk-skia-test-framebuffer-size" "$emacs_tree/src/pgtkterm.c"; then
        smoke_cmd="$(quote "$script_dir/skia-smoke.sh") $(quote "$emacs_bin")"
        if [[ -n "$smoke_groups" ]]; then
            smoke_cmd="EWM_SKIA_SMOKE_GROUPS=$(quote "$smoke_groups") $smoke_cmd"
        fi
        nix_run "$smoke_cmd"
    else
        echo "warning: skipping smoke tests; Skia test hooks are not present" >&2
    fi
fi

echo "ok: Skia source checks passed"
