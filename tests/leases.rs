use fs_agent::workspaces::leases::{LeaseError, LeaseRegistry, Owner};
fn owner(tab: &str) -> Owner {
    Owner {
        principal: "user".into(),
        instance_id: tab.into(),
    }
}

#[test]
fn stale_owner_and_timer_cannot_release_transferred_control() {
    let directory = tempfile::tempdir().unwrap();
    let registry = LeaseRegistry::open(directory.path()).unwrap();
    let old = registry
        .create(owner("a"), "grant-1".into(), 0, 1000)
        .unwrap();
    let next = registry
        .transfer(&old, &owner("a"), owner("b"), "grant-1", 100)
        .unwrap();
    assert_eq!(
        registry.revoke(&old, &owner("a"), "grant-1", 200),
        Err(LeaseError::Stale)
    );
    assert_eq!(
        registry.renew(&old, &owner("a"), "grant-1", 200, 1000),
        Err(LeaseError::Stale)
    );
    assert_eq!(
        registry.expire(&old.workspace_id, old.lease_generation, 2000),
        Ok(false)
    );
    assert_eq!(
        registry.validate(&next, &owner("b"), "grant-1", 999),
        Ok(())
    );
    assert_eq!(
        registry.validate(&next, &owner("b"), "grant-2", 999),
        Err(LeaseError::Stale)
    );
    assert_eq!(
        registry.expire(&next.workspace_id, next.lease_generation, 1000),
        Ok(true)
    );
    assert_eq!(
        registry.renew(&next, &owner("b"), "grant-1", 1000, 1000),
        Err(LeaseError::Stale)
    );
}
#[test]
fn restart_revokes_tokens_and_preserves_unknown_workspaces_for_reconciliation() {
    let directory = tempfile::tempdir().unwrap();
    let handle = {
        let registry = LeaseRegistry::open(directory.path()).unwrap();
        assert!(LeaseRegistry::open(directory.path()).is_err());
        registry
            .create(owner("a"), "grant".into(), 0, 1000)
            .unwrap()
    };
    let raw = std::fs::read_to_string(directory.path().join("leases.json")).unwrap();
    assert!(!raw.contains(&handle.lease_id));
    let registry = LeaseRegistry::open(directory.path()).unwrap();
    assert_eq!(
        registry.validate(&handle, &owner("a"), "grant", 1),
        Err(LeaseError::Stale)
    );
    assert_eq!(
        registry.pending_recovery().unwrap(),
        vec![(handle.workspace_id.clone(), handle.lease_generation)]
    );
    registry
        .complete_release(&handle.workspace_id, handle.lease_generation)
        .unwrap();
    assert!(registry.pending_recovery().unwrap().is_empty());
}
#[test]
fn corrupt_journals_are_not_reset_and_tokens_do_not_authorize_another_principal() {
    let directory = tempfile::tempdir().unwrap();
    {
        let registry = LeaseRegistry::open(directory.path()).unwrap();
        let handle = registry
            .create(owner("a"), "grant".into(), 0, 1000)
            .unwrap();
        let other = Owner {
            principal: "other".into(),
            instance_id: "a".into(),
        };
        assert_eq!(
            registry.validate(&handle, &other, "grant", 1),
            Err(LeaseError::Stale)
        );
    }
    std::fs::write(directory.path().join("leases.json"), "broken").unwrap();
    assert!(LeaseRegistry::open(directory.path()).is_err());
    assert_eq!(
        std::fs::read_to_string(directory.path().join("leases.json")).unwrap(),
        "broken"
    );
}

#[test]
fn release_retries_are_idempotent_but_do_not_accept_a_rotated_token() {
    let directory = tempfile::tempdir().unwrap();
    let registry = LeaseRegistry::open(directory.path()).unwrap();
    let handle = registry
        .create(owner("a"), "grant".into(), 0, 1000)
        .unwrap();
    registry.revoke(&handle, &owner("a"), "grant", 100).unwrap();
    registry
        .revoke(&handle, &owner("a"), "grant", 2000)
        .unwrap();
    registry
        .complete_release(&handle.workspace_id, handle.lease_generation)
        .unwrap();
    registry
        .complete_release(&handle.workspace_id, handle.lease_generation)
        .unwrap();
    registry
        .revoke(&handle, &owner("a"), "grant", 2000)
        .unwrap();
    assert_eq!(
        registry.with_current(&handle, &owner("a"), "grant", 100, || panic!(
            "stale effect ran"
        )),
        Err::<(), _>(LeaseError::Stale)
    );
}

#[test]
fn failed_persistence_poisoning_prevents_use_of_the_old_memory_image() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("registry");
    let registry = LeaseRegistry::open(&path).unwrap();
    let handle = registry
        .create(owner("a"), "grant".into(), 0, 1000)
        .unwrap();
    std::fs::rename(&path, parent.path().join("moved")).unwrap();
    assert_eq!(
        registry.renew(&handle, &owner("a"), "grant", 100, 1000),
        Err(LeaseError::Storage)
    );
    std::fs::rename(parent.path().join("moved"), &path).unwrap();
    assert_eq!(
        registry.validate(&handle, &owner("a"), "grant", 100),
        Err(LeaseError::Storage)
    );
}
