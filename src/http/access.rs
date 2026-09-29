//! Request-boundary policy: identity, export authorization and deadlines.
//!
//! Every handler goes through these helpers, so the rules "authentication is
//! required", "the alias must be authorized" and "every request has a bounded
//! budget" are stated exactly once.

use crate::{app::State, core::error::Error, fs::Export};
use axum::http::HeaderMap;
use std::{sync::Arc, time::Duration};

/// Budget used when the caller does not ask for one.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Hard ceiling on a caller-supplied budget.
const MAX_TIMEOUT_MS: u64 = 30_000;

/// Resolve the request identity.
pub fn identity(state: &State, headers: &HeaderMap) -> Result<usize, Error> {
    state.auth.identify(headers)
}

/// Resolve the identity and prove it may read `alias`.
pub fn export(
    state: &State,
    headers: &HeaderMap,
    alias: &str,
) -> Result<(usize, Arc<Export>), Error> {
    let identity = identity(state, headers)?;
    state.auth.authorize(identity, alias)?;
    let export = state
        .exports
        .get(alias)
        .ok_or_else(|| Error::forbidden("EACCES"))?
        .clone();
    Ok((identity, export))
}

/// Like [`export`], but the caller must also be allowed to write the alias.
pub fn writable_export(
    state: &State,
    headers: &HeaderMap,
    alias: &str,
) -> Result<(usize, Arc<Export>), Error> {
    let (identity, export) = export(state, headers, alias)?;
    if !state.auth.client(identity).may_write(alias) || !export.writable() {
        return Err(Error::forbidden("EROFS"));
    }
    Ok((identity, export))
}

/// The idempotency key every write must carry.
pub fn operation_id(headers: &HeaderMap) -> Result<&str, Error> {
    headers
        .get("x-operation-id")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(Error::invalid)
}

/// The absolute instant this request must finish by.
pub fn deadline(headers: &HeaderMap) -> Result<tokio::time::Instant, Error> {
    let budget = match headers.get("x-timeout-ms") {
        None => DEFAULT_TIMEOUT_MS,
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|budget| *budget > 0)
            .ok_or_else(Error::invalid)?,
    };
    Ok(tokio::time::Instant::now() + Duration::from_millis(budget.min(MAX_TIMEOUT_MS)))
}
