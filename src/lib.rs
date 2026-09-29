pub mod config;
mod content;
mod cursor;
mod error;
pub mod filesystem;
pub mod launch;
mod mutations;
pub mod operations;
mod recovery;
mod request;
mod revision;
mod routes;
pub mod workspaces;

use axum::{
    extract::DefaultBodyLimit,
    http::{HeaderName, HeaderValue, Method},
    middleware,
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};

pub fn router(state: Arc<config::State>, origins: &[String]) -> Result<Router, String> {
    let headers = [
        "authorization",
        "content-type",
        "range",
        "if-range",
        "if-match",
        "x-timeout-ms",
        "x-operation-id",
        "if-none-match",
    ]
    .map(HeaderName::from_static);
    let cors = CorsLayer::new()
        .allow_origin(allow_origin(origins)?)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::OPTIONS])
        .allow_headers(headers)
        .expose_headers([
            HeaderName::from_static("content-range"),
            HeaderName::from_static("etag"),
        ]);
    Ok(Router::new()
        .route("/v1/capabilities", get(routes::capabilities))
        .route("/v1/exports", get(routes::exports))
        .route("/v1/fs/:alias/stat", post(routes::stat))
        .route("/v1/fs/:alias/entries", get(routes::entries))
        .route(
            "/v1/fs/:alias/content",
            get(content::content).put(mutations::replace),
        )
        .route("/v1/fs/:alias/mutate", post(mutations::mutate))
        .route("/v1/fs/:alias/operations/:id", get(operations::status))
        .route(
            "/v1/fs/:alias/operations/:id/cancel",
            post(operations::cancel),
        )
        .layer(DefaultBodyLimit::max(128 * 1024))
        .layer(cors)
        .layer(middleware::map_response(
            |mut response: axum::response::Response| async move {
                response
                    .headers_mut()
                    .insert("cache-control", HeaderValue::from_static("no-store"));
                response
            },
        ))
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
