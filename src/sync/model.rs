use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, Clone)]
pub struct Error {
    pub code: String,
    pub status: u16,
    pub unknown: bool,
    pub storage_fault: bool,
}
impl Error {
    pub fn new(code: &str, status: u16) -> Self {
        Self {
            code: code.into(),
            status,
            unknown: false,
            storage_fault: false,
        }
    }
    pub fn storage() -> Self {
        Self {
            code: "SYNC_STORAGE_UNCERTAIN".into(),
            status: 503,
            unknown: true,
            storage_fault: true,
        }
    }
    pub fn value(&self) -> Value {
        json!({"code":self.code,"outcome":if self.unknown {"unknown"} else {"not-committed"}})
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.code)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        let mut error = Self::new(
            if e.raw_os_error() == Some(28) {
                "ENOSPC"
            } else {
                "STORAGE_IO_FAILED"
            },
            503,
        );
        error.unknown = true;
        error
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::ErrorCode::*;
        let mut error = Self::new("METADATA_QUERY_FAILED", 503);
        error.unknown = true;
        error.storage_fault = matches!(e, rusqlite::Error::SqliteFailure(ref code, _)
            if matches!(code.code, SystemIoFailure | DatabaseCorrupt | NotADatabase | DiskFull | OutOfMemory));
        error
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::new("INVALID_JSON", 400)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub root: PathBuf,
    pub principal_id: String,
    pub namespace_id: String,
    pub expected_authority_id: Option<String>,
    pub max_object_bytes: u64,
    pub max_manifest_bytes: u64,
    pub max_manifest_entries: usize,
    pub max_retained_bytes: u64,
    pub max_concurrent_uploads: usize,
    pub upload_ttl_seconds: u64,
    pub history_retention_seconds: u64,
    pub trash_retention_seconds: u64,
    pub operation_retention_seconds: u64,
    pub replica_expiry_seconds: u64,
    pub read_pin_seconds: u64,
    pub change_retention_seconds: u64,
    pub metadata_reserve_bytes: u64,
    pub max_projects: usize,
    pub max_datasets: usize,
    pub max_replicas: usize,
    pub max_pins: usize,
    pub max_metadata_records: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            root: PathBuf::new(),
            principal_id: "owner".into(),
            namespace_id: "personal".into(),
            expected_authority_id: None,
            max_object_bytes: 268435456,
            max_manifest_bytes: 8388608,
            max_manifest_entries: 100000,
            max_retained_bytes: 10737418240,
            max_concurrent_uploads: 4,
            upload_ttl_seconds: 86400,
            history_retention_seconds: 2592000,
            trash_retention_seconds: 2592000,
            operation_retention_seconds: 604800,
            replica_expiry_seconds: 7776000,
            read_pin_seconds: 900,
            change_retention_seconds: 604800,
            metadata_reserve_bytes: 1073741824,
            max_projects: 1000,
            max_datasets: 10000,
            max_replicas: 1000,
            max_pins: 10000,
            max_metadata_records: 1000000,
        }
    }
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub authority_id: String,
    pub history_epoch: String,
    pub namespace_id: String,
    pub principal_id: String,
    pub cursor_key: String,
    pub schema_version: u32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub project_id: String,
    pub state: String,
    pub metadata_revision: String,
    pub lifecycle_revision: String,
    pub deleted_at: Option<u64>,
    pub recoverable_until: Option<u64>,
    pub sequence: u64,
    #[serde(default)]
    pub change_floor: u64,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Head {
    pub generation: String,
    pub manifest_hash: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dataset {
    pub dataset_id: String,
    pub kind: String,
    pub logical_id: String,
    pub state: String,
    pub head: Head,
    pub recoverable_until: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Version {
    pub head: Head,
    pub committed_at: u64,
    pub superseded_at: Option<u64>,
    pub retain_until: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Replica {
    pub replica_id: String,
    pub state: String,
    pub last_admitted_seq: u64,
    pub last_seen: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OperationIdentity {
    pub operation_id: String,
    pub replica_id: String,
    pub op_seq: String,
    pub history_epoch: String,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn random_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| Error::storage())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
