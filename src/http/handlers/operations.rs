//! `GET /v1/fs/:alias/operations/:id` and its `/cancel` sibling.
//!
//! The ledger owns the semantics; these handlers only resolve the identity and
//! the operation.

use crate::{app::State, core::error::Error, http::access};
use axum::{
    extract::{Path, State as AxumState},
    http::HeaderMap,
    Json,
};
use serde_json::Value;
use std::sync::Arc;

pub async fn status(
    AxumState(state): AxumState<Arc<State>>,
    Path((alias, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, Error> {
    let (identity, _) = access::export(&state, &headers, &alias)?;
    Ok(Json(
        state.operations.lookup(identity, &alias, &id)?.status(),
    ))
}

pub async fn cancel(
    AxumState(state): AxumState<Arc<State>>,
    Path((alias, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, Error> {
    let (identity, _) = access::export(&state, &headers, &alias)?;
    Ok(Json(
        state.operations.cancel(identity, &alias, &id)?.status(),
    ))
}
