//! Capability discovery.

use crate::common::{app, body_json, request};
use axum::{body::Body, http::Request, http::StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn support_discovery_is_authenticated_and_files_only() {
    let root = tempfile::tempdir().unwrap();
    let server = app(root.path());
    let response = server
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/capabilities")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = server
        .oneshot(request("/v1/capabilities").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["serverId"], "test-node");
    assert_eq!(value["process"]["exec"], false);
    assert_eq!(value["sync"]["push"], false);
    assert_eq!(value["files"]["read"], true);
    assert_eq!(value["files"]["write"], true);
}

/// `/v1/exports` describes access per alias and never leaks host paths.
#[tokio::test]
async fn exports_are_listed_with_their_access_mode() {
    let root = tempfile::tempdir().unwrap();
    let server = app(root.path());
    let response = server
        .oneshot(request("/v1/exports").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["version"], 1);
    assert_eq!(value["exports"][0]["alias"], "docs");
    // The export is read-only, so no revision can be promised.
    assert_eq!(value["exports"][0]["access"], "ro");
    assert_eq!(value["exports"][0]["strongRevision"], false);
    assert!(value
        .to_string()
        .find(root.path().to_str().unwrap())
        .is_none());
}
