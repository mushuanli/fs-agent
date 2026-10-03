//! HTTP parsing only; storage work is bounded and runs outside the reactor.
use super::{model::*, SyncService};
use crate::app::State;
use axum::{
    body::{to_bytes, Body},
    extract::{Path, State as AxumState},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(self.value()),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use tower::ServiceExt;

    #[tokio::test]
    async fn shutdown_waits_for_streaming_upload_and_cancelled_cleanup() {
        for cancel in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let storage = crate::sync::tests::service(root.path());
            crate::sync::tests::project(&storage);
            drop(storage);
            let config = format!("execution=false\nlisten='127.0.0.1:0'\ntoken='sync-test-secret-at-least-24-bytes'\n[sync]\nenabled=true\nroot='{}'\nmetadata_reserve_bytes=0", root.path().display());
            let state = crate::app::State::from_config(&toml::from_str(&config).unwrap()).unwrap();
            let storage = state.sync.as_ref().unwrap().clone();
            let app = crate::router(state, &[]).unwrap();
            let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(2);
            let stream = futures_util::stream::unfold(receiver, |mut rx| async move {
                rx.recv()
                    .await
                    .map(|chunk| (Ok::<_, std::io::Error>(chunk), rx))
            });
            let hash = digest(b"xy");
            let request = axum::http::Request::builder()
                .method("PUT")
                .uri(format!("/v1/sync/projects/p/objects/{hash}"))
                .header("authorization", "Bearer sync-test-secret-at-least-24-bytes")
                .header("x-sync-history-epoch", storage.epoch())
                .header("content-length", 2)
                .body(Body::from_stream(stream))
                .unwrap();
            let task = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
            sender.send(Bytes::from_static(b"x")).await.unwrap();
            wait_staging(root.path()).await;
            storage.stop();
            assert!(storage.drain_timeout(std::time::Duration::ZERO).is_err());
            if cancel {
                task.abort();
                let _ = task.await;
            } else {
                sender.send(Bytes::from_static(b"y")).await.unwrap();
                drop(sender);
                assert_eq!(task.await.unwrap().status(), StatusCode::OK);
            }
            let worker = storage.clone();
            tokio::task::spawn_blocking(move || worker.drain())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                std::fs::read_dir(root.path().join("staging"))
                    .unwrap()
                    .count(),
                0
            );
            let ready = storage
                .check_objects("p", &json!({"hashes":[hash]}))
                .unwrap();
            assert_eq!(
                ready["ready"].as_array().unwrap().len(),
                usize::from(!cancel)
            );
        }
    }
    async fn wait_staging(root: &std::path::Path) {
        for _ in 0..200 {
            if std::fs::read_dir(root.join("staging"))
                .unwrap()
                .next()
                .is_some()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("upload did not enter streaming phase");
    }
}

async fn work<T: Send + 'static>(
    state: &Arc<State>,
    action: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let activity = state
        .sync
        .as_ref()
        .map(|s| s.cleanup_activity())
        .transpose()?;
    let permit = state
        .workers
        .acquire(tokio::time::Instant::now() + std::time::Duration::from_secs(30))
        .await
        .map_err(|_| Error::new("LIMIT_EXCEEDED", 429))?;
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _activity = activity;
        action()
    })
    .await
    .unwrap_or_else(|_| Err(Error::storage()));
    if result.as_ref().err().is_some_and(|e| e.storage_fault) {
        if let Some(service) = &state.sync {
            service.mark_uncertain();
        }
    }
    result
}
fn authorized(state: &State, headers: &HeaderMap, path: &str) -> Result<Arc<SyncService>> {
    let identity = state
        .auth
        .identify(headers)
        .map_err(|e| Error::new(e.code, e.status.as_u16()))?;
    if identity != 0 {
        return Err(Error::new("FORBIDDEN", 403));
    }
    let service = state
        .sync
        .clone()
        .ok_or_else(|| Error::new("SYNC_DISABLED", 404))?;
    if path != "capabilities"
        && headers
            .get("x-sync-history-epoch")
            .and_then(|h| h.to_str().ok())
            != Some(service.epoch())
    {
        return Err(Error::new("HISTORY_EPOCH_CHANGED", 409));
    }
    Ok(service)
}
#[derive(serde::Deserialize, Default)]
pub struct Query {
    cursor: Option<String>,
    state: Option<String>,
    limit: Option<usize>,
}
pub async fn get(
    AxumState(state): AxumState<Arc<State>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<Query>,
) -> Result<Response> {
    let service = authorized(&state, &headers, &path)?;
    let _activity = service.activity()?;
    let parts: Vec<_> = path.split('/').collect();
    if let ["projects", project, "objects", hash] = parts.as_slice() {
        return download(
            &state,
            service,
            project.to_string(),
            hash.to_string(),
            headers,
        )
        .await;
    }
    if let ["projects", project, "manifests", hash] = parts.as_slice() {
        let (project, hash) = (project.to_string(), hash.to_string());
        let bytes = work(&state, move || service.manifest_bytes(&project, &hash)).await?;
        return Ok(([("content-type", "application/json")], bytes).into_response());
    }
    let value = work(&state, move || read(&service, &path, query)).await?;
    Ok(Json(value).into_response())
}
fn read(service: &SyncService, path: &str, q: Query) -> Result<Value> {
    let parts: Vec<_> = path.split('/').collect();
    let limit = q.limit.unwrap_or(100);
    let cursor = q.cursor.as_deref();
    match parts.as_slice() {
        ["capabilities"] => Ok(service.capabilities()),
        ["replicas", replica] => service.replica(replica),
        ["replicas", replica, "operations", seq] => service.operation(replica, seq),
        ["projects"] => service.projects(q.state.as_deref().unwrap_or("active")),
        ["projects", project, "datasets"] => service.catalog(
            project,
            cursor,
            q.state.as_deref().unwrap_or("active"),
            limit,
        ),
        ["projects", project, "datasets", dataset, "head"] => service.head(project, dataset),
        ["projects", project, "datasets", dataset, "versions"] => {
            service.versions(project, dataset, None, cursor, limit)
        }
        ["projects", project, "datasets", dataset, "versions", generation] => {
            service.versions(project, dataset, Some(generation), None, limit)
        }
        ["projects", project, "changes"] => service.changes(project, cursor, limit),
        _ => Err(Error::new("NOT_FOUND", 404)),
    }
}
pub async fn post(
    AxumState(state): AxumState<Arc<State>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    let service = authorized(&state, &headers, &path)?;
    let _activity = service.activity()?;
    let bytes = to_bytes(body, 512 * 1024)
        .await
        .map_err(|_| Error::new("LIMIT_EXCEEDED", 413))?;
    let value: Value = serde_json::from_slice(&bytes)?;
    if value
        .get("historyEpoch")
        .is_some_and(|v| v != service.epoch())
    {
        return Err(Error::new("HISTORY_EPOCH_CHANGED", 409));
    }
    let result = work(&state, move || write(&service, &path, &value)).await?;
    let status = result
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|n| StatusCode::from_u16(n as u16).ok())
        .unwrap_or(StatusCode::OK);
    Ok((status, Json(result)).into_response())
}
fn write(service: &SyncService, path: &str, body: &Value) -> Result<Value> {
    let parts: Vec<_> = path.split('/').collect();
    match parts.as_slice() {
        ["replicas"] => service.register(body),
        ["replicas", replica, "activate"] => service.activate(replica, body),
        ["replicas", replica, "operations", seq, "cancel"] => {
            service.cancel_operation(replica, seq, body)
        }
        ["projects", project, "objects", "check"] => service.check_objects(project, body),
        ["projects", project, "read-pins"] => service.pin(project, body),
        ["projects", project, "read-pins", pin, action]
            if *action == "renew" || *action == "release" =>
        {
            service.pin_action(project, pin, action)
        }
        ["projects", project, "replicas", replica, "ack"] => service.ack(project, replica, body),
        _ => service.command(path, body, false),
    }
}
pub async fn put(
    AxumState(state): AxumState<Arc<State>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    let service = authorized(&state, &headers, &path)?;
    let _activity = service.activity()?;
    let parts: Vec<_> = path.split('/').collect();
    let ["projects", project, "objects", hash] = parts.as_slice() else {
        return Err(Error::new("NOT_FOUND", 404));
    };
    let size = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| Error::new("CONTENT_LENGTH_REQUIRED", 411))?;
    upload(
        &state,
        service,
        project.to_string(),
        hash.to_string(),
        size,
        body,
    )
    .await
}
async fn upload(
    state: &Arc<State>,
    service: Arc<SyncService>,
    project: String,
    hash: String,
    size: u64,
    body: Body,
) -> Result<Response> {
    let copy = service.clone();
    let (p, h) = (project.clone(), hash.clone());
    if let Some(value) = work(state, move || copy.reuse(&p, &h, size)).await? {
        return Ok(Json(value).into_response());
    }
    let permit = service
        .uploads
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::new("LIMIT_EXCEEDED", 429))?;
    let copy = service.clone();
    let guard = work(state, move || {
        let id = copy.reserve(&project, &hash, size)?;
        let activity = Some(copy.cleanup_activity()?);
        Ok(UploadGuard {
            service: copy,
            id,
            activity,
        })
    })
    .await?;
    let id = guard.id.clone();
    let copy = service.clone();
    let upload_id = id.clone();
    let file = work(state, move || copy.temporary_file(&upload_id)).await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(service.config.upload_ttl_seconds),
        receive(&service, file, size, body),
    )
    .await
    .map_err(|_| Error::new("UPLOAD_EXPIRED", 410))??;
    let copy = service.clone();
    let result = work(state, move || copy.install(&id)).await?;
    drop(guard);
    drop(permit);
    Ok(Json(result).into_response())
}

struct UploadGuard {
    service: Arc<SyncService>,
    id: String,
    activity: Option<super::coordination::Activity>,
}
impl Drop for UploadGuard {
    fn drop(&mut self) {
        let service = self.service.clone();
        let id = self.id.clone();
        let activity = self.activity.take();
        tokio::task::spawn_blocking(move || {
            let _activity = activity;
            if let Err(error) = service.abandon(&id) {
                service.mark_uncertain();
                crate::core::events::emit(
                    crate::core::events::Level::Warn,
                    "sync.cleanup_failed",
                    json!({"code":error.code}),
                );
            }
        });
    }
}
async fn receive(service: &SyncService, file: std::fs::File, size: u64, body: Body) -> Result<()> {
    let mut file = tokio::fs::File::from_std(file);
    let mut stream = body.into_data_stream();
    let mut received = 0u64;
    while let Some(chunk) = tokio::time::timeout(std::time::Duration::from_secs(60), stream.next())
        .await
        .map_err(|_| Error::new("UPLOAD_TIMEOUT", 408))?
    {
        service.completion_allowed()?;
        let chunk = chunk.map_err(|_| Error::new("UPLOAD_INTERRUPTED", 400))?;
        received = received
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| Error::new("LIMIT_EXCEEDED", 413))?;
        if received > size {
            return Err(Error::new("OBJECT_SIZE_MISMATCH", 422));
        }
        file.write_all(&chunk).await?;
    }
    if received != size {
        return Err(Error::new("OBJECT_SIZE_MISMATCH", 422));
    }
    file.sync_all().await?;
    Ok(())
}
async fn download(
    state: &Arc<State>,
    service: Arc<SyncService>,
    project: String,
    hash: String,
    headers: HeaderMap,
) -> Result<Response> {
    let copy = hash.clone();
    let (file, size) = work(state, move || service.object_file(&project, &copy)).await?;
    let range = match download_range(&headers, &hash, size) {
        Ok(range) => range,
        Err(e) => {
            return Ok((
                e.status,
                [("content-range", format!("bytes */{size}"))],
                Json(json!({"code":"INVALID_RANGE"})),
            )
                .into_response())
        }
    };
    object_response(file, size, hash, range).await
}

fn download_range(
    headers: &HeaderMap,
    hash: &str,
    size: u64,
) -> std::result::Result<crate::http::range::ByteRange, crate::core::error::Error> {
    let etag = format!("\"{hash}\"");
    let requested = if headers.get("if-range").is_some_and(|v| v != etag.as_str()) {
        None
    } else {
        headers.get("range").and_then(|v| v.to_str().ok())
    };
    crate::http::range::resolve(requested, size)
}
async fn object_response(
    file: std::fs::File,
    size: u64,
    hash: String,
    range: crate::http::range::ByteRange,
) -> Result<Response> {
    let mut file = tokio::fs::File::from_std(file);
    file.seek(std::io::SeekFrom::Start(range.start)).await?;
    use tokio::io::AsyncReadExt;
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file.take(range.length)));
    let mut response = body.into_response();
    *response.status_mut() = if range.partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let headers = response.headers_mut();
    headers.insert("etag", format!("\"{hash}\"").parse().unwrap());
    headers.insert("content-length", range.length.to_string().parse().unwrap());
    headers.insert("accept-ranges", "bytes".parse().unwrap());
    headers.insert("content-type", "application/octet-stream".parse().unwrap());
    if range.partial {
        headers.insert("content-range", range.content_range(size).parse().unwrap());
    }
    Ok(response)
}
