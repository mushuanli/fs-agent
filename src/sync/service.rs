use super::{
    model::*,
    policy,
    store::{blobs, metadata as m},
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    fs::File,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

pub struct SyncService {
    pub config: Config,
    pub(super) identity: Identity,
    pub(super) db: Mutex<Connection>,
    metrics: super::metrics::Metrics,
    _lock: File,
    pub(super) tasks: Arc<super::coordination::Coordination>,
    pub(super) healthy: AtomicBool,
    pub(super) accepting: AtomicBool,
    pub uploads: Arc<tokio::sync::Semaphore>,
    started_at: std::time::Instant,
    wall_time: u64,
}
impl SyncService {
    pub fn validate_isolation(config: &Config, app: &crate::config::Config) -> Result<()> {
        super::store::boundary::validate(config, app)
    }
    /// Refuse to initialize into a directory that holds anything other than the
    /// empty `sync.lock` left behind by an earlier open attempt.
    fn ensure_initializable(config: &Config) -> Result<()> {
        if !config.root.exists() || config.root.join("init.pending").exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(&config.root)? {
            if entry?.file_name() != "sync.lock" {
                return Err(Error::new("SYNC_ROOT_NOT_EMPTY", 409));
            }
        }
        Ok(())
    }
    /// True when this root has never held a store, so it is safe to initialize.
    ///
    /// Only a missing database counts. A root that still has `storage.json`, a
    /// WAL or objects is a damaged store, not a new one: it must be repaired or
    /// restored explicitly instead of being replaced by a fresh identity.
    fn needs_initialization(config: &Config) -> bool {
        !config.root.join("metadata.db").exists() && !config.root.join("metadata.db-wal").exists()
    }
    pub fn init(config: &Config) -> Result<()> {
        policy::validate(config)?;
        Self::ensure_initializable(config)?;
        blobs::private_dir(&config.root)?;
        let _lock = blobs::lock(&config.root)?;
        let pending = config.root.join("init.pending");
        let marker = Self::initialization_marker(config)?;
        blobs::atomic(&pending, b"initializing")?;
        let db = m::initialize(&config.root.join("metadata.db"))?;
        let identity = Self::initialization_identity(config, &db, marker.as_ref())?;
        m::put(&db, "", "info", "identity", &identity)?;
        if m::get::<u64>(&db, "", "info", "clock")?.is_none() {
            m::put(&db, "", "info", "clock", &now())?;
        }
        Self::initialize_directories(config, &db, &identity)?;
        std::fs::remove_file(pending)?;
        blobs::sync_dir(&config.root)
    }
    fn initialization_identity(
        config: &Config,
        db: &Connection,
        marker: Option<&Identity>,
    ) -> Result<Identity> {
        let identity = match m::get::<Identity>(db, "", "info", "identity")? {
            Some(identity) => identity,
            None if marker.is_some() || Self::has_objects(config)? => {
                return Err(Error::new("SYNC_INITIALIZATION_UNSAFE", 503))
            }
            None => Self::new_identity(config)?,
        };
        validate_identity(config, marker.unwrap_or(&identity), &identity)?;
        Ok(identity)
    }
    fn initialization_marker(config: &Config) -> Result<Option<Identity>> {
        let marker = config.root.join("storage.json");
        let database = config.root.join("metadata.db");
        if !database.exists()
            && (marker.exists()
                || config.root.join("metadata.db-wal").exists()
                || config.root.join("restore.pending").exists()
                || Self::has_objects(config)?)
        {
            return Err(Error::new("SYNC_INITIALIZATION_UNSAFE", 503));
        }
        if marker.exists() {
            return Ok(Some(serde_json::from_slice(&std::fs::read(marker)?)?));
        }
        Ok(None)
    }
    fn has_objects(config: &Config) -> Result<bool> {
        let mut found = false;
        blobs::visit(&config.root.join("objects"), &mut |_| {
            found = true;
            Ok(())
        })?;
        Ok(found)
    }
    fn new_identity(config: &Config) -> Result<Identity> {
        Ok(Identity {
            authority_id: random_id()?,
            history_epoch: random_id()?,
            cursor_key: random_id()?,
            principal_id: config.principal_id.clone(),
            namespace_id: config.namespace_id.clone(),
            schema_version: 1,
        })
    }
    fn initialize_directories(config: &Config, db: &Connection, identity: &Identity) -> Result<()> {
        blobs::private_dir(&config.root.join("objects"))?;
        blobs::private_dir(&config.root.join("staging"))?;
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        blobs::atomic(
            &config.root.join("storage.json"),
            &serde_json::to_vec(identity)?,
        )
    }
    /// Open the store, initializing a missing or empty root on first start.
    pub fn open(config: &Config) -> Result<Arc<Self>> {
        Self::open_mode(config, false, true)
    }
    pub(super) fn open_restoring(config: &Config) -> Result<Arc<Self>> {
        Self::open_mode(config, true, true)
    }
    pub(super) fn open_verify(config: &Config) -> Result<Arc<Self>> {
        Self::open_mode(config, false, false)
    }
    fn open_mode(config: &Config, restoring: bool, recover: bool) -> Result<Arc<Self>> {
        policy::validate(config)?;
        if (!restoring && config.root.join("restore.pending").exists())
            || config.root.join("init.pending").exists()
        {
            return Err(Error::new("SYNC_INITIALIZATION_INCOMPLETE", 503));
        }
        if Self::needs_initialization(config) {
            // A first launch on a missing or empty root creates the store
            // instead of failing the start. Damage that is not an absent store
            // still fails closed in init, and a read-only open never writes.
            if !recover || restoring {
                return Err(Error::new("SYNC_NOT_INITIALIZED", 503));
            }
            Self::init(config)?;
            crate::core::events::emit(
                crate::core::events::Level::Info,
                "sync.initialized",
                json!({"root": config.root.display().to_string()}),
            );
        }
        let lock = blobs::lock(&config.root)?;
        let marker: Identity =
            serde_json::from_slice(&std::fs::read(config.root.join("storage.json"))?)?;
        let db = if recover {
            m::open(&config.root.join("metadata.db"))?
        } else {
            m::open_readonly(&config.root.join("metadata.db"))?
        };
        let identity: Identity = m::require(&db, "", "info", "identity")?;
        validate_identity(config, &marker, &identity)?;
        let service = Arc::new(Self {
            config: config.clone(),
            identity,
            db: Mutex::new(db),
            metrics: Default::default(),
            _lock: lock,
            tasks: Arc::default(),
            healthy: AtomicBool::new(true),
            accepting: AtomicBool::new(recover),
            uploads: Arc::new(tokio::sync::Semaphore::new(config.max_concurrent_uploads)),
            started_at: std::time::Instant::now(),
            wall_time: now(),
        });
        if recover {
            service.recover()?;
        }
        Ok(service)
    }
    pub(super) fn time(&self) -> u64 {
        self.wall_time
            .saturating_add(self.started_at.elapsed().as_secs())
    }
    pub fn epoch(&self) -> &str {
        &self.identity.history_epoch
    }
    pub fn stop(&self) {
        self.tasks.stop();
        self.accepting.store(false, Ordering::SeqCst);
        self.uploads.close();
    }
    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::SeqCst)
    }
    pub(super) fn write_allowed(&self) -> Result<()> {
        self.tasks.ensure_accepting()?;
        if !self.healthy() {
            return Err(Error::storage());
        }
        if !self.accepting.load(Ordering::SeqCst) {
            return Err(Error::new("SYNC_RECOVERING", 503));
        }
        Ok(())
    }
    pub(super) fn connection(&self) -> Result<super::metrics::LockedConnection<'_>> {
        let started = std::time::Instant::now();
        let guard = self.db.lock().map_err(|_| Error::storage())?;
        self.metrics.wait.record(started);
        Ok(super::metrics::LockedConnection {
            guard,
            started: std::time::Instant::now(),
            sample: &self.metrics.hold,
        })
    }
    pub(super) fn transaction<'a>(
        &'a self,
        db: &'a mut Connection,
    ) -> Result<super::metrics::TimedTransaction<'a>> {
        let started = std::time::Instant::now();
        let inner = Some(db.transaction()?);
        Ok(super::metrics::TimedTransaction {
            inner,
            started,
            metrics: &self.metrics,
        })
    }
    pub(super) fn commit(&self, tx: super::metrics::TimedTransaction<'_>) -> Result<()> {
        if tx.commit().is_err() {
            self.mark_uncertain();
            return Err(Error::storage());
        }
        Ok(())
    }
    pub fn diagnostics(&self) -> Value {
        self.metrics.value()
    }
    pub fn capabilities(&self) -> Value {
        json!({"protocolVersion":1,"authorityId":self.identity.authority_id,"historyEpoch":self.epoch(),
            "namespaceId":self.identity.namespace_id,"filesManifest":true,"opaqueBundle":true,
            "atomicPublish":true,"durableOperations":true,"changeFeed":true,"readPins":true,
            "objectUpload":{"requiresContentLength":true,"resumable":false},
            "historyList":true,"datasetVersionRestore":true,"datasetTrashRestore":true,"projectUndelete":true,
            "projectCheckpoint":false,"offlineStoreBackup":true,"limits":self.limits(),"healthy":self.healthy()})
    }
    fn limits(&self) -> Value {
        let mut limits = serde_json::to_value(&self.config).expect("serializable limits");
        for key in [
            "root",
            "enabled",
            "principal_id",
            "namespace_id",
            "expected_authority_id",
        ] {
            limits.as_object_mut().unwrap().remove(key);
        }
        limits
    }
    pub fn start_maintenance(self: &Arc<Self>) {
        let service = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                if !service.accepting.load(Ordering::SeqCst) {
                    break;
                }
                let task = service.clone();
                let result = tokio::task::spawn_blocking(move || task.gc()).await;
                if crate::core::events::enabled(crate::core::events::Level::Debug) {
                    crate::core::events::emit(
                        crate::core::events::Level::Debug,
                        "sync.diagnostics",
                        service.diagnostics(),
                    );
                }
                if let Ok(Err(error)) = result {
                    if error.unknown {
                        service.mark_uncertain();
                    }
                    crate::core::events::emit(
                        crate::core::events::Level::Warn,
                        "sync.maintenance_failed",
                        json!({"code":error.code}),
                    );
                }
            }
        });
    }
    pub fn drain(&self) -> Result<()> {
        self.drain_timeout(std::time::Duration::from_secs(5))
    }
    pub fn drain_timeout(&self, timeout: std::time::Duration) -> Result<()> {
        self.tasks.drain(timeout)
    }
    pub(super) fn activity(&self) -> Result<super::coordination::Activity> {
        self.tasks.enter(false)
    }
    pub(super) fn cleanup_activity(&self) -> Result<super::coordination::Activity> {
        self.tasks.enter(true)
    }
    pub(super) fn mark_uncertain(&self) {
        self.healthy.store(false, Ordering::SeqCst);
    }
    pub(super) fn completion_allowed(&self) -> Result<()> {
        if !self.healthy() {
            return Err(Error::storage());
        }
        Ok(())
    }
    pub(super) fn with_read<T>(&self, action: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let db = self.connection()?;
        if !db.is_autocommit() {
            return Err(Error::storage());
        }
        let result = action(&db);
        if result.as_ref().err().is_some_and(|e| e.storage_fault) {
            self.mark_uncertain();
        }
        result
    }
    pub(super) fn with_write<T>(
        &self,
        action: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        let _activity = self.activity()?;
        let mut db = self.connection()?;
        self.write_allowed()?;
        let result = action(&mut db);
        self.finish_storage(&db, result)
    }
    pub(super) fn with_completion<T>(
        &self,
        action: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        let _activity = self.cleanup_activity()?;
        let mut db = self.connection()?;
        self.completion_allowed()?;
        let result = action(&mut db);
        self.finish_storage(&db, result)
    }
    fn finish_storage<T>(&self, db: &Connection, result: Result<T>) -> Result<T> {
        if !db.is_autocommit() {
            self.mark_uncertain();
            db.execute_batch("ROLLBACK").map_err(|_| Error::storage())?;
            return Err(Error::storage());
        }
        if result.as_ref().err().is_some_and(|e| e.unknown) {
            self.mark_uncertain();
        }
        result
    }
    pub(super) fn capacity_for(&self, db: &Connection, growth: u64) -> Result<()> {
        let count: u64 = db.query_row("SELECT (SELECT COUNT(*) FROM records)+(SELECT COUNT(*) FROM objects)+(SELECT COUNT(*) FROM manifest_refs)+(SELECT COUNT(*) FROM uploads)", [], |r| r.get(0))?;
        if count.saturating_add(growth) > self.config.max_metadata_records as u64 {
            return Err(Error::new("LIMIT_EXCEEDED", 429));
        }
        Ok(())
    }
    pub(super) fn project(&self, db: &Connection, project: &str, active: bool) -> Result<Project> {
        policy::id(project)?;
        let p: Project = m::require(db, "", "project", project)?;
        if active && p.state != "active" {
            return Err(Error::new("PROJECT_DELETED", 409));
        }
        Ok(p)
    }
    pub(super) fn quota(&self, db: &Connection, growth: u64) -> Result<()> {
        self.quota_for(db, growth, 1)
    }
    fn quota_for(&self, db: &Connection, growth: u64, rows: u64) -> Result<()> {
        self.capacity_for(db, rows)?;
        let used: u64 = db.query_row("SELECT COALESCE(SUM(size),0) FROM objects", [], |r| {
            r.get(0)
        })?;
        let reserved: u64 = db.query_row("SELECT COALESCE(SUM(size),0) FROM uploads", [], |r| {
            r.get(0)
        })?;
        if used.saturating_add(reserved).saturating_add(growth) > self.config.max_retained_bytes {
            return Err(Error::new("QUOTA_EXCEEDED", 507));
        }
        blobs::space(
            &self.config.root,
            self.config.metadata_reserve_bytes,
            growth,
        )
    }
    pub(super) fn path(&self, project: &str, hash: &str) -> Result<std::path::PathBuf> {
        blobs::object(
            &self.config.root,
            &self.identity.namespace_id,
            project,
            hash,
        )
    }
    pub(super) fn manifest(
        &self,
        db: &Connection,
        project: &str,
        hash: &str,
        kind: &str,
    ) -> Result<()> {
        let parsed = self.validate_manifest(db, project, hash, kind)?;
        for reference in parsed.refs {
            db.execute(
                "INSERT OR IGNORE INTO manifest_refs VALUES (?1,?2,?3)",
                (project, hash, reference.hash),
            )?;
        }
        m::put(
            db,
            project,
            "manifest",
            hash,
            &json!({"format":parsed.format}),
        )?;
        Ok(())
    }
    pub(super) fn validate_manifest(
        &self,
        db: &Connection,
        project: &str,
        hash: &str,
        kind: &str,
    ) -> Result<super::manifest::Validated> {
        self.ready(db, project, hash, None)?;
        let bytes = blobs::read(
            &self.path(project, hash)?,
            hash,
            self.config.max_manifest_bytes,
        )?;
        let parsed = super::manifest::validate(&bytes, &self.config)?;
        if (kind == "files") != (parsed.format == "fs-agent.files") {
            return Err(Error::new("INVALID_MANIFEST", 422));
        }
        for reference in &parsed.refs {
            self.ready(
                db,
                project,
                &reference.hash,
                Some(policy::number(&reference.size)?),
            )?;
        }
        Ok(parsed)
    }
    pub(super) fn ready(
        &self,
        db: &Connection,
        project: &str,
        hash: &str,
        size: Option<u64>,
    ) -> Result<()> {
        policy::hash(hash)?;
        use rusqlite::OptionalExtension;
        let row: Option<(u64, String)> = db
            .query_row(
                "SELECT size,state FROM objects WHERE project=?1 AND hash=?2",
                (project, hash),
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((n, state)) if state == "ready" && size.is_none_or(|s| s == n) => Ok(()),
            Some((_, state)) if state == "corrupt" => Err(Error::new("OBJECT_CORRUPT", 503)),
            _ => Err(Error::new("OBJECT_MISSING", 422)),
        }
    }
    pub(super) fn protect(
        &self,
        db: &Connection,
        project: &str,
        hash: &str,
        until: u64,
    ) -> Result<()> {
        db.execute(
            "UPDATE objects SET retain_until=MAX(retain_until,?3) WHERE project=?1 AND
            (hash=?2 OR hash IN (SELECT hash FROM manifest_refs WHERE project=?1 AND manifest=?2))",
            (project, hash, until),
        )?;
        Ok(())
    }
}
fn validate_identity(config: &Config, marker: &Identity, identity: &Identity) -> Result<()> {
    if marker != identity
        || identity.schema_version != 1
        || identity.namespace_id != config.namespace_id
        || identity.principal_id != config.principal_id
        || config
            .expected_authority_id
            .as_ref()
            .is_some_and(|id| id != &identity.authority_id)
    {
        return Err(Error::new("SYNC_IDENTITY_MISMATCH", 503));
    }
    Ok(())
}

impl SyncService {
    pub fn reserve(&self, project: &str, hash: &str, size: u64) -> Result<String> {
        self.with_write(|db| {
            policy::hash(hash)?;
            if size > self.config.max_object_bytes {
                return Err(Error::new("LIMIT_EXCEEDED", 413));
            }
            let tx = self.transaction(db)?;
            self.project(&tx, project, true)?;
            self.quota(&tx, size)?;
            let id = random_id()?;
            tx.execute(
                "INSERT INTO uploads VALUES (?1,?2,?3,?4,?5)",
                (
                    &id,
                    project,
                    hash,
                    size,
                    self.time() + self.config.upload_ttl_seconds,
                ),
            )?;
            self.commit(tx)?;
            Ok(id)
        })
    }
    pub fn install(&self, id: &str) -> Result<Value> {
        self.with_completion(|db| {
        policy::id(id)?;
        let (project, hash, size) = self.upload_record(db, id)?;
        self.project(db, &project, true)?;
        self.install_file(&project, &hash, id, size)?;
        super::fault::point("after-object-install");
        let tx = self.transaction(db)?;
        self.quota_for(&tx, 0, 0)?;
        let until = self.time() + self.config.upload_ttl_seconds;
        tx.execute("INSERT INTO objects VALUES (?1,?2,?3,'ready',?4) ON CONFLICT(project,hash) DO UPDATE SET retain_until=MAX(retain_until,excluded.retain_until)",(&project,&hash,size,until))?;
        tx.execute("DELETE FROM uploads WHERE id=?1", [id])?;
        self.commit(tx)?;
        super::fault::point("after-object-ready");
        std::fs::remove_file(self.config.root.join("staging").join(id))?;
        blobs::sync_dir(&self.config.root.join("staging"))?;
        Ok(json!({"hash":hash,"size":size.to_string(),"retainUntil":until}))
            })
    }
    fn upload_record(&self, db: &Connection, id: &str) -> Result<(String, String, u64)> {
        use rusqlite::OptionalExtension;
        let row: Option<(String, String, u64, u64)> = db
            .query_row(
                "SELECT project,hash,size,expires FROM uploads WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let (project, hash, size, expires) =
            row.ok_or_else(|| Error::new("UPLOAD_EXPIRED", 410))?;
        if expires <= self.time() {
            return Err(Error::new("UPLOAD_EXPIRED", 410));
        }
        Ok((project, hash, size))
    }
    fn install_file(&self, project: &str, hash: &str, id: &str, size: u64) -> Result<()> {
        let temporary = self.config.root.join("staging").join(id);
        let verified = blobs::verify(&temporary, hash).map_err(|e| {
            if e.code == "OBJECT_CORRUPT" {
                Error::new("OBJECT_HASH_MISMATCH", 422)
            } else {
                e
            }
        })?;
        if verified != size {
            return Err(Error::new("OBJECT_SIZE_MISMATCH", 422));
        }
        let target = self.path(project, hash)?;
        blobs::private_dir(target.parent().unwrap())?;
        if target.exists() {
            blobs::verify(&target, hash)?;
        } else {
            std::fs::hard_link(&temporary, &target)?;
            blobs::sync_dir(target.parent().unwrap())?;
        }
        Ok(())
    }
    pub fn reuse(&self, project: &str, hash: &str, size: u64) -> Result<Option<Value>> {
        self.with_write(|db| {
            policy::hash(hash)?;
            self.project(db, project, true)?;
            use rusqlite::OptionalExtension;
            let row: Option<(u64, String)> = db
                .query_row(
                    "SELECT size,state FROM objects WHERE project=?1 AND hash=?2",
                    (project, hash),
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((stored, state)) = row else {
                return Ok(None);
            };
            if state != "ready" {
                return Err(Error::new("OBJECT_CORRUPT", 503));
            }
            if stored != size {
                return Err(Error::new("OBJECT_SIZE_MISMATCH", 422));
            }
            self.verify_existing(db, project, hash, stored)?;
            let until = self.time() + self.config.upload_ttl_seconds;
            self.protect(db, project, hash, until)?;
            Ok(Some(
                json!({"hash":hash,"size":size.to_string(),"retainUntil":until,"reused":true}),
            ))
        })
    }
    pub(super) fn verify_existing(
        &self,
        db: &Connection,
        project: &str,
        hash: &str,
        size: u64,
    ) -> Result<()> {
        let result = blobs::verify(&self.path(project, hash)?, hash);
        match result {
            Ok(actual) if actual == size => Ok(()),
            Err(error) if error.code != "OBJECT_CORRUPT" && error.code != "OBJECT_MISSING" => {
                Err(error)
            }
            _ => {
                db.execute(
                    "UPDATE objects SET state='corrupt' WHERE project=?1 AND hash=?2",
                    (project, hash),
                )?;
                Err(Error::new("OBJECT_CORRUPT", 503))
            }
        }
    }
    pub fn abandon(&self, id: &str) -> Result<()> {
        let db = self.connection()?;
        if !db.is_autocommit() {
            return Err(Error::storage());
        }
        let _ = std::fs::remove_file(self.config.root.join("staging").join(id));
        db.execute("DELETE FROM uploads WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn object_file(&self, project: &str, hash: &str) -> Result<(File, u64)> {
        self.with_read(|db| {
            self.project(db, project, false)?;
            self.ready(db, project, hash, None)?;
            let path = self.path(project, hash)?;
            let size: u64 = db.query_row(
                "SELECT size FROM objects WHERE project=?1 AND hash=?2",
                (project, hash),
                |row| row.get(0),
            )?;
            self.verify_existing(db, project, hash, size)?;
            Ok((File::open(path)?, size))
        })
    }
    pub fn manifest_bytes(&self, project: &str, hash: &str) -> Result<Vec<u8>> {
        self.with_read(|db| {
            self.project(db, project, false)?;
            policy::hash(hash)?;
            let _: Value = m::require(db, project, "manifest", hash)?;
            self.closure_ready(db, project, hash)?;
            let result = blobs::read(
                &self.path(project, hash)?,
                hash,
                self.config.max_manifest_bytes,
            );
            if result
                .as_ref()
                .err()
                .is_some_and(|e| e.code == "OBJECT_CORRUPT" || e.code == "OBJECT_MISSING")
            {
                db.execute(
                    "UPDATE objects SET state='corrupt' WHERE project=?1 AND hash=?2",
                    (project, hash),
                )?;
            }
            result
        })
    }
    pub fn temporary_file(&self, id: &str) -> Result<File> {
        self.with_completion(|db| {
            policy::id(id)?;
            self.upload_record(db, id)?;
            blobs::temporary(&self.config.root, id)
        })
    }
    pub fn check_objects(&self, project: &str, body: &Value) -> Result<Value> {
        self.with_read(|db| {
            let hashes = body["hashes"]
                .as_array()
                .ok_or_else(|| Error::new("INVALID_JSON", 400))?;
            if hashes.len() > 1000 {
                return Err(Error::new("LIMIT_EXCEEDED", 413));
            }
            self.project(db, project, false)?;
            let mut ready = Vec::new();
            for hash in hashes {
                let hash = hash
                    .as_str()
                    .ok_or_else(|| Error::new("INVALID_HASH", 400))?;
                policy::hash(hash)?;
                match self.ready(db, project, hash, None) {
                    Ok(()) => ready.push(hash),
                    Err(e) if e.code == "OBJECT_MISSING" || e.code == "OBJECT_CORRUPT" => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(json!({"ready":ready}))
        })
    }
}
