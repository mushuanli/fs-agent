//! Export-level behaviour that does not need HTTP: capability opening, restart
//! recovery and revision retirement.

use pi_agent::fs::{Export, Exports};

#[test]
fn exclusive_open_holds_the_lock_and_a_second_open_fails() {
    let root = tempfile::tempdir().unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    assert!(export.writable());
    assert!(Export::exclusive(root.path()).is_err());
    // A read-only capability is still possible while the writer holds the lock.
    assert!(Export::open(root.path()).is_ok());
}

#[test]
fn restart_retires_validators_and_recovers_reserved_uploads() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note"), "content").unwrap();
    let old = {
        let export = Export::exclusive(root.path()).unwrap();
        export.stat("note").unwrap().unwrap().revision
    };
    std::fs::write(root.path().join(".itookit-upload-abandoned"), "incomplete").unwrap();
    // An outside entry with a reserved name must not block startup.
    std::fs::create_dir(root.path().join(".itookit-upload-directory")).unwrap();
    let export = Export::exclusive(root.path()).unwrap();
    assert_ne!(export.stat("note").unwrap().unwrap().revision, old);
    assert!(!root.path().join(".itookit-upload-abandoned").exists());
    assert!(root.path().join(".itookit-upload-directory").exists());
}

/// Reserved staging names are never listed, but their siblings are.
#[test]
fn reserved_names_are_hidden_from_listings() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("visible"), "x").unwrap();
    std::fs::write(root.path().join(".itookit-upload-hidden"), "x").unwrap();
    let export = Export::open(root.path()).unwrap();
    let listing = export
        .list("", &tokio_util::sync::CancellationToken::new())
        .unwrap();
    let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["visible"]);
}

#[test]
fn the_registry_lists_aliases_and_can_invalidate_all_revisions() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("note"), "content").unwrap();
    let writable = Export::exclusive(root.path()).unwrap();
    let reference = root.path().join("reference");
    std::fs::create_dir(&reference).unwrap();
    let exports = Exports::new(
        [
            ("project".to_owned(), std::sync::Arc::new(writable)),
            (
                "reference".to_owned(),
                std::sync::Arc::new(Export::open(&reference).unwrap()),
            ),
        ]
        .into_iter()
        .collect(),
    );
    assert_eq!(
        exports.aliases().collect::<Vec<_>>(),
        ["project", "reference"]
    );
    assert!(exports.get("project").unwrap().writable());
    assert!(!exports.get("reference").unwrap().writable());
    let before = exports
        .get("project")
        .unwrap()
        .stat("note")
        .unwrap()
        .unwrap();
    exports.invalidate_revisions();
    let after = exports
        .get("project")
        .unwrap()
        .stat("note")
        .unwrap()
        .unwrap();
    assert_ne!(before.revision, after.revision);
}
