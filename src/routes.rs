use crate::{
    config::State as AppState,
    cursor,
    error::{invalid, Error},
    request,
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub async fn exports(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Error> {
    let client = &state.clients[request::identity(&state, &headers)?];
    Ok(Json(
        json!({"version":1,"exports":client.exports.iter().map(|alias| json!({
        "alias": alias, "access":if state.exports[alias].writable() && client.write_exports.contains(alias) { "rw" } else { "ro" }, "nameSemantics":"source", "strongRevision":state.exports[alias].writable()
    })).collect::<Vec<_>>()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatRequest {
    paths: Vec<String>,
}
pub async fn stat(
    State(state): State<Arc<AppState>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Json(input): Json<StatRequest>,
) -> Result<Json<Value>, Error> {
    let export = request::export(&state, &headers, &alias)?;
    let deadline = request::deadline(&headers)?;
    if input.paths.len() > 256 || input.paths.iter().map(String::len).sum::<usize>() > 64 * 1024 {
        return Err(invalid());
    }
    request::blocking(&state, deadline, move |token| {
        let mut results = Vec::new();
        for path in input.paths {
            if token.is_cancelled() {
                return Err(Error(StatusCode::REQUEST_TIMEOUT, "ECANCELLED"));
            }
            results.push(match export.stat(&path) {
                Ok(stat) => json!({"stat":stat}),
                Err(error) => json!({"error":error.1}),
            });
        }
        Ok(Json(json!({"results": results})))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    pub path: String,
    pub cursor: Option<String>,
}
pub async fn entries(
    State(state): State<Arc<AppState>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, Error> {
    let export = request::export(&state, &headers, &alias)?;
    let deadline = request::deadline(&headers)?;
    let binding = format!(
        "{}:{alias}:{}",
        request::identity(&state, &headers)?,
        query.path
    );
    let after = query
        .cursor
        .as_deref()
        .map(|v| cursor::decode(&state.cursor_key, &binding, v))
        .transpose()?;
    let (entries, warnings) = request::blocking(&state, deadline, move |token| {
        export.list(&query.path, &token)
    })
    .await?;
    let mut entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| after.as_ref().is_none_or(|last| entry.name > *last))
        .take(513)
        .collect();
    let more = entries.len() > 512;
    entries.truncate(512);
    let next = if more {
        entries
            .last()
            .map(|entry| cursor::encode(&state.cursor_key, &binding, &entry.name))
    } else {
        None
    };
    Ok(Json(
        json!({"entries":entries, "nextCursor":next, "warnings":warnings}),
    ))
}
