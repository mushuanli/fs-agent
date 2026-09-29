//! `GET /v1/exports` — the aliases this identity can reach.

use crate::{app::State, core::error::Error, http::access};
use axum::{extract::State as AxumState, http::HeaderMap, Json};
use serde_json::{json, Value};
use std::sync::Arc;

pub async fn exports(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Error> {
    let client = state.auth.client(access::identity(&state, &headers)?);
    let aliases = client
        .exports()
        .iter()
        .map(|alias| {
            let writable = state
                .exports
                .get(alias)
                .is_some_and(|export| export.writable());
            json!({
                "alias": alias,
                "access": if writable && client.may_write(alias) { "rw" } else { "ro" },
                "nameSemantics": "source",
                "strongRevision": writable,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"version": 1, "exports": aliases})))
}
