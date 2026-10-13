//! Ported from niri's src/utils/spawning.rs: run a command the compositor never waits on.
use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::{io, thread};

use tracing::warn;

/// Spawns the command to run independently of the compositor.
pub(crate) fn spawn<T: AsRef<OsStr> + Send + 'static>(command: Vec<T>) {
    if command.is_empty() {
        return;
    }

    // Spawning and waiting takes some milliseconds, so do it in a thread.
    let res = thread::Builder::new()
        .name("Command Spawner".to_owned())
        .spawn(move || {
            let (command, args) = command.split_first().unwrap();
            spawn_sync(command, args);
        });

    if let Err(err) = res {
        warn!("error spawning a thread to spawn the command: {err:?}");
    }
}

fn spawn_sync(command: impl AsRef<OsStr>, args: impl IntoIterator<Item = impl AsRef<OsStr>>) {
    let command = command.as_ref();
    let mut process = Command::new(command);
    process
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    unsafe {
        // Double-fork to avoid having to waitpid the child.
        process.pre_exec(move || match libc::fork() {
            -1 => Err(io::Error::last_os_error()),
            0 => Ok(()),
            _ => libc::_exit(0),
        });
    }

    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(err) => {
            warn!("error spawning {command:?}: {err:?}");
            return;
        }
    };

    match child.wait() {
        Ok(status) if !status.success() => warn!("child did not exit successfully: {status:?}"),
        Ok(_) => {}
        Err(err) => warn!("error waiting for child: {err:?}"),
    }
}
