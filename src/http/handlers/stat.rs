//! `POST /v1/fs/:alias/stat` — batch metadata lookup.
//!
//! A batch is bounded on both ends: at most [`MAX_PATHS`] paths and
//! [`MAX_PATH_BYTES`] of raw path bytes, so one request cannot pin a worker
//! indefinitely. Per-path failures are reported inside the batch rather than
//! failing the whole request.

use crate::{app::State, core::error::Error, http::access};
use axum::{
    extract::{Path, State as AxumState},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

const MAX_PATHS: usize = 256;
const MAX_PATH_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatRequest {
    paths: Vec<String>,
}

pub async fn stat(
    AxumState(state): AxumState<Arc<State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Json(input): Json<StatRequest>,
) -> Result<Json<Value>, Error> {
    let (_, export) = access::export(&state, &headers, &alias)?;
    let deadline = access::deadline(&headers)?;
    if input.paths.len() > MAX_PATHS
        || input.paths.iter().map(String::len).sum::<usize>() > MAX_PATH_BYTES
    {
        return Err(Error::invalid());
    }
    let _project_guards = input
        .paths
        .iter()
        .map(|path| access::project_path(&state, &headers, &alias, path, false, true))
        .collect::<Result<Vec<_>, _>>()?;
    state
        .workers
        .run(&state.files, deadline, move |token| {
            let mut results = Vec::with_capacity(input.paths.len());
            for path in input.paths {
                if token.is_cancelled() {
                    return Err(Error::cancelled());
                }
                results.push(match export.stat(&path) {
                    Ok(stat) => json!({"stat": stat}),
                    Err(error) => json!({"error": error.code}),
                });
            }
            Ok(Json(json!({"results": results})))
        })
        .await
}
