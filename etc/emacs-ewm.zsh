# Source from zsh config to track CWD in EWM surface buffers.
# Add to ~/.zshrc:
#   source /path/to/ewm/etc/emacs-ewm.zsh

if [[ -n "$WAYLAND_DISPLAY" ]]; then
    __ewm_cwd_hook() {
        command emacsclient -e "(ewm--report-cwd $$ \"$PWD\")" &>/dev/null &!
    }
    add-zsh-hook precmd __ewm_cwd_hook
fi
