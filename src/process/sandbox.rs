//! The bubblewrap launcher: how a [`Plan`] becomes a sandboxed process.
//!
//! Pure mechanism. Authorization and path validation already happened in
//! [`crate::process::policy`]; this module only turns resolved directory
//! capabilities into `--bind-fd` arguments and arranges descriptor inheritance.
//!
//! Descriptor discipline:
//!
//! * source directories are opened as `File`s and stay owned by [`Prepared`]
//!   until after `spawn`, so the numbers passed to bubblewrap are never stale;
//! * `pre_exec` only clears `FD_CLOEXEC` in the forked child, using
//!   async-signal-safe `fcntl` calls and touching no shared state;
//! * bubblewrap closes each `--bind-fd` source itself after mounting it, so the
//!   export descriptors never become visible inside the sandbox.

use crate::{core::error::Error, process::policy::Plan};
use rustix::io::{fcntl_getfd, fcntl_setfd, FdFlags};
use std::{fs::File, os::fd::AsRawFd, process::Stdio};

/// A launcher command plus the descriptors it still needs to inherit.
pub struct Prepared {
    pub command: tokio::process::Command,
    pub directories: Vec<File>,
}

/// Build the launcher invocation for an authorized plan.
///
/// `lock` is retained by bubblewrap's monitor through `--sync-fd`, which keeps
/// the exclusive export lock alive across a daemon crash.
pub fn assemble(plan: &Plan, lock: File) -> Result<Prepared, Error> {
    let mut directories = Vec::with_capacity(plan.mounts.len() + 1);
    let mut command = base();
    for mount in &plan.mounts {
        let directory = mount.export.open_dir(&mount.path)?;
        command
            .arg(if mount.writable {
                "--bind-fd"
            } else {
                "--ro-bind-fd"
            })
            .arg(directory.as_raw_fd().to_string())
            .arg(&mount.at);
        directories.push(directory);
    }
    command.arg("--sync-fd").arg(lock.as_raw_fd().to_string());
    directories.push(lock);
    command
        .args(["--chdir", &plan.cwd, "--", &plan.command])
        .args(&plan.args);
    inherit_fds(&mut command, &directories);
    Ok(Prepared {
        command,
        directories,
    })
}

/// Check that this host can actually run the launcher we advertise.
pub async fn probe() -> Result<(), String> {
    let directory = File::open("/tmp").map_err(|error| error.to_string())?;
    let mut command = base();
    command
        .arg("--ro-bind-fd")
        .arg(directory.as_raw_fd().to_string())
        .arg("/probe");
    inherit_fds(&mut command, std::slice::from_ref(&directory));
    command.args(["--", "/bin/true"]);
    match tokio::time::timeout(std::time::Duration::from_secs(5), command.output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => Err(format!(
            "Execution launcher failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )),
        _ => Err(
            "Execution requires working bubblewrap with fd mounts and user namespace support"
                .into(),
        ),
    }
}

/// The isolated runtime every command starts from.
fn base() -> tokio::process::Command {
    let mut command = tokio::process::Command::new("/usr/bin/bwrap");
    command.args([
        "--unshare-all",
        "--unshare-user",
        "--die-with-parent",
        "--new-session",
        "--disable-userns",
        "--cap-drop",
        "ALL",
    ]);
    for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
        command.args(["--ro-bind-try", path, path]);
    }
    command.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    command
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", "/tmp")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Make the capability descriptors inheritable by the launcher only.
fn inherit_fds(command: &mut tokio::process::Command, directories: &[File]) {
    let numbers: Vec<i32> = directories.iter().map(AsRawFd::as_raw_fd).collect();
    // SAFETY: the closure runs after `fork` and calls only async-signal-safe
    // `fcntl` operations on descriptors this process still owns.
    unsafe {
        command.pre_exec(move || {
            for number in &numbers {
                let descriptor = std::os::fd::BorrowedFd::borrow_raw(*number);
                let flags = fcntl_getfd(descriptor)?;
                fcntl_setfd(descriptor, flags & !FdFlags::CLOEXEC)?;
            }
            Ok(())
        });
    }
}
