use crate::{
    config::State,
    error::{invalid, Error},
    filesystem::Export,
};
use axum::http::{HeaderMap, StatusCode};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{sync::Arc, time::Duration};

pub fn identity(state: &State, headers: &HeaderMap) -> Result<usize, Error> {
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(Error(StatusCode::UNAUTHORIZED, "EACCES"))?;
    let basic = authorization
        .strip_prefix("Basic ")
        .and_then(|value| STANDARD.decode(value).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok());
    let credentials = basic.as_deref().and_then(|value| value.split_once(':'));
    state
        .clients
        .iter()
        .position(|client| match (&client.username, credentials) {
            (Some(user), Some((name, password))) => {
                equal(user.as_bytes(), name.as_bytes())
                    & equal(client.token.as_bytes(), password.as_bytes())
            }
            (None, _) => authorization
                .strip_prefix("Bearer ")
                .is_some_and(|token| equal(client.token.as_bytes(), token.as_bytes())),
            _ => false,
        })
        .ok_or(Error(StatusCode::UNAUTHORIZED, "EACCES"))
}
fn equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}
pub fn export(state: &State, headers: &HeaderMap, alias: &str) -> Result<Arc<Export>, Error> {
    let client = &state.clients[identity(state, headers)?];
    if !client.exports.iter().any(|v| v == alias) {
        return Err(Error(StatusCode::FORBIDDEN, "EACCES"));
    }
    state
        .exports
        .get(alias)
        .cloned()
        .ok_or(Error(StatusCode::FORBIDDEN, "EACCES"))
}
pub fn deadline(headers: &HeaderMap) -> Result<tokio::time::Instant, Error> {
    let budget = match headers.get("x-timeout-ms") {
        None => 30_000,
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .ok_or_else(invalid)?,
    };
    Ok(tokio::time::Instant::now() + Duration::from_millis(budget.min(30_000)))
}
pub async fn blocking<T: Send + 'static>(
    state: &Arc<State>,
    deadline: tokio::time::Instant,
    run: impl FnOnce(tokio_util::sync::CancellationToken) -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    let token = tokio_util::sync::CancellationToken::new();
    let _guard = token.clone().drop_guard();
    let work = async {
        let permit = state
            .workers
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error(StatusCode::SERVICE_UNAVAILABLE, "EIO"))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if token.is_cancelled() {
                return Err(Error(StatusCode::REQUEST_TIMEOUT, "ECANCELLED"));
            }
            run(token)
        })
        .await
        .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "EIO"))?
    };
    tokio::time::timeout_at(deadline, work)
        .await
        .map_err(|_| Error(StatusCode::GATEWAY_TIMEOUT, "ETIMEDOUT"))?
}
