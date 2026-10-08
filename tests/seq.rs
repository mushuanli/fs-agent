use pi_agent::fs::{
    seq::{self, Change, Update},
    Export,
};
use serde_json::json;

#[test]
fn sqlite_seqfile_is_portable_and_conditional_transactions_do_not_lose_updates() {
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    let snapshot = seq::snapshot(&export, "info.seq").unwrap();
    assert!(snapshot.entries.is_empty());
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: snapshot.revision,
            changes: vec![Change::Set {
                key: "identity".into(),
                value: json!({"id":"project-a"}),
            }],
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        &std::fs::read(root.path().join("info.seq")).unwrap()[..16],
        b"SQLite format 3\0"
    );
    let snapshot = seq::snapshot(&export, "info.seq").unwrap();
    assert_eq!(snapshot.entries[0].value, json!({"id":"project-a"}));
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: snapshot.revision.clone(),
            changes: vec![
                Change::Set {
                    key: "title".into(),
                    value: json!("new"),
                },
                Change::Delete {
                    key: "identity".into(),
                },
            ],
        },
        || Ok(()),
    )
    .unwrap();
    let stale = seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: snapshot.revision,
            changes: vec![Change::Set {
                key: "title".into(),
                value: json!("stale"),
            }],
        },
        || Ok(()),
    )
    .unwrap_err();
    assert_eq!(stale.code, "ECONFLICT");
    std::fs::copy(root.path().join("info.seq"), root.path().join("copy.seq")).unwrap();
    drop(export);
    let reader = Export::open(root.path()).unwrap();
    assert_eq!(
        seq::snapshot(&reader, "copy.seq").unwrap().entries[0].value,
        json!("new")
    );
    assert!(seq::snapshot(&reader, "../escape.seq").is_err());
}

#[test]
fn cancelled_or_invalid_batch_keeps_the_original_database() {
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: None,
            changes: vec![Change::Set {
                key: "original".into(),
                value: json!(1),
            }],
        },
        || Ok(()),
    )
    .unwrap();
    let snapshot = seq::snapshot(&export, "info.seq").unwrap();
    let bytes = std::fs::read(root.path().join("info.seq")).unwrap();
    assert!(seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: snapshot.revision,
            changes: vec![
                Change::Delete {
                    key: "original".into()
                },
                Change::Set {
                    key: "".into(),
                    value: json!(2)
                }
            ]
        },
        || Ok(())
    )
    .is_err());
    assert_eq!(std::fs::read(root.path().join("info.seq")).unwrap(), bytes);
    let snapshot = seq::snapshot(&export, "info.seq").unwrap();
    assert!(seq::update(
        &export,
        Update {
            path: "info.seq".into(),
            expected_revision: snapshot.revision,
            changes: vec![]
        },
        || Err(pi_agent::core::error::Error::cancelled())
    )
    .is_err());
    assert_eq!(std::fs::read(root.path().join("info.seq")).unwrap(), bytes);
}

#[test]
fn reads_do_not_create_files_and_invalid_sqlite_or_symlinks_are_not_replaced() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    assert!(seq::snapshot(&export, "missing.seq")
        .unwrap()
        .entries
        .is_empty());
    assert!(!root.path().join("missing.seq").exists());
    std::fs::write(root.path().join("broken.seq"), b"not sqlite").unwrap();
    assert!(seq::snapshot(&export, "broken.seq").is_err());
    assert!(seq::update(
        &export,
        Update {
            path: "broken.seq".into(),
            expected_revision: None,
            changes: vec![]
        },
        || Ok(())
    )
    .is_err());
    assert_eq!(
        std::fs::read(root.path().join("broken.seq")).unwrap(),
        b"not sqlite"
    );
    std::fs::write(outside.path().join("secret.seq"), b"secret").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.seq"),
        root.path().join("link.seq"),
    )
    .unwrap();
    assert!(seq::snapshot(&export, "link.seq").is_err());
    assert!(seq::update(
        &export,
        Update {
            path: "link.seq".into(),
            expected_revision: None,
            changes: vec![]
        },
        || Ok(())
    )
    .is_err());
    assert_eq!(
        std::fs::read(outside.path().join("secret.seq")).unwrap(),
        b"secret"
    );
    drop(export);
    let readonly = Export::open(root.path()).unwrap();
    assert_eq!(
        seq::update(
            &readonly,
            Update {
                path: "new.seq".into(),
                expected_revision: None,
                changes: vec![]
            },
            || Ok(())
        )
        .unwrap_err()
        .code,
        "EROFS"
    );
    assert!(!root.path().join("new.seq").exists());
}

#[test]
fn concurrent_seqfile_creators_cannot_overwrite_each_other() {
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let write = |name: &str| {
            let calls = std::cell::Cell::new(0);
            seq::update(
                &export,
                Update {
                    path: "info.seq".into(),
                    expected_revision: None,
                    changes: vec![Change::Set {
                        key: "winner".into(),
                        value: json!(name),
                    }],
                },
                || {
                    calls.set(calls.get() + 1);
                    if calls.get() == 2 {
                        barrier.wait();
                    }
                    Ok(())
                },
            )
        };
        let a = scope.spawn(move || write("A"));
        let b = scope.spawn(move || write("B"));
        let results = [a.join().unwrap(), b.join().unwrap()];
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results.iter().find_map(|r| r.as_ref().err()).unwrap().code,
            "ECONFLICT"
        );
    });
    assert_eq!(seq::snapshot(&export, "info.seq").unwrap().entries.len(), 1);
}
