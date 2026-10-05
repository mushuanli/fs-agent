//! Structured SeqFile queries and conditional transactions; arbitrary SQL is never accepted.
use crate::{app::State, core::error::Error, fs::seq, http::access};
use axum::{
    extract::{Path, State as AxumState},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    path: String,
}
pub async fn snapshot(
    AxumState(state): AxumState<Arc<State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Json(query): Json<Query>,
) -> Result<Json<seq::Snapshot>, Error> {
    let (_, export) = access::export(&state, &headers, &alias)?;
    let deadline = access::deadline(&headers)?;
    state
        .workers
        .run(&state.files, deadline, move |token| {
            if token.is_cancelled() {
                return Err(Error::cancelled());
            }
            seq::snapshot(&export, &query.path).map(Json)
        })
        .await
}
pub async fn transaction(
    AxumState(state): AxumState<Arc<State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Json(input): Json<seq::Update>,
) -> Result<(StatusCode, Json<Value>), Error> {
    let (identity, export) = access::writable_export(&state, &headers, &alias)?;
    let deadline = access::deadline(&headers)?;
    let gate = state.files.shared()?;
    let operation = state
        .operations
        .register(identity, &alias, access::operation_id(&headers)?)?;
    let task = operation.clone();
    let completion = operation.clone();
    tokio::spawn(async move {
        let result = match state.workers.acquire(deadline).await {
            Ok(permit) => tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let _gate = gate;
                seq::update(&export, input, || task.checkpoint(deadline))
            })
            .await
            .map_err(|_| Error::internal())
            .and_then(|result| result),
            Err(error) => Err(error),
        };
        completion.finish(result);
    });
    // Registration is owned by the detached task; response loss is queried through operations.
    let (status, receipt) = operation.wait(deadline).await;
    Ok((status, Json(receipt)))
}
