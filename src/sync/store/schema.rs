use super::super::model::Result;
use rusqlite::Connection;

pub fn setup(db: &Connection) -> Result<()> {
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
    CREATE TABLE IF NOT EXISTS records (
      scope TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
      PRIMARY KEY(scope,kind,key));
    CREATE TABLE IF NOT EXISTS objects (
      project TEXT NOT NULL, hash TEXT NOT NULL, size INTEGER NOT NULL CHECK(size>=0),
      state TEXT NOT NULL, retain_until INTEGER NOT NULL,
      PRIMARY KEY(project,hash));
    CREATE TABLE IF NOT EXISTS manifest_refs (
      project TEXT NOT NULL, manifest TEXT NOT NULL, hash TEXT NOT NULL,
      PRIMARY KEY(project,manifest,hash),
      FOREIGN KEY(project,manifest) REFERENCES objects(project,hash),
      FOREIGN KEY(project,hash) REFERENCES objects(project,hash));
    CREATE INDEX IF NOT EXISTS version_hash ON records(scope,json_extract(value,'$.head.manifestHash')) WHERE kind='version';
    CREATE TABLE IF NOT EXISTS uploads (
      id TEXT PRIMARY KEY, project TEXT NOT NULL, hash TEXT NOT NULL, size INTEGER NOT NULL,
      expires INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS gc_items (project TEXT NOT NULL, hash TEXT NOT NULL, PRIMARY KEY(project,hash));")?;
    Ok(())
}
