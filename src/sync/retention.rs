use super::{
    model::*,
    operations::text,
    policy,
    service::SyncService,
    store::{blobs, metadata as m},
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeSet;

impl SyncService {
    pub(super) fn closure_ready(&self, db: &Connection, project: &str, hash: &str) -> Result<()> {
        self.ready(db, project, hash, None)?;
        let mut stmt =
            db.prepare("SELECT hash FROM manifest_refs WHERE project=?1 AND manifest=?2")?;
        for row in stmt.query_map((project, hash), |r| r.get::<_, String>(0))? {
            self.ready(db, project, &row?, None)?;
        }
        Ok(())
    }
    pub fn pin(&self, project: &str, body: &Value) -> Result<Value> {
        self.with_write(|db| {
            let request = text(body, "requestKey")?;
            policy::id(request)?;
            let hash = text(body, "manifestHash")?;
            policy::hash(hash)?;
            let tx = db.transaction()?;
            self.project(&tx, project, false)?;
            if let Some(pin) = self.existing_pin(&tx, project, request, hash)? {
                return Ok(pin);
            }
            self.quota(&tx, 0)?;
            if !self.readable_manifest(&tx, project, hash)? {
                return Err(Error::new("VERSION_EXPIRED", 410));
            }
            self.closure_ready(&tx, project, hash)?;
            let ttl = body["ttlSeconds"]
                .as_u64()
                .unwrap_or(self.config.read_pin_seconds)
                .clamp(1, self.config.read_pin_seconds);
            let until = self.time() + ttl;
            let id = random_id()?;
            let pin = json!({"pinId":id,"manifestHash":hash,"requestKey":request,"expires":until});
            m::put(&tx, project, "pin", &id, &pin)?;
            self.protect(&tx, project, hash, until)?;
            self.commit(tx)?;
            Ok(pin)
        })
    }
    pub fn pin_action(&self, project: &str, id: &str, action: &str) -> Result<Value> {
        self.with_write(|db| {
            policy::id(id)?;
            let tx = db.transaction()?;
            self.project(&tx, project, false)?;
            let pin = m::get::<Value>(&tx, project, "pin", id)?;
            if action == "release" {
                m::remove(&tx, project, "pin", id)?;
                self.commit(tx)?;
                return Ok(json!({"released":true}));
            }
            let mut pin = pin.ok_or_else(|| Error::new("READ_PIN_EXPIRED", 410))?;
            if pin["expires"].as_u64().unwrap_or(0) <= self.time() {
                return Err(Error::new("READ_PIN_EXPIRED", 410));
            }
            let hash = text(&pin, "manifestHash")?.to_owned();
            self.closure_ready(&tx, project, &hash)?;
            pin["expires"] = json!(self.time() + self.config.read_pin_seconds);
            self.protect(
                &tx,
                project,
                &hash,
                self.time() + self.config.read_pin_seconds,
            )?;
            m::put(&tx, project, "pin", id, &pin)?;
            self.commit(tx)?;
            Ok(pin)
        })
    }
    fn readable_manifest(&self, db: &Connection, project: &str, hash: &str) -> Result<bool> {
        let mut stmt = db.prepare(
            "SELECT key,value FROM records WHERE scope=?1 AND kind='version'
            AND json_extract(value,'$.head.manifestHash')=?2 ORDER BY key DESC",
        )?;
        for row in stmt.query_map((project, hash), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (key, raw) = row?;
            let id = key.split('/').next().ok_or_else(Error::storage)?;
            let d: Dataset = m::require(db, project, "dataset", id)?;
            let version: Version = serde_json::from_str(&raw).map_err(|_| Error::storage())?;
            if self.version_available(db, project, &d, &version)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn roots(&self, db: &Connection) -> Result<BTreeSet<(String, String)>> {
        let mut roots = BTreeSet::new();
        for (_, p) in m::list::<Project>(db, "", "project")? {
            if p.state != "active" && p.recoverable_until.unwrap_or(0) <= self.time() {
                continue;
            }
            for (_, d) in m::list::<Dataset>(db, &p.project_id, "dataset")? {
                if d.state == "active" {
                    roots.insert((p.project_id.clone(), d.head.manifest_hash));
                }
            }
        }
        let manifests: Vec<_> = roots.clone().into_iter().collect();
        for (project, hash) in manifests {
            let mut stmt =
                db.prepare("SELECT hash FROM manifest_refs WHERE project=?1 AND manifest=?2")?;
            for row in stmt.query_map((&project, &hash), |r| r.get::<_, String>(0))? {
                roots.insert((project.clone(), row?));
            }
        }
        Ok(roots)
    }
    pub fn gc(&self) -> Result<Value> {
        self.with_write(|db| {
            let water: u64 = m::require(db, "", "info", "clock")?;
            if self.time() < water {
                return Err(Error::new("CLOCK_REGRESSION", 503));
            }
            let roots = self.roots(db)?;
            let tx = db.transaction()?;
            let count = self.mark_gc(&tx, &roots)?;
            m::put(&tx, "", "info", "clock", &self.time())?;
            self.commit(tx)?;
            super::fault::point("after-gc-mark");
            self.finish_gc(db)?;
            self.prune(db)?;
            Ok(json!({"deletedObjects":count}))
        })
    }
    fn mark_gc(&self, db: &Connection, roots: &BTreeSet<(String, String)>) -> Result<u64> {
        let mut stmt=db.prepare("SELECT project,hash FROM objects WHERE retain_until<=?1 AND state IN ('ready','corrupt')")?;
        let rows = stmt.query_map([self.time()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut count = 0;
        for row in rows {
            let (project, hash) = row?;
            if roots.contains(&(project.clone(), hash.clone())) {
                continue;
            }
            db.execute(
                "UPDATE objects SET state='deleting' WHERE project=?1 AND hash=?2",
                (&project, &hash),
            )?;
            db.execute(
                "INSERT OR IGNORE INTO gc_items VALUES (?1,?2)",
                (&project, &hash),
            )?;
            count += 1;
            if count >= 1000 {
                break;
            }
        }
        Ok(count)
    }
    fn existing_pin(
        &self,
        db: &Connection,
        project: &str,
        request: &str,
        hash: &str,
    ) -> Result<Option<Value>> {
        let pins = m::list::<Value>(db, project, "pin")?;
        if let Some((_, old)) = pins.iter().find(|(_, p)| p["requestKey"] == request) {
            if old["manifestHash"] != hash {
                return Err(Error::new("OPERATION_REUSED", 409));
            }
            if old["expires"].as_u64().unwrap_or(0) <= self.time() {
                return Err(Error::new("READ_PIN_EXPIRED", 410));
            }
            return Ok(Some(old.clone()));
        }
        if pins.len() >= self.config.max_pins {
            return Err(Error::new("LIMIT_EXCEEDED", 429));
        }
        Ok(None)
    }
    fn finish_gc(&self, db: &Connection) -> Result<()> {
        let mut stmt = db.prepare("SELECT project,hash FROM gc_items")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (project, hash) = row?;
            let path = self.path(&project, &hash)?;
            if path.exists() {
                std::fs::remove_file(&path)?;
                blobs::sync_dir(path.parent().unwrap())?;
            }
            db.execute(
                "DELETE FROM manifest_refs WHERE project=?1 AND (manifest=?2 OR hash=?2)",
                (&project, &hash),
            )?;
            m::remove(db, &project, "manifest", &hash)?;
            db.execute(
                "DELETE FROM objects WHERE project=?1 AND hash=?2",
                (&project, &hash),
            )?;
            db.execute(
                "DELETE FROM gc_items WHERE project=?1 AND hash=?2",
                (&project, &hash),
            )?;
        }
        Ok(())
    }
    fn prune(&self, db: &Connection) -> Result<()> {
        for (_, p) in m::list::<Project>(db, "", "project")? {
            for (id, pin) in m::list::<Value>(db, &p.project_id, "pin")? {
                if pin["expires"].as_u64().unwrap_or(0) <= self.time() {
                    m::remove(db, &p.project_id, "pin", &id)?;
                }
            }
        }
        db.execute("DELETE FROM records WHERE scope=?1 AND kind='operation' AND json_extract(value,'$.finishedAt')+?2<=?3",
            (self.epoch(),self.config.operation_retention_seconds,self.time()))?;
        Ok(())
    }
    pub(super) fn recover(&self) -> Result<()> {
        let mut db = self.connection()?;
        let tx = db.transaction()?;
        tx.execute("UPDATE records SET value=json_set(value,'$.receipt',json(?2),'$.finishedAt',?3)
            WHERE scope=?1 AND kind='operation' AND json_extract(value,'$.receipt.state')='pending'",
            (self.epoch(),json!({"outcome":"not-committed","code":"SERVER_RESTART","status":409}).to_string(),self.time()))?;
        tx.execute("DELETE FROM uploads", [])?;
        self.commit(tx)?;
        self.finish_gc(&db)?;
        self.recover_repairs(&db)?;
        self.sweep_orphans(&db)?;
        blobs::visit(&self.config.root.join("staging"), &mut |path| {
            std::fs::remove_file(path)?;
            Ok(())
        })?;
        self.check_object_presence(&db)
    }
    fn sweep_orphans(&self, db: &Connection) -> Result<()> {
        for (_, p) in m::list::<Project>(db, "", "project")? {
            let dir = self
                .config
                .root
                .join("objects")
                .join(&self.identity.namespace_id)
                .join(&p.project_id);
            blobs::visit(&dir, &mut |path| {
                let Some(hash) = path.file_name().and_then(|v| v.to_str()) else {
                    return Err(Error::storage());
                };
                if policy::hash(hash).is_err() {
                    return Ok(());
                }
                let count: u64 = db.query_row(
                    "SELECT COUNT(*) FROM objects WHERE project=?1 AND hash=?2",
                    (&p.project_id, hash),
                    |r| r.get(0),
                )?;
                if count == 0 {
                    std::fs::remove_file(path)?;
                    blobs::sync_dir(path.parent().unwrap())?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }
    fn check_object_presence(&self, db: &Connection) -> Result<()> {
        let mut stmt = db.prepare("SELECT project,hash,size FROM objects WHERE state='ready'")?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u64>(2)?,
            ))
        })? {
            let (project, hash, size) = row?;
            let mismatch = match std::fs::metadata(self.path(&project, &hash)?) {
                Ok(metadata) => metadata.len() != size,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => return Err(e.into()),
            };
            if mismatch {
                db.execute(
                    "UPDATE objects SET state='corrupt' WHERE project=?1 AND hash=?2",
                    (&project, &hash),
                )?;
            }
        }
        Ok(())
    }
}
