use super::lease_journal::LeaseJournal;
use super::lease_model::{
    checked, checked_identity, expiry, random_id, token_hash, validate_owner, Lease, Result, State,
};
pub use super::lease_model::{Handle, LeaseError, Owner};
use std::path::Path;

/// Single-daemon durable fencing. The exclusive lock lasts for the registry lifetime.
pub struct LeaseRegistry {
    journal: LeaseJournal,
}
impl LeaseRegistry {
    pub fn open(directory: &Path) -> Result<Self> {
        let journal = LeaseJournal::open(directory)?;
        journal.update(|records| {
            for lease in records.values_mut() {
                if lease.state != State::Released {
                    lease.state = State::Unknown;
                }
            }
            Ok(())
        })?;
        Ok(Self { journal })
    }
    pub fn create(
        &self,
        owner: Owner,
        authorization_revision: String,
        now: u64,
        ttl_ms: u64,
    ) -> Result<Handle> {
        validate_owner(&owner)?;
        if authorization_revision.is_empty() {
            return Err(LeaseError::Invalid);
        }
        let expires_at_ms = expiry(now, ttl_ms)?;
        let handle = Handle {
            workspace_id: random_id()?,
            lease_id: random_id()?,
            lease_generation: 1,
        };
        self.journal.update(|records| {
            records.insert(
                handle.workspace_id.clone(),
                Lease {
                    owner,
                    authorization_revision,
                    expires_at_ms,
                    token_hash: token_hash(&handle.lease_id),
                    generation: 1,
                    state: State::Active,
                },
            );
            Ok(())
        })?;
        Ok(handle)
    }
    pub fn validate(
        &self,
        handle: &Handle,
        owner: &Owner,
        authorization_revision: &str,
        now: u64,
    ) -> Result<()> {
        self.with_current(handle, owner, authorization_revision, now, || Ok(()))
    }
    /// Execute a short commit step under the fencing lock. Do not re-enter this registry.
    /// `validate` alone is observational and cannot protect a later effect from revocation.
    pub fn with_current<T>(
        &self,
        handle: &Handle,
        owner: &Owner,
        authorization_revision: &str,
        now: u64,
        action: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.journal.read(|records| {
            checked(records, handle, owner, authorization_revision, now)?;
            action()
        })
    }

    pub fn renew(
        &self,
        handle: &Handle,
        owner: &Owner,
        authorization_revision: &str,
        now: u64,
        ttl_ms: u64,
    ) -> Result<()> {
        let end = expiry(now, ttl_ms)?;
        self.journal.update(|records| {
            checked(records, handle, owner, authorization_revision, now)?;
            records.get_mut(&handle.workspace_id).unwrap().expires_at_ms = end;
            Ok(())
        })
    }
    pub fn transfer(
        &self,
        handle: &Handle,
        owner: &Owner,
        next: Owner,
        authorization_revision: &str,
        now: u64,
    ) -> Result<Handle> {
        validate_owner(&next)?;
        if owner.principal != next.principal {
            return Err(LeaseError::Invalid);
        }
        let mut result = handle.clone();
        result.lease_id = random_id()?;
        result.lease_generation = result
            .lease_generation
            .checked_add(1)
            .ok_or(LeaseError::Invalid)?;
        self.journal.update(|records| {
            checked(records, handle, owner, authorization_revision, now)?;
            let lease = records.get_mut(&handle.workspace_id).unwrap();
            lease.owner = next;
            lease.generation = result.lease_generation;
            lease.token_hash = token_hash(&result.lease_id);
            Ok(())
        })?;
        Ok(result)
    }
    /// This revokes control only. Actual release still requires process/file cleanup.
    pub fn revoke(
        &self,
        handle: &Handle,
        owner: &Owner,
        authorization_revision: &str,
        now: u64,
    ) -> Result<()> {
        self.journal.update(|records| {
            let lease = checked_identity(records, handle, owner, authorization_revision)?;
            if matches!(lease.state, State::Revoking | State::Released) {
                return Ok(());
            }
            checked(records, handle, owner, authorization_revision, now)?;
            records.get_mut(&handle.workspace_id).unwrap().state = State::Revoking;
            Ok(())
        })
    }

    /// A timer carries its expected generation and cannot revoke a transferred lease.
    pub fn expire(&self, workspace_id: &str, generation: u64, now: u64) -> Result<bool> {
        self.journal.update(|records| {
            let Some(lease) = records.get_mut(workspace_id) else {
                return Ok(false);
            };
            if lease.generation != generation
                || lease.state != State::Active
                || now < lease.expires_at_ms
            {
                return Ok(false);
            }
            lease.state = State::Revoking;
            Ok(true)
        })
    }
    /// Only the internal reaper may complete release after cleanup, never a client token.
    pub fn complete_release(&self, workspace_id: &str, generation: u64) -> Result<()> {
        self.journal.update(|records| {
            let lease = records.get_mut(workspace_id).ok_or(LeaseError::Stale)?;
            if lease.generation != generation
                || !matches!(
                    lease.state,
                    State::Revoking | State::Unknown | State::Released
                )
            {
                return Err(LeaseError::Stale);
            }
            lease.state = State::Released;
            Ok(())
        })
    }
    pub fn pending_recovery(&self) -> Result<Vec<(String, u64)>> {
        self.journal.read(|records| {
            Ok(records
                .iter()
                .filter(|(_, value)| matches!(value.state, State::Unknown | State::Revoking))
                .map(|(id, value)| (id.clone(), value.generation))
                .collect())
        })
    }
}
