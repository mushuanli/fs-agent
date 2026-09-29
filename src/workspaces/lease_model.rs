use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq)]
pub enum LeaseError {
    Stale,
    Invalid,
    Storage,
}
pub(super) type Result<T> = std::result::Result<T, LeaseError>;

/// Authentication establishes principal; a token alone is never sufficient.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Owner {
    pub principal: String,
    pub instance_id: String,
}

/// Deliberately not Debug: the lease token must not be logged.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Handle {
    pub workspace_id: String,
    pub lease_id: String,
    pub lease_generation: u64,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Active,
    Revoking,
    Released,
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
pub(super) fn checked_identity<'a>(
    records: &'a BTreeMap<String, Lease>,
    handle: &Handle,
    owner: &Owner,
    revision: &str,
) -> Result<&'a Lease> {
    if handle.lease_id.len() != 64 {
        return Err(LeaseError::Stale);
    }
    let lease = records.get(&handle.workspace_id).ok_or(LeaseError::Stale)?;
    if lease.owner != *owner
        || lease.generation != handle.lease_generation
        || lease.authorization_revision != revision
        || !equal(&lease.token_hash, &token_hash(&handle.lease_id))
    {
        return Err(LeaseError::Stale);
    }
    Ok(lease)
}

pub(super) fn validate_owner(owner: &Owner) -> Result<()> {
    if owner.principal.is_empty()
        || owner.instance_id.is_empty()
        || owner.principal.len() > 256
        || owner.instance_id.len() > 256
    {
        return Err(LeaseError::Invalid);
    }
    Ok(())
}
pub(super) fn expiry(now: u64, ttl: u64) -> Result<u64> {
    if !(1000..=300_000).contains(&ttl) {
        return Err(LeaseError::Invalid);
    }
    now.checked_add(ttl).ok_or(LeaseError::Invalid)
}
pub(super) fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
fn equal(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |diff, (x, y)| diff | (x ^ y))
            == 0
}
pub(super) fn random_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| LeaseError::Storage)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
