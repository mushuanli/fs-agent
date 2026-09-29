//! Bounded execution of blocking filesystem work.
//!
//! Filesystem syscalls run on the blocking pool so a slow disk cannot stall
//! the reactor. Two policies are enforced here once, for every caller:
//!
//! * concurrency is capped by a semaphore, and
//! * every call has a deadline and a cancellation token that the blocking
//!   closure polls at its own safe points.

use crate::core::{error::Error, gate::FileGate};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// Cloneable handle to the shared blocking-worker budget.
#[derive(Clone)]
pub struct Workers {
    permits: Arc<Semaphore>,
}

impl Workers {
    pub fn new(limit: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(limit.max(1))),
        }
    }

    /// Reserve a slot without starting work; used by streaming uploads that
    /// hold the slot across awaits.
    pub async fn acquire(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<OwnedSemaphorePermit, Error> {
        tokio::time::timeout_at(deadline, self.permits.clone().acquire_owned())
            .await
            .map_err(|_| Error::timed_out())?
            .map_err(|_| Error::unavailable())
    }

    /// Run `work` on the blocking pool while holding a shared file gate.
    ///
    /// The gate is acquired before the worker slot, so a command's exclusive
    /// guard is never queued behind a slot that can never be released.
    pub async fn run<T: Send + 'static>(
        &self,
        gate: &FileGate,
        deadline: tokio::time::Instant,
        work: impl FnOnce(CancellationToken) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let _guard = gate.shared()?;
        self.run_ungated(deadline, work).await
    }

    /// Run `work` on the blocking pool without acquiring a gate.
    ///
    /// The caller must already hold a shared gate and keep it alive for the
    /// whole call, which is what streaming downloads do.
    pub async fn run_ungated<T: Send + 'static>(
        &self,
        deadline: tokio::time::Instant,
        work: impl FnOnce(CancellationToken) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let cancellation = CancellationToken::new();
        // The token stops a blocking closure that outlives this future.
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let task = async move {
            let permit = self.acquire(deadline).await?;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                if cancellation.is_cancelled() {
                    return Err(Error::cancelled());
                }
                work(cancellation)
            })
            .await
            .map_err(|_| Error::internal())?
        };
        tokio::time::timeout_at(deadline, task)
            .await
            .map_err(|_| Error::timed_out())?
    }
}
