//! `PUT /v1/fs/:alias/content` and `POST /v1/fs/:alias/mutate`.
//!
//! Both endpoints share one shape: validate the precondition, claim the
//! operation id, hand the work to a task that owns it after the HTTP waiter
//! disconnects, and return the recorded receipt. The domain work itself lives
//! in [`crate::fs::upload`] and [`crate::fs::mutation`].

use crate::{
    app::State,
    core::error::Error,
    fs::{mutation::Change, path, upload, Export},
    http::{access, query::ListQuery},
    operations::Operation,
};
use axum::{
    body::Body,
    extract::{Path, Query, State as AxumState},
    http::{HeaderMap, StatusCode},
    Json,
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::{io::AsyncWriteExt, sync::OwnedRwLockReadGuard, time::Instant};

/// Largest accepted upload body.
const MAX_UPLOAD_BYTES: usize = 256 * 1024 * 1024;

/// What the client asserted about the current state of the target.
enum WriteCondition {
    /// `If-None-Match: *`
    Create,
    /// `If-Match: <revision>`
    Replace(String),
}

impl WriteCondition {
    fn from_headers(headers: &HeaderMap) -> Result<Self, Error> {
        let expected = headers
            .get("if-match")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let create = headers
            .get("if-none-match")
            .is_some_and(|value| value == "*");
        match (create, expected) {
            (true, None) => Ok(Self::Create),
            (false, Some(revision)) => Ok(Self::Replace(revision)),
            // Neither, or both: the caller has not stated a usable precondition.
            _ => Err(Error::precondition_required("REVISION_REQUIRED")),
        }
    }

    fn expected(&self) -> Option<&str> {
        match self {
            Self::Create => None,
            Self::Replace(revision) => Some(revision),
        }
    }
}

pub async fn replace(
    AxumState(state): AxumState<Arc<State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
    body: Body,
) -> Result<(StatusCode, Json<Value>), Error> {
    let deadline = access::deadline(&headers)?;
    path::validate(&query.path)?;
    let condition = WriteCondition::from_headers(&headers)?;
    let gate = state.files.shared()?;
    let (identity, export) = access::writable_export(&state, &headers, &alias)?;
    let operation = state
        .operations
        .register(identity, &alias, access::operation_id(&headers)?)?;
    let task = operation.clone();
    let audit = json!({"identity": identity, "alias": alias, "operationId": access::operation_id(&headers)?,
        "action": "replace", "path": query.path});
    crate::core::events::emit(
        crate::core::events::Level::Debug,
        "mutation.accepted",
        audit.clone(),
    );
    let replacement = Replacement {
        state,
        export,
        path: query.path,
        condition,
        deadline,
        operation: task.clone(),
        gate,
    };
    // The operation owns the work and its cleanup after the waiter disconnects.
    tokio::spawn(async move {
        let result = replacement.run(body).await;
        mutation_finished(audit, &result);
        task.finish(result);
    });
    let (status, receipt) = operation.wait(deadline).await;
    Ok((status, Json(receipt)))
}

/// One in-flight replacement: everything the body writer and the commit need.
struct Replacement {
    state: Arc<State>,
    export: Arc<Export>,
    path: String,
    condition: WriteCondition,
    deadline: Instant,
    operation: Arc<Operation>,
    gate: OwnedRwLockReadGuard<()>,
}

impl Replacement {
    /// Stream the request body into a staging file, then publish it atomically.
    async fn run(self, body: Body) -> Result<Value, Error> {
        let Self {
            state,
            export,
            path,
            condition,
            deadline,
            operation,
            gate,
        } = self;
        let permit = state.workers.acquire(deadline).await?;
        operation.checkpoint(deadline)?;
        let staging = export.clone();
        let staged_path = path.clone();
        let staged = tokio::task::spawn_blocking(move || staging.stage(&staged_path))
            .await
            .map_err(|_| Error::internal())??;

        let mut writer = tokio::fs::File::from_std(staged.writer()?);
        let mut stream = body.into_data_stream();
        let mut size = 0usize;
        loop {
            operation.checkpoint(deadline)?;
            let chunk = tokio::select! {
                _ = operation.token().cancelled() => return Err(Error::cancelled()),
                chunk = tokio::time::timeout_at(deadline, stream.next()) => {
                    chunk.map_err(|_| Error::timed_out())?
                }
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(|_| Error::invalid())?;
            size += chunk.len();
            if size > MAX_UPLOAD_BYTES {
                return Err(Error::too_large("UPLOAD_LIMIT"));
            }
            tokio::time::timeout_at(deadline, writer.write_all(&chunk))
                .await
                .map_err(|_| Error::timed_out())??;
        }
        writer.flush().await?;
        drop(writer);

        let expected = condition.expected().map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _gate = gate;
            upload::commit(&export, &path, staged, expected.as_deref(), || {
                operation.checkpoint(deadline)
            })
        })
        .await
        .map_err(|_| Error::internal())?
        .map(|stat| json!(stat))
    }
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
    AxumState(state): AxumState<Arc<State>>,
    Path(alias): Path<String>,
    headers: HeaderMap,
    Json(command): Json<Command>,
) -> Result<(StatusCode, Json<Value>), Error> {
    let deadline = access::deadline(&headers)?;
    if command.recursive {
        return Err(Error::unsupported());
    }
    let audit_paths = json!({"action": command.action, "path": command.path, "to": command.to});
    let change = Change::parse(&command.action, command.path, command.to)?;
    let gate = state.files.shared()?;
    let (identity, export) = access::writable_export(&state, &headers, &alias)?;
    let operation = state
        .operations
        .register(identity, &alias, access::operation_id(&headers)?)?;
    let task = operation.clone();
    let audit = json!({"identity": identity, "alias": alias, "operationId": access::operation_id(&headers)?, "change": audit_paths});
    crate::core::events::emit(
        crate::core::events::Level::Debug,
        "mutation.accepted",
        audit.clone(),
    );
    tokio::spawn(async move {
        let result = match state.workers.acquire(deadline).await {
            Ok(permit) => {
                let tracked = task.clone();
                match tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let _gate = gate;
                    export.apply(&change, || tracked.checkpoint(deadline))
                })
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(Error::internal()),
                }
            }
            Err(error) => Err(error),
        };
        let result = result.map(|()| json!({}));
        mutation_finished(audit, &result);
        task.finish(result);
    });
    let (status, receipt) = operation.wait(deadline).await;
    Ok((status, Json(receipt)))
}

fn mutation_finished(operation: Value, result: &Result<Value, Error>) {
    crate::core::events::emit(
        if result.is_ok() {
            crate::core::events::Level::Info
        } else {
            crate::core::events::Level::Warn
        },
        "mutation.finished",
        json!({"operation": operation,
        "outcome": if result.is_ok() { "committed" } else { "not-committed" },
        "code": result.as_ref().err().map(|error| error.code)}),
    );
}
