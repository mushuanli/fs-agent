//! Execution admission: whether commands may start, under which epoch, and the
//! descriptor that keeps the export fenced for the lifetime of the process.
//!
//! The epoch changes on every restart, so a handle from a previous run can
//! never be replayed. [`Execution::lock_handle`] returns the descriptor that
//! the launcher retains with `--sync-fd`: it keeps the advisory lock alive even
//! if this daemon dies while a command is still running.

use crate::{core::error::Error, process::model::Registry, process::sandbox};
use rustix::fs::FlockOperation;
use std::{
    fs::File,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, MutexGuard,
    },
};

pub struct Execution {
    registry: Registry,
    epoch: String,
    ready: AtomicBool,
    lock: Mutex<Option<File>>,
}

impl Execution {
    pub fn new(epoch: String) -> Self {
        Self {
            registry: Registry::default(),
            epoch,
            ready: AtomicBool::new(false),
            lock: Mutex::new(None),
        }
    }

    /// Epoch advertised to clients and required on every process request.
    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// Whether the launcher was probed successfully at startup.
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Admit commands; called once bootstrap probing succeeded.
    pub fn accept(&self) {
        self.ready.store(true, Ordering::Release);
    }

    /// Refuse new commands; called during shutdown.
    pub fn stop_accepting(&self) {
        self.ready.store(false, Ordering::Release);
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Retain `lock` for the process lifetime.
    pub fn retain_lock(&self, lock: File) {
        *self.lock() = Some(lock);
    }

    /// A duplicate of the retained descriptor, sharing the advisory lock.
    pub fn lock_handle(&self) -> Result<File, Error> {
        self.lock()
            .as_ref()
            .ok_or_else(Error::invalid)?
            .try_clone()
            .map_err(Error::from)
    }

    /// Ask every tracked command to stop; used during shutdown.
    pub fn cancel_all(&self) {
        self.registry.cancel_all();
    }

    fn lock(&self) -> MutexGuard<'_, Option<File>> {
        self.lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Bootstrap the execution capability, or explain why it is unavailable.
///
/// Failure is fatal by design: advertising `process.exec=true` without a
/// working sandbox would let a client believe it is contained when it is not.
pub async fn enable(state: &crate::app::State) -> Result<(), String> {
    let export = state
        .exports
        .first()
        .ok_or("Execution requires an export")?;
    let lock = export
        .try_clone_root()
        .map_err(|_| "Cannot retain execution lock")?;
    rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| "Another execution server still owns this export")?;
    sandbox::probe().await?;
    state.execution.retain_lock(lock);
    state.execution.accept();
    Ok(())
}
