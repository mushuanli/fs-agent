//! Wire model for the process API.
//!
//! Requests carry *virtual* paths only (`at`, `cwd`); a host path can never
//! reach this layer. [`Process`] owns the mutable status behind a lock and
//! exposes only the transitions the runner is allowed to perform.

use crate::core::error::Error;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio_util::sync::CancellationToken;

/// Maximum retained process records before new commands are refused.
pub const CAPACITY: usize = 1024;

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Mount {
    /// Export alias the directory comes from.
    pub alias: String,
    /// Path inside that export.
    pub path: String,
    /// Virtual path inside the execution environment.
    pub at: String,
    /// `"ro"` or `"rw"`.
    pub access: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Request {
    pub server_id: String,
    pub epoch: String,
    pub request_id: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub project_revision: Option<u64>,
    #[serde(default)]
    pub read_only: bool,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub mounts: Vec<Mount>,
    pub timeout_ms: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub state: &'static str,
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
    pub error: Option<&'static str>,
    pub truncated: bool,
}

impl Status {
    fn running() -> Self {
        Self {
            state: "running",
            stdout: String::new(),
            stderr: String::new(),
            code: None,
            error: None,
            truncated: false,
        }
    }
}

/// Terminal outcome of one command.
pub struct Outcome {
    pub state: &'static str,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

pub struct Process {
    cancel: CancellationToken,
    status: Mutex<Status>,
}

impl Default for Process {
    fn default() -> Self {
        Self::new()
    }
}

impl Process {
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            status: Mutex::new(Status::running()),
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

    pub fn status(&self) -> Status {
        self.lock().clone()
    }

    /// The launcher never started; nothing ran.
    pub fn spawn_failed(&self) {
        self.fail("failed", "SPAWN_FAILED");
    }

    /// Ownership of the process is uncertain, so the file gate stays closed.
    pub fn unreaped(&self) {
        self.fail("unknown", "REAP_FAILED");
    }

    /// Cancelled before the launcher was reached.
    pub fn cancelled_before_start(&self) {
        self.lock().state = "cancelled";
    }

    pub fn complete(&self, outcome: Outcome) {
        let mut status = self.lock();
        status.state = outcome.state;
        status.code = outcome.code;
        status.stdout = outcome.stdout;
        status.stderr = outcome.stderr;
        status.truncated = outcome.truncated;
    }

    fn fail(&self, state: &'static str, error: &'static str) {
        let mut status = self.lock();
        status.state = state;
        status.error = Some(error);
    }

    fn lock(&self) -> MutexGuard<'_, Status> {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One command per `(identity, requestId)`.
type Key = (usize, String);

#[derive(Default)]
pub struct Registry {
    entries: Mutex<HashMap<Key, Arc<Process>>>,
}

impl Registry {
    /// Look up a process without creating one.
    pub fn get(&self, identity: usize, id: &str) -> Option<Arc<Process>> {
        self.lock().get(&(identity, id.to_owned())).cloned()
    }

    /// Return the existing process for `id`, or create one via `create`.
    ///
    /// `create` runs while the registry is locked so a duplicate request id
    /// cannot prepare a second command concurrently. It must not re-enter the
    /// registry, and it must be quick.
    pub fn get_or_create(
        &self,
        identity: usize,
        id: &str,
        create: impl FnOnce() -> Result<Arc<Process>, Error>,
    ) -> Result<Arc<Process>, Error> {
        let mut entries = self.lock();
        let key = (identity, id.to_owned());
        if let Some(existing) = entries.get(&key) {
            return Ok(existing.clone());
        }
        if entries.len() >= CAPACITY {
            return Err(Error::too_many("PROCESS_LIMIT"));
        }
        let process = create()?;
        entries.insert(key, process.clone());
        Ok(process)
    }

    /// Cancel a process, recording a refusal when the id was never seen.
    pub fn cancel(&self, identity: usize, id: &str) -> Result<Arc<Process>, Error> {
        let mut entries = self.lock();
        let key = (identity, id.to_owned());
        if !entries.contains_key(&key) && entries.len() >= CAPACITY {
            return Err(Error::too_many("PROCESS_LIMIT"));
        }
        let process = entries.entry(key).or_insert_with(|| {
            let process = Arc::new(Process::new());
            process.cancelled_before_start();
            process
        });
        process.cancel();
        Ok(process.clone())
    }

    /// Ask every tracked command to stop; used during shutdown.
    pub fn cancel_all(&self) {
        for process in self.lock().values() {
            process.cancel();
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Key, Arc<Process>>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
