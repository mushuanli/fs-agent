//! Reuse the sync command/receipt boundary for directory-to-dataset publication.
use super::{
    catalog, files,
    model::{Binding, Content},
    sync_error,
};
use crate::{app::State, core::error::Error, fs::Export};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
};

pub(super) fn prepare(state: &State, export: &Export, binding: &mut Binding) -> Result<(), Error> {
    let plan = binding.plan.as_ref().ok_or_else(Error::internal)?;
    if plan.publish_command.is_some() || !plan.actions.iter().any(|a| a.side == "upload") {
        return Ok(());
    }
    let sync = state.sync.as_ref().ok_or_else(Error::unsupported)?;
    let hash = upload_manifest(state, export, binding)?;
    if binding.replica_id.is_empty() {
        binding.replica_id = format!("directory-{}", super::random_id()?);
    }
    activate(sync, binding)?;
    let command = command(sync, binding, hash)?;
    binding.next_seq = binding
        .next_seq
        .checked_add(1)
        .ok_or_else(Error::internal)?;
    binding.plan.as_mut().unwrap().publish_command = Some(command);
    binding.plan.as_mut().unwrap().state = "applying".into();
    catalog(state)?.save_binding(binding.clone())
}
fn upload_manifest(state: &State, export: &Export, binding: &Binding) -> Result<String, Error> {
    let entries = manifest(state, export, binding)?;
    let bytes =
        serde_json::to_vec(&json!({"format":"fs-agent.files","version":1,"entries":entries}))
            .map_err(|_| Error::internal())?;
    upload(
        state.sync.as_ref().ok_or_else(Error::unsupported)?,
        &binding.sync_project_id,
        &bytes,
    )
}
fn command(
    sync: &crate::sync::SyncService,
    binding: &Binding,
    hash: String,
) -> Result<Value, Error> {
    let plan = binding.plan.as_ref().ok_or_else(Error::internal)?;
    let head = sync
        .head(&binding.sync_project_id, &binding.dataset_id)
        .map_err(sync_error)?;
    if head["manifestHash"] != plan.manifest_hash || head["generation"] != plan.generation {
        return Err(Error::conflict("SYNC_SOURCE_CHANGED"));
    }
    Ok(
        json!({"authorityId":binding.authority_id,"historyEpoch":binding.history_epoch,
        "replicaId":binding.replica_id,"operationId":plan.id,"opSeq":binding.next_seq.to_string(),
        "expectedProjectLifecycleRevision":head["projectLifecycleRevision"],
        "expectedHead":{"manifestHash":plan.manifest_hash,"generation":plan.generation},"nextManifestHash":hash}),
    )
}
fn activate(sync: &crate::sync::SyncService, binding: &Binding) -> Result<(), Error> {
    let replica = sync
        .register(&json!({"replicaId":binding.replica_id}))
        .map_err(sync_error)?;
    if replica["state"] == "expired" {
        return Err(Error::conflict("REPLICA_EXPIRED"));
    }
    if replica["state"] == "reconciling" {
        let mut cursor: Option<String> = None;
        loop {
            let page = sync
                .catalog(&binding.sync_project_id, cursor.as_deref(), "all", 1000)
                .map_err(sync_error)?;
            if page["nextCursor"].is_null() {
                sync.activate(&binding.replica_id, &json!({"scopes":[{"projectId":binding.sync_project_id,"cursor":page["cursor"]}]})).map_err(sync_error)?;
                break;
            }
            cursor = page["nextCursor"].as_str().map(str::to_owned);
        }
    }
    Ok(())
}
pub(super) fn execute(state: &State, binding: &mut Binding) -> Result<(), Error> {
    let Some(command) = binding
        .plan
        .as_ref()
        .and_then(|p| p.publish_command.clone())
    else {
        return Ok(());
    };
    let sync = state.sync.as_ref().ok_or_else(Error::unsupported)?;
    let receipt = receipt(sync, binding, &command)?;
    match receipt["outcome"].as_str() {
        Some("committed") => Ok(()),
        Some("not-committed") => {
            binding.plan.as_mut().unwrap().state = "rejected".into();
            catalog(state)?.save_binding(binding.clone())?;
            Err(Error::conflict("SYNC_SOURCE_CHANGED"))
        }
        _ => Err(Error::conflict("SYNC_PUBLISH_PENDING")),
    }
}
fn receipt(
    sync: &crate::sync::SyncService,
    binding: &Binding,
    command: &Value,
) -> Result<Value, Error> {
    let target = format!(
        "projects/{}/datasets/{}/publish",
        binding.sync_project_id, binding.dataset_id
    );
    // Query first after a restart: a newer head never hides the original receipt.
    match sync.operation(
        &binding.replica_id,
        command["opSeq"].as_str().ok_or_else(Error::internal)?,
    ) {
        Ok(value) => Ok(value),
        Err(error) if error.code == "NOT_FOUND" => {
            sync.command(&target, command, false).map_err(sync_error)
        }
        Err(error) => Err(sync_error(error)),
    }
}
fn manifest(state: &State, export: &Export, binding: &Binding) -> Result<Vec<Value>, Error> {
    let plan = binding.plan.as_ref().ok_or_else(Error::internal)?;
    let mut merged: BTreeMap<String, Content> = plan.dataset.clone();
    for action in plan.actions.iter().filter(|a| a.side == "upload") {
        let path = files::join(&binding.source, &action.path);
        if files::capture(export, &path)? != plan.directory.get(&action.path).cloned() {
            return Err(Error::conflict("TARGET_CHANGED"));
        }
        if action.after.kind == "file" {
            upload_input(state, export, binding, &path, &action.after)?;
        }
        merged.insert(action.path.clone(), action.after.clone());
    }
    merged
        .into_iter()
        .map(|(path, content)| manifest_entry(state, binding, path, content))
        .collect()
}
fn upload_input(
    state: &State,
    export: &Export,
    binding: &Binding,
    path: &str,
    after: &Content,
) -> Result<(), Error> {
    let mut bytes = vec![];
    export
        .read(path)?
        .0
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if Some(format!("{:x}", Sha256::digest(&bytes))) != after.hash {
        return Err(Error::conflict("TARGET_CHANGED"));
    }
    upload(
        state.sync.as_ref().unwrap(),
        &binding.sync_project_id,
        &bytes,
    )?;
    Ok(())
}
fn manifest_entry(
    state: &State,
    binding: &Binding,
    path: String,
    content: Content,
) -> Result<Value, Error> {
    if content.kind == "directory" {
        return Ok(json!({"kind":"directory","path":path}));
    }
    let hash = content.hash.ok_or_else(Error::invalid)?;
    let (_, size) = state
        .sync
        .as_ref()
        .unwrap()
        .object_file(&binding.sync_project_id, &hash)
        .map_err(sync_error)?;
    Ok(
        json!({"kind":"file","path":path,"hash":hash,"size":size.to_string(),"executable":content.executable}),
    )
}
fn upload(sync: &crate::sync::SyncService, project: &str, bytes: &[u8]) -> Result<String, Error> {
    let hash = format!("{:x}", Sha256::digest(bytes));
    let id = sync
        .reserve(project, &hash, bytes.len() as u64)
        .map_err(sync_error)?;
    let mut file = sync.temporary_file(&id).map_err(sync_error)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    sync.install(&id).map_err(sync_error)?;
    Ok(hash)
}
