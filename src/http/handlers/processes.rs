//! `POST /v1/processes` and the status/cancel siblings.
//!
//! Handlers resolve the authenticated identity and delegate to
//! [`crate::process`], which owns epochs, idempotency and admission.

use crate::{app::State, core::error::Error, http::access, process};
use axum::{
    extract::{Path, State as AxumState},
    http::HeaderMap,
    Json,
};
use std::sync::Arc;

pub async fn start(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
    Json(request): Json<process::Request>,
) -> Result<Json<process::Status>, Error> {
    let identity = access::identity(&state, &headers)?;
    Ok(Json(process::start(&state, identity, request)?))
}

pub async fn status(
    AxumState(state): AxumState<Arc<State>>,
    Path((epoch, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<process::Status>, Error> {
    let identity = access::identity(&state, &headers)?;
    Ok(Json(process::status(&state, identity, &epoch, &id)?))
}

pub async fn cancel(
    AxumState(state): AxumState<Arc<State>>,
    Path((epoch, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<process::Status>, Error> {
    let identity = access::identity(&state, &headers)?;
    Ok(Json(process::cancel(&state, identity, &epoch, &id)?))
}
