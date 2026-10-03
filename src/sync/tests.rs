use super::{model::*, store::metadata as m, SyncService};
use serde_json::{json, Value};
use std::{io::Write, sync::Arc, time::Duration};

#[test]
fn invalid_uploaded_digest_is_input_error_and_existing_objects_stay_ready() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let good = object(&s, b"old");
    let id = s.reserve("p", &digest(b"new"), 3).unwrap();
    let mut file = s.temporary_file(&id).unwrap();
    file.write_all(b"bad").unwrap();
    drop(file);
    let error = s.install(&id).unwrap_err();
    assert_eq!(error.code, "OBJECT_HASH_MISMATCH");
    assert!(!error.unknown);
    assert!(s.healthy());
    assert!(s.object_file("p", &good).is_ok());
    s.abandon(&id).unwrap();
}

#[test]
fn old_current_project_and_dataset_get_delete_time_protection() {
    for whole_project in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let s = service(root.path());
        project(&s);
        let content = object(&s, b"retained");
        let bytes = serde_json::to_vec(&json!({"entries":[{"kind":"file","path":"note.txt","hash":content,"size":"8"}],"format":"fs-agent.files","version":1})).unwrap();
        let hash = object(&s, &bytes);
        s.command(
            "projects/p/datasets",
            &command(
                &s,
                2,
                json!({"datasetId":"files","kind":"files","logicalId":"files","manifestHash":hash}),
            ),
            false,
        )
        .unwrap();
        let db = s.connection().unwrap();
        db.execute(
            "UPDATE records SET value=json_set(value,'$.committedAt',?1) WHERE kind='version'",
            [s.time() - 90 * 86400],
        )
        .unwrap();
        db.execute("UPDATE objects SET retain_until=0", []).unwrap();
        drop(db);
        let target = if whole_project {
            "projects/p/delete"
        } else {
            "projects/p/datasets/files/delete"
        };
        let result = s
            .command(
                target,
                &command(
                    &s,
                    3,
                    json!({"expectedHead":{"generation":"1","manifestHash":hash}}),
                ),
                false,
            )
            .unwrap();
        assert_eq!(result["outcome"], "committed");
        s.gc().unwrap();
        let until: u64 = s
            .connection()
            .unwrap()
            .query_row(
                "SELECT retain_until FROM objects WHERE hash=?1",
                [&hash],
                |r| r.get(0),
            )
            .unwrap();
        assert!(until >= s.time() + s.config.trash_retention_seconds - 1);
        if whole_project {
            let mut body = command(&s, 4, json!({}));
            body["expectedProjectLifecycleRevision"] = json!("2");
            assert_eq!(
                s.command("projects/p/restore", &body, false).unwrap()["outcome"],
                "committed"
            );
        } else {
            let body = command(
                &s,
                4,
                json!({"expectedDeletedGeneration":"2","sourceGeneration":"1"}),
            );
            assert_eq!(
                s.command("projects/p/datasets/files/restore", &body, false)
                    .unwrap()["outcome"],
                "committed"
            );
        }
        assert!(s.object_file("p", &hash).is_ok());
        assert_eq!(s.object_file("p", &content).unwrap().1, 8);
    }
}

#[test]
fn expired_change_cursor_does_not_prevent_history_discovery_and_restore() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let old = dataset(&s);
    let next = object(&s, b"{\"entries\":[{\"kind\":\"directory\",\"path\":\"new\"}],\"format\":\"fs-agent.files\",\"version\":1}");
    let body = command(
        &s,
        3,
        json!({"expectedHead":{"generation":"1","manifestHash":old},"nextManifestHash":next}),
    );
    s.command("projects/p/datasets/files/publish", &body, false)
        .unwrap();
    let mut token: Value = json!({"epoch":s.epoch(),"namespace":s.identity.namespace_id,"project":"p","kind":"changes","upper":2,"last":"2","state":"all","expires":0});
    token["expires"] = json!(s.time() - 1);
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use hmac::{Hmac, Mac};
    let bytes = serde_json::to_vec(&token).unwrap();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(s.identity.cursor_key.as_bytes()).unwrap();
    mac.update(&bytes);
    let cursor = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(bytes),
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    );
    assert_eq!(
        s.changes("p", Some(&cursor), 100).unwrap_err().code,
        "CURSOR_EXPIRED"
    );
    let versions = s.versions("p", "files", None, None, 100).unwrap();
    assert_eq!(versions["versions"][0]["contentStatus"], "available");
    let body = command(
        &s,
        4,
        json!({"expectedHead":{"generation":"2","manifestHash":next},"nextManifestHash":old}),
    );
    let result = s
        .command("projects/p/datasets/files/publish", &body, false)
        .unwrap();
    assert_eq!(result["result"]["head"]["generation"], "3");
}

#[test]
fn accepted_command_can_finish_after_shutdown_stops_admission() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    let (started, received) = std::sync::mpsc::channel();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let worker_barrier = barrier.clone();
    let task = s.clone();
    let worker = std::thread::spawn(move || {
        task.with_write(|db| {
            let tx = db.transaction()?;
            started.send(()).unwrap();
            worker_barrier.wait();
            m::put(&tx, "", "info", "finished", &json!(true))?;
            task.commit(tx)
        })
    });
    received.recv_timeout(Duration::from_secs(2)).unwrap();
    s.stop();
    assert!(s.drain_timeout(Duration::ZERO).is_err());
    barrier.wait();
    worker.join().unwrap().unwrap();
    s.drain_timeout(Duration::ZERO).unwrap();
    assert_eq!(
        m::get::<Value>(&s.connection().unwrap(), "", "info", "finished").unwrap(),
        Some(json!(true))
    );
}

#[test]
fn ack_rejects_expired_replicas_and_growth_at_metadata_limit() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let cursor = s.changes("p", None, 100).unwrap()["cursor"].clone();
    let body = json!({"cursor":cursor,"scopeRevision":"1"});
    let db = s.connection().unwrap();
    db.execute(
        "UPDATE records SET value=json_set(value,'$.lastSeen',0) WHERE kind='replica'",
        [],
    )
    .unwrap();
    drop(db);
    assert_eq!(s.ack("p", "A", &body).unwrap_err().code, "REPLICA_EXPIRED");
    let db = s.connection().unwrap();
    db.execute(
        "UPDATE records SET value=json_set(value,'$.lastSeen',?1) WHERE kind='replica'",
        [s.time()],
    )
    .unwrap();
    let count: usize = db
        .query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))
        .unwrap();
    drop(db);
    drop(s);
    let mut c = config(root.path());
    c.max_metadata_records = count;
    let s = SyncService::open(&c).unwrap();
    assert_eq!(s.ack("p", "A", &body).unwrap_err().code, "LIMIT_EXCEEDED");
    s.mark_uncertain();
    assert!(s.ack("p", "A", &body).unwrap_err().unknown);
}

#[test]
fn readonly_admin_verify_does_not_run_recovery_or_rebuild_references() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    dataset(&s);
    drop(s);
    let bytes = std::fs::read(root.path().join("metadata.db")).unwrap();
    let s = SyncService::open_verify(&config(root.path())).unwrap();
    s.verify().unwrap();
    assert!(s.register(&json!({"replicaId":"B"})).is_err());
    drop(s);
    assert_eq!(
        std::fs::read(root.path().join("metadata.db")).unwrap(),
        bytes
    );
}

#[test]
fn corrupt_bytes_fail_verify_without_marking_or_repairing_metadata() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let hash = object(&s, b"valid");
    std::fs::write(s.path("p", &hash).unwrap(), b"wrong").unwrap();
    assert_eq!(s.verify().unwrap_err().code, "OBJECT_CORRUPT");
    let state: String = s
        .connection()
        .unwrap()
        .query_row("SELECT state FROM objects WHERE hash=?1", [&hash], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "ready");
}

fn config(root: &std::path::Path) -> Config {
    Config {
        root: root.to_owned(),
        metadata_reserve_bytes: 0,
        ..Config::default()
    }
}
pub(super) fn service(root: &std::path::Path) -> Arc<SyncService> {
    SyncService::init(&config(root)).unwrap();
    SyncService::open(&config(root)).unwrap()
}
fn command(s: &SyncService, seq: u64, extra: Value) -> Value {
    let caps = s.capabilities();
    let mut body = json!({"authorityId":caps["authorityId"],"historyEpoch":s.epoch(),
        "replicaId":"A","operationId":format!("A-{seq}"),"opSeq":seq.to_string(),"expectedProjectLifecycleRevision":"1"});
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    body
}
pub(super) fn project(s: &SyncService) {
    s.register(&json!({"replicaId":"A"})).unwrap();
    s.activate("A", &json!({"scopes":[]})).unwrap();
    assert_eq!(
        s.command("projects", &command(s, 1, json!({"projectId":"p"})), false)
            .unwrap()["outcome"],
        "committed"
    );
}
fn object(s: &SyncService, bytes: &[u8]) -> String {
    let hash = digest(bytes);
    let id = s.reserve("p", &hash, bytes.len() as u64).unwrap();
    let mut file = s.temporary_file(&id).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
    drop(file);
    s.install(&id).unwrap();
    hash
}
fn dataset(s: &SyncService) -> String {
    let hash = object(
        s,
        b"{\"entries\":[],\"format\":\"fs-agent.files\",\"version\":1}",
    );
    s.command(
        "projects/p/datasets",
        &command(
            s,
            2,
            json!({"datasetId":"files","kind":"files","logicalId":"files","manifestHash":hash}),
        ),
        false,
    )
    .unwrap();
    hash
}

#[test]
fn initialization_never_recreates_an_established_identity() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    let s = service(&root);
    drop(s);
    let marker = std::fs::read(root.join("storage.json")).unwrap();
    std::fs::write(root.join("init.pending"), "initializing").unwrap();
    std::fs::rename(root.join("metadata.db"), parent.path().join("saved.db")).unwrap();
    assert_eq!(
        SyncService::init(&config(&root)).unwrap_err().code,
        "SYNC_INITIALIZATION_UNSAFE"
    );
    assert!(!root.join("metadata.db").exists());
    assert_eq!(std::fs::read(root.join("storage.json")).unwrap(), marker);
}

#[test]
fn interrupted_first_initialization_preserves_database_identity() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    let marker = std::fs::read(root.path().join("storage.json")).unwrap();
    drop(s);
    std::fs::remove_file(root.path().join("storage.json")).unwrap();
    std::fs::write(root.path().join("init.pending"), "initializing").unwrap();
    SyncService::init(&config(root.path())).unwrap();
    assert_eq!(
        std::fs::read(root.path().join("storage.json")).unwrap(),
        marker
    );
}

#[test]
fn waiting_writer_rechecks_shutdown_and_uncertain_storage_under_lock() {
    for shutdown in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let s = service(root.path());
        let db = s.connection().unwrap();
        let task = s.clone();
        let writer = std::thread::spawn(move || task.register(&json!({"replicaId":"late"})));
        s.tasks.wait_active();
        if shutdown {
            s.stop();
        } else {
            s.mark_uncertain();
        }
        drop(db);
        assert!(writer.join().unwrap().is_err());
        assert!(
            m::get::<Value>(&s.connection().unwrap(), s.epoch(), "replica", "late")
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn drain_tracks_receiving_uploads_blocking_work_and_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let request = s.activity().unwrap();
    let id = s.reserve("p", &digest(b"x"), 1).unwrap();
    s.stop();
    assert_eq!(
        s.drain_timeout(Duration::ZERO).unwrap_err().code,
        "SYNC_DRAIN_TIMEOUT"
    );
    let mut file = s.temporary_file(&id).unwrap();
    file.write_all(b"x").unwrap();
    drop(file);
    s.install(&id).unwrap();
    let cleanup = s.cleanup_activity().unwrap();
    drop(request);
    assert!(s.drain_timeout(Duration::ZERO).is_err());
    s.abandon(&id).unwrap();
    drop(cleanup);
    s.drain_timeout(Duration::ZERO).unwrap();
    assert!(s.cleanup_activity().is_err());
    assert!(s.register(&json!({"replicaId":"late"})).is_err());
}

#[test]
fn ack_checks_lifecycle_budget_and_monotonic_consumption() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let before = s.changes("p", None, 100).unwrap()["cursor"].clone();
    dataset(&s);
    let after = s.changes("p", None, 100).unwrap()["cursor"].clone();
    s.ack("p", "A", &json!({"cursor":after,"scopeRevision":"1"}))
        .unwrap();
    assert_eq!(
        s.ack("p", "A", &json!({"cursor":before,"scopeRevision":"1"}))
            .unwrap_err()
            .code,
        "ACK_REGRESSION"
    );
    s.stop();
    assert!(s
        .ack("p", "A", &json!({"cursor":after,"scopeRevision":"1"}))
        .is_err());
}

#[test]
fn sql_query_failure_is_not_an_expired_version_or_a_missing_object() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    dataset(&s);
    s.connection()
        .unwrap()
        .execute_batch("DROP TABLE manifest_refs")
        .unwrap();
    assert_eq!(
        s.versions("p", "files", Some("1"), None, 100)
            .unwrap_err()
            .code,
        "METADATA_QUERY_FAILED"
    );
    assert!(s.healthy());
    s.connection()
        .unwrap()
        .execute_batch("DROP TABLE objects")
        .unwrap();
    assert_eq!(
        s.check_objects("p", &json!({"hashes":[digest(b"x")]}))
            .unwrap_err()
            .code,
        "METADATA_QUERY_FAILED"
    );
}

#[test]
fn read_io_failure_does_not_persist_corruption() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let hash = object(&s, b"x");
    let path = s.path("p", &hash).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert_eq!(
        s.object_file("p", &hash).unwrap_err().code,
        "STORAGE_IO_FAILED"
    );
    let state: String = s
        .connection()
        .unwrap()
        .query_row("SELECT state FROM objects WHERE hash=?1", [&hash], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(state, "ready");
    assert!(s.healthy());
}

#[test]
fn metadata_limit_preserves_rejection_receipt_and_rolls_back_business_growth() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    drop(s);
    let db = super::store::metadata::open(&root.path().join("metadata.db")).unwrap();
    let count: usize = db
        .query_row("SELECT COUNT(*) FROM records", [], |row| row.get(0))
        .unwrap();
    drop(db);
    let mut c = config(root.path());
    c.max_metadata_records = count + 2;
    let s = SyncService::open(&c).unwrap();
    let result = s
        .command("projects", &command(&s, 2, json!({"projectId":"q"})), false)
        .unwrap();
    assert_eq!(result["outcome"], "not-committed");
    assert_eq!(result["code"], "LIMIT_EXCEEDED");
    assert_eq!(s.operation("A", "2").unwrap(), result);
    assert_eq!(
        s.projects("all").unwrap()["projects"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn sqlite_full_automatically_rolled_back_transaction_is_recovered() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    {
        let db = s.connection().unwrap();
        db.execute_batch("CREATE TABLE fill(value BLOB); CREATE TRIGGER force_full BEFORE INSERT ON records WHEN NEW.kind='project' AND NEW.key='q' BEGIN INSERT INTO fill VALUES(zeroblob(1048576)); END;").unwrap();
        let pages: u64 = db.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap();
        db.execute_batch(&format!("PRAGMA max_page_count={}", pages + 16))
            .unwrap();
    }
    assert!(
        s.command("projects", &command(&s, 2, json!({"projectId":"q"})), false)
            .unwrap_err()
            .unknown
    );
    assert!(!s.healthy());
    assert!(s.connection().unwrap().is_autocommit());
    assert_eq!(s.operation("A", "2").unwrap()["state"], "pending");
    drop(s);
    let s = SyncService::open(&config(root.path())).unwrap();
    assert_eq!(s.operation("A", "2").unwrap()["code"], "SERVER_RESTART");
    assert_eq!(
        s.projects("all").unwrap()["projects"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn failed_terminal_receipt_never_claims_a_durable_rejection() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    s.connection().unwrap().execute_batch("CREATE TRIGGER fail_receipt BEFORE UPDATE ON records WHEN NEW.kind='operation' AND json_extract(NEW.value,'$.finishedAt') IS NOT NULL BEGIN SELECT RAISE(ABORT, 'receipt failed'); END;").unwrap();
    assert!(
        s.command("projects", &command(&s, 2, json!({"projectId":"p"})), false)
            .unwrap_err()
            .unknown
    );
    assert!(!s.healthy());
    assert!(s.connection().unwrap().is_autocommit());
    assert_eq!(s.operation("A", "2").unwrap()["state"], "pending");
    s.connection()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_receipt")
        .unwrap();
    drop(s);
    let s = SyncService::open(&config(root.path())).unwrap();
    assert_eq!(s.operation("A", "2").unwrap()["code"], "SERVER_RESTART");
}

#[test]
fn verify_is_read_only_and_missing_protected_reference_is_integrity_failure() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let hash = dataset(&s);
    let before: u64 = s
        .connection()
        .unwrap()
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    s.verify().unwrap();
    let after: u64 = s
        .connection()
        .unwrap()
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
    s.connection()
        .unwrap()
        .execute("UPDATE objects SET state='deleting' WHERE hash=?1", [&hash])
        .unwrap();
    assert_eq!(
        s.versions("p", "files", Some("1"), None, 100)
            .unwrap_err()
            .code,
        "REFERENCE_INTEGRITY_FAILED"
    );
}

#[test]
fn committed_change_and_receipt_share_one_typed_operation_identity() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    dataset(&s);
    let receipt = s.operation("A", "2").unwrap();
    let changes = s.changes("p", None, 100).unwrap();
    assert_eq!(changes["changes"][0]["operation"], receipt["operation"]);
}
