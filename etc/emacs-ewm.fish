# Source from fish config to track CWD in EWM surface buffers.
# Add to ~/.config/fish/config.fish:
#   source /path/to/ewm/etc/emacs-ewm.fish

if test -n "$WAYLAND_DISPLAY"
    function __ewm_cwd_hook --on-event fish_prompt
        command emacsclient -e "(ewm--report-cwd $fish_pid \"$PWD\")" &>/dev/null &
        disown
    end
end
