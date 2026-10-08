use crate::common::{app_export, body_json, request};
use axum::{body::Body, http::StatusCode};
use pi_agent::fs::Export;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn seq_routes_enforce_authorization_and_write_permissions() {
    let root = tempfile::tempdir().unwrap();
    let server = app_export(Export::exclusive(root.path()).unwrap());
    let update = json!({"path":"info.seq","expectedRevision":null,"changes":[{"action":"set","key":"name","value":"Project"}]}).to_string();
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/seq/transaction")
                .method("POST")
                .header("content-type", "application/json")
                .header("x-operation-id", "seq-create")
                .body(Body::from(update.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["outcome"], "committed");
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/seq/snapshot")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"info.seq"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(response).await["entries"][0]["value"], "Project");
    let readonly = app_export(Export::open(root.path()).unwrap());
    let response = readonly
        .oneshot(
            request("/v1/fs/docs/seq/transaction")
                .method("POST")
                .header("content-type", "application/json")
                .header("x-operation-id", "readonly")
                .body(Body::from(update))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = server
        .oneshot(
            request("/v1/fs/unknown/seq/snapshot")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"path":"info.seq"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
