use super::model::{Config, Error, Replica, Result};
use std::path::Path;

pub fn id(value: &str) -> Result<()> {
    if crate::core::ids::is_identifier_within(value, 128) {
        Ok(())
    } else {
        Err(Error::new("INVALID_ID", 400))
    }
}
pub fn hash(value: &str) -> Result<()> {
    if value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(Error::new("INVALID_HASH", 400))
    }
}
pub fn number(value: &str) -> Result<u64> {
    let n = value
        .parse::<u64>()
        .map_err(|_| Error::new("INVALID_SEQUENCE", 400))?;
    if n.to_string() != value || n >= i64::MAX as u64 {
        return Err(Error::new("LIMIT_EXCEEDED", 429));
    }
    Ok(n)
}
pub fn next(value: &str) -> Result<String> {
    let n = number(value)?;
    number(&(n + 1).to_string())?;
    Ok((n + 1).to_string())
}
pub fn validate(c: &Config) -> Result<()> {
    id(&c.principal_id)?;
    id(&c.namespace_id)?;
    let limits = [
        c.max_object_bytes,
        c.max_manifest_bytes,
        c.max_retained_bytes,
        c.upload_ttl_seconds,
        c.history_retention_seconds,
        c.trash_retention_seconds,
        c.operation_retention_seconds,
        c.replica_expiry_seconds,
        c.read_pin_seconds,
        c.max_projects as u64,
        c.max_datasets as u64,
        c.max_replicas as u64,
        c.max_pins as u64,
        c.max_metadata_records as u64,
        c.max_concurrent_uploads as u64,
        c.max_manifest_entries as u64,
    ];
    if c.root.as_os_str().is_empty()
        || limits.iter().any(|v| *v == 0 || *v > i64::MAX as u64 / 4)
        || c.max_object_bytes > c.max_retained_bytes
        || c.max_manifest_bytes > c.max_object_bytes
        || c.max_concurrent_uploads > 64
    {
        return Err(Error::new("INVALID_SYNC_CONFIG", 400));
    }
    validate_bounds(c)
}
fn validate_bounds(c: &Config) -> Result<()> {
    let seconds = [
        c.upload_ttl_seconds,
        c.history_retention_seconds,
        c.trash_retention_seconds,
        c.operation_retention_seconds,
        c.replica_expiry_seconds,
        c.read_pin_seconds,
    ];
    if seconds.iter().any(|s| *s > 315360000)
        || c.read_pin_seconds > 86400
        || c.upload_ttl_seconds > 604800
        || c.max_object_bytes > 4294967296
        || c.max_manifest_bytes > 67108864
        || c.max_manifest_entries > 1000000
        || c.max_projects > 10000
        || c.max_datasets > 100000
        || c.max_replicas > 10000
        || c.max_pins > 100000
        || c.max_metadata_records > 5000000
    {
        return Err(Error::new("INVALID_SYNC_CONFIG", 400));
    }
    Ok(())
}
pub fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

pub fn replica_active(replica: &Replica, time: u64, expiry: u64) -> bool {
    replica.state == "active" && replica.last_seen.saturating_add(expiry) > time
}
