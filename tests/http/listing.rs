//! `GET /v1/fs/:alias/entries` and `POST /v1/fs/:alias/stat` — listing policy.

use crate::common::{app, body_json, request};
use axum::{body::Body, http::StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn pagination_has_an_explicit_boundary() {
    let root = tempfile::tempdir().unwrap();
    for n in 0..520 {
        std::fs::write(root.path().join(format!("f{n:04}")), "").unwrap();
    }
    let server = app(root.path());
    let first = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/entries?path=")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let page = body_json(first).await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 512);
    let second = server
        .oneshot(
            request(&format!(
                "/v1/fs/docs/entries?path=&cursor={}",
                page["nextCursor"].as_str().unwrap()
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    let page = body_json(second).await;
    // 512 + 8 = 520 entries in total.
    assert_eq!(page["entries"].as_array().unwrap().len(), 8);
    assert!(page["nextCursor"].is_null());
}

/// A cursor is bound to its identity, alias and path, and cannot be replayed.
#[tokio::test]
async fn cursors_are_bound_to_their_listing() {
    let root = tempfile::tempdir().unwrap();
    for n in 0..520 {
        std::fs::write(root.path().join(format!("f{n:04}")), "").unwrap();
    }
    let server = app(root.path());
    let page = body_json(
        server
            .clone()
            .oneshot(
                request("/v1/fs/docs/entries?path=")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    let cursor = page["nextCursor"].as_str().unwrap();
    let tampered = format!("{cursor}x");
    let response = server
        .oneshot(
            request(&format!("/v1/fs/docs/entries?path=&cursor={tampered}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// A colon is legal inside a name; only a leading drive prefix is rejected.
#[tokio::test]
async fn colon_names_are_listable_and_readable() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("2024:Q1.md"), "quarter").unwrap();
    let server = app(root.path());
    let page = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/entries?path=")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(body_json(page).await["entries"][0]["name"], "2024:Q1.md");
    let content = server
        .oneshot(
            request("/v1/fs/docs/content?path=2024:Q1.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(content.status(), StatusCode::OK);
}

#[tokio::test]
async fn stat_batches_are_bounded_and_report_per_path_failures() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("present"), "x").unwrap();
    let server = app(root.path());
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/stat")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"paths":["present","missing"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let data = body_json(response).await;
    assert_eq!(data["results"][0]["stat"]["kind"], "file");
    assert!(data["results"][1]["stat"].is_null());
    // An unknown field is rejected rather than ignored.
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/stat")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"paths":[],"extra":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    // A batch larger than the documented limit is rejected.
    let paths: Vec<String> = (0..257).map(|n| format!("f{n}")).collect();
    let response = server
        .oneshot(
            request("/v1/fs/docs/stat")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"paths": paths}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_invalid_deadline_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let response = app(root.path())
        .oneshot(
            request("/v1/fs/docs/entries?path=")
                .header("x-timeout-ms", "0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
