//! Durable dataset/directory bindings and resumable, explicitly reviewed plans.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Content {
    pub kind: String,
    pub hash: Option<String>,
    pub executable: bool,
    pub identity: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Action {
    pub path: String,
    pub before: Option<Content>,
    pub after: Content,
    #[serde(default = "download")]
    pub side: String,
}
pub fn download() -> String {
    "download".into()
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Conflict {
    pub path: String,
    pub code: String,
    pub baseline: Option<Content>,
    pub dataset: Option<Content>,
    pub directory: Option<Content>,
    pub resolvable: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Plan {
    pub id: String,
    pub manifest_hash: String,
    pub generation: String,
    pub state: String,
    pub actions: Vec<Action>,
    pub conflicts: Vec<String>,
    pub accepted: BTreeMap<String, Content>,
    #[serde(default = "download")]
    pub direction: String,
    #[serde(default)]
    pub conflict_details: Vec<Conflict>,
    #[serde(default)]
    pub dataset: BTreeMap<String, Content>,
    #[serde(default)]
    pub directory: BTreeMap<String, Content>,
    #[serde(default)]
    pub publish_command: Option<serde_json::Value>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Binding {
    pub id: String,
    pub owner: usize,
    pub project_id: String,
    pub revision: u64,
    pub sync_project_id: String,
    pub dataset_id: String,
    pub history_epoch: String,
    pub authority_id: String,
    pub namespace_id: String,
    pub target: String,
    pub alias: String,
    pub source: String,
    pub target_identity: String,
    pub baseline: BTreeMap<String, Content>,
    pub plan: Option<Plan>,
    #[serde(default = "download")]
    pub direction: String,
    #[serde(default)]
    pub policy_revision: u64,
    #[serde(default)]
    pub replica_id: String,
    #[serde(default = "first_sequence")]
    pub next_seq: u64,
}
fn first_sequence() -> u64 {
    1
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Bind {
    pub binding_id: String,
    pub project_id: String,
    pub revision: u64,
    pub sync_project_id: String,
    pub dataset_id: String,
    pub history_epoch: String,
    #[serde(default)]
    pub target: String,
    #[serde(default = "download")]
    pub direction: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub entries: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Entry {
    Directory {
        path: String,
    },
    File {
        path: String,
        hash: String,
        size: String,
        #[serde(default)]
        executable: Option<bool>,
    },
}
impl Entry {
    pub fn supported(&self) -> bool {
        match self {
            Self::Directory { .. } => true,
            Self::File { size, .. } => size.parse::<u64>().is_ok_and(|n| n <= 32 * 1024 * 1024),
        }
    }
    pub fn content(self) -> (String, Content) {
        let (path, kind, hash, executable) = match self {
            Self::Directory { path } => (path, "directory", None, false),
            Self::File {
                path,
                hash,
                executable,
                ..
            } => (path, "file", Some(hash), executable.unwrap_or(false)),
        };
        (
            path,
            Content {
                kind: kind.into(),
                hash,
                executable,
                identity: None,
            },
        )
    }
}
impl Plan {
    pub fn new(hash: String, generation: String) -> Result<Self, crate::core::error::Error> {
        Ok(Self {
            id: super::random_id()?,
            manifest_hash: hash,
            generation,
            state: "ready".into(),
            actions: vec![],
            conflicts: vec![],
            accepted: BTreeMap::new(),
            direction: download(),
            conflict_details: vec![],
            dataset: BTreeMap::new(),
            directory: BTreeMap::new(),
            publish_command: None,
        })
    }
}

impl Binding {
    pub fn new(
        input: Bind,
        project: &crate::projects::model::Project,
        target_identity: String,
        caps: serde_json::Value,
    ) -> Result<Self, crate::core::error::Error> {
        Ok(Self {
            id: input.binding_id.clone(),
            owner: project.owner,
            project_id: project.id.clone(),
            revision: project.revision,
            sync_project_id: input.sync_project_id,
            dataset_id: input.dataset_id,
            history_epoch: input.history_epoch.clone(),
            authority_id: cap(&caps, "authorityId")?,
            namespace_id: cap(&caps, "namespaceId")?,
            target: input.target.clone(),
            source: super::files::join(&project.path, &input.target),
            alias: project.alias.clone(),
            target_identity,
            baseline: Default::default(),
            plan: None,
            direction: input.direction,
            policy_revision: 1,
            replica_id: format!("directory-{}", super::random_id()?),
            next_seq: 1,
        })
    }
}
fn cap(caps: &serde_json::Value, key: &str) -> Result<String, crate::core::error::Error> {
    caps[key]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(crate::core::error::Error::internal)
}
