//! Read traffic is silent unless HTTP reports a failure; bodies and queries are omitted.
use axum::{extract::Request, middleware::Next, response::Response};

pub async fn failures(request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let path: String = request.uri().path().chars().take(512).collect();
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    if response.status().is_client_error() || response.status().is_server_error() {
        crate::core::events::emit(
            if response.status().is_server_error() {
                crate::core::events::Level::Error
            } else {
                crate::core::events::Level::Warn
            },
            "http.failed",
            serde_json::json!({"method": method, "path": path,
            "status": response.status().as_u16(), "elapsedMs": started.elapsed().as_millis()}),
        );
    }
    response
}
