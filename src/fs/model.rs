//! Serializable views of filesystem objects.
//!
//! These are the only shapes the HTTP layer sends for `stat` and `entries`;
//! keeping the projection here means `openat2`-derived metadata is converted
//! in exactly one place.

use crate::core::error::Error;
use serde::Serialize;
use std::time::UNIX_EPOCH;

/// Milliseconds since the epoch, or `0` when the platform cannot report it.
const MAX_JSON_INTEGER: u128 = 9_007_199_254_740_991;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stat {
    pub kind: &'static str,
    pub size: u64,
    pub modified_at: u64,
    pub created_at: u64,
    /// Present only for writable exports; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

#[derive(Serialize)]
pub struct Entry {
    pub name: String,
    pub stat: Stat,
}

/// Project kernel metadata into a protocol [`Stat`].
pub(crate) fn attributes(file: &std::fs::File) -> Result<Stat, Error> {
    let metadata = file.metadata()?;
    let kind = if metadata.is_symlink() {
        "symlink"
    } else if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else {
        // Sockets, FIFOs and devices have no representation in this protocol.
        return Err(Error::unsupported());
    };
    Ok(Stat {
        kind,
        size: metadata.len(),
        modified_at: millis(metadata.modified()),
        created_at: millis(metadata.created()),
        revision: None,
    })
}

fn millis(time: std::io::Result<std::time::SystemTime>) -> u64 {
    time.ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_millis().min(MAX_JSON_INTEGER) as u64)
        .unwrap_or(0)
}
