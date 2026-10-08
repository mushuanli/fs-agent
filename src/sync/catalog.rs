use super::{
    model::*,
    operations::{operation_identity, text, version_key},
    policy,
    service::SyncService,
    store::metadata as m,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::Sha256;

#[derive(PartialEq)]
pub(super) enum VersionAvailability {
    Available,
    Expired,
    Corrupt,
}
impl VersionAvailability {
    fn label(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Expired => "expired",
            Self::Corrupt => "corrupt",
        }
    }
}
impl SyncService {
    pub fn directory_head(&self, project: &str, id: &str) -> Result<Value> {
        self.with_read(|db| {
            policy::id(id)?; self.project(db, project, true)?;
            let dataset: Dataset = m::require(db, project, "dataset", id)?;
            if dataset.state != "active" || dataset.kind != "files" { return Err(Error::new("INVALID_DIRECTORY_DATASET",409)); }
            Ok(json!({"generation":dataset.head.generation,"manifestHash":dataset.head.manifest_hash}))
        })
    }
    pub(super) fn event(
        &self,
        db: &Connection,
        project: &str,
        dataset: &Dataset,
        kind: &str,
        prior: Option<&Head>,
        body: &Value,
    ) -> Result<()> {
        self.protect_receipt(db, project, &dataset.head.manifest_hash)?;
        let p = self.increment(db, project)?;
        m::put(db, project, "dataset", &dataset.dataset_id, dataset)?;
        m::put(
            db,
            project,
            "catalog",
            &format!("{:020}/{}", p.sequence, dataset.dataset_id),
            dataset,
        )?;
        let event = json!({"sequence":p.sequence.to_string(),"type":kind,"dataset":dataset,
            "recordedAt":self.time(),"previousHead":prior,"projectLifecycleRevision":p.lifecycle_revision,"operation":operation_identity(body)?});
        m::put(
            db,
            project,
            "change",
            &format!("{:020}", p.sequence),
            &event,
        )
    }
    pub(super) fn project_event(
        &self,
        db: &Connection,
        project: &str,
        kind: &str,
        body: &Value,
    ) -> Result<()> {
        let p = self.increment(db, project)?;
        m::put(
            db,
            project,
            "change",
            &format!("{:020}", p.sequence),
            &json!({"sequence":p.sequence.to_string(),
            "type":kind,"recordedAt":self.time(),"projectLifecycleRevision":p.lifecycle_revision,"operation":operation_identity(body)?}),
        )
    }
    fn increment(&self, db: &Connection, project: &str) -> Result<Project> {
        let mut p = self.project(db, project, false)?;
        p.sequence = policy::number(&policy::next(&p.sequence.to_string())?)?;
        m::put(db, "", "project", project, &p)?;
        Ok(p)
    }
    pub(super) fn encode_cursor(
        &self,
        project: &str,
        kind: &str,
        upper: u64,
        last: &str,
        state: &str,
    ) -> Result<String> {
        self.encode_cursor_until(
            project,
            kind,
            upper,
            last,
            state,
            self.time() + self.config.read_pin_seconds,
        )
    }
    pub(super) fn encode_cursor_until(
        &self,
        project: &str,
        kind: &str,
        upper: u64,
        last: &str,
        state: &str,
        expires: u64,
    ) -> Result<String> {
        let payload = json!({"epoch":self.epoch(),"namespace":self.identity.namespace_id,"project":project,"kind":kind,
            "upper":upper,"last":last,"state":state,"expires":expires});
        let bytes = serde_json::to_vec(&payload)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(self.identity.cursor_key.as_bytes())
            .map_err(|_| Error::storage())?;
        mac.update(&bytes);
        Ok(format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(bytes),
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        ))
    }
    pub(super) fn decode_cursor(&self, cursor: &str, project: &str, kind: &str) -> Result<Value> {
        if cursor.len() > 8192 {
            return Err(Error::new("CURSOR_EXPIRED", 410));
        }
        let invalid = || Error::new("CURSOR_EXPIRED", 410);
        let (payload, signature) = cursor.split_once('.').ok_or_else(invalid)?;
        let bytes = URL_SAFE_NO_PAD.decode(payload).map_err(|_| invalid())?;
        let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| invalid())?;
        let mut mac = Hmac::<Sha256>::new_from_slice(self.identity.cursor_key.as_bytes())
            .map_err(|_| Error::storage())?;
        mac.update(&bytes);
        mac.verify_slice(&signature).map_err(|_| invalid())?;
        let v: Value = serde_json::from_slice(&bytes)?;
        if v["epoch"] != self.epoch()
            || v["namespace"] != self.identity.namespace_id
            || v["project"] != project
            || v["kind"] != kind
            || v["expires"].as_u64().unwrap_or(0) <= self.time()
        {
            return Err(invalid());
        }
        Ok(v)
    }
    pub fn projects(&self, state: &str) -> Result<Value> {
        self.with_read(|db| {
            filter(state)?;
            let projects: Vec<_> = m::list::<Project>(db, "", "project")?
                .into_iter()
                .map(|(_, p)| p)
                .filter(|p| state == "all" || p.state == state)
                .collect();
            Ok(json!({"projects":projects}))
        })
    }
    pub fn catalog(
        &self,
        project: &str,
        cursor: Option<&str>,
        state: &str,
        limit: usize,
    ) -> Result<Value> {
        self.with_read(|db| {
            filter(state)?;
            let limit = limit.clamp(1, 1000);
            let p = self.project(db, project, false)?;
            let token = cursor
                .map(|c| self.decode_cursor(c, project, "catalog"))
                .transpose()?;
            if token.as_ref().is_some_and(|t| t["state"] != state) {
                return Err(Error::new("CURSOR_EXPIRED", 410));
            }
            let upper = token
                .as_ref()
                .and_then(|t| t["upper"].as_u64())
                .unwrap_or(p.sequence);
            let last = token
                .as_ref()
                .and_then(|t| t["last"].as_str())
                .unwrap_or("");
            self.check_change_floor(db, project, upper)?;
            let expires = token.as_ref().and_then(|t| t["expires"].as_u64())
                .unwrap_or(self.time()+self.config.read_pin_seconds);
            let mut items = m::catalog(db, project, upper, last, state, limit + 1)?;
            let more = items.len() > limit;
            items.truncate(limit);
            let last = items.last().map(|d| d.dataset_id.as_str()).unwrap_or(last);
            let receipt = self.encode_cursor_until(project, "catalog", upper, last, state, expires)?;
            Ok(
                json!({"datasets":items,"catalogRevision":upper.to_string(),"cursor":receipt,
            "nextCursor":if more {Some(&receipt)} else {None},
            "changesCursor":self.encode_cursor_until(project,"changes",upper,&upper.to_string(),"all",expires)?}),
            )
        })
    }
    pub fn changes(&self, project: &str, cursor: Option<&str>, limit: usize) -> Result<Value> {
        self.with_read(|db| {
            let p = self.project(db, project, false)?;
            let token = cursor
                .map(|c| self.decode_cursor(c, project, "changes"))
                .transpose()?;
            let (after, upper) = change_window(token.as_ref(), p.sequence)?;
            self.check_change_floor(db, project, after)?;
            let limit = limit.clamp(1, 1000);
            let low = format!("{after:020}");
            let high = format!("{upper:020}");
            let mut events = m::range::<Value>(db, project, "change", &low, &high, limit + 1)?;
            let more = events.len() > limit;
            events.truncate(limit);
            let last = if more {
                text(&events.last().unwrap().1, "sequence")?.to_owned()
            } else {
                upper.to_string()
            };
            let expires = if more {
                token.as_ref().and_then(|t| t["expires"].as_u64())
            } else {
                None
            }
            .unwrap_or(self.time() + self.config.read_pin_seconds);
            let values: Vec<_> = events.into_iter().map(|(_, v)| v).collect();
            Ok(
                json!({"changes":values,"upper":upper.to_string(),"hasMore":more,
            "cursor":self.encode_cursor_until(project,"changes",upper,&last,"all",expires)?}),
            )
        })
    }
    pub fn head(&self, project: &str, id: &str) -> Result<Value> {
        self.with_read(|db| {
        policy::id(id)?;
        let p = self.project(db, project, false)?;
        let dataset: Dataset = m::require(db, project, "dataset", id)?;
        Ok(
            json!({"generation":dataset.head.generation,"manifestHash":dataset.head.manifest_hash,
            "state":dataset.state,"projectLifecycleRevision":p.lifecycle_revision}),
        )
            })
    }
    pub(super) fn version_availability(
        &self,
        db: &Connection,
        project: &str,
        d: &Dataset,
        v: &Version,
    ) -> Result<VersionAvailability> {
        let p = self.project(db, project, false)?;
        let current = d.state == "active"
            && v.head == d.head
            && (p.state == "active" || p.recoverable_until.unwrap_or(0) > self.time());
        if !current && v.retain_until <= self.time() {
            return Ok(VersionAvailability::Expired);
        }
        match self.closure_ready(db, project, &v.head.manifest_hash) {
            Ok(()) => Ok(VersionAvailability::Available),
            Err(e) if e.code == "OBJECT_CORRUPT" => Ok(VersionAvailability::Corrupt),
            Err(e) if e.code == "OBJECT_MISSING" => {
                Err(Error::new("REFERENCE_INTEGRITY_FAILED", 503))
            }
            Err(e) => Err(e),
        }
    }
    pub(super) fn version_available(
        &self,
        db: &Connection,
        project: &str,
        d: &Dataset,
        v: &Version,
    ) -> Result<bool> {
        match self.version_availability(db, project, d, v)? {
            VersionAvailability::Available => Ok(true),
            VersionAvailability::Expired => Ok(false),
            VersionAvailability::Corrupt => Err(Error::new("OBJECT_CORRUPT", 503)),
        }
    }
    pub fn versions(
        &self,
        project: &str,
        id: &str,
        generation: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Value> {
        self.with_read(|db| {
            policy::id(id)?;
            self.project(db, project, false)?;
            let d: Dataset = m::require(db, project, "dataset", id)?;
            if let Some(g) = generation {
                let v: Version = m::require(db, project, "version", &version_key(id, g)?)?;
                return self.version_value(db, project, &d, &v);
            }
            self.version_page(db, project, &d, cursor, limit.clamp(1, 1000))
        })
    }
    fn version_page(
        &self,
        db: &Connection,
        project: &str,
        d: &Dataset,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Value> {
        let kind = format!("versions/{}", d.dataset_id);
        let token = cursor
            .map(|c| self.decode_cursor(c, project, &kind))
            .transpose()?;
        let upper = token
            .as_ref()
            .and_then(|t| t["upper"].as_u64())
            .unwrap_or(policy::number(&d.head.generation)?);
        let initial = format!("{}/", d.dataset_id);
        let last = token
            .as_ref()
            .and_then(|t| t["last"].as_str())
            .unwrap_or(&initial);
        let high = version_key(&d.dataset_id, &upper.to_string())?;
        let mut rows = m::range::<Version>(db, project, "version", last, &high, limit + 1)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let result: Vec<_> = rows
            .iter()
            .map(|(_, v)| self.version_value(db, project, d, v))
            .collect::<Result<_>>()?;
        let last = rows.last().map(|(key, _)| key.as_str()).unwrap_or(last);
        Ok(
            json!({"versions":result,"nextCursor":if more {Some(self.encode_cursor(project,&kind,upper,last,"all")?)} else {None}}),
        )
    }
    fn version_value(
        &self,
        db: &Connection,
        project: &str,
        d: &Dataset,
        v: &Version,
    ) -> Result<Value> {
        let mut value = serde_json::to_value(v)?;
        value["generation"] = json!(v.head.generation);
        value["manifestHash"] = json!(v.head.manifest_hash);
        value["contentStatus"] = json!(self.version_availability(db, project, d, v)?.label());
        Ok(value)
    }
    fn validate_ack(
        &self,
        db: &Connection,
        project: &str,
        replica: &str,
        cursor: &Value,
        revision: u64,
    ) -> Result<()> {
        if let Some(prior) = m::get::<Value>(db, project, "ack", replica)? {
            let before = policy::number(text(&prior["cursor"], "last")?)?;
            let before_revision = policy::number(text(&prior, "scopeRevision")?)?;
            if revision < before_revision
                || (revision == before_revision && policy::number(text(cursor, "last")?)? < before)
            {
                return Err(Error::new("ACK_REGRESSION", 409));
            }
        } else {
            self.quota(db, 0)?;
        }
        Ok(())
    }
    pub fn ack(&self, project: &str, replica: &str, body: &Value) -> Result<Value> {
        policy::id(replica)?;
        let cursor = self.decode_cursor(text(body, "cursor")?, project, "changes")?;
        let revision = policy::number(text(body, "scopeRevision")?)?;
        self.with_write(|db| {
            let tx = self.transaction(db)?;
            self.check_change_floor(&tx, project, policy::number(text(&cursor, "last")?)?)?;
            let mut device: Replica = m::require(&tx, self.epoch(), "replica", replica)?;
            if !policy::replica_active(&device, self.time(), self.config.replica_expiry_seconds) {
                return Err(Error::new("REPLICA_EXPIRED", 410));
            }
            self.validate_ack(&tx, project, replica, &cursor, revision)?;
            m::put(
                &tx,
                project,
                "ack",
                replica,
                &json!({"cursor":cursor,"scopeRevision":revision.to_string()}),
            )?;
            device.last_seen = self.time();
            m::put(&tx, self.epoch(), "replica", replica, &device)?;
            self.commit(tx)?;
            Ok(json!({"acknowledged":true}))
        })
    }
}
fn filter(state: &str) -> Result<()> {
    if ["active", "deleted", "all"].contains(&state) {
        Ok(())
    } else {
        Err(Error::new("INVALID_STATE", 400))
    }
}

fn change_window(token: Option<&Value>, latest: u64) -> Result<(u64, u64)> {
    let after = token
        .and_then(|t| t["last"].as_str())
        .map(policy::number)
        .transpose()?
        .unwrap_or(0);
    let prior = token.and_then(|t| t["upper"].as_u64()).unwrap_or(0);
    Ok((after, if after < prior { prior } else { latest }))
}
