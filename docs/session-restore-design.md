# Session Restore

## Goal

Start ewm again, after a logout, a crash or for a new Emacs or compositor
build, and come back to the same frames in the same strip order with their
workspace names, the same window layout inside each frame, the floats where
they were, the applications relaunched into the windows they occupied, and
the terminal processes still running.

Out of scope: keeping live Wayland surfaces across the restart. The
compositor is a thread of the Emacs process, a restart is a new process, the
socket and every client connection die with it. Surfaces are relaunched, not
kept. Qt 6.6 and later reconnect on their own when `QT_WAYLAND_RECONNECT` is
set and the socket name is stable (it is, `wayland-ewm-vt1`). GTK, Firefox,
Chromium and Electron exit.

## The mode is desktop-save-mode

`desktop-save-mode` is the mode. EWM makes
its frames and surfaces citizens of desktop.el, frameset.el and bookmark.el,
so a user who turns `desktop-save-mode` on gets the session back with no EWM
switch. `lisp/ewm-session.el` loads with `ewm-mode` and acts only where
`desktop-save-mode` is on. The recipe is on the wiki page Session Restore.

### Frames

The frame parameters are the layout, and frameset saves every parameter it
is not told to drop. A frame carries `ewm-output`, the connector it was made
for, `ewm-home`, the display identity and connector it belongs to,
`ewm-strip-position`, its slot in the strip, `ewm-workspace-name`, and a
float `ewm-floating`, `ewm-position`, its surface origin on the output, and
`ewm-size`, its surface size. `ewm-session--filter-placement` is the save filter of
the placement parameters, it replaces each value with what the compositor
reports at save, through `ewm-get-frame-output`, `ewm-get-frame-home`,
`ewm-get-strip-frame-ids` and `ewm-get-frame-origin`. The workspace name
needs no refresh, Lisp is its only source. The identities of one run are
never saved: `ewm-surface-id`, `ewm-after`, `ewm-focus-on-create`,
`auto-hide-function`, `ewm-floating-origin-frame-id`, and `display`, because
frameset keeps the saved display under a daemon and a session on another
socket would fail every frame.

frameset makes a restored frame invisible with its geometry, applies the rest
of the parameters, puts the window state and shows it. The compositor
announces the surface, Lisp pairs it with the frame and places it from the
parameters with `ewm-place-frame-module`: on `ewm-output`, falling back to
the active output, parked until an output maps when there is none, before the
placed frame whose saved position is the next one, so the strip comes back in
order whatever the creation order, a float at its position and size, and
homed at `ewm-home`, which the compositor reclaims when that display
attaches. Every frame restored by frameset or made by plain `make-frame` goes
through the same placement, frameset needs nothing of EWM.

### Buffers and applications

A surface buffer is the launch record of its application: `desktop-save-buffer`
returns `(:app A :command C :directory D)` read from `/proc/PID/cmdline` and
`/proc/PID/cwd`, with a one-element argv re-split because Electron rewrites
it, and `bookmark-make-record-function` returns the same record under the
`EWM` handler type. Desktop, bookmark and everything built on bookmarks
persist applications through one function. This breaks for sandboxed clients
and for multi-process apps whose connecting process is not the one launched.

The desktop handler of a surface buffer makes a placeholder under the saved
name, a buffer saying the application is not ready whose `g` runs the command
again, and the placeholder is itself a desktop record, so it survives a
second restart. Once the desktop is read every placeholder relaunches its
command with a quiet `ewm-launch-hint`, one process per command, the `g`
relaunch and the bookmark jump also name the selected frame in it. When the
surface maps, `ewm-place-surface-functions` finds the placeholder by the token
in `ewm-surface-token`, then by pid ancestry, then by app id, shown
placeholders first, shows the buffer in the windows of the placeholder without
selecting it, or leaves it buried as the placeholder was, and kills the
placeholder.

What the window contains is the session restore of the application itself.
Terminals are el-rio's own: rio-mode saves `(:socket S :directory D)` through
`desktop-save-buffer`, and under `rio-detach` the shell runs under dtach in a
transient user scope outside the `ewm.service` cgroup, so the handler
reattaches it after the restart instead of opening a fresh shell. frameset
binds a window to its buffer by the name desktop saved, and desktop renames
the buffer a handler returns to that name, so a buffer that renames itself
again inside its handler comes back restored but buried. A window whose
buffer did not come back is left to `window-restore-killed-buffer-windows`.

### Saving

`desktop-kill` sits on `kill-emacs-query-functions`, which `kill-emacs` never
runs, and the desktop auto-save is gated on owning the lock. EWM leaves both
as they are, whether and when the desktop is saved is the `desktop-save` and
`desktop-auto-save-timeout` of the user. An exit through
`save-buffers-kill-emacs` saves or asks as `desktop-save` says and a refusal
keeps the session running, `ewm-restart` runs the same queries before
it exits. A stop from outside, the SIGTERM of `systemctl`, leaves the file as
the last auto-save wrote it, as for any Emacs daemon.

### Startup

`desktop-read` runs at `after-init-hook` and refuses frames while the
terminal frame of the daemon is selected, so the first output keeps its
startup frame and EWM selects it ahead of the read, the focus event of that
frame can still sit in the queue. While `desktop-save-mode` is on, a desktop
file exists and the read has not happened, `ewm-output-populate-functions`
leaves the other outputs empty. The mode goes on before `ewm-start-module` in
the init file, the gate reads it when the outputs complete during the start.
EWM binds two options around `desktop-read`. `desktop-load-locked-desktop` at
`check-pid` where it is `ask`, the default, a daemon cannot ask and the lock
of an Emacs that crashed would keep the desktop from loading until removed by
hand, `t` and nil are choices about the file of the user and stand.
`desktop-restore-reuses-frames` at nil, the frame frameset would reuse is the
terminal frame of the daemon, which shows nothing, and no frame of the user
exists at the read. Every saved frame is made afresh through the placement
and the cleanup of frameset deletes the startup frame, which `delete-frame`
lets go once a frame is in flight for its output.
`desktop-after-read-hook` and `after-init-hook` launch the placeholders and
fill the outputs still empty.

### Restart mechanics

`ewm-restart` offers modified files for saving, runs the exit queries
and exits Emacs itself with status 15, the status Emacs returns for the
SIGTERM of a stop. Emacs exits itself because a SIGTERM can land during a
garbage collection and then any error in a `kill-emacs-hook` function aborts
it. The unit decides what an exit means, as the GNOME Shell X11 unit does.
`SuccessExitStatus=15` keeps a stop and this exit out of the failure log,
`Restart=on-failure` brings a crash back, `RestartForceExitStatus=15` brings
the requested restart back, and a plain exit or `systemctl stop` ends the
unit, the documented meaning of both in `systemd.service(5)`.
`RestartMode=direct` keeps the unit out of the inactive state while it
restarts, the one `systemctl --user --wait start` of `ewm-session` returns
only when the unit has ended, and `graphical-session.target` stays up
throughout. After each exit the unit holds a `handle-lid-switch` lock for 8 s
from `ExecStopPost`, logind would suspend a closed laptop in the gap before
the next instance, and the hold expires on its own when the start is broken.
The start limit stays at the systemd default of five starts in ten seconds. A
NixOS switch leaves the unit alone, `nix/service.nix` marks it
`restartIfChanged = false` with `enableDefaultPath = false`, the drop-in this
makes carries no default `PATH` over the one the session imported, a stop
would end the login, and the new build applies at the next restart from
inside. The logind session and the VT stay, the unit starts the launcher
afresh, a new Emacs and a new module
come up. `restart-emacs` is no shortcut, it re-execs the running binary with
the inherited load path and can never pick up a new build. A client unit of
the session that restarts on failure needs a `RestartSec` long enough for the
new socket to appear, or it bursts its start limit while no compositor
listens.

## Compositor primitives

1. `ewm-place-frame-module`, the placement of an Emacs frame from Lisp once
   its surface has arrived, with output, slot, float geometry and home, the
   workspace name follows in the rename command. An Emacs toplevel is out of
   the layout, and unseen, until then.
2. `ewm-get-frame-output`, `ewm-get-frame-home`, `ewm-get-strip-frame-ids`
   and `ewm-get-frame-origin`, the snapshot queries the save filter reads.
3. The redeemed token echoed in `mapped`, kept on the surface from redeem to
   map. `new` is queued before a redeem can find the surface.
4. `ewm-create-launch-token` with a `PlacementHint` in the token user data,
   moved to the surface at redeem: host frame and `quiet`. At map the output
   of the host replaces the active one, a host is never a float, the hint
   only vetoes selection and never grants it. A token stays redeemable 180 s,
   enough for a two minute warm start, the focus grant stays 10 s.

Everything else is Elisp: the frameset filters, the desktop handlers, the
bookmark record and the placeholder in `lisp/ewm-session.el`, `ewm-restart` in
`lisp/ewm-commands.el`, and `ewm-output-populate-functions` and
`ewm-place-surface-functions` in `lisp/ewm.el`.

## Verification

`nix/vm-test.nix` runs the session under `desktop-save-mode` with
`desktop-save` at t, restarts it in place, restarts the service from outside
after a `desktop-save`, then logs out and in again, and checks each time that
a named frame comes back showing its file and a self-renaming buffer beside
the relaunched application and that a float comes back where it was, the
restarts in the same login. By hand on the tty2 test daemon: restart with the
lid closed, open the lid, the frames homed on eDP-1 return to the panel. A
terminal reattaches to its running process.
