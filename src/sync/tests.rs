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
            let tx = task.transaction(db)?;
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

fn discovery_fixture(s: &SyncService, count: u64, aged: u64) {
    let mut db = s.connection().unwrap();
    let tx = s.transaction(&mut db).unwrap();
    let mut p: Project = m::require(&tx, "", "project", "p").unwrap();
    let dataset = Dataset {
        dataset_id: "files".into(),
        kind: "files".into(),
        logical_id: "files".into(),
        state: "deleted".into(),
        head: Head {
            generation: "1".into(),
            manifest_hash: digest(b"fixture"),
        },
        recoverable_until: None,
    };
    let old = s.time() - s.config.change_retention_seconds - s.config.read_pin_seconds - 10;
    for seq in 1..=count {
        m::put(
            &tx,
            "p",
            "change",
            &format!("{seq:020}"),
            &json!({"sequence":seq.to_string(),"recordedAt":if seq<=aged {old} else {s.time()}}),
        )
        .unwrap();
        m::put(&tx, "p", "catalog", &format!("{seq:020}/files"), &dataset).unwrap();
    }
    m::put(&tx, "p", "dataset", "files", &dataset).unwrap();
    p.sequence = count;
    m::put(&tx, "", "project", "p", &p).unwrap();
    s.commit(tx).unwrap();
}

#[test]
fn compaction_preserves_catalog_snapshots_tombstones_and_replay_identity() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    discovery_fixture(&s, 20, 15);
    let cursor = s.encode_cursor("p", "catalog", 18, "", "all").unwrap();
    let before = s.catalog("p", Some(&cursor), "all", 10).unwrap();
    let count = s.gc().unwrap()["compactedRecords"].as_u64().unwrap();
    assert_eq!(count, 29); // 15 events and 14 redundant snapshots.
    assert_eq!(
        s.catalog("p", Some(&cursor), "all", 10).unwrap()["datasets"],
        before["datasets"]
    );
    assert_eq!(s.changes("p", None, 10).unwrap_err().code, "CURSOR_EXPIRED");
    let stale = s.encode_cursor("p", "catalog", 14, "", "all").unwrap();
    assert_eq!(
        s.catalog("p", Some(&stale), "all", 10).unwrap_err().code,
        "CURSOR_EXPIRED"
    );
    let resume = s.encode_cursor("p", "changes", 15, "15", "all").unwrap();
    assert_eq!(
        s.changes("p", Some(&resume), 10).unwrap()["changes"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    let stale_changes = s.encode_cursor("p", "changes", 14, "14", "all").unwrap();
    assert_eq!(
        s.ack(
            "p",
            "A",
            &json!({"cursor":stale_changes,"scopeRevision":"1"})
        )
        .unwrap_err()
        .code,
        "CURSOR_EXPIRED"
    );
    s.register(&json!({"replicaId":"B"})).unwrap();
    assert_eq!(
        s.activate("B", &json!({"scopes":[{"projectId":"p","cursor":stale}]}))
            .unwrap_err()
            .code,
        "CURSOR_EXPIRED"
    );
    let current = s.catalog("p", None, "all", 10).unwrap();
    s.activate(
        "B",
        &json!({"scopes":[{"projectId":"p","cursor":current["cursor"]}]}),
    )
    .unwrap();
    let db = s.connection().unwrap();
    assert_eq!(m::count(&db, "p", "catalog").unwrap(), 6);
    assert_eq!(
        m::require::<Dataset>(&db, "p", "dataset", "files")
            .unwrap()
            .state,
        "deleted"
    );
    assert_eq!(m::count(&db, s.epoch(), "operation-id").unwrap(), 1);
}

#[test]
fn compaction_is_bounded_atomic_and_respects_legacy_timestamp_barriers() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    discovery_fixture(&s, 2500, 2500);
    {
        let db = s.connection().unwrap();
        let mut second = s.project(&db, "p", false).unwrap();
        second.project_id = "p2".into();
        m::put(&db, "", "project", "p2", &second).unwrap();
        db.execute(
            "INSERT INTO records SELECT 'p2',kind,key,value FROM records WHERE scope='p'",
            [],
        )
        .unwrap();
    }
    {
        let db = s.connection().unwrap();
        db.execute("CREATE TRIGGER fail_floor BEFORE UPDATE ON records WHEN NEW.kind='project' BEGIN SELECT RAISE(ABORT,'fault'); END",[]).unwrap();
        drop(db);
        assert!(s.with_write(|db| s.compact(db)).is_err());
        let db = s.connection().unwrap();
        assert_eq!(m::count(&db, "p", "change").unwrap(), 2500);
        assert_eq!(m::count(&db, "p", "catalog").unwrap(), 2500);
        assert_eq!(s.project(&db, "p", false).unwrap().change_floor, 0);
        db.execute("DROP TRIGGER fail_floor", []).unwrap();
    }
    // A write I/O/SQL failure closes admission; reopening performs recovery.
    drop(s);
    let s = SyncService::open(&config(root.path())).unwrap();
    assert_eq!(s.gc().unwrap()["compactedRecords"], 2000);
    {
        let db = s.connection().unwrap();
        assert_eq!(m::count(&db, "p", "change").unwrap(), 1500);
        assert_eq!(s.project(&db, "p", false).unwrap().change_floor, 1000);
        db.execute("UPDATE records SET value=json_remove(value,'$.recordedAt') WHERE kind='change' AND key='00000000000000001001'",[]).unwrap();
    }
    s.gc().unwrap();
    assert_eq!(
        s.project(&s.connection().unwrap(), "p", false)
            .unwrap()
            .change_floor,
        1000
    );
}

#[test]
fn catalog_pagination_keeps_original_expiry_and_recent_events_survive_gc() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    discovery_fixture(&s, 4, 0);
    let expiry = s.time() + 30;
    let cursor = s
        .encode_cursor_until("p", "catalog", 4, "", "all", expiry)
        .unwrap();
    let page = s.catalog("p", Some(&cursor), "all", 1).unwrap();
    let renewed = s
        .decode_cursor(page["cursor"].as_str().unwrap(), "p", "catalog")
        .unwrap();
    assert_eq!(renewed["expires"], expiry);
    assert_eq!(s.gc().unwrap()["compactedRecords"], 0);
    let mut invalid = config(root.path());
    invalid.change_retention_seconds = invalid.read_pin_seconds - 1;
    assert_eq!(
        super::policy::validate(&invalid).unwrap_err().code,
        "INVALID_SYNC_CONFIG"
    );
}

#[test]
#[ignore = "diagnostic workload, run explicitly with --nocapture"]
fn sync_publish_diagnostics() {
    let root = tempfile::tempdir().unwrap();
    let s = service(root.path());
    project(&s);
    let content = object(&s, b"content");
    let mut entries: Vec<_> = (0..1000)
        .map(|i| json!({"kind":"file","path":format!("file-{i:04}.txt"),"hash":content,"size":"7"}))
        .collect();
    let bytes =
        serde_json::to_vec(&json!({"entries":entries,"format":"fs-agent.files","version":1}))
            .unwrap();
    let hash = object(&s, &bytes);
    entries.last_mut().unwrap()["path"] = json!("file-9999.txt");
    let next_bytes =
        serde_json::to_vec(&json!({"entries":entries,"format":"fs-agent.files","version":1}))
            .unwrap();
    let alternate = object(&s, &next_bytes);
    assert_eq!(
        s.command(
            "projects/p/datasets",
            &command(
                &s,
                2,
                json!({"datasetId":"files","kind":"files","logicalId":"files","manifestHash":hash})
            ),
            false
        )
        .unwrap()["outcome"],
        "committed"
    );
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let s = &s;
            scope.spawn(move || {
                for _ in 0..200 {
                    s.head("p", "files").unwrap();
                }
            });
        }
        for seq in 3..=102 {
            let head = s.head("p", "files").unwrap();
            let next = if seq % 2 == 1 { &alternate } else { &hash };
            let result=s.command("projects/p/datasets/files/publish",&command(&s,seq,json!({"expectedHead":{"generation":head["generation"],"manifestHash":head["manifestHash"]},"nextManifestHash":next})),false).unwrap();
            assert_eq!(result["outcome"], "committed", "{result}");
        }
    });
    let diagnostics = s.diagnostics();
    assert!(diagnostics["transaction"]["count"].as_u64().unwrap() > 100);
    println!(
        "manifestBytes={}, files=1000, publishes=100, readers=4: {}",
        bytes.len(),
        diagnostics
    );
}
