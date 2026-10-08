//! Materialization is a separate binding; sync dataset IDs never grant execution.
mod apply;
mod binding;
mod policy;
use binding::bind;
use policy::{allowed, authorize};
mod comparison;
mod files;
pub(super) mod model;
mod plan;
mod publish;
use crate::{app::State, core::error::Error};
use model::Binding;
use serde_json::{json, Value};
use std::sync::Arc;

pub async fn call(
    state: &Arc<State>,
    identity: usize,
    name: &str,
    args: Value,
) -> Result<Value, Error> {
    if identity != 0 {
        return Err(Error::forbidden("EACCES"));
    }
    let activity = state
        .sync
        .as_ref()
        .ok_or_else(Error::unsupported)?
        .activity()
        .map_err(sync_error)?;
    let gate = state.files.exclusive()?;
    let state = state.clone();
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || {
        let _activity = activity;
        let _gate = gate;
        dispatch(&state, identity, &name, args)
    })
    .await
    .map_err(|_| Error::internal())?
}
fn dispatch(state: &State, identity: usize, name: &str, args: Value) -> Result<Value, Error> {
    if name == "project_sync_directories" {
        return directories(state, identity, &args);
    }
    if name == "project_sync_bind" {
        return bind(
            state,
            identity,
            serde_json::from_value(args).map_err(|_| Error::invalid())?,
        );
    }
    let id = args["bindingId"].as_str().ok_or_else(Error::invalid)?;
    let mut binding = catalog(state)?
        .bindings()?
        .into_iter()
        .find(|b| b.id == id && b.owner == identity)
        .ok_or_else(|| Error::not_found("BINDING_UNKNOWN"))?;
    if name == "project_sync_status" {
        return Ok(json!({"binding":binding}));
    }
    if name == "project_sync_unbind" {
        if binding.plan.as_ref().is_some_and(|p| p.state == "applying") {
            return Err(Error::busy());
        }
        catalog(state)?.remove_binding(id)?;
        return Ok(json!({"removed":id}));
    }
    run_plan(state, name, args, &mut binding)
}
fn directories(state: &State, identity: usize, args: &Value) -> Result<Value, Error> {
    let id = args["projectId"].as_str().ok_or_else(Error::invalid)?;
    let revision = args["revision"].as_u64().ok_or_else(Error::invalid)?;
    let project = policy::project(state, identity, id, revision)?;
    let path = super::service::normalize(args["path"].as_str().ok_or_else(Error::invalid)?)?;
    allowed(&project, &path)?;
    let export = state
        .exports
        .get(&project.alias)
        .ok_or_else(Error::invalid)?;
    let listing = export.list(
        &files::join(&project.path, &path),
        &tokio_util::sync::CancellationToken::new(),
    )?;
    let paths: Vec<String> = listing
        .entries
        .into_iter()
        .filter(|e| e.stat.kind == "directory")
        .map(|e| files::join(&path, &e.name))
        .filter(|p| allowed(&project, p).is_ok())
        .collect();
    Ok(json!({"paths":paths}))
}
fn run_plan(state: &State, name: &str, args: Value, binding: &mut Binding) -> Result<Value, Error> {
    let (project, export) = authorize(state, binding)?;
    match name {
        "project_sync_configure" => configure(state, binding, &args),
        "project_sync_compare" => {
            let id = args["planId"].as_str().ok_or_else(Error::invalid)?;
            let path = args["path"].as_str().ok_or_else(Error::invalid)?;
            comparison::compare(state, export, binding, id, path)
        }
        "project_sync_resolve" => {
            let id = args["planId"].as_str().ok_or_else(Error::invalid)?;
            let decisions =
                serde_json::from_value(args["decisions"].clone()).map_err(|_| Error::invalid())?;
            plan::resolve(state, &project, export, binding, id, &decisions)?;
            Ok(json!({"plan":binding.plan}))
        }
        "project_sync_preview" => {
            plan::preview(state, &project, export, binding)?;
            Ok(json!({"plan":binding.plan}))
        }
        "project_sync_execute" => {
            let id = args["planId"].as_str().ok_or_else(Error::invalid)?;
            apply::execute(state, &project, export, binding, id)?;
            Ok(json!({"plan":binding.plan}))
        }
        _ => Err(Error::unsupported()),
    }
}
fn configure(state: &State, binding: &mut Binding, args: &Value) -> Result<Value, Error> {
    if binding.plan.as_ref().is_some_and(|p| p.state == "applying") {
        return Err(Error::busy());
    }
    let revision = args["policyRevision"].as_u64().ok_or_else(Error::invalid)?;
    if revision != binding.policy_revision {
        return Err(Error::conflict("BINDING_CHANGED"));
    }
    let direction = args["direction"].as_str().ok_or_else(Error::invalid)?;
    binding::valid_direction(direction)?;
    binding.direction = direction.into();
    binding.policy_revision = binding
        .policy_revision
        .checked_add(1)
        .ok_or_else(Error::internal)?;
    binding.plan = None;
    catalog(state)?.save_binding(binding.clone())?;
    Ok(json!({"binding":binding}))
}
fn catalog(state: &State) -> Result<&super::store::Store, Error> {
    Ok(&state
        .projects
        .as_ref()
        .ok_or_else(Error::unsupported)?
        .store)
}
fn sync_error(e: crate::sync::Error) -> Error {
    if e.code == "OPERATION_EXPIRED" {
        return Error::conflict("OPERATION_EXPIRED");
    }
    if e.code == "REPLICA_EXPIRED" {
        return Error::conflict("REPLICA_EXPIRED");
    }
    if e.status == 409 || e.status == 410 || e.status == 404 {
        Error::conflict("SYNC_SOURCE_CHANGED")
    } else {
        Error::internal()
    }
}
fn random_id() -> Result<String, Error> {
    let mut bytes = [0; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| Error::internal())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
