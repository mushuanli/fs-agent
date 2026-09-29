//! `GET /v1/fs/:alias/content` — streaming read with byte-range support.
//!
//! The read guard on the file gate is held for the whole response body, so a
//! command cannot start while bytes are still streaming out of an export.

use crate::{
    app::State,
    core::error::Error,
    http::{access, query::ListQuery, range},
};
use axum::{
    body::Body,
    extract::{Path, Query, State as AxumState},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Bytes read per stream item.
const CHUNK_BYTES: u64 = 64 * 1024;

pub async fn content(
    AxumState(state): AxumState<Arc<State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Response, Error> {
    let (_, export) = access::export(&state, &headers, &alias)?;
    let gate = state.files.shared()?;
    let deadline = access::deadline(&headers)?;
    let (file, revision) = state
        .workers
        .run_ungated(deadline, move |_| export.read(&query.path))
        .await?;

    // A read-only export has no validator, so a client-supplied `If-Match`
    // cannot be evaluated and must not turn every read into a 412.
    if let Some(revision) = revision.as_deref() {
        if let Some(expected) = headers.get("if-match") {
            if expected.to_str().unwrap_or("") != revision {
                return Err(Error::precondition_failed());
            }
        }
    }
    let size = file.metadata()?.len();
    let range_header = if if_range_matches(&headers, revision.as_deref()) {
        header(&headers, "range")
    } else {
        None
    };
    let window = match range::resolve(range_header, size) {
        Ok(window) => window,
        Err(error) if error.is_range_unsatisfiable() => {
            return Ok(Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(
                    "content-range",
                    range::ByteRange::unsatisfiable_header(size),
                )
                .body(Body::empty())
                .expect("static response"));
        }
        Err(error) => return Err(error),
    };

    let mut file = tokio::fs::File::from_std(file);
    file.seek(std::io::SeekFrom::Start(window.start)).await?;
    // The gate guard travels with the stream so the export stays read-locked.
    let stream = futures_util::stream::try_unfold(
        (file, window.length, gate),
        move |(mut file, remaining, gate)| async move {
            if remaining == 0 {
                return Ok::<_, std::io::Error>(None);
            }
            let mut data = vec![0; remaining.min(CHUNK_BYTES) as usize];
            let read = tokio::time::timeout_at(deadline, file.read(&mut data))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "read deadline")
                })??;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "content changed while streaming",
                ));
            }
            data.truncate(read);
            Ok(Some((data, (file, remaining - read as u64, gate))))
        },
    );
    let mut response = Response::builder()
        .status(if window.partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header("content-type", "application/octet-stream")
        .header("content-length", window.length)
        .header("accept-ranges", "bytes");
    if let Some(revision) = revision {
        response = response.header("etag", revision);
    }
    if window.partial {
        response = response.header("content-range", window.content_range(size));
    }
    Ok(response
        .body(Body::from_stream(stream))
        .expect("static response"))
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// `If-Range` matches only when the server has a validator and it is equal.
fn if_range_matches(headers: &HeaderMap, revision: Option<&str>) -> bool {
    match (revision, headers.get("if-range")) {
        (Some(revision), Some(value)) => value.to_str().unwrap_or("") == revision,
        (None, Some(_)) => false,
        (_, None) => true,
    }
}
