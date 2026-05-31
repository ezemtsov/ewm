#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"

emacs_tree="${1:-${EMACS_TREE:-$HOME/git/emacs}}"
base_ref="${EWM_SKIA_BASE_REF:-emacs-30.2}"

if [[ ! -d "$emacs_tree/.git" ]]; then
    echo "usage: $0 [EMACS_TREE]" >&2
    echo "error: not an Emacs git tree: $emacs_tree" >&2
    exit 2
fi

emacs_tree="$(cd "$emacs_tree" && pwd -P)"

production_patch="$script_dir/emacs-30.2-pgtk-skia-integration.patch"
new_files_patch="$script_dir/emacs-30.2-pgtk-skia-sources.patch"
test_hooks_patch="$script_dir/emacs-30.2-pgtk-skia-test-hooks.patch"

tmp_dir="$(mktemp -d -t ewm-skia-refresh.XXXXXX)"
patch_tmp=""
cleanup() {
    if [[ -z "${EWM_SKIA_KEEP_TMP:-}" ]]; then
        rm -rf "$tmp_dir"
        if [[ -n "$patch_tmp" ]]; then
            rm -rf "$patch_tmp"
        fi
    fi
}
trap cleanup EXIT

git -C "$emacs_tree" rev-parse --verify "$base_ref" >/dev/null

mkdir -p "$tmp_dir/production/src" "$tmp_dir/hooked/src"
cp "$emacs_tree/src/pgtkterm.c" "$tmp_dir/production/src/pgtkterm.c"
cp "$emacs_tree/src/pgtkterm.c" "$tmp_dir/hooked/src/pgtkterm.c"
cp "$emacs_tree/src/pgtkfns.c" "$tmp_dir/production/src/pgtkfns.c"
cp "$emacs_tree/src/pgtkfns.c" "$tmp_dir/hooked/src/pgtkfns.c"

perl -0pi -e \
    's/\n(?:static Lisp_Object\npgtk_skia_test_pointer_id \(void \*ptr\)\n\{.*?\}\n\n)?DEFUN \("pgtk-skia-test-(?:context-state|framebuffer-color-counts)",.*?\n#endif\n\nvoid\nsyms_of_pgtkterm/\n#endif\n\nvoid\nsyms_of_pgtkterm/s' \
    "$tmp_dir/production/src/pgtkterm.c"
perl -ni -e 'print unless /Spgtk_skia_test_/' \
    "$tmp_dir/production/src/pgtkterm.c"
perl -0pi -e \
    's/\n#ifdef USE_SKIA\nDEFUN \("pgtk-skia-test-current-tooltip-frame",.*?\n#endif\n(?=\n\/\* Return geometric attributes)//s' \
    "$tmp_dir/production/src/pgtkfns.c"
perl -0pi -e \
    's/\n#ifdef USE_SKIA\n  defsubr \(&Spgtk_skia_test_current_tooltip_frame\);\n#endif\n/\n/s' \
    "$tmp_dir/production/src/pgtkfns.c"

if rg -n "pgtk-skia-test-|Spgtk_skia_test" \
    "$tmp_dir/production/src/pgtkterm.c" \
    "$tmp_dir/production/src/pgtkfns.c" >/dev/null
then
    echo "error: failed to strip Skia test hooks from production sources" >&2
    exit 1
fi

(
    cd "$tmp_dir"
    git diff --no-index --no-prefix -- production hooked \
        > "$test_hooks_patch" || true
)

mkdir -p "$tmp_dir/a" "$tmp_dir/b"
git -C "$emacs_tree" archive "$base_ref" | tar -x -C "$tmp_dir/a"
git -C "$emacs_tree" archive "$base_ref" | tar -x -C "$tmp_dir/b"

git -C "$emacs_tree" diff --name-only --diff-filter=M "$base_ref" |
while IFS= read -r path; do
    mkdir -p "$tmp_dir/b/$(dirname "$path")"
    cp "$emacs_tree/$path" "$tmp_dir/b/$path"
done
cp "$tmp_dir/production/src/pgtkterm.c" "$tmp_dir/b/src/pgtkterm.c"
cp "$tmp_dir/production/src/pgtkfns.c" "$tmp_dir/b/src/pgtkfns.c"

(
    cd "$tmp_dir"
    git diff --no-index --no-prefix -- a b > "$production_patch" || true
)

mapfile -t new_files < <(
    git -C "$emacs_tree" diff --name-only --diff-filter=A "$base_ref"
)
if ((${#new_files[@]} == 0)); then
    : > "$new_files_patch"
else
    git -C "$emacs_tree" diff "$base_ref" \
        --output="$new_files_patch" -- "${new_files[@]}"
fi

if rg -n "pgtk-skia-test-|Spgtk_skia_test" "$production_patch" >/dev/null
then
    echo "error: production patch contains local Skia test hooks" >&2
    exit 1
fi

patch_tmp="$(mktemp -d -t ewm-skia-refresh-apply.XXXXXX)"
git -C "$emacs_tree" archive "$base_ref" | tar -x -C "$patch_tmp"
test ! -e "$patch_tmp/src/skia"
git -C "$patch_tmp" apply --check "$new_files_patch"
git -C "$patch_tmp" apply "$new_files_patch"
git -C "$patch_tmp" apply --check "$production_patch"
git -C "$patch_tmp" apply "$production_patch"
git -C "$patch_tmp" apply --check "$test_hooks_patch"

printf 'refreshed Skia patches from %s against %s\n' \
    "$emacs_tree" "$base_ref"
if [[ -n "${EWM_SKIA_KEEP_TMP:-}" ]]; then
    printf 'clean patch stack applies in %s\n' "$patch_tmp"
else
    printf 'clean patch stack applies\n'
fi
