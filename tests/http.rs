use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use fs_agent::{
    config::{Client, State},
    filesystem::Export,
    router,
};
use http_body_util::BodyExt;
use std::{collections::BTreeMap, sync::Arc};
use tower::ServiceExt;

fn app(root: &std::path::Path) -> axum::Router {
    app_export(Export::open(root.to_str().unwrap()).unwrap())
}
fn app_export(export: Export) -> axum::Router {
    let state = Arc::new(State {
        server_id: Some("test-node".into()),
        exports: BTreeMap::from([("docs".into(), Arc::new(export))]),
        clients: vec![Client {
            token: "test-secret-at-least-24-bytes".into(),
            username: None,
            exports: vec!["docs".into()],
            write_exports: vec!["docs".into()],
        }],
        operations: Default::default(),
        workers: Arc::new(tokio::sync::Semaphore::new(2)),
        cursor_key: [42; 32],
    });
    router(state, &["https://mindos.example".into()]).unwrap()
}

#[tokio::test]
async fn conditional_replace_serializes_writers_and_never_replays_ids() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note"), "original").unwrap();
    let export = Export::exclusive(root.path().to_str().unwrap()).unwrap();
    assert!(Export::exclusive(root.path().to_str().unwrap()).is_err());
    let server = app_export(export);
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/content?path=note")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let revision = response.headers()["etag"].clone();
    let put = |id: &str, text: &str| {
        request("/v1/fs/docs/content?path=note")
            .method("PUT")
            .header("if-match", &revision)
            .header("x-operation-id", id)
            .body(Body::from(text.to_owned()))
            .unwrap()
    };
    let (a, b) = tokio::join!(
        server.clone().oneshot(put("first", "one")),
        server.clone().oneshot(put("second", "two"))
    );
    let mut codes = [a.unwrap().status().as_u16(), b.unwrap().status().as_u16()];
    codes.sort();
    assert_eq!(codes, [200, 412]);
    let current = std::fs::read(root.path().join("note")).unwrap();
    let duplicate = server
        .clone()
        .oneshot(put("first", "different body"))
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    assert_eq!(std::fs::read(root.path().join("note")).unwrap(), current);
    let status = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/operations/first")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    let cancelled = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/operations/before/cancel")
                .method("POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cancelled.status(), StatusCode::OK);
    assert_eq!(
        server
            .oneshot(put("before", "cancelled"))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert!(!std::fs::read_dir(root.path()).unwrap().any(|v| v
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".itookit-upload-")));
}

#[tokio::test]
async fn creation_and_structure_operations_respect_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let server = app_export(Export::exclusive(root.path().to_str().unwrap()).unwrap());
    for (id, expected) in [
        ("create", StatusCode::OK),
        ("collision", StatusCode::PRECONDITION_FAILED),
    ] {
        let response = server
            .clone()
            .oneshot(
                request("/v1/fs/docs/content?path=new")
                    .method("PUT")
                    .header("if-none-match", "*")
                    .header("x-operation-id", id)
                    .body(Body::from("new bytes"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    for (id, body, expected) in [
        (
            "dir",
            r#"{"action":"mkdir","path":"folder"}"#,
            StatusCode::OK,
        ),
        (
            "move",
            r#"{"action":"rename","path":"new","to":"folder/moved"}"#,
            StatusCode::OK,
        ),
        (
            "escape",
            r#"{"action":"rename","path":"folder/moved","to":"../escape"}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "nonempty",
            r#"{"action":"remove","path":"folder"}"#,
            StatusCode::CONFLICT,
        ),
        (
            "remove",
            r#"{"action":"remove","path":"folder/moved"}"#,
            StatusCode::OK,
        ),
    ] {
        let response = server
            .clone()
            .oneshot(
                request("/v1/fs/docs/mutate")
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("x-operation-id", id)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{id}");
    }
}
fn request(uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .uri(uri)
        .header("authorization", "Bearer test-secret-at-least-24-bytes")
}

#[tokio::test]
async fn cancellation_during_upload_cleans_staging_and_never_publishes() {
    let root = tempfile::tempdir().unwrap();
    let server = app_export(Export::exclusive(root.path().to_str().unwrap()).unwrap());
    let slow = futures_util::stream::once(async { Ok::<_, std::io::Error>("first bytes") })
        .chain(futures_util::stream::pending());
    use futures_util::StreamExt;
    let upload = tokio::spawn(
        server.clone().oneshot(
            request("/v1/fs/docs/content?path=cancelled")
                .method("PUT")
                .header("if-none-match", "*")
                .header("x-operation-id", "uploading")
                .body(Body::from_stream(slow))
                .unwrap(),
        ),
    );
    for _ in 0..100 {
        if std::fs::read_dir(root.path()).unwrap().next().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    let response = server
        .clone()
        .oneshot(
            request("/v1/fs/docs/operations/uploading/cancel")
                .method("POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        upload.await.unwrap().unwrap().status(),
        StatusCode::REQUEST_TIMEOUT
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    let response = server
        .oneshot(
            request("/v1/fs/docs/operations/uploading")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let receipt: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(receipt["outcome"], "not-committed");
}

#[test]
fn restart_retires_validators_and_recovers_reserved_uploads() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note"), "content").unwrap();
    let old = {
        let export = Export::exclusive(root.path().to_str().unwrap()).unwrap();
        export.stat("note").unwrap().unwrap().revision
    };
    std::fs::write(root.path().join(".itookit-upload-abandoned"), "incomplete").unwrap();
    let export = Export::exclusive(root.path().to_str().unwrap()).unwrap();
    assert_ne!(export.stat("note").unwrap().unwrap().revision, old);
    assert!(!root.path().join(".itookit-upload-abandoned").exists());
}

#[tokio::test]
async fn reads_binary_and_enforces_authorization() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.bin"), [0, 255, 1, 128]).unwrap();
    let response = app(root.path())
        .oneshot(
            request("/v1/fs/docs/content?path=file.bin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        &response.into_body().collect().await.unwrap().to_bytes()[..],
        &[0, 255, 1, 128]
    );
    let response = app(root.path())
        .oneshot(
            Request::builder()
                .uri("/v1/exports")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = app(root.path())
        .oneshot(
            request("/v1/fs/unknown/content?path=file.bin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn traversal_and_links_cannot_read_outside_export() {
    let root = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/etc", root.path().join("escape")).unwrap();
    for path in ["..%2Fetc%2Fpasswd", "%2Fetc%2Fpasswd", "escape%2Fpasswd"] {
        let response = app(root.path())
            .oneshot(
                request(&format!("/v1/fs/docs/content?path={path}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(!response.status().is_success());
    }
    let response = app(root.path())
        .oneshot(
            request("/v1/fs/docs/stat")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"paths":["escape","missing","escape/passwd"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let data: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(data["results"][0]["stat"]["kind"], "symlink");
    assert!(data["results"][1]["stat"].is_null());
    assert_eq!(data["results"][2]["error"], "EACCES");
}

#[tokio::test]
async fn range_and_pagination_have_explicit_boundaries() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("bytes"), b"0123456789").unwrap();
    let response = app(root.path())
        .oneshot(
            request("/v1/fs/docs/content?path=bytes")
                .header("range", "bytes=3-5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["content-range"], "bytes 3-5/10");
    assert_eq!(
        &response.into_body().collect().await.unwrap().to_bytes()[..],
        b"345"
    );
    let response = app(root.path())
        .oneshot(
            request("/v1/fs/docs/content?path=bytes")
                .header("if-match", "\"old\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
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
    let page: serde_json::Value =
        serde_json::from_slice(&first.into_body().collect().await.unwrap().to_bytes()).unwrap();
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
    let page: serde_json::Value =
        serde_json::from_slice(&second.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(page["entries"].as_array().unwrap().len(), 9);
    assert!(page["nextCursor"].is_null());
}

#[tokio::test]
async fn rejects_mutations_and_invalid_deadline() {
    let root = tempfile::tempdir().unwrap();
    let response = app(root.path())
        .oneshot(
            request("/v1/fs/docs/content?path=x")
                .method("PUT")
                .body(Body::from("x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
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

#[tokio::test]
async fn password_clients_authenticate_with_basic_without_bearer_fallback() {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(State {
        server_id: Some("test-node".into()),
        exports: BTreeMap::from([(
            "docs".into(),
            Arc::new(Export::open(root.path().to_str().unwrap()).unwrap()),
        )]),
        clients: vec![Client {
            username: Some("alice".into()),
            token: "password-at-least-8-bytes".into(),
            exports: vec!["docs".into()],
            write_exports: vec![],
        }],
        operations: Default::default(),
        workers: Arc::new(tokio::sync::Semaphore::new(2)),
        cursor_key: [42; 32],
    });
    let server = router(state, &[]).unwrap();
    for (value, expected) in [
        (
            format!(
                "Basic {}",
                STANDARD.encode("alice:password-at-least-8-bytes")
            ),
            StatusCode::OK,
        ),
        (
            format!("Basic {}", STANDARD.encode("bob:password-at-least-8-bytes")),
            StatusCode::UNAUTHORIZED,
        ),
        (
            format!("Basic {}", STANDARD.encode("alice:wrong")),
            StatusCode::UNAUTHORIZED,
        ),
        ("Basic invalid!".into(), StatusCode::UNAUTHORIZED),
        (
            "Bearer password-at-least-8-bytes".into(),
            StatusCode::UNAUTHORIZED,
        ),
    ] {
        let response = server
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/exports")
                    .header("authorization", value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}

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
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["serverId"], "test-node");
    assert_eq!(value["process"]["exec"], false);
    assert_eq!(value["sync"]["push"], false);
}
