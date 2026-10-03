use super::{
    model::*,
    operations::{text, version_key},
    policy,
    service::SyncService,
    store::metadata as m,
};
use rusqlite::Connection;
use serde_json::{json, Value};

impl SyncService {
    pub(super) fn apply(&self, db: &Connection, target: &str, body: &Value) -> Result<Value> {
        let parts: Vec<_> = target.split('/').collect();
        match parts.as_slice() {
            ["projects"] => self.create_project(db, body),
            ["projects", project, action] if *action == "delete" || *action == "restore" => {
                self.project_lifecycle(db, project, action, body)
            }
            ["projects", project, "datasets"] => self.create_dataset(db, project, body),
            ["projects", project, "datasets", dataset, action] => {
                self.dataset_command(db, project, dataset, action, body)
            }
            _ => Err(Error::new("INVALID_COMMAND", 400)),
        }
    }
    fn create_project(&self, db: &Connection, body: &Value) -> Result<Value> {
        let id = text(body, "projectId")?;
        policy::id(id)?;
        if m::get::<Project>(db, "", "project", id)?.is_some() {
            return Err(Error::new("PROJECT_EXISTS", 409));
        }
        if m::count(db, "", "project")? >= self.config.max_projects {
            return Err(Error::new("LIMIT_EXCEEDED", 429));
        }
        let project = Project {
            project_id: id.into(),
            state: "active".into(),
            metadata_revision: "1".into(),
            lifecycle_revision: "1".into(),
            deleted_at: None,
            recoverable_until: None,
            sequence: 0,
            change_floor: 0,
        };
        m::put(db, "", "project", id, &project)?;
        Ok(serde_json::to_value(project)?)
    }
    pub(super) fn lifecycle(
        &self,
        db: &Connection,
        project: &str,
        body: &Value,
        active: bool,
    ) -> Result<Project> {
        let p = self.project(db, project, active)?;
        if text(body, "expectedProjectLifecycleRevision")? != p.lifecycle_revision {
            return Err(Error::new("PROJECT_LIFECYCLE_CHANGED", 412));
        }
        Ok(p)
    }
    fn create_dataset(&self, db: &Connection, project: &str, body: &Value) -> Result<Value> {
        self.lifecycle(db, project, body, true)?;
        let dataset = new_dataset(body)?;
        self.validate_new_dataset(db, project, &dataset)?;
        self.manifest(db, project, &dataset.head.manifest_hash, &dataset.kind)?;
        self.record_version(db, project, &dataset, None)?;
        self.event(db, project, &dataset, "dataset-created", None, body)?;
        Ok(serde_json::to_value(dataset)?)
    }
    fn dataset_command(
        &self,
        db: &Connection,
        project: &str,
        id: &str,
        action: &str,
        body: &Value,
    ) -> Result<Value> {
        self.lifecycle(db, project, body, true)?;
        policy::id(id)?;
        let mut dataset: Dataset = m::require(db, project, "dataset", id)?;
        if action == "restore" {
            return self.restore_dataset(db, project, dataset, body);
        }
        validate_expected_head(&dataset, body)?;
        let prior = dataset.head.clone();
        match action {
            "publish" => return self.publish_dataset(db, project, dataset, body),
            "delete" => {
                dataset.state = "deleted".into();
                dataset.head.generation = policy::next(&prior.generation)?;
                dataset.recoverable_until = Some(self.time() + self.config.trash_retention_seconds);
                self.supersede(db, project, id, &prior, self.config.trash_retention_seconds)?;
            }
            _ => return Err(Error::new("INVALID_COMMAND", 400)),
        }
        self.event(db, project, &dataset, "dataset-deleted", Some(&prior), body)?;
        Ok(json!({"head":dataset.head,"state":dataset.state}))
    }
    pub(super) fn record_version(
        &self,
        db: &Connection,
        project: &str,
        dataset: &Dataset,
        prior: Option<&Head>,
    ) -> Result<()> {
        if let Some(prior) = prior {
            self.supersede(
                db,
                project,
                &dataset.dataset_id,
                prior,
                self.config.history_retention_seconds,
            )?;
        }
        let version = Version {
            head: dataset.head.clone(),
            committed_at: self.time(),
            superseded_at: None,
            retain_until: 0,
        };
        let key = version_key(&dataset.dataset_id, &dataset.head.generation)?;
        m::put(db, project, "version", &key, &version)?;
        self.protect_receipt(db, project, &dataset.head.manifest_hash)
    }
    fn supersede(
        &self,
        db: &Connection,
        project: &str,
        id: &str,
        head: &Head,
        ttl: u64,
    ) -> Result<()> {
        let key = version_key(id, &head.generation)?;
        let mut version: Version = m::require(db, project, "version", &key)?;
        let until = self.time() + ttl;
        version.superseded_at = Some(self.time());
        version.retain_until = version.retain_until.max(until);
        m::put(db, project, "version", &key, &version)?;
        self.protect(db, project, &head.manifest_hash, until)
    }
    fn restore_dataset(
        &self,
        db: &Connection,
        project: &str,
        mut dataset: Dataset,
        body: &Value,
    ) -> Result<Value> {
        let version = self.restore_source(db, project, &dataset, body)?;
        self.manifest(db, project, &version.head.manifest_hash, &dataset.kind)?;
        let prior = dataset.head.clone();
        dataset.state = "active".into();
        dataset.recoverable_until = None;
        dataset.head = Head {
            generation: policy::next(&prior.generation)?,
            manifest_hash: version.head.manifest_hash,
        };
        self.record_version(db, project, &dataset, None)?;
        self.event(
            db,
            project,
            &dataset,
            "dataset-restored",
            Some(&prior),
            body,
        )?;
        Ok(json!({"head":dataset.head,"state":dataset.state}))
    }
    fn project_lifecycle(
        &self,
        db: &Connection,
        id: &str,
        action: &str,
        body: &Value,
    ) -> Result<Value> {
        let mut project = self.lifecycle(db, id, body, false)?;
        validate_project_transition(&project, action, self.time())?;
        self.protect_project_members(db, id, action)?;
        project_transition(
            &mut project,
            action,
            self.time(),
            self.config.trash_retention_seconds,
        )?;
        m::put(db, "", "project", id, &project)?;
        let event = if action == "delete" {
            "project-deleted"
        } else {
            "project-restored"
        };
        self.project_event(db, id, event, body)?;
        Ok(serde_json::to_value(project)?)
    }
    fn validate_new_dataset(&self, db: &Connection, project: &str, d: &Dataset) -> Result<()> {
        let datasets = m::list::<Dataset>(db, project, "dataset")?;
        if datasets
            .iter()
            .any(|(_, item)| item.dataset_id == d.dataset_id || item.logical_id == d.logical_id)
        {
            return Err(Error::new("DATASET_EXISTS", 409));
        }
        if datasets.len() >= self.config.max_datasets {
            return Err(Error::new("LIMIT_EXCEEDED", 429));
        }
        Ok(())
    }
    fn publish_dataset(
        &self,
        db: &Connection,
        project: &str,
        mut dataset: Dataset,
        body: &Value,
    ) -> Result<Value> {
        let hash = text(body, "nextManifestHash")?;
        self.manifest(db, project, hash, &dataset.kind)?;
        let prior = dataset.head.clone();
        if hash == prior.manifest_hash {
            self.protect_receipt(db, project, hash)?;
            return Ok(json!({"head":prior,"noChange":true}));
        }
        dataset.head = Head {
            generation: policy::next(&prior.generation)?,
            manifest_hash: hash.into(),
        };
        self.record_version(db, project, &dataset, Some(&prior))?;
        self.event(
            db,
            project,
            &dataset,
            "dataset-published",
            Some(&prior),
            body,
        )?;
        Ok(json!({"head":dataset.head,"state":dataset.state}))
    }
    fn restore_source(
        &self,
        db: &Connection,
        project: &str,
        d: &Dataset,
        body: &Value,
    ) -> Result<Version> {
        if d.state != "deleted" || text(body, "expectedDeletedGeneration")? != d.head.generation {
            return Err(Error::new("HEAD_CONFLICT", 412));
        }
        if d.recoverable_until.unwrap_or(0) <= self.time() {
            return Err(Error::new("TRASH_EXPIRED", 410));
        }
        let key = version_key(&d.dataset_id, text(body, "sourceGeneration")?)?;
        let version: Version = m::require(db, project, "version", &key)?;
        if !self.version_available(db, project, d, &version)? {
            return Err(Error::new("VERSION_EXPIRED", 410));
        }
        Ok(version)
    }
    fn protect_project_members(&self, db: &Connection, id: &str, action: &str) -> Result<()> {
        for (_, dataset) in m::list::<Dataset>(db, id, "dataset")? {
            if dataset.state != "active" {
                continue;
            }
            if action == "delete" {
                self.protect(
                    db,
                    id,
                    &dataset.head.manifest_hash,
                    self.time() + self.config.trash_retention_seconds,
                )?;
            } else {
                self.manifest(db, id, &dataset.head.manifest_hash, &dataset.kind)?;
                self.protect_receipt(db, id, &dataset.head.manifest_hash)?;
            }
        }
        Ok(())
    }
    pub(super) fn protect_receipt(&self, db: &Connection, project: &str, hash: &str) -> Result<()> {
        self.protect(
            db,
            project,
            hash,
            self.time() + self.config.operation_retention_seconds,
        )
    }
}

fn new_dataset(body: &Value) -> Result<Dataset> {
    let id = text(body, "datasetId")?;
    let logical = text(body, "logicalId")?;
    policy::id(id)?;
    policy::id(logical)?;
    let kind = text(body, "kind")?;
    if !["files", "session", "organization", "definitions", "bundle"].contains(&kind) {
        return Err(Error::new("INVALID_KIND", 422));
    }
    Ok(Dataset {
        dataset_id: id.into(),
        logical_id: logical.into(),
        kind: kind.into(),
        state: "active".into(),
        head: Head {
            generation: "1".into(),
            manifest_hash: text(body, "manifestHash")?.into(),
        },
        recoverable_until: None,
    })
}
fn validate_expected_head(d: &Dataset, body: &Value) -> Result<()> {
    if d.state != "active" {
        return Err(Error::new("DATASET_DELETED", 409));
    }
    let expected: Head = serde_json::from_value(body["expectedHead"].clone())
        .map_err(|_| Error::new("INVALID_COMMAND", 400))?;
    if expected != d.head {
        return Err(Error::new("HEAD_CONFLICT", 412));
    }
    Ok(())
}
fn validate_project_transition(p: &Project, action: &str, time: u64) -> Result<()> {
    if (action == "delete") != (p.state == "active") {
        return Err(Error::new("PROJECT_STATE_CONFLICT", 409));
    }
    if action == "restore" && p.recoverable_until.unwrap_or(0) <= time {
        return Err(Error::new("TRASH_EXPIRED", 410));
    }
    Ok(())
}
fn project_transition(p: &mut Project, action: &str, time: u64, ttl: u64) -> Result<()> {
    let deleted = action == "delete";
    p.state = if deleted { "deleted" } else { "active" }.into();
    p.lifecycle_revision = policy::next(&p.lifecycle_revision)?;
    p.deleted_at = deleted.then_some(time);
    p.recoverable_until = deleted.then_some(time + ttl);
    Ok(())
}
