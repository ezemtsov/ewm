# ext-workspace-v1

Emacs tabs are exposed as Wayland workspaces via ext-workspace-unstable-v1.

Each Emacs frame maps to a workspace group (one group per output).
Each tab within a frame maps to a workspace. The active tab becomes
the active workspace. Workspace names are 1-based indexes ("1", "2", ...).

External tools (waybar, DankMaterialShell) can display and switch
workspaces, which translates to switching Emacs tabs.

State is synced via a pull/refresh model (ported from niri):
`workspace::refresh()` runs every event-loop iteration, diffs Emacs
tab state against protocol mirrors, and sends only changes. A push
model (updating only on `OutputLayout` arrival) has timing bugs:
clients that bind before Emacs sends its first tab state get empty
groups and may never recover.
