use crate::{
    config,
    error::{invalid, Error},
    request,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct Operations(Mutex<HashMap<(usize, String, String), Arc<Operation>>>);
pub struct Operation {
    pub cancel: CancellationToken,
    pub result: Mutex<Option<(StatusCode, Value)>>,
    pub notify: Notify,
    created: Instant,
}
impl Operation {
    fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            result: Mutex::new(None),
            notify: Notify::new(),
            created: Instant::now(),
        }
    }
    pub fn finish(&self, result: Result<Value, Error>) {
        let result = match result {
            Ok(value) => (
                StatusCode::OK,
                json!({"outcome":"committed", "result":value}),
            ),
            Err(error) => (error.0, json!({"outcome":"not-committed", "code":error.1})),
        };
        *self.result.lock().unwrap() = Some(result);
        self.notify.notify_waiters();
    }
    pub fn status(&self) -> Value {
        self.result
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| json!({"outcome":"unknown", "state":"running"}))
    }
}
impl Operations {
    pub fn cancel_all(&self) {
        for op in self.0.lock().unwrap().values() {
            op.cancel.cancel();
        }
    }
    fn access(
        &self,
        identity: usize,
        alias: &str,
        id: &str,
        create: bool,
    ) -> Result<Arc<Operation>, Error> {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|v| v.is_ascii_alphanumeric() || b"_-".contains(&v))
        {
            return Err(invalid());
        }
        let mut entries = self.0.lock().unwrap();
        entries.retain(|_, op| {
            op.result.lock().unwrap().is_none() || op.created.elapsed() < Duration::from_secs(3600)
        });
        let key = (identity, alias.to_owned(), id.to_owned());
        if let Some(op) = entries.get(&key) {
            if create {
                return Err(Error(StatusCode::CONFLICT, "OPERATION_ID_REUSED"));
            }
            return Ok(op.clone());
        }
        if !create {
            return Err(Error(StatusCode::NOT_FOUND, "OPERATION_UNKNOWN"));
        }
        if entries.len() >= 4096 {
            return Err(Error(StatusCode::TOO_MANY_REQUESTS, "OPERATION_LIMIT"));
        }
        let op = Arc::new(Operation::new());
        entries.insert(key, op.clone());
        Ok(op)
    }
    pub fn register(
        &self,
        identity: usize,
        alias: &str,
        id: &str,
    ) -> Result<Arc<Operation>, Error> {
        self.access(identity, alias, id, true)
    }
}
pub fn operation_id(headers: &HeaderMap) -> Result<&str, Error> {
    headers
        .get("x-operation-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(invalid)
}
pub async fn status(
    State(state): State<Arc<config::State>>,
    Path((alias, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, Error> {
    request::export(&state, &headers, &alias)?;
    Ok(Json(
        state
            .operations
            .access(request::identity(&state, &headers)?, &alias, &id, false)?
            .status(),
    ))
}
pub async fn cancel(
    State(state): State<Arc<config::State>>,
    Path((alias, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, Error> {
    request::export(&state, &headers, &alias)?;
    let identity = request::identity(&state, &headers)?;
    let op = match state.operations.access(identity, &alias, &id, false) {
        Ok(op) => op,
        Err(Error(StatusCode::NOT_FOUND, _)) => {
            match state.operations.register(identity, &alias, &id) {
                Ok(op) => {
                    op.finish(Err(Error(StatusCode::REQUEST_TIMEOUT, "ECANCELLED")));
                    op
                }
                Err(_) => state.operations.access(identity, &alias, &id, false)?,
            }
        }
        Err(error) => return Err(error),
    };
    op.cancel.cancel();
    Ok(Json(op.status()))
}
