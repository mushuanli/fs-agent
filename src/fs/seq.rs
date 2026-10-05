//! SQLite-backed, portable SeqFiles. SQL executes in memory; publication reuses atomic file CAS.
use crate::{
    core::error::Error,
    fs::{upload, Export},
};
use rusqlite::{serialize::OwnedData, Connection, DatabaseName};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{Read, Write},
    ptr::NonNull,
};

const MAX_BYTES: usize = 16 * 1024 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Update {
    pub path: String,
    pub expected_revision: Option<String>,
    pub changes: Vec<Change>,
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub enum Change {
    Set { key: String, value: Value },
    Delete { key: String },
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub revision: Option<String>,
    pub entries: Vec<Entry>,
}
#[derive(Serialize)]
pub struct Entry {
    pub key: String,
    pub value: Value,
}

pub fn snapshot(export: &Export, path: &str) -> Result<Snapshot, Error> {
    validate(path)?;
    let (db, revision) = load(export, path)?;
    let mut statement = db
        .prepare("SELECT key,value FROM entries ORDER BY key")
        .map_err(sql)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql)?;
    let mut entries = Vec::new();
    for row in rows {
        let (key, value) = row.map_err(sql)?;
        entries.push(Entry {
            key,
            value: serde_json::from_str(&value).map_err(|_| Error::internal())?,
        });
    }
    Ok(Snapshot { revision, entries })
}
pub fn update(
    export: &Export,
    input: Update,
    checkpoint: impl Fn() -> Result<(), Error>,
) -> Result<Value, Error> {
    if !export.writable() {
        return Err(Error::forbidden("EROFS"));
    }
    validate(&input.path)?;
    if input.changes.len() > 256 {
        return Err(Error::invalid());
    }
    checkpoint()?;
    let (mut db, revision) = load(export, &input.path)?;
    if revision != input.expected_revision {
        return Err(Error::precondition_failed());
    }
    apply(&mut db, input.changes, &checkpoint)?;
    publish(export, &input.path, &db, revision.as_deref(), checkpoint)
}
fn publish(
    export: &Export,
    path: &str,
    db: &Connection,
    revision: Option<&str>,
    checkpoint: impl Fn() -> Result<(), Error>,
) -> Result<Value, Error> {
    let bytes = db.serialize(DatabaseName::Main).map_err(sql)?;
    if bytes.len() > MAX_BYTES {
        return Err(Error::invalid());
    }
    let staged = export.stage(path)?;
    staged.writer()?.write_all(&bytes)?;
    Ok(
        serde_json::to_value(upload::commit(export, path, staged, revision, checkpoint)?)
            .map_err(|_| Error::internal())?,
    )
}
fn apply(
    db: &mut Connection,
    changes: Vec<Change>,
    checkpoint: &impl Fn() -> Result<(), Error>,
) -> Result<(), Error> {
    let tx = db.transaction().map_err(sql)?;
    for change in changes {
        checkpoint()?;
        match change {
            Change::Set { key, value } => {
                validate_key(&key)?;
                tx.execute("INSERT INTO entries(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", (&key, value.to_string())).map_err(sql)?;
            }
            Change::Delete { key } => {
                validate_key(&key)?;
                tx.execute("DELETE FROM entries WHERE key=?1", [&key])
                    .map_err(sql)?;
            }
        }
    }
    tx.commit().map_err(sql)
}
fn load(export: &Export, path: &str) -> Result<(Connection, Option<String>), Error> {
    let mut db = Connection::open_in_memory().map_err(sql)?;
    let (bytes, revision) = match export.read(path) {
        Ok((file, revision)) => {
            if file.metadata()?.len() > MAX_BYTES as u64 {
                return Err(Error::invalid());
            }
            let mut bytes = Vec::new();
            file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
            if bytes.len() > MAX_BYTES {
                return Err(Error::invalid());
            }
            (bytes, revision)
        }
        Err(error) if error.code == "ENOENT" => (Vec::new(), None),
        Err(error) => return Err(error),
    };
    if !bytes.is_empty() {
        deserialize(&mut db, &bytes)?;
    }
    db.execute_batch("PRAGMA trusted_schema=OFF; CREATE TABLE IF NOT EXISTS entries(key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL) WITHOUT ROWID;").map_err(sql)?;
    Ok((db, revision))
}
fn deserialize(db: &mut Connection, bytes: &[u8]) -> Result<(), Error> {
    // SQLite owns this allocation after deserialize; OwnedData frees it on an early failure.
    let pointer =
        NonNull::new(unsafe { rusqlite::ffi::sqlite3_malloc64(bytes.len() as u64) }.cast::<u8>())
            .ok_or_else(Error::internal)?;
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.as_ptr(), bytes.len());
    }
    let data = unsafe { OwnedData::from_raw_nonnull(pointer, bytes.len()) };
    db.deserialize(DatabaseName::Main, data, false).map_err(sql)
}
fn validate(path: &str) -> Result<(), Error> {
    crate::fs::path::validate(path)?;
    if !path.ends_with(".seq") {
        return Err(Error::invalid());
    }
    Ok(())
}
fn validate_key(key: &str) -> Result<(), Error> {
    if key.is_empty() || key.len() > 1024 || key.contains('\0') {
        return Err(Error::invalid());
    }
    Ok(())
}
fn sql(error: rusqlite::Error) -> Error {
    if let rusqlite::Error::SqliteFailure(failure, _) = error {
        use rusqlite::ErrorCode;
        return match failure.code {
            ErrorCode::DiskFull => {
                Error::new(axum::http::StatusCode::INSUFFICIENT_STORAGE, "ENOSPC")
            }
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => Error::busy(),
            _ => Error::internal(),
        };
    }
    Error::internal()
}
