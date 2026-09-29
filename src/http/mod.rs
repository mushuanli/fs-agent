//! HTTP transport.
//!
//! The router is the only place that knows the URL layout, the only place that
//! decides CORS and body limits, and it delegates every request to a handler
//! that extracts input and calls one domain operation. Handlers never reach
//! into filesystem or process internals directly.

pub mod access;
mod events;
pub mod handlers;
pub mod query;
pub mod range;

use crate::app::State;
use axum::{
    extract::DefaultBodyLimit,
    http::{HeaderName, HeaderValue, Method},
    middleware,
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Largest JSON body the extractors accept.
///
/// The client budgets 64 KiB of raw paths per batch; JSON escaping can inflate
/// that, so the limit leaves headroom instead of rejecting a legal batch.
const MAX_JSON_BODY: usize = 512 * 1024;

/// Headers a browser client may send; anything else fails the CORS preflight.
const ALLOWED_HEADERS: [&str; 8] = [
    "authorization",
    "content-type",
    "range",
    "if-range",
    "if-match",
    "if-none-match",
    "x-timeout-ms",
    "x-operation-id",
];

pub fn router(state: Arc<State>, origins: &[String]) -> Result<Router, String> {
    let cors = CorsLayer::new()
        .allow_origin(allow_origin(origins)?)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::OPTIONS])
        .allow_headers(ALLOWED_HEADERS.map(HeaderName::from_static))
        .expose_headers([
            HeaderName::from_static("content-range"),
            HeaderName::from_static("etag"),
        ]);
    Ok(Router::new()
        .route(
            "/v1/capabilities",
            get(handlers::capabilities::capabilities),
        )
        .route("/v1/exports", get(handlers::exports::exports))
        .route("/v1/fs/:alias/stat", post(handlers::stat::stat))
        .route("/v1/fs/:alias/entries", get(handlers::entries::entries))
        .route(
            "/v1/fs/:alias/content",
            get(handlers::content::content).put(handlers::mutations::replace),
        )
        .route("/v1/fs/:alias/mutate", post(handlers::mutations::mutate))
        .route(
            "/v1/fs/:alias/operations/:id",
            get(handlers::operations::status),
        )
        .route(
            "/v1/fs/:alias/operations/:id/cancel",
            post(handlers::operations::cancel),
        )
        .route("/v1/processes", post(handlers::processes::start))
        .route("/v1/processes/:epoch/:id", get(handlers::processes::status))
        .route(
            "/v1/processes/:epoch/:id/cancel",
            post(handlers::processes::cancel),
        )
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY))
        .layer(cors)
        // File content and metadata must never be cached by an intermediary.
        .layer(middleware::map_response(
            |mut response: axum::response::Response| async move {
                response
                    .headers_mut()
                    .insert("cache-control", HeaderValue::from_static("no-store"));
                response
            },
        ))
        .layer(middleware::from_fn(events::failures))
        .with_state(state))
}

/// `["*"]` allows every browser origin; otherwise entries must match exactly.
fn allow_origin(origins: &[String]) -> Result<AllowOrigin, String> {
    if origins.iter().any(|origin| origin == "*") {
        return Ok(AllowOrigin::any());
    }
    let values = origins
        .iter()
        .map(|value| {
            value
                .parse::<HeaderValue>()
                .map_err(|_| format!("Invalid CORS origin {value:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AllowOrigin::list(values))
}

#[cfg(test)]
mod tests {
    use super::allow_origin;

    #[test]
    fn wildcard_and_exact_origins_are_accepted() {
        assert!(allow_origin(&["*".to_owned()]).is_ok());
        assert!(allow_origin(&["*".to_owned(), "http://localhost:3000".to_owned()]).is_ok());
        assert!(allow_origin(&["http://localhost:3000".to_owned()]).is_ok());
        assert!(allow_origin(&[]).is_ok());
        assert!(allow_origin(&["http://bad\norigin".to_owned()]).is_err());
    }
}
