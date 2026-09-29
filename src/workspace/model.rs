//! Durable lease records and the invariants that guard them.
//!
//! The types here are both the in-memory model and the on-disk journal format,
//! so serialized names are part of the storage contract and must not change
//! casually. Validation helpers are deliberately free functions: the registry
//! decides *when* a transition is allowed, this module decides *whether* a
//! presented handle can be trusted at all.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq)]
pub enum LeaseError {
    /// The handle, owner, generation or authorization no longer match.
    Stale,
    /// The request was malformed before any state was consulted.
    Invalid,
    /// The journal could not be read or durably written.
    Storage,
    /// The registry is at capacity and holds no releasable record to evict.
    Capacity,
}

pub(super) type Result<T> = std::result::Result<T, LeaseError>;

/// Authentication establishes the principal; a token alone is never enough.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Owner {
    pub principal: String,
    pub instance_id: String,
}

/// The token handed to a client. Deliberately not `Debug` and not `Display`:
/// the lease secret must never reach a log line.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Handle {
    pub workspace_id: String,
    pub lease_id: String,
    pub lease_generation: u64,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Active,
    Revoking,
    Released,
    /// Survived a restart without proof of cleanup; reconciled by the reaper.
    Unknown,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Lease {
    pub(super) owner: Owner,
    pub(super) token_hash: String,
    pub(super) generation: u64,
    pub(super) expires_at_ms: u64,
    pub(super) authorization_revision: String,
    pub(super) state: State,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    pub(super) version: u32,
    pub(super) leases: BTreeMap<String, Lease>,
}

/// Length of a lease id: 32 random bytes rendered as hex.
const LEASE_ID_CHARS: usize = 64;
/// Bounds for an owner identity.
const OWNER_FIELD_MAX: usize = 256;
/// Lease lifetime bounds, in milliseconds.
pub(super) const MIN_TTL_MS: u64 = 1_000;
pub(super) const MAX_TTL_MS: u64 = 300_000;

/// Trust check: the handle must name a live lease owned by the same principal
/// and instance, at the same generation, under the same authorization.
pub(super) fn checked_identity<'a>(
    records: &'a BTreeMap<String, Lease>,
    handle: &Handle,
    owner: &Owner,
    revision: &str,
) -> Result<&'a Lease> {
    if handle.lease_id.len() != LEASE_ID_CHARS {
        return Err(LeaseError::Stale);
    }
    let lease = records.get(&handle.workspace_id).ok_or(LeaseError::Stale)?;
    // Compare the token unconditionally: an early return would let a caller
    // distinguish "wrong owner/generation" from "wrong token" by timing.
    let token_matches = equal(&lease.token_hash, &token_hash(&handle.lease_id));
    if lease.owner != *owner
        || lease.generation != handle.lease_generation
        || lease.authorization_revision != revision
        || !token_matches
    {
        return Err(LeaseError::Stale);
    }
    Ok(lease)
}

/// [`checked_identity`] plus the liveness requirement: active and unexpired.
pub(super) fn checked<'a>(
    records: &'a BTreeMap<String, Lease>,
    handle: &Handle,
    owner: &Owner,
    revision: &str,
    now: u64,
) -> Result<&'a Lease> {
    let lease = checked_identity(records, handle, owner, revision)?;
    if lease.state != State::Active || lease.expires_at_ms <= now {
        return Err(LeaseError::Stale);
    }
    Ok(lease)
}

pub(super) fn validate_owner(owner: &Owner) -> Result<()> {
    if owner.principal.is_empty()
        || owner.instance_id.is_empty()
        || owner.principal.len() > OWNER_FIELD_MAX
        || owner.instance_id.len() > OWNER_FIELD_MAX
    {
        return Err(LeaseError::Invalid);
    }
    Ok(())
}

/// Compute the absolute expiry of a lease granted at `now`.
pub(super) fn expiry(now: u64, ttl_ms: u64) -> Result<u64> {
    if !(MIN_TTL_MS..=MAX_TTL_MS).contains(&ttl_ms) {
        return Err(LeaseError::Invalid);
    }
    now.checked_add(ttl_ms).ok_or(LeaseError::Invalid)
}

/// Only the digest of a lease token is ever stored.
pub(super) fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

pub(super) fn random_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| LeaseError::Storage)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn equal(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |diff, (x, y)| diff | (x ^ y))
            == 0
}
