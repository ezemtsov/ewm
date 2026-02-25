# Source from bash config to track CWD in EWM surface buffers.
# Add to ~/.bashrc:
#   source /path/to/ewm/etc/emacs-ewm.bash

if [ -n "$WAYLAND_DISPLAY" ]; then
    __ewm_cwd_hook() {
        command emacsclient -e "(ewm--report-cwd $$ \"$PWD\")" &>/dev/null &
        disown
    }
    PROMPT_COMMAND="__ewm_cwd_hook${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
fi
