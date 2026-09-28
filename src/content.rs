use crate::{
    config::State as AppState,
    error::{invalid, Error},
    request,
    routes::ListQuery,
};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub async fn content(
    State(state): State<Arc<AppState>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Response, Error> {
    let export = request::export(&state, &headers, &alias)?;
    let deadline = request::deadline(&headers)?;
    let (file, revision) =
        request::blocking(&state, deadline, move |_| export.read(&query.path)).await?;
    if headers
        .get("if-match")
        .is_some_and(|v| Some(v.to_str().unwrap_or("")) != revision.as_deref())
    {
        return Err(Error(StatusCode::PRECONDITION_FAILED, "ECONFLICT"));
    }
    let size = file.metadata()?.len();
    let range = if headers
        .get("if-range")
        .is_some_and(|v| Some(v.to_str().unwrap_or("")) != revision.as_deref())
    {
        None
    } else {
        headers.get("range").and_then(|v| v.to_str().ok())
    };
    let (start, length, partial) = match range_bounds(range, size) {
        Err(Error(StatusCode::RANGE_NOT_SATISFIABLE, _)) => {
            return Ok(Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("content-range", format!("bytes */{size}"))
                .body(Body::empty())
                .unwrap())
        }
        result => result?,
    };
    let mut file = tokio::fs::File::from_std(file);
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let stream =
        futures_util::stream::try_unfold((file, length), move |(mut file, remaining)| async move {
            if remaining == 0 {
                return Ok::<_, std::io::Error>(None);
            }
            let mut data = vec![0; remaining.min(64 * 1024) as usize];
            let read = tokio::time::timeout_at(deadline, file.read(&mut data))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "Read deadline")
                })??;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Content changed",
                ));
            }
            data.truncate(read);
            Ok(Some((data, (file, remaining - read as u64))))
        });
    let mut response = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header("content-type", "application/octet-stream")
        .header("content-length", length)
        .header("accept-ranges", "bytes");
    if let Some(revision) = revision {
        response = response.header("etag", revision);
    }
    if partial {
        response = response.header(
            "content-range",
            format!("bytes {start}-{}/{size}", start + length - 1),
        );
    }
    Ok(response.body(Body::from_stream(stream)).unwrap())
}

fn range_bounds(range: Option<&str>, size: u64) -> Result<(u64, u64, bool), Error> {
    let Some(range) = range else {
        return Ok((0, size, false));
    };
    if range.contains(',') {
        return Ok((0, size, false));
    }
    let (start, end) = range
        .strip_prefix("bytes=")
        .and_then(|v| v.split_once('-'))
        .ok_or_else(invalid)?;
    if start.is_empty() {
        let suffix: u64 = end.parse().map_err(|_| invalid())?;
        if suffix == 0 || size == 0 {
            return Err(Error(StatusCode::RANGE_NOT_SATISFIABLE, "EINVAL"));
        }
        let length = suffix.min(size);
        return Ok((size - length, length, true));
    }
    let start: u64 = start.parse().map_err(|_| invalid())?;
    let end: u64 = if end.is_empty() {
        size.saturating_sub(1)
    } else {
        end.parse().map_err(|_| invalid())?
    };
    if start >= size || end < start {
        return Err(Error(StatusCode::RANGE_NOT_SATISFIABLE, "EINVAL"));
    }
    Ok((start, end.min(size - 1) - start + 1, true))
}
