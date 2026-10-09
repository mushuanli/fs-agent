use super::service::{authorize, request, ProjectService};
use crate::{app::State, core::error::Error, process};
use serde_json::{json, Value};
use std::sync::Arc;
pub async fn call(
    state: &Arc<State>,
    identity: usize,
    name: &str,
    args: Value,
) -> Result<Value, Error> {
    if name.starts_with("project_sync_") {
        return super::directory_sync::call(state, identity, name, args).await;
    }
    let service = state.projects.as_ref().ok_or_else(Error::unsupported)?;
    match name {
        "project_roots" => Ok(
            json!({"roots":state.auth.client(identity).exports().iter().map(|alias|json!({"alias":alias,"access":if state.auth.client(identity).may_write(alias){"rw"}else{"ro"}})).collect::<Vec<_>>(),"execution":state.execution.ready(),"backend":"bubblewrap"}),
        ),
        "project_list" => Ok(json!({"projects":service.list(identity)?})),
        "project_read" => Ok(json!({"project":service.get(identity,text(&args,"projectId")?)?})),
        "project_register" => {
            let _gate = state.files.exclusive()?;
            let input = serde_json::from_value(args).map_err(|_| Error::invalid())?;
            Ok(json!({"project":service.register(state,identity,input)?}))
        }
        "project_configure" => configure(state, identity, service, args).await,
        "project_forget" => {
            let _gate = state.files.exclusive()?;
            let id = text(&args, "projectId")?;
            service.forget(
                identity,
                id,
                args["revision"].as_u64().ok_or_else(Error::invalid)?,
            )?;
            state.harness.close_project(id).await;
            service
                .watches
                .lock()
                .map_err(|_| Error::internal())?
                .remove_project(id);
            Ok(json!({"forgotten":id}))
        }
        "project_exec" => execute(state, identity, service, args),
        "project_watch" | "project_unwatch" => {
            super::watch::call(state, identity, name, args).await
        }
        "project_search" => super::search::search(state, identity, args).await,
        _ => Err(Error::unsupported()),
    }
}
fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str, Error> {
    args[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(Error::invalid)
}

async fn configure(
    state: &Arc<State>,
    identity: usize,
    service: &ProjectService,
    args: Value,
) -> Result<Value, Error> {
    let _gate = state.files.exclusive()?;
    let id = text(&args, "projectId")?;
    let revision = args["revision"].as_u64().ok_or_else(Error::invalid)?;
    let current = service.get(identity, id)?;
    authorize(state, identity, &current)?;
    if current.revision != revision {
        return Err(Error::conflict("PROJECT_REVISION_CHANGED"));
    }
    let mounts = serde_json::from_value(args["mounts"].clone()).map_err(|_| Error::invalid())?;
    state.harness.close_project(id).await;
    let project = service.configure(
        state,
        identity,
        id,
        revision,
        text(&args, "name")?.into(),
        mounts,
    )?;
    service
        .watches
        .lock()
        .map_err(|_| Error::internal())?
        .remove_project(id);
    Ok(json!({"project":project}))
}

fn execute(
    state: &Arc<State>,
    identity: usize,
    service: &ProjectService,
    args: Value,
) -> Result<Value, Error> {
    let project = service.get(identity, text(&args, "projectId")?)?;
    authorize(state, identity, &project)?;
    if args["revision"].as_u64() != Some(project.revision) {
        return Err(Error::conflict("PROJECT_REVISION_CHANGED"));
    }
    let mut request = request(
        state,
        &project,
        text(&args, "command")?.into(),
        serde_json::from_value(args["args"].clone()).map_err(|_| Error::invalid())?,
        args["timeoutMs"].as_u64().unwrap_or(30_000),
    );
    request.project_id = Some(project.id.clone());
    request.project_revision = Some(project.revision);
    request.read_only = args["readOnly"].as_bool().unwrap_or(false);
    if request.read_only {
        for mount in &mut request.mounts {
            mount.access = "ro".into();
        }
    }
    request.request_id = text(&args, "requestId")?.into();
    request.epoch = text(&args, "epoch")?.into();
    request.cwd = args["cwd"].as_str().unwrap_or("/workspace").into();
    if request.cwd != "/workspace" && !request.cwd.starts_with("/workspace/") {
        return Err(Error::forbidden("EACCES"));
    }
    Ok(json!({"process":process::start(state,identity,request)?}))
}
