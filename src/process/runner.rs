//! Command supervision: spawn, bound, reap, then reopen the file APIs.
//!
//! The runner owns the exclusive file gate from the moment admission succeeds
//! until the command has been reaped, the output drained and every revision
//! invalidated. If ownership of the process becomes uncertain, the gate is
//! poisoned rather than released — file APIs must stay closed, but the lock is
//! not leaked.

use crate::{
    app::State,
    process::{
        model::{Outcome, Process},
        sandbox::Prepared,
    },
};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Child,
    sync::OwnedRwLockWriteGuard,
};
use tokio_util::sync::CancellationToken;

/// How long the output pipes may stay open after the command itself ended.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-stream output cap; exceeding it cancels the command.
const OUTPUT_LIMIT: usize = 64 * 1024;
const READ_CHUNK: usize = 4 * 1024;

pub async fn run(
    state: Arc<State>,
    mut prepared: Prepared,
    process: Arc<Process>,
    gate: OwnedRwLockWriteGuard<()>,
    timeout_ms: u64,
) {
    if process.cancelled() {
        process.cancelled_before_start();
        return;
    }
    let mut child = match prepared.command.spawn() {
        Ok(child) => child,
        Err(_) => {
            process.spawn_failed();
            return;
        }
    };
    // The launcher inherited the descriptors; the parent can let them go.
    drop(prepared.directories);
    let stdout = child.stdout.take().expect("base() pipes stdout");
    let stderr = child.stderr.take().expect("base() pipes stderr");
    let out_reader = tokio::spawn(output(stdout, process.token().clone()));
    let err_reader = tokio::spawn(output(stderr, process.token().clone()));

    match supervise(&mut child, process.token(), timeout_ms).await {
        Ok((terminal, code)) => {
            let (stdout, stdout_truncated) = drain(out_reader).await;
            let (stderr, stderr_truncated) = drain(err_reader).await;
            // The command may have written through a bind mount, so every
            // revision minted before it ran must be considered stale. This
            // happens while the gate is still held.
            state.exports.invalidate_revisions();
            drop(gate);
            process.complete(Outcome {
                state: terminal,
                code,
                stdout,
                stderr,
                truncated: stdout_truncated || stderr_truncated,
            });
        }
        Err(()) => {
            // Reaping failed: process ownership is unknown, so keep the
            // exports closed instead of reopening them.
            state.files.poison();
            drop(gate);
            process.unreaped();
        }
    }
}

/// Wait for the command, or stop it when the caller cancels or the deadline
/// passes. `Err` means the reap failed and ownership is uncertain.
async fn supervise(
    child: &mut Child,
    cancel: &CancellationToken,
    timeout_ms: u64,
) -> Result<(&'static str, Option<i32>), ()> {
    tokio::select! {
        status = child.wait() => match status {
            Ok(status) => Ok(("exited", status.code())),
            Err(_) => Err(()),
        },
        _ = cancel.cancelled() => stop(child).await.map(|()| ("cancelled", None)),
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
            stop(child).await.map(|()| ("timed-out", None))
        }
    }
}

/// `kill` sends `SIGKILL` and reaps the child.
async fn stop(child: &mut Child) -> Result<(), ()> {
    child.kill().await.map_err(|_| ())
}

/// Collect a stream's bounded output, or report it truncated when the reader
/// outlives [`DRAIN_TIMEOUT`] (a descendant holding the inherited pipe open).
async fn drain(reader: tokio::task::JoinHandle<(String, bool)>) -> (String, bool) {
    match tokio::time::timeout(DRAIN_TIMEOUT, reader).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) | Err(_) => (String::new(), true),
    }
}

/// Read one stream until EOF, capping the retained bytes.
async fn output(mut reader: impl AsyncRead + Unpin, cancel: CancellationToken) -> (String, bool) {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => {
                truncated = true;
                cancel.cancel();
                break;
            }
        };
        let keep = read.min(OUTPUT_LIMIT.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..keep]);
        if keep < read {
            truncated = true;
            cancel.cancel();
        }
    }
    (String::from_utf8_lossy(&bytes).into_owned(), truncated)
}
