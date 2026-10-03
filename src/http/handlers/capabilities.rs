//! `GET /v1/capabilities` — what this node can actually do.
//!
//! Support discovery is authenticated and never substitutes for the
//! per-directory authorization checks each data endpoint performs.

use crate::{app::State, core::error::Error, http::access};
use axum::{extract::State as AxumState, http::HeaderMap, Json};
use serde_json::{json, Value};
use std::sync::Arc;

pub async fn capabilities(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Error> {
    let client = state.auth.client(access::identity(&state, &headers)?);
    let execution = state.execution.ready();
    Ok(Json(json!({
        "processEpoch": state.execution.epoch(),
        "version": 1,
        "serverId": state.auth.server_id(),
        "files": {
            "read": !client.exports().is_empty(),
            "write": client.can_write(),
        },
        "sync": { "push": state.sync.as_ref().is_some_and(|s|s.healthy()),
            "protocolVersion": if state.sync.is_some() {Some(1)} else {None},
            "discovery": if state.sync.is_some() {Some("/v1/sync/capabilities")} else {None} },
        "process": { "exec": execution },
        "terminal": { "pty": false },
        "executionModel": if execution { "sandbox" } else { "none" },
        "workspaceConsistency": if execution { "isolated" } else { "none" },
        "readOnlyEnforcement": if execution { "kernel-enforced" } else { "none" },
        "pathModel": if execution { "virtual-root" } else { "none" },
    })))
}
