use super::{
    allowed, catalog, files,
    model::{Action, Binding},
    sync_error,
};
use crate::{
    app::State,
    core::error::Error,
    fs::{mutation::Change, upload, Export},
    projects::model::Project,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
pub fn execute(
    state: &State,
    project: &Project,
    export: &Export,
    binding: &mut Binding,
    id: &str,
) -> Result<(), Error> {
    if reviewed(binding, id)? {
        return Ok(());
    }
    let sync = state.sync.as_ref().ok_or_else(Error::unsupported)?;
    if binding.plan.as_ref().unwrap().state == "ready" {
        check_head(sync, binding)?;
    }
    preflight(project, export, binding)?;
    reserve_metadata(state, binding)?;
    super::publish::prepare(state, export, binding)?;
    let pin_id = pin(sync, binding)?;
    let result = apply_all(state, project, export, binding, &pin_id);
    let _ = sync.pin_action(&binding.sync_project_id, &pin_id, "release");
    result
}
fn reviewed(binding: &Binding, id: &str) -> Result<bool, Error> {
    let plan = binding
        .plan
        .as_ref()
        .filter(|p| p.id == id)
        .ok_or_else(|| Error::conflict("PLAN_CHANGED"))?;
    if plan.state == "complete" {
        return Ok(true);
    }
    if !["ready", "applying"].contains(&plan.state.as_str()) {
        return Err(Error::conflict("PLAN_CHANGED"));
    }
    if !plan.conflicts.is_empty() {
        return Err(Error::conflict("SYNC_CONFLICT"));
    }
    Ok(false)
}
fn reserve_metadata(state: &State, binding: &Binding) -> Result<(), Error> {
    let mut projected = binding.clone();
    let plan = projected.plan.as_ref().ok_or_else(Error::internal)?;
    for (path, content) in &plan.accepted {
        projected.baseline.insert(path.clone(), content.clone());
    }
    for action in &plan.actions {
        let mut content = action.after.clone();
        if content.kind == "directory" {
            content.identity = Some("0".repeat(128));
        }
        projected.baseline.insert(action.path.clone(), content);
    }
    projected.plan.as_mut().unwrap().state = "applying".into();
    catalog(state)?.check_binding(projected)
}
pub(super) fn check_head(sync: &crate::sync::SyncService, binding: &Binding) -> Result<(), Error> {
    let plan = binding.plan.as_ref().ok_or_else(Error::internal)?;
    let head = sync
        .directory_head(&binding.sync_project_id, &binding.dataset_id)
        .map_err(sync_error)?;
    if plan.state == "ready"
        && (head["manifestHash"] != plan.manifest_hash || head["generation"] != plan.generation)
    {
        return Err(Error::conflict("SYNC_SOURCE_CHANGED"));
    }
    Ok(())
}
fn pin(sync: &crate::sync::SyncService, binding: &Binding) -> Result<String, Error> {
    let plan = binding.plan.as_ref().ok_or_else(Error::internal)?;
    let pin = sync
        .pin(
            &binding.sync_project_id,
            &json!({"manifestHash":plan.manifest_hash,"requestKey":super::random_id()?}),
        )
        .map_err(sync_error)?;
    pin["pinId"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(Error::internal)
}
fn preflight(project: &Project, export: &Export, binding: &Binding) -> Result<(), Error> {
    let plan = binding.plan.as_ref().ok_or_else(Error::internal)?;
    for action in &plan.actions {
        preflight_action(project, export, binding, action)?;
    }
    if plan.state == "ready" && plan.actions.iter().any(|a| a.side == "upload") {
        let mut conflicts = vec![];
        if super::plan::scan(project, export, binding, &mut conflicts)? != plan.directory
            || !conflicts.is_empty()
        {
            return Err(Error::conflict("TARGET_CHANGED"));
        }
    }
    for (path, expected) in &plan.accepted {
        if files::capture(export, &files::join(&binding.source, path))?.as_ref() != Some(expected) {
            return Err(Error::conflict("TARGET_CHANGED"));
        }
    }
    Ok(())
}
fn preflight_action(
    project: &Project,
    export: &Export,
    binding: &Binding,
    action: &Action,
) -> Result<(), Error> {
    let plan = binding.plan.as_ref().ok_or_else(Error::internal)?;
    allowed(project, &files::join(&binding.target, &action.path))?;
    if action.side == "upload" {
        if plan.state == "ready"
            && files::capture(export, &files::join(&binding.source, &action.path))?
                != plan.directory.get(&action.path).cloned()
        {
            return Err(Error::conflict("TARGET_CHANGED"));
        }
        return Ok(());
    }
    let actual = files::capture(export, &files::join(&binding.source, &action.path))?;
    if actual != action.before
        && !(plan.state == "applying" && files::same(actual.as_ref(), &action.after))
    {
        return Err(Error::conflict("TARGET_CHANGED"));
    }
    Ok(())
}
fn apply_all(
    state: &State,
    project: &Project,
    export: &Export,
    binding: &mut Binding,
    pin: &str,
) -> Result<(), Error> {
    binding.plan.as_mut().ok_or_else(Error::internal)?.state = "applying".into();
    catalog(state)?.save_binding(binding.clone())?;
    super::publish::execute(state, binding)?;
    let actions = binding.plan.as_ref().unwrap().actions.clone();
    for action in actions {
        if action.side == "upload" {
            binding
                .baseline
                .insert(action.path.clone(), action.after.clone());
            catalog(state)?.save_binding(binding.clone())?;
        } else {
            checkpoint(state, project, export, binding, &action, pin)?;
        }
    }
    for (path, content) in binding.plan.as_ref().unwrap().accepted.clone() {
        binding.baseline.insert(path, content);
    }
    binding.plan.as_mut().unwrap().state = "complete".into();
    catalog(state)?.save_binding(binding.clone())
}
fn checkpoint(
    state: &State,
    project: &Project,
    export: &Export,
    binding: &mut Binding,
    action: &Action,
    pin: &str,
) -> Result<(), Error> {
    super::authorize(state, binding)?;
    allowed(project, &files::join(&binding.target, &action.path))?;
    state
        .sync
        .as_ref()
        .unwrap()
        .pin_action(&binding.sync_project_id, pin, "renew")
        .map_err(sync_error)?;
    apply_one(state, export, binding, action)?;
    let current = files::capture(export, &files::join(&binding.source, &action.path))?
        .ok_or_else(Error::internal)?;
    if !files::same(Some(&current), &action.after) {
        return Err(Error::conflict("TARGET_CHANGED"));
    }
    binding.baseline.insert(action.path.clone(), current);
    catalog(state)?.save_binding(binding.clone())?;
    Ok(())
}
fn apply_one(
    state: &State,
    export: &Export,
    binding: &Binding,
    action: &Action,
) -> Result<(), Error> {
    let path = files::join(&binding.source, &action.path);
    let current = files::capture(export, &path)?;
    if files::same(current.as_ref(), &action.after) {
        return sync_materialized(export, &path, &action.after.kind);
    }
    if current != action.before {
        return Err(Error::conflict("TARGET_CHANGED"));
    }
    if action.after.kind == "directory" {
        export.apply(&Change::Mkdir { path: path.clone() }, || {
            super::authorize(state, binding).map(|_| ())
        })?;
        return sync_materialized(export, &path, "directory");
    }
    write_file(state, export, binding, action, &path)
}
fn write_file(
    state: &State,
    export: &Export,
    binding: &Binding,
    action: &Action,
    path: &str,
) -> Result<(), Error> {
    let hash = action.after.hash.as_deref().ok_or_else(Error::invalid)?;
    let bytes = object(state, binding, hash)?;
    let (current, revision) = files::snapshot(export, path)?;
    if current != action.before {
        return Err(Error::conflict("TARGET_CHANGED"));
    }
    let upload = export.stage(path)?;
    let mut writer = upload.writer()?;
    writer.write_all(&bytes)?;
    writer.sync_all()?;
    upload::commit_executable(
        export,
        path,
        upload,
        revision.as_deref(),
        action.after.executable,
        || super::authorize(state, binding).map(|_| ()),
    )?;
    files::sync_parent(export, path)
}

fn object(state: &State, binding: &Binding, hash: &str) -> Result<Vec<u8>, Error> {
    let (mut source, size) = state
        .sync
        .as_ref()
        .unwrap()
        .object_file(&binding.sync_project_id, hash)
        .map_err(sync_error)?;
    if size > 32 * 1024 * 1024 {
        return Err(Error::too_large("SYNC_FILE_LIMIT"));
    }
    let mut bytes = vec![];
    Read::by_ref(&mut source)
        .take(size + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != size || format!("{:x}", Sha256::digest(&bytes)) != hash {
        return Err(Error::conflict("SYNC_OBJECT_CORRUPT"));
    }
    Ok(bytes)
}
fn sync_materialized(export: &Export, path: &str, kind: &str) -> Result<(), Error> {
    if kind == "directory" {
        export.open_dir(path)?.sync_all()?;
    } else {
        export.read(path)?.0.sync_all()?;
    }
    files::sync_parent(export, path)
}
