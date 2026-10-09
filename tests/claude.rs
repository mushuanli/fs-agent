use pi_agent::{config::Config, harness::Harnesses};
use serde_json::{json, Value};
use std::os::unix::fs::{symlink, PermissionsExt};
fn fixture(root: &std::path::Path) -> Harnesses {
    let script = root.join("claude-fixture.py");
    std::fs::write(&script, include_str!("fixtures/claude-sdk.py")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.join("native");
    std::fs::create_dir(&home).unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let config:Config=toml::from_str(&format!("listen=\"127.0.0.1:0\"\nexecution=false\ntoken=\"claude-fixture-token-0123456789\"\n[[harnesses]]\nid=\"claude\"\nkind=\"claude\"\ncommand={script:?}\nhome={home:?}\n[[harnesses.workspaces]]\nid=\"workspace\"\npath={workspace:?}\n")).unwrap();
    Harnesses::new(
        pi_agent::harness::config::validate(&config).unwrap(),
        "epoch".into(),
    )
    .unwrap()
}
async fn call(service: &Harnesses, name: &str, extra: Value) -> Value {
    let mut args = json!({"profileId":"claude"});
    for (k, v) in extra.as_object().unwrap() {
        args[k] = v.clone();
    }
    service.call(name, args).await.unwrap()
}
async fn mutate(service: &Harnesses, name: &str, id: &str, extra: Value) -> Value {
    let mut args = extra;
    args["epoch"] = json!("epoch");
    args["requestId"] = json!(id);
    call(service, name, args).await
}
async fn idle(service: &Harnesses, id: &Value) {
    for _ in 0..100 {
        if call(service, "harness_session_info", json!({"sessionId":id})).await["session"]["status"]
            == "idle"
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("turn did not complete");
}
#[tokio::test]
async fn sdk_roundtrips_native_history_receipts_approvals_questions_and_interruption() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let descriptor = service.profiles();
    assert_eq!(descriptor["profiles"][0]["kind"], "claude");
    assert_eq!(descriptor["profiles"][0]["capabilities"]["archive"], false);
    let created = mutate(
        &service,
        "harness_create",
        "create",
        json!({"workspaceId":"workspace"}),
    )
    .await;
    assert_eq!(created["outcome"], "committed");
    let id = &created["result"]["session"]["id"];
    let resumed_empty = mutate(
        &service,
        "harness_resume",
        "resume-empty",
        json!({"sessionId":id}),
    )
    .await;
    assert_eq!(resumed_empty["outcome"], "committed");
    let args = json!({"sessionId":id,"prompt":"quoted \"text\"\n中文","attachments":[{"kind":"text","name":"notes.md","content":"fixture attachment"}]});
    let turn = mutate(&service, "harness_turn", "turn", args.clone()).await;
    assert_eq!(turn["outcome"], "committed");
    assert_eq!(mutate(&service, "harness_turn", "turn", args).await, turn);
    idle(&service, id).await;
    let history = call(
        &service,
        "harness_session_read",
        json!({"sessionId":id,"toolDetail":"summary"}),
    )
    .await;
    assert!(history.to_string().contains("quoted"));
    assert!(history.to_string().contains("Fixture reply"));
    let search = call(
        &service,
        "harness_session_search",
        json!({"query":"fixture reply","mode":"content"}),
    )
    .await;
    assert_eq!(search["matches"].as_array().unwrap().len(), 1);
    assert_eq!(
        mutate(
            &service,
            "harness_turn",
            "tool",
            json!({"sessionId":id,"prompt":"tool-roundtrip"})
        )
        .await["outcome"],
        "committed"
    );
    idle(&service, id).await;
    let events = call(&service, "harness_events", json!({})).await;
    let tool_events = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["message"]["params"]["item"]["id"] == "fixture-tool")
        .collect::<Vec<_>>();
    assert_eq!(tool_events.len(), 2);
    assert_eq!(tool_events[0]["message"]["method"], "item/started");
    assert_eq!(
        tool_events[1]["message"]["params"]["item"]["status"],
        "completed"
    );
    assert!(!events.to_string().contains("private fixture tool output"));
    for (prompt, request, response) in [
        ("approval", "approval-request", json!({"decision":"accept"})),
        (
            "approval-cancel",
            "approval-request",
            json!({"decision":"cancel"}),
        ),
        (
            "question",
            "question-request",
            json!({"answers":{"0":{"answers":["A"]}}}),
        ),
    ] {
        let turn = mutate(
            &service,
            "harness_turn",
            prompt,
            json!({"sessionId":id,"prompt":prompt}),
        )
        .await;
        assert_eq!(turn["outcome"], "committed");
        for _ in 0..100 {
            if !call(&service, "harness_events", json!({})).await["requests"]
                .as_array()
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            mutate(
                &service,
                "harness_respond",
                &format!("respond-{prompt}"),
                json!({"nativeRequestId":request,"response":response})
            )
            .await["outcome"],
            "committed"
        );
        idle(&service, id).await;
        if prompt == "approval-cancel" {
            assert_eq!(
                call(&service, "harness_session_info", json!({"sessionId":id})).await["session"]
                    ["lastTurnResult"],
                "cancelled"
            );
        }
    }
    let waiting = mutate(
        &service,
        "harness_turn",
        "wait",
        json!({"sessionId":id,"prompt":"wait"}),
    )
    .await;
    assert_eq!(
        mutate(
            &service,
            "harness_interrupt",
            "stop",
            json!({"sessionId":id,"turnId":waiting["result"]["turnId"]})
        )
        .await["outcome"],
        "committed"
    );
    idle(&service, id).await;
    assert_eq!(
        call(&service, "harness_session_info", json!({"sessionId":id})).await["session"]
            ["lastTurnResult"],
        "cancelled"
    );
    assert_eq!(
        mutate(
            &service,
            "harness_archive",
            "archive",
            json!({"sessionId":id})
        )
        .await["code"],
        "ECAPABILITY"
    );
    service.close().await;
}
#[tokio::test]
async fn catalog_checks_actual_cwd_and_never_follows_foreign_transcript_links() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let workspace = root.path().join("workspace");
    let key = workspace
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let directory = root.path().join("native/projects").join(key);
    std::fs::create_dir_all(&directory).unwrap();
    let id = "11111111-1111-4111-8111-111111111111";
    let record = json!({"type":"user","sessionId":id,"cwd":root.path(),"uuid":"user","message":{"content":"foreign"},"timestamp":"2026-10-09T03:00:00Z"});
    std::fs::write(directory.join(format!("{id}.jsonl")), format!("{record}\n")).unwrap();
    assert!(
        call(&service, "harness_sessions", json!({})).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let linked = "22222222-2222-4222-8222-222222222222";
    symlink(
        directory.join(format!("{id}.jsonl")),
        directory.join(format!("{linked}.jsonl")),
    )
    .unwrap();
    assert!(
        call(&service, "harness_sessions", json!({})).await["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    service.close().await;
}

#[tokio::test]
async fn native_pages_preserve_turn_identity_and_resume_uses_verified_transcript() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let cwd = root.path().join("workspace");
    let key = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let folder = root.path().join("native/projects").join(key);
    std::fs::create_dir_all(&folder).unwrap();
    let id = "33333333-3333-4333-8333-333333333333";
    let mut records = Vec::new();
    for turn in 0..3 {
        let user = format!("user-{turn}");
        records.push(json!({"type":"user","uuid":user,"sessionId":id,"cwd":cwd,"message":{"content":format!("Prompt {turn} 中文\nquoted \"text\"")}}));
        for index in 0..60 {
            records.push(json!({"type":"assistant","uuid":format!("record-{turn}-{index}"),"parentUuid":format!("other-{index}"),"sessionId":id,"cwd":cwd,"message":{"id":format!("message-{turn}-{index}"),"content":[{"type":"text","text":format!("Reply {turn}:{index}")}]}}));
        }
    }
    let path = folder.join(format!("{id}.jsonl"));
    std::fs::write(
        &path,
        records.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    let session = call(&service, "harness_session_info", json!({"sessionId":id})).await;
    assert_eq!(session["session"]["owned"], false);
    let mut cursor = Value::Null;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let page = call(
            &service,
            "harness_session_read",
            json!({"sessionId":id,"cursor":cursor}),
        )
        .await;
        let mut count = 0;
        for envelope in page["turns"].as_array().unwrap() {
            for item in envelope["items"].as_array().unwrap() {
                count += 1;
                let identity = item["id"].as_str().unwrap();
                assert!(
                    seen.insert(identity.to_owned()),
                    "duplicate native item {identity}"
                );
                let turn = identity.split('-').nth(1).unwrap();
                assert_eq!(item["turnId"], format!("user-{turn}"));
            }
        }
        assert!(count <= 100);
        cursor = page["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    assert_eq!(seen.len(), 183);
    let resumed = mutate(
        &service,
        "harness_resume",
        "resume",
        json!({"sessionId":id}),
    )
    .await;
    assert_eq!(resumed["outcome"], "committed");
    assert_eq!(resumed["result"]["session"]["id"], id);
    assert_eq!(resumed["result"]["session"]["owned"], true);
    service.close().await;
}

#[tokio::test]
async fn rejects_foreign_records_inside_an_otherwise_authorized_transcript() {
    let root = tempfile::tempdir().unwrap();
    let service = fixture(root.path());
    let cwd = root.path().join("workspace");
    let key = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let folder = root.path().join("native/projects").join(key);
    std::fs::create_dir_all(&folder).unwrap();
    let id = "44444444-4444-4444-8444-444444444444";
    let mut transcript = format!(
        "{}\n",
        json!({"type":"user","uuid":"user","sessionId":id,"cwd":cwd,"message":{"content":"Authorized prompt"}})
    );
    for index in 0..500 {
        let mut record = json!({"type":"assistant","uuid":format!("item-{index}"),"sessionId":id,"message":{"id":format!("message-{index}"),"content":[{"type":"text","text":"x".repeat(1024)}]}});
        if index == 250 {
            record["cwd"] = json!(root.path());
        }
        transcript.push_str(&format!("{record}\n"));
    }
    std::fs::write(folder.join(format!("{id}.jsonl")), transcript).unwrap();
    assert_eq!(
        call(&service, "harness_session_info", json!({"sessionId":id})).await["session"]["owned"],
        false
    );
    let page = service
        .call(
            "harness_session_read",
            json!({"profileId":"claude","sessionId":id}),
        )
        .await
        .unwrap();
    // The newest page fits; an older page must reject the foreign record.
    let error = service
        .call(
            "harness_session_read",
            json!({"profileId":"claude","sessionId":id,"cursor":page["nextCursor"]}),
        )
        .await;
    let error = if let Ok(page) = error {
        service
            .call(
                "harness_session_read",
                json!({"profileId":"claude","sessionId":id,"cursor":page["nextCursor"]}),
            )
            .await
            .unwrap_err()
    } else {
        error.unwrap_err()
    };
    assert_eq!(error.code, "CLAUDE_SESSION_MISMATCH");
    let resumed = mutate(
        &service,
        "harness_resume",
        "resume-foreign",
        json!({"sessionId":id}),
    )
    .await;
    assert_eq!(resumed["code"], "CLAUDE_SESSION_MISMATCH");
    assert_ne!(resumed["outcome"], "committed");
    service.close().await;
}
