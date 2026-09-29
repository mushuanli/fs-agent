//! Mutual exclusion between the file APIs and the process API.
//!
//! File work takes a shared guard; starting a command takes the exclusive
//! guard and keeps it until the command has been reaped. Every failure to
//! acquire a guard is reported as `EBUSY`, never by blocking a request.
//!
//! If a command cannot be reaped, its process ownership is unknown, so the
//! gate is *poisoned* instead of releasing the guard: every later file or
//! process request fails closed with `EIO` until the operator restarts. This
//! keeps the safety of never reopening the exports without leaking a lock.

use crate::core::error::Error;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

/// Cloneable handle to the process-wide file/process gate.
#[derive(Clone, Default)]
pub struct FileGate {
    inner: Arc<RwLock<()>>,
    poisoned: Arc<AtomicBool>,
}

impl FileGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Shared access for reads, listings and revisions.
    pub fn shared(&self) -> Result<OwnedRwLockReadGuard<()>, Error> {
        self.check()?;
        self.inner
            .clone()
            .try_read_owned()
            .map_err(|_| Error::busy())
    }

    /// Exclusive access while a command owns the exports.
    pub fn exclusive(&self) -> Result<OwnedRwLockWriteGuard<()>, Error> {
        self.check()?;
        self.inner
            .clone()
            .try_write_owned()
            .map_err(|_| Error::busy())
    }

    /// Permanently close the gate after an unreaped command.
    pub fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// Wait for every in-flight file operation; used only during shutdown.
    pub async fn drain(&self) -> OwnedRwLockWriteGuard<()> {
        self.inner.clone().write_owned().await
    }

    fn check(&self) -> Result<(), Error> {
        if self.is_poisoned() {
            return Err(Error::unavailable());
        }
        Ok(())
    }
}
