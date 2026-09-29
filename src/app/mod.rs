//! Wired runtime state.
//!
//! [`State`] is the composition root: it turns a validated [`Config`] into the
//! handful of components the HTTP layer needs. Each component owns exactly one
//! concern and is addressed directly, so a handler never reaches into a field
//! it does not need:
//!
//! | component | concern |
//! |---|---|
//! | [`Auth`] | who may read or write which alias |
//! | [`Exports`] | the directory capabilities |
//! | [`FileGate`] | mutual exclusion with command execution |
//! | [`Workers`] | bounded blocking-filesystem concurrency |
//! | [`Operations`] | idempotent write receipts |
//! | [`Execution`] | process epoch, admission and the retained lock |

use crate::{
    auth::{Auth, Client},
    config::{open_exports, resolve_credentials, Config},
    core::{gate::FileGate, ids, workers::Workers},
    fs::Exports,
    operations::Operations,
    process::Execution,
};
use std::sync::Arc;

/// Blocking filesystem worker budget.
pub const WORKER_LIMIT: usize = 16;
/// Random material used for cursors and the process epoch.
const KEY_BYTES: usize = 32;

pub struct State {
    pub auth: Auth,
    pub exports: Exports,
    pub cursor_key: [u8; KEY_BYTES],
    pub files: FileGate,
    pub workers: Workers,
    pub operations: Operations,
    pub execution: Execution,
}

impl State {
    /// Validate a parsed configuration and build the runtime.
    pub fn from_config(config: &Config) -> Result<Arc<Self>, String> {
        validate_identity(config)?;
        let exports = open_exports(&config.exports)?;
        let credentials = resolve_credentials(config)?;
        let writable = exports
            .iter()
            .filter(|(_, export)| export.writable())
            .map(|(alias, _)| alias.clone())
            .collect();
        let client = Client::new(
            credentials.secret,
            credentials.username,
            exports.aliases().cloned().collect(),
            writable,
        );
        let server_id = match &config.server_id {
            Some(id) => Some(id.clone()),
            None if config.execution => Some(format!("fs-agent-{}", hex(&random_bytes()?))),
            None => None,
        };
        Self::new(Auth::new(server_id, vec![client]), exports)
    }

    /// Assemble the runtime with fresh random material.
    ///
    /// The pagination cursor key and the process epoch are independent secrets:
    /// the epoch is published through `/v1/capabilities`, the key is not.
    pub fn new(auth: Auth, exports: Exports) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            auth,
            exports,
            cursor_key: random_bytes()?,
            files: FileGate::new(),
            workers: Workers::new(WORKER_LIMIT),
            operations: Operations::default(),
            execution: Execution::new(hex(&random_bytes()?)),
        }))
    }

    /// Test constructor with deterministic random material.
    #[doc(hidden)]
    pub fn with_secrets(
        auth: Auth,
        exports: Exports,
        cursor_key: [u8; KEY_BYTES],
        process_epoch: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            auth,
            exports,
            cursor_key,
            files: FileGate::new(),
            workers: Workers::new(WORKER_LIMIT),
            operations: Operations::default(),
            execution: Execution::new(process_epoch),
        })
    }
}

/// Reject configurations that cannot be served safely.
fn validate_identity(config: &Config) -> Result<(), String> {
    if let Some(id) = &config.server_id {
        if !ids::is_identifier_within(id, ids::IDENTIFIER_MAX) {
            return Err("server_id must contain 1..128 ASCII letters, digits, '-' or '_'".into());
        }
    }
    // A username with a token would look like Basic authentication but silently
    // serve Bearer only, so it is rejected instead of ignored.
    if config.username.is_some() && (config.token.is_some() || config.token_env.is_some()) {
        return Err("username cannot be combined with token or token_env".into());
    }
    Ok(())
}

fn random_bytes() -> Result<[u8; KEY_BYTES], String> {
    let mut bytes = [0u8; KEY_BYTES];
    getrandom::getrandom(&mut bytes).map_err(|_| "Random source unavailable".to_owned())?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
