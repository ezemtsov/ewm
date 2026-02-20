# Process Isolation via Systemd Scopes

## Problem

EWM's compositor runs as a thread inside Emacs — they share a single process
and therefore a single cgroup. When Emacs spawns a child application (e.g. a
browser via `ewm-launch-app`), that child inherits the same cgroup.

If the child application consumes excessive memory and triggers the Linux OOM
killer, the kernel may kill the entire cgroup — taking down Emacs and the
compositor along with the misbehaving application.

## Solution

Place each spawned application into its own **systemd transient scope** via the
`StartTransientUnit` D-Bus API. Transient scopes create isolated cgroups, so
the OOM killer can terminate a runaway application without affecting the
Emacs+compositor process.

## Prior Art

Niri solves the same problem using a double-fork technique: it forks an
intermediate process, which forks the actual application, then calls
`StartTransientUnit` with the grandchild PID before letting it proceed. This
ensures the child is never in the compositor's cgroup, even briefly.

## Design

EWM can take a simpler approach because Emacs has built-in D-Bus support and
direct access to child PIDs.

### Spawn Flow

1. Spawn the process normally (e.g. `start-process`)
2. Retrieve the child PID via `process-id`
3. Call `org.freedesktop.systemd1.Manager.StartTransientUnit` via
   `dbus-call-method` to create a transient scope (e.g. `app-ewm-firefox.scope`)
   containing that PID

### Race Window

There is a small window between spawn and scope assignment where the child
lives in the Emacs cgroup. If this matters in practice, the child can be
spawned stopped (SIGSTOP), assigned to its scope, then resumed (SIGCONT).

In practice the race is unlikely to cause problems — the OOM killer would need
to trigger in the milliseconds between spawn and scope creation.

### Scope

This is purely an Elisp concern. No compositor/Rust changes are needed. The
implementation would wrap the process-spawning path in `ewm-launch-app` (or a
lower-level utility) to add scope assignment after spawn.

### Prerequisites

- EWM must be running as a systemd user service (already the case in the Nix
  configuration)
- `dbus` Emacs package (built-in)
