//! Lease lifecycle policy over an atomic journal.
//!
//! A lease answers one question: *is this holder still the one authorized to
//! act on this workspace, at this generation, under this authorization
//! revision?* Everything about durability is delegated to [`LeaseJournal`];
//! everything about legality of a transition lives here.
//!
//! Two interfaces, on purpose:
//!
//! * [`LeaseRegistry`] is what a lease holder uses — create, validate, renew,
//!   transfer, revoke. Everything is keyed by the presented handle plus owner.
//! * [`LeaseReaper`] is the internal reconciliation interface — expire a timer
//!   and complete a release after cleanup. It is deliberately separate so a
//!   client token can never release a lease on its own.
//!
//! Single-daemon fencing: the journal holds an exclusive advisory lock for its
//! whole lifetime, so only one process can hand out leases from a directory.

use super::journal::LeaseJournal;
use super::model::{
    checked, checked_identity, expiry, random_id, token_hash, validate_owner, Lease, Result, State,
};
pub use super::model::{Handle, LeaseError, Owner};
use std::path::Path;

/// Maximum retained lease records before new leases are refused.
const CAPACITY: usize = 4096;

pub struct LeaseRegistry {
    journal: LeaseJournal,
}

impl LeaseRegistry {
    /// Open the registry, demoting every non-released lease to `unknown`.
    ///
    /// After a restart nothing is known about the processes that held a lease,
    /// so the safe answer is "not released yet": the caller must reconcile and
    /// then complete the release through [`LeaseReaper`].
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

    /// The reconciliation interface for this registry.
    pub fn reaper(&self) -> LeaseReaper<'_> {
        LeaseReaper { registry: self }
    }

    /// Grant a new lease owned by `owner`.
    ///
    /// Released records are reclaimed first, so a long-lived registry does not
    /// grow without bound.
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
            records.retain(|_, lease| lease.state != State::Released);
            if records.len() >= CAPACITY {
                return Err(LeaseError::Capacity);
            }
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

    /// Observational check only. To protect an effect, use [`Self::with_current`].
    pub fn validate(
        &self,
        handle: &Handle,
        owner: &Owner,
        authorization_revision: &str,
        now: u64,
    ) -> Result<()> {
        self.with_current(handle, owner, authorization_revision, now, || Ok(()))
    }

    /// Run a short commit step while holding the fencing lock.
    ///
    /// The action must not re-enter this registry, and it must be short: the
    /// journal lock is held for its whole duration.
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

    /// Extend the lease without changing its generation or token.
    pub fn renew(
        &self,
        handle: &Handle,
        owner: &Owner,
        authorization_revision: &str,
        now: u64,
        ttl_ms: u64,
    ) -> Result<()> {
        let expires_at_ms = expiry(now, ttl_ms)?;
        self.journal.update(|records| {
            checked(records, handle, owner, authorization_revision, now)?;
            records
                .get_mut(&handle.workspace_id)
                .expect("checked lease exists")
                .expires_at_ms = expires_at_ms;
            Ok(())
        })
    }

    /// Hand control to another instance of the same principal.
    ///
    /// The returned handle has a new token and a higher generation, so every
    /// handle derived from the previous one is permanently stale.
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
            let lease = records
                .get_mut(&handle.workspace_id)
                .expect("checked lease exists");
            lease.owner = next;
            lease.generation = result.lease_generation;
            lease.token_hash = token_hash(&result.lease_id);
            Ok(())
        })?;
        Ok(result)
    }

    /// Begin revocation. Idempotent, but never succeeds with a rotated token,
    /// and never succeeds once the lease has expired.
    ///
    /// This only withdraws control; releasing resources is a separate step.
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
            records
                .get_mut(&handle.workspace_id)
                .expect("checked lease exists")
                .state = State::Revoking;
            Ok(())
        })
    }
}

/// Reconciliation operations that are not reachable from a client token.
pub struct LeaseReaper<'a> {
    registry: &'a LeaseRegistry,
}

impl LeaseReaper<'_> {
    /// Expire an active lease. The expected generation makes a stale timer
    /// harmless: it can never revoke a lease that was transferred meanwhile.
    pub fn expire(&self, workspace_id: &str, generation: u64, now: u64) -> Result<bool> {
        self.registry.journal.update(|records| {
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

    /// Finish a revocation after cleanup. `Unknown` is accepted because a
    /// restart cannot prove that cleanup already happened.
    pub fn complete_release(&self, workspace_id: &str, generation: u64) -> Result<()> {
        self.registry.journal.update(|records| {
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

    /// How many records the journal currently retains. Released records are
    /// reclaimed by the next `create`.
    pub fn retained(&self) -> Result<usize> {
        self.registry.journal.read(|records| Ok(records.len()))
    }

    /// Leases awaiting reconciliation after a restart or revocation.
    pub fn pending_recovery(&self) -> Result<Vec<(String, u64)>> {
        self.registry.journal.read(|records| {
            Ok(records
                .iter()
                .filter(|(_, lease)| matches!(lease.state, State::Unknown | State::Revoking))
                .map(|(id, lease)| (id.clone(), lease.generation))
                .collect())
        })
    }
}
