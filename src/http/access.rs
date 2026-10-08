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
    state.auth.identify(headers).inspect_err(|error| {
        crate::core::events::emit(
            crate::core::events::Level::Warn,
            "auth.rejected",
            serde_json::json!({"code": error.code}),
        );
    })
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

/// Project-aware clients keep the original file protocol but carry a directory fence.
pub fn project_path(
    state: &State,
    headers: &HeaderMap,
    alias: &str,
    path: &str,
    write: bool,
    metadata: bool,
) -> Result<Option<tokio::sync::OwnedRwLockReadGuard<()>>, Error> {
    let Some(id) = headers.get("x-fsagent-project") else {
        return Ok(None);
    };
    let guard = state.files.shared()?;
    let id = id.to_str().map_err(|_| Error::invalid())?;
    let identity = identity(state, headers)?;
    let project = state
        .projects
        .as_ref()
        .ok_or_else(Error::unsupported)?
        .get(identity, id)?;
    crate::projects::service::authorize(state, identity, &project)?;
    if headers
        .get("x-fsagent-project-revision")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.parse().ok())
        != Some(project.revision)
    {
        return Err(Error::conflict("PROJECT_REVISION_CHANGED"));
    }
    if write
        && headers
            .get("x-fsagent-project-readonly")
            .is_some_and(|h| h == "true")
    {
        return Err(Error::forbidden("EROFS"));
    }
    crate::fs::path::validate(path)?;
    if project_allows(&project, alias, path, write, metadata) {
        Ok(Some(guard))
    } else {
        Err(Error::forbidden(if write { "EROFS" } else { "EACCES" }))
    }
}
fn beneath(path: &str, root: &str) -> bool {
    root.is_empty() || path == root || path.starts_with(&(root.to_owned() + "/"))
}
fn project_allows(
    project: &crate::projects::model::Project,
    alias: &str,
    path: &str,
    write: bool,
    metadata: bool,
) -> bool {
    let mounts = crate::projects::service::mounts(project);
    mounts.iter().filter(|m| m.alias == alias).any(|mount| {
        if metadata && !write && beneath(&mount.path, path) {
            return true;
        }
        if !beneath(path, &mount.path) || write && mount.access != "rw" {
            return false;
        }
        let relative = path
            .strip_prefix(&mount.path)
            .unwrap_or(path)
            .trim_start_matches('/');
        let virtual_path = if relative.is_empty() {
            mount.at.clone()
        } else {
            format!("{}/{}", mount.at, relative)
        };
        let shadow = mounts
            .iter()
            .filter(|m| beneath(&virtual_path, &m.at))
            .max_by_key(|m| m.at.len())
            .unwrap();
        let suffix = virtual_path[shadow.at.len()..].trim_start_matches('/');
        let source = [shadow.path.as_str(), suffix]
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        shadow.alias == alias && source == path && (!write || shadow.access == "rw")
    })
}
