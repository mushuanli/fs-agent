//! `GET /v1/fs/:alias/entries` — one page of a directory listing.
//!
//! Pagination is by name, not by snapshot: each page re-scans the directory
//! and skips names up to the signed cursor. The cursor is bound to the
//! identity, alias and path, so it cannot be replayed against another listing.

use crate::{
    app::State,
    core::{cursor, error::Error},
    http::{access, query::ListQuery},
};
use axum::{
    extract::{Path, Query, State as AxumState},
    http::HeaderMap,
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

/// Entries returned per page.
const PAGE_SIZE: usize = 512;

pub async fn entries(
    AxumState(state): AxumState<Arc<State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, Error> {
    let (identity, export) = access::export(&state, &headers, &alias)?;
    let deadline = access::deadline(&headers)?;
    let binding = format!("{identity}:{alias}:{}", query.path);
    let after = query
        .cursor
        .as_deref()
        .map(|token| cursor::decode(&state.cursor_key, &binding, token))
        .transpose()?;
    let listing = state
        .workers
        .run(&state.files, deadline, move |token| {
            export.list(&query.path, &token)
        })
        .await?;
    // Fetch one extra entry to learn whether another page exists.
    let mut entries: Vec<_> = listing
        .entries
        .into_iter()
        .filter(|entry| after.as_ref().is_none_or(|last| entry.name > *last))
        .take(PAGE_SIZE + 1)
        .collect();
    let more = entries.len() > PAGE_SIZE;
    entries.truncate(PAGE_SIZE);
    let next_cursor = if more {
        entries
            .last()
            .map(|entry| cursor::encode(&state.cursor_key, &binding, &entry.name))
    } else {
        None
    };
    Ok(Json(json!({
        "entries": entries,
        "nextCursor": next_cursor,
        "warnings": listing.warnings,
    })))
}
