use crate::{
    config,
    error::{invalid, Error},
    filesystem::{validate_path, Export},
    operations::{self, Operation},
    request,
    routes::ListQuery,
};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use futures_util::StreamExt;
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{fs::File, sync::Arc};
use tokio::io::AsyncWriteExt;

struct Upload {
    parent: File,
    name: String,
    file: File,
}
impl Drop for Upload {
    fn drop(&mut self) {
        let _ = rustix::fs::unlinkat(&self.parent, self.name.as_str(), AtFlags::empty());
    }
}
fn parent(export: &Export, path: &str) -> Result<(File, String), Error> {
    validate_path(path)?;
    if path.is_empty() {
        return Err(invalid());
    }
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    Ok((
        export.open_path(dir, OFlags::RDONLY | OFlags::DIRECTORY)?,
        name.to_owned(),
    ))
}
fn stage(export: &Export, path: &str) -> Result<Upload, Error> {
    let (parent, _) = parent(export, path)?;
    let mut random = [0u8; 16];
    getrandom::getrandom(&mut random).map_err(|_| invalid())?;
    let name = format!(
        ".itookit-upload-{}",
        random
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>()
    );
    let file = File::from(rustix::fs::openat(
        &parent,
        name.as_str(),
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )?);
    Ok(Upload { parent, name, file })
}
fn check(op: &Operation, deadline: tokio::time::Instant) -> Result<(), Error> {
    if op.cancel.is_cancelled() {
        return Err(Error(StatusCode::REQUEST_TIMEOUT, "ECANCELLED"));
    }
    if tokio::time::Instant::now() >= deadline {
        return Err(Error(StatusCode::GATEWAY_TIMEOUT, "ETIMEDOUT"));
    }
    Ok(())
}
fn register(
    state: &config::State,
    headers: &HeaderMap,
    alias: &str,
) -> Result<(Arc<Export>, Arc<Operation>), Error> {
    let export = request::export(state, headers, alias)?;
    if !state.clients[request::identity(state, headers)?]
        .write_exports
        .iter()
        .any(|item| item == alias)
        || !export.writable()
    {
        return Err(Error(StatusCode::FORBIDDEN, "EROFS"));
    }
    let op = state.operations.register(
        request::identity(state, headers)?,
        alias,
        operations::operation_id(headers)?,
    )?;
    Ok((export, op))
}
async fn wait(op: Arc<Operation>, deadline: tokio::time::Instant) -> (StatusCode, Json<Value>) {
    loop {
        let notified = op.notify.notified();
        if let Some(result) = op.result.lock().unwrap().clone() {
            return (result.0, Json(result.1));
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            op.cancel.cancel();
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({"outcome":"unknown", "code":"ETIMEDOUT"})),
            );
        }
    }
}
pub async fn replace(
    State(state): State<Arc<config::State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
    body: Body,
) -> Result<(StatusCode, Json<Value>), Error> {
    let deadline = request::deadline(&headers)?;
    validate_path(&query.path)?;
    let expected = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let create = headers.get("if-none-match").is_some_and(|v| v == "*");
    if create == expected.is_some() {
        return Err(Error(
            StatusCode::PRECONDITION_REQUIRED,
            "REVISION_REQUIRED",
        ));
    }
    let (export, op) = register(&state, &headers, &alias)?;
    let task_op = op.clone();
    // The operation owns work and cleanup after the HTTP waiter disconnects.
    tokio::spawn(async move {
        let result = upload_and_commit(
            state,
            export,
            query.path,
            body,
            expected,
            deadline,
            task_op.clone(),
        )
        .await;
        task_op.finish(result);
    });
    Ok(wait(op, deadline).await)
}
async fn upload_and_commit(
    state: Arc<config::State>,
    export: Arc<Export>,
    path: String,
    body: Body,
    expected: Option<String>,
    deadline: tokio::time::Instant,
    op: Arc<Operation>,
) -> Result<Value, Error> {
    let permit = tokio::time::timeout_at(deadline, state.workers.clone().acquire_owned())
        .await
        .map_err(|_| Error(StatusCode::GATEWAY_TIMEOUT, "ETIMEDOUT"))?
        .map_err(|_| invalid())?;
    check(&op, deadline)?;
    let source = export.clone();
    let target = path.clone();
    let upload = tokio::task::spawn_blocking(move || stage(&source, &target))
        .await
        .map_err(|_| invalid())??;
    let mut file = tokio::fs::File::from_std(upload.file.try_clone()?);
    let mut stream = body.into_data_stream();
    let mut size = 0usize;
    loop {
        check(&op, deadline)?;
        let chunk = tokio::select! {
            _ = op.cancel.cancelled() => return Err(Error(StatusCode::REQUEST_TIMEOUT, "ECANCELLED")),
            chunk = tokio::time::timeout_at(deadline, stream.next()) => chunk.map_err(|_| Error(StatusCode::GATEWAY_TIMEOUT, "ETIMEDOUT"))?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(|_| invalid())?;
        size += chunk.len();
        if size > 256 * 1024 * 1024 {
            return Err(Error(StatusCode::PAYLOAD_TOO_LARGE, "UPLOAD_LIMIT"));
        }
        tokio::time::timeout_at(deadline, file.write_all(&chunk))
            .await
            .map_err(|_| Error(StatusCode::GATEWAY_TIMEOUT, "ETIMEDOUT"))??;
    }
    file.flush().await?;
    drop(file);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        commit(&export, &path, upload, expected.as_deref(), &op, deadline)
    })
    .await
    .map_err(|_| invalid())?
}
fn commit(
    export: &Export,
    path: &str,
    upload: Upload,
    expected: Option<&str>,
    op: &Operation,
    deadline: tokio::time::Instant,
) -> Result<Value, Error> {
    let mut revisions = export.revisions.as_ref().unwrap().lock().unwrap();
    check(op, deadline)?;
    // Resolve again under the commit lock: rename of an upload's parent invalidates the operation.
    let (target, name) = parent(export, path)?;
    use std::os::unix::fs::MetadataExt;
    let a = target.metadata()?;
    let b = upload.parent.metadata()?;
    if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
        return Err(Error(StatusCode::PRECONDITION_FAILED, "ECONFLICT"));
    }
    if let Some(expected) = expected {
        let current = export
            .open_path(path, OFlags::PATH)
            .map_err(|_| Error(StatusCode::PRECONDITION_FAILED, "ECONFLICT"))?;
        if !current.metadata()?.is_file() || revisions.get(&current)? != expected {
            return Err(Error(StatusCode::PRECONDITION_FAILED, "ECONFLICT"));
        }
        revisions.retire(&current)?;
    }
    check(op, deadline)?;
    let mut stat = crate::filesystem::attributes(&upload.file)?;
    stat.revision = Some(revisions.get(&upload.file)?);
    rustix::fs::renameat_with(
        &upload.parent,
        upload.name.as_str(),
        &target,
        name.as_str(),
        if expected.is_none() {
            RenameFlags::NOREPLACE
        } else {
            RenameFlags::empty()
        },
    )
    .map_err(|error| {
        if expected.is_none() && error == rustix::io::Errno::EXIST {
            Error(StatusCode::PRECONDITION_FAILED, "ECONFLICT")
        } else {
            error.into()
        }
    })?;
    Ok(json!(stat))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    action: String,
    path: String,
    to: Option<String>,
    #[serde(default)]
    recursive: bool,
}
pub async fn mutate(
    State(state): State<Arc<config::State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Json(command): Json<Command>,
) -> Result<(StatusCode, Json<Value>), Error> {
    let deadline = request::deadline(&headers)?;
    validate_path(&command.path)?;
    if command.recursive {
        return Err(Error(StatusCode::UNPROCESSABLE_ENTITY, "ECAPABILITY"));
    }
    let (export, op) = register(&state, &headers, &alias)?;
    let task_op = op.clone();
    let waiter = op.clone();
    tokio::spawn(async move {
        let permit = tokio::time::timeout_at(deadline, state.workers.clone().acquire_owned()).await;
        match permit {
            Ok(Ok(permit)) => {
                let tracked = task_op.clone();
                if tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    change(&export, command, &tracked, deadline)
                })
                .await
                .is_err()
                {
                    task_op.finish(Err(Error(StatusCode::INTERNAL_SERVER_ERROR, "EIO")));
                }
            }
            _ => task_op.finish(Err(Error(StatusCode::GATEWAY_TIMEOUT, "ETIMEDOUT"))),
        }
    });
    let op = waiter;
    Ok(wait(op, deadline).await)
}
fn change(
    export: &Export,
    command: Command,
    op: &Operation,
    deadline: tokio::time::Instant,
) -> Result<(), Error> {
    let result = (|| {
        let mut revisions = export.revisions.as_ref().unwrap().lock().unwrap();
        check(op, deadline)?;
        let (dir, name) = parent(export, &command.path)?;
        match command.action.as_str() {
            "mkdir" => {
                rustix::fs::mkdirat(&dir, name.as_str(), Mode::RUSR | Mode::WUSR | Mode::XUSR)?
            }
            "rename" => {
                let (dest, dest_name) = parent(export, command.to.as_deref().ok_or_else(invalid)?)?;
                rustix::fs::renameat_with(
                    &dir,
                    name.as_str(),
                    &dest,
                    dest_name.as_str(),
                    RenameFlags::NOREPLACE,
                )?;
            }
            "remove" => {
                let file = export.open_path(&command.path, OFlags::PATH)?;
                let flags = if file.metadata()?.is_dir() {
                    AtFlags::REMOVEDIR
                } else {
                    AtFlags::empty()
                };
                rustix::fs::unlinkat(&dir, name.as_str(), flags)?;
            }
            _ => return Err(invalid()),
        }
        revisions.invalidate();
        Ok(json!({}))
    })();
    op.finish(result);
    Ok(())
}
