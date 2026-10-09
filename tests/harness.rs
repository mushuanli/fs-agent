use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use pi_agent::{
    app::State,
    config::Config,
    harness::{config::ProfileConfig, Harnesses},
};
use serde_json::{json, Value};
use std::{os::unix::fs::PermissionsExt, path::Path};
use tower::ServiceExt;

fn config(root: &Path, command: &str) -> Config {
    toml::from_str(&format!(
        r#"
listen = "127.0.0.1:0"
execution = false
token = "test-harness-token-0123456789"
[[harnesses]]
id = "codex"
kind = "codex"
command = {command:?}
home = {root:?}
[[harnesses.workspaces]]
id = "project"
path = {root:?}
"#
    ))
    .unwrap()
}

fn service(root: &Path) -> Harnesses {
    let fixture = root.join("codex-fixture.py");
    std::fs::write(&fixture, include_str!("fixtures/codex-app-server.py")).unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = config(root, fixture.to_str().unwrap());
    Harnesses::new(
        pi_agent::harness::config::validate(&config).unwrap(),
        "epoch".into(),
    )
    .unwrap()
}

async fn mutate(service: &Harnesses, name: &str, id: &str, extra: Value) -> Value {
    let mut args = json!({"profileId":"codex","epoch":"epoch","requestId":id});
    for (key, value) in extra.as_object().unwrap() {
        args[key] = value.clone();
    }
    service.call(name, args).await.unwrap()
}

#[tokio::test]
async fn creates_reads_resumes_streams_approves_and_interrupts_without_replay() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let created = mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    assert_eq!(created["outcome"], "committed");
    let repeated = mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    assert_eq!(created, repeated);
    let listed = service
        .call("harness_sessions", json!({"profileId":"codex"}))
        .await
        .unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["resumable"], true);
    assert_eq!(listed["sessions"][0]["createdAt"], 1791458500000i64);
    assert_eq!(listed["sessions"][0]["updatedAt"], 1791458565000i64);
    assert!(listed["sessions"][0]["branchName"].is_null());
    let history = service
        .call(
            "harness_session_read",
            json!({"profileId":"codex","sessionId":"session-1"}),
        )
        .await
        .unwrap();
    assert!(history["turns"].is_array());
    assert!(history["session"]["native"].get("turns").is_none());
    assert_eq!(
        mutate(
            &service,
            "harness_resume",
            "resume",
            json!({"sessionId":"session-1"})
        )
        .await["outcome"],
        "committed"
    );
    let turn = mutate(
        &service,
        "harness_turn",
        "turn",
        json!({"sessionId":"session-1","prompt":"test"}),
    )
    .await;
    assert_eq!(turn["result"]["turnId"], "turn-1");
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let events = service
        .call("harness_events", json!({"profileId":"codex"}))
        .await
        .unwrap();
    assert_eq!(events["requests"].as_array().unwrap().len(), 1);
    let approval = mutate(
        &service,
        "harness_respond",
        "approve",
        json!({"nativeRequestId":"approval-1","response":{"decision":"accept"}}),
    )
    .await;
    assert_eq!(approval["outcome"], "committed");
    assert_eq!(
        mutate(
            &service,
            "harness_interrupt",
            "interrupt",
            json!({"sessionId":"session-1","turnId":"turn-1"})
        )
        .await["outcome"],
        "committed"
    );
    assert_eq!(
        mutate(
            &service,
            "harness_turn",
            "turn",
            json!({"sessionId":"session-1","prompt":"test"})
        )
        .await,
        turn
    );
    service.close().await;
}

#[tokio::test]
async fn rejects_unauthorized_resume_foreign_turn_epoch_and_changed_id_payload() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let denied = mutate(
        &service,
        "harness_resume",
        "outside",
        json!({"sessionId":"foreign"}),
    )
    .await;
    assert_eq!(denied["code"], "EACCES");
    let denied = mutate(
        &service,
        "harness_turn",
        "foreign",
        json!({"sessionId":"foreign","prompt":"test"}),
    )
    .await;
    assert_eq!(denied["code"], "HARNESS_SESSION_NOT_OWNED");
    assert!(service
        .call(
            "harness_create",
            json!({"profileId":"codex","epoch":"old","requestId":"id","workspaceId":"project"})
        )
        .await
        .is_err());
    mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    assert_eq!(service.call("harness_create",json!({"profileId":"codex","epoch":"epoch","requestId":"create","workspaceId":"other"})).await.unwrap_err().code,"HARNESS_REQUEST_REUSED");
    service.close().await;
}

#[test]
fn rejects_profiles_overlapping_exclusive_exports() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config(root.path(), "codex");
    config.exports.push(pi_agent::config::ExportConfig {
        path: root.path().to_str().unwrap().into(),
        alias: Some("rw".into()),
        access: pi_agent::config::Access::Rw,
    });
    assert!(pi_agent::harness::config::validate(&config).is_err());
    let mut duplicate: ProfileConfig = config.harnesses[0].clone();
    duplicate.id = "codex".into();
    config.harnesses.push(duplicate);
    assert!(pi_agent::harness::config::validate(&config).is_err());
}

fn body(method: &str) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":method,"params":{"_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientInfo":{"name":"test","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}}}})
}

#[tokio::test]
async fn mcp_auth_version_metadata_origin_and_discovery() {
    let root = tempfile::tempdir().unwrap();
    let state = State::from_config(&config(root.path(), "codex")).unwrap();
    let app = pi_agent::router(state, &[]).unwrap();
    let request = |auth: bool, version: &str, origin: Option<&str>| {
        let mut builder = Request::post("/mcp")
            .header("content-type", "application/json")
            .header("mcp-method", "server/discover")
            .header("mcp-protocol-version", version);
        if auth {
            builder = builder.header("authorization", "Bearer test-harness-token-0123456789");
        }
        if let Some(origin) = origin {
            builder = builder.header("origin", origin);
        }
        builder
            .body(Body::from(body("server/discover").to_string()))
            .unwrap()
    };
    assert_eq!(
        app.clone()
            .oneshot(request(false, "2026-07-28", None))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.clone()
            .oneshot(request(true, "2025-11-25", None))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.clone()
            .oneshot(request(true, "2026-07-28", Some("https://evil.example")))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = app
        .oneshot(request(true, "2026-07-28", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(result["result"]["supportedVersions"][0], "2026-07-28");
    assert_eq!(result["result"]["_meta"]["itookit/pi-agent"]["version"], 1);
    assert_eq!(
        result["result"]["_meta"]["itookit/pi-agent"]["harness"],
        true
    );
    assert_eq!(
        result["result"]["_meta"]["itookit/pi-agent"]["fileProtocol"],
        "fs-agent-http-v1"
    );
}

#[tokio::test]
async fn native_codex_creates_legacy_sessions_and_reads_unmaterialized_history() {
    if std::env::var("PI_AGENT_CODEX_TEST").as_deref() != Ok("1") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path(), "codex");
    let service = Harnesses::new(
        pi_agent::harness::config::validate(&config).unwrap(),
        "epoch".into(),
    )
    .unwrap();
    let sessions = service
        .call("harness_sessions", json!({"profileId":"codex"}))
        .await
        .unwrap();
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 0);
    let created = mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    assert_eq!(created["outcome"], "committed");
    assert_eq!(
        created["result"]["session"]["native"]["historyMode"],
        "legacy"
    );
    let id = created["result"]["session"]["id"].as_str().unwrap();
    let history = service
        .call(
            "harness_session_read",
            json!({"profileId":"codex","sessionId":id}),
        )
        .await
        .unwrap();
    assert_eq!(history["turns"], json!([]));
    assert_eq!(history["session"]["owned"], true);
    let compact = service
        .call(
            "harness_session_read",
            json!({"profileId":"codex","sessionId":id,"toolDetail":"summary"}),
        )
        .await
        .unwrap();
    assert_eq!(compact["turns"], json!([]));
    service.close().await;
}

#[tokio::test]
async fn shutdown_fences_new_harness_commands() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    service.close().await;
    assert_eq!(service.call("harness_create", json!({"profileId":"codex","epoch":"epoch","requestId":"after","workspaceId":"project"})).await.unwrap_err().code, "EIO");
}

#[tokio::test]
async fn approvals_can_unblock_a_turn_before_its_start_reply() {
    let root = tempfile::tempdir().unwrap();
    let service = std::sync::Arc::new(service(root.path()));
    mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"project"}),
    )
    .await;
    let owner = service.clone();
    let turn = tokio::spawn(async move {
        mutate(
            &owner,
            "harness_turn",
            "turn",
            json!({"sessionId":"session-1","prompt":"approval-before-start-reply"}),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let events = service
                .call("harness_events", json!({"profileId":"codex"}))
                .await
                .unwrap();
            if !events["requests"].as_array().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let reply = mutate(
            &service,
            "harness_respond",
            "approval",
            json!({"nativeRequestId":"approval-early","response":{"decision":"accept"}}),
        )
        .await;
        assert_eq!(reply["outcome"], "committed");
        assert_eq!(turn.await.unwrap()["outcome"], "committed");
    })
    .await
    .unwrap();
    service.close().await;
}

#[tokio::test]
async fn native_branches_keep_parent_identity_and_fork_receipts_prevent_duplicates() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    mutate(
        &service,
        "harness_create",
        "base",
        json!({"workspaceId":"project"}),
    )
    .await;
    let args = json!({"sessionId":"session-1","name":"Alternative"});
    let forked = mutate(&service, "harness_fork", "fork", args.clone()).await;
    assert_eq!(forked["outcome"], "committed");
    assert_eq!(forked["result"]["session"]["parentSessionId"], "session-1");
    assert_eq!(forked["result"]["session"]["title"], "Alternative");
    assert_eq!(forked["result"]["session"]["branchName"], "Alternative");
    assert_eq!(mutate(&service, "harness_fork", "fork", args).await, forked);
    let list = service
        .call("harness_sessions", json!({"profileId":"codex"}))
        .await
        .unwrap();
    assert_eq!(list["sessions"].as_array().unwrap().len(), 2);
    let denied = mutate(
        &service,
        "harness_fork",
        "denied",
        json!({"sessionId":"foreign"}),
    )
    .await;
    assert_eq!(denied["code"], "EACCES");
    service.close().await;
}

#[tokio::test]
async fn native_management_preserves_history_and_never_adopts_or_replays_mutations() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    mutate(
        &service,
        "harness_create",
        "create-management",
        json!({"workspaceId":"project"}),
    )
    .await;
    let args = json!({"sessionId":"session-1","name":"Renamed title"});
    let renamed = mutate(
        &service,
        "harness_rename",
        "rename-management",
        args.clone(),
    )
    .await;
    assert_eq!(renamed["result"]["session"]["title"], "Renamed title");
    assert_eq!(
        mutate(&service, "harness_rename", "rename-management", args).await,
        renamed
    );
    let outside = mutate(
        &service,
        "harness_rename",
        "outside-management",
        json!({"sessionId":"foreign","name":"Denied"}),
    )
    .await;
    assert_eq!(outside["code"], "EACCES");
    let archived = mutate(
        &service,
        "harness_archive",
        "archive-management",
        json!({"sessionId":"session-1"}),
    )
    .await;
    assert_eq!(archived["result"]["session"]["archived"], true);
    assert_eq!(archived["result"]["session"]["owned"], false);
    let page = service
        .call(
            "harness_sessions",
            json!({"profileId":"codex","archived":true}),
        )
        .await
        .unwrap();
    assert_eq!(page["sessions"].as_array().unwrap().len(), 1);
    let restored = mutate(
        &service,
        "harness_unarchive",
        "restore-management",
        json!({"sessionId":"session-1"}),
    )
    .await;
    assert_eq!(restored["result"]["session"]["archived"], false);
    assert_eq!(restored["result"]["session"]["owned"], false);
    let denied = mutate(
        &service,
        "harness_archive",
        "unowned-management",
        json!({"sessionId":"session-1"}),
    )
    .await;
    assert_eq!(denied["code"], "HARNESS_SESSION_NOT_OWNED");
    service.close().await;
}
