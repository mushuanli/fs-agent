//! Durable identity for in-flight writes.
//!
//! The protocol is explicitly not transactional: a client sends one request
//! with an `X-Operation-Id`, and re-sending the same id must never apply the
//! effect twice. This ledger is the record that makes that true — it remembers
//! the receipt for a bounded time so the client can query the outcome after a
//! dropped response.
//!
//! Policy held here: id validation, reuse rejection, retention and capacity,
//! what "cancelled" means for an id that was never seen, and the receipt shape.

use crate::core::{error::Error, ids};
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// How long a finished operation stays queryable.
const RETENTION: Duration = Duration::from_secs(3600);
/// Maximum number of tracked operations before new ids are refused.
const CAPACITY: usize = 4096;

/// One operation, scoped to the identity that created it and the export.
type Key = (usize, String, String);

#[derive(Default)]
pub struct Operations {
    entries: Mutex<HashMap<Key, Arc<Operation>>>,
}

pub struct Operation {
    cancel: CancellationToken,
    result: Mutex<Option<Receipt>>,
    notify: Notify,
    created: Instant,
}

#[derive(Clone)]
struct Receipt(StatusCode, Value);

impl Operation {
    fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            result: Mutex::new(None),
            notify: Notify::new(),
            created: Instant::now(),
        }
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Abort a blocking step when the operation was cancelled or is out of time.
    pub fn checkpoint(&self, deadline: Instant) -> Result<(), Error> {
        if self.cancelled() {
            return Err(Error::cancelled());
        }
        if Instant::now() >= deadline {
            return Err(Error::timed_out());
        }
        Ok(())
    }

    /// Record the terminal receipt exactly once.
    pub fn finish(&self, result: Result<Value, Error>) {
        let receipt = match result {
            Ok(value) => Receipt(
                StatusCode::OK,
                json!({"outcome":"committed", "result":value}),
            ),
            Err(error) => Receipt(
                error.status,
                json!({"outcome":"not-committed", "code":error.code}),
            ),
        };
        *self.lock() = Some(receipt);
        self.notify.notify_waiters();
    }

    /// The recorded receipt, or a running placeholder.
    pub fn status(&self) -> Value {
        self.lock()
            .as_ref()
            .map(|receipt| receipt.1.clone())
            .unwrap_or_else(|| json!({"outcome":"unknown", "state":"running"}))
    }

    /// Wait for the receipt; at `deadline` the operation is asked to stop and
    /// the caller is told the outcome is unknown rather than not-committed.
    pub async fn wait(&self, deadline: Instant) -> (StatusCode, Value) {
        loop {
            let notified = self.notify.notified();
            if let Some(receipt) = self.lock().clone() {
                return (receipt.0, receipt.1);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                self.cancel();
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    json!({"outcome":"unknown", "code":"ETIMEDOUT"}),
                );
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<Receipt>> {
        self.result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Operations {
    /// Ask every tracked operation to stop; used during shutdown.
    pub fn cancel_all(&self) {
        for operation in self.lock().values() {
            operation.cancel();
        }
    }

    /// Claim `id` for `(identity, alias)`, or fail if it was already used.
    pub fn register(
        &self,
        identity: usize,
        alias: &str,
        id: &str,
    ) -> Result<Arc<Operation>, Error> {
        self.access(identity, alias, id, true)
    }

    /// Look up an existing operation without creating one.
    pub fn lookup(&self, identity: usize, alias: &str, id: &str) -> Result<Arc<Operation>, Error> {
        self.access(identity, alias, id, false)
    }

    /// Cancel an operation, recording a refusal when the id was never seen.
    ///
    /// A cancel that arrives before the write has a receipt is still a
    /// meaningful answer: the client learns its request will not commit.
    pub fn cancel(&self, identity: usize, alias: &str, id: &str) -> Result<Arc<Operation>, Error> {
        let operation = match self.lookup(identity, alias, id) {
            Ok(operation) => operation,
            Err(error) if error.status == StatusCode::NOT_FOUND => {
                match self.register(identity, alias, id) {
                    Ok(operation) => {
                        operation.finish(Err(Error::cancelled()));
                        operation
                    }
                    // Lost a race with a concurrent writer; fall back to it.
                    Err(_) => self.lookup(identity, alias, id)?,
                }
            }
            Err(error) => return Err(error),
        };
        operation.cancel();
        Ok(operation)
    }

    fn access(
        &self,
        identity: usize,
        alias: &str,
        id: &str,
        create: bool,
    ) -> Result<Arc<Operation>, Error> {
        if !ids::is_identifier_within(id, ids::IDENTIFIER_MAX) {
            return Err(Error::invalid());
        }
        let mut entries = self.lock();
        entries.retain(|_, operation| !operation.expired());
        let key = (identity, alias.to_owned(), id.to_owned());
        if let Some(operation) = entries.get(&key) {
            if create {
                return Err(Error::conflict("OPERATION_ID_REUSED"));
            }
            return Ok(operation.clone());
        }
        if !create {
            return Err(Error::not_found("OPERATION_UNKNOWN"));
        }
        if entries.len() >= CAPACITY {
            return Err(Error::too_many("OPERATION_LIMIT"));
        }
        let operation = Arc::new(Operation::new());
        entries.insert(key, operation.clone());
        Ok(operation)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Key, Arc<Operation>>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Operation {
    /// Retention is measured from creation, so a long-running operation is
    /// never evicted while it is still being polled.
    fn expired(&self) -> bool {
        self.result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
            && self.created.elapsed() >= RETENTION
    }
}
