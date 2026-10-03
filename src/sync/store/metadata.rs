use super::super::model::{Error, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;

pub fn open(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
    Ok(db)
}
pub fn open_readonly(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}
pub fn initialize(path: &Path) -> Result<Connection> {
    let db = Connection::open(path)?;
    super::schema::setup(&db)?;
    Ok(db)
}
pub fn get<T: DeserializeOwned>(
    db: &Connection,
    scope: &str,
    kind: &str,
    key: &str,
) -> Result<Option<T>> {
    let value: Option<String> = db
        .query_row(
            "SELECT value FROM records WHERE scope=?1 AND kind=?2 AND key=?3",
            (scope, kind, key),
            |r| r.get(0),
        )
        .optional()?;
    value
        .map(|s| serde_json::from_str(&s).map_err(|_| Error::storage()))
        .transpose()
}
pub fn require<T: DeserializeOwned>(
    db: &Connection,
    scope: &str,
    kind: &str,
    key: &str,
) -> Result<T> {
    get(db, scope, kind, key)?.ok_or_else(|| Error::new("NOT_FOUND", 404))
}
pub fn put<T: Serialize>(
    db: &Connection,
    scope: &str,
    kind: &str,
    key: &str,
    value: &T,
) -> Result<()> {
    db.execute("INSERT INTO records VALUES (?1,?2,?3,?4) ON CONFLICT(scope,kind,key) DO UPDATE SET value=excluded.value",
        (scope,kind,key,serde_json::to_string(value)?))?;
    Ok(())
}
pub fn list<T: DeserializeOwned>(
    db: &Connection,
    scope: &str,
    kind: &str,
) -> Result<Vec<(String, T)>> {
    let mut stmt =
        db.prepare("SELECT key,value FROM records WHERE scope=?1 AND kind=?2 ORDER BY key")?;
    let rows = stmt.query_map((scope, kind), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (k, v) = row?;
        Ok((k, serde_json::from_str(&v).map_err(|_| Error::storage())?))
    })
    .collect()
}
pub fn remove(db: &Connection, scope: &str, kind: &str, key: &str) -> Result<()> {
    db.execute(
        "DELETE FROM records WHERE scope=?1 AND kind=?2 AND key=?3",
        (scope, kind, key),
    )?;
    Ok(())
}

pub fn range<T: DeserializeOwned>(
    db: &Connection,
    scope: &str,
    kind: &str,
    after: &str,
    upper: &str,
    limit: usize,
) -> Result<Vec<(String, T)>> {
    let mut stmt=db.prepare("SELECT key,value FROM records WHERE scope=?1 AND kind=?2 AND key>?3 AND key<=?4 ORDER BY key LIMIT ?5")?;
    let rows = stmt.query_map((scope, kind, after, upper, limit), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (key, value) = row?;
        Ok((
            key,
            serde_json::from_str(&value).map_err(|_| Error::storage())?,
        ))
    })
    .collect()
}
pub fn catalog(
    db: &Connection,
    scope: &str,
    upper: u64,
    after: &str,
    state: &str,
    limit: usize,
) -> Result<Vec<super::super::model::Dataset>> {
    let mut stmt=db.prepare("WITH snapshot AS (
        SELECT value, ROW_NUMBER() OVER (PARTITION BY json_extract(value,'$.datasetId') ORDER BY key DESC) AS position
        FROM records WHERE scope=?1 AND kind='catalog' AND key<=?2)
        SELECT value FROM snapshot WHERE position=1 AND json_extract(value,'$.datasetId')>?3
        AND (?4='all' OR json_extract(value,'$.state')=?4) ORDER BY json_extract(value,'$.datasetId') LIMIT ?5")?;
    let rows = stmt.query_map(
        (scope, format!("{upper:020}/~"), after, state, limit),
        |r| r.get::<_, String>(0),
    )?;
    rows.map(|row| serde_json::from_str(&row?).map_err(|_| Error::storage()))
        .collect()
}
pub fn count(db: &Connection, scope: &str, kind: &str) -> Result<usize> {
    Ok(db.query_row(
        "SELECT COUNT(*) FROM records WHERE scope=?1 AND kind=?2",
        (scope, kind),
        |r| r.get(0),
    )?)
}
