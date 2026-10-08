use super::{
    catalog, files,
    model::{Bind, Binding},
    sync_error,
};
use crate::{app::State, core::error::Error, projects::service};
use serde_json::{json, Value};
pub(super) fn bind(state: &State, identity: usize, input: Bind) -> Result<Value, Error> {
    valid_direction(&input.direction)?;
    valid_id(&input.binding_id)?;
    valid_id(&input.sync_project_id)?;
    valid_id(&input.dataset_id)?;
    let binding = create(state, identity, input)?;
    super::authorize(state, &binding)?;
    let sync = state.sync.as_ref().ok_or_else(Error::unsupported)?;
    sync.directory_head(&binding.sync_project_id, &binding.dataset_id)
        .map_err(sync_error)?;
    for prior in catalog(state)?.bindings()? {
        if prior.id == binding.id {
            if same_binding(&prior, &binding) {
                return Ok(json!({"binding":prior}));
            }
            return Err(Error::conflict("BINDING_CHANGED"));
        }
        if prior.alias == binding.alias && super::policy::overlaps(&prior.source, &binding.source) {
            return Err(Error::conflict("TARGET_ALREADY_BOUND"));
        }
    }
    catalog(state)?.save_binding(binding.clone())?;
    Ok(json!({"binding":binding}))
}
fn create(state: &State, identity: usize, mut input: Bind) -> Result<Binding, Error> {
    let project = super::policy::project(state, identity, &input.project_id, input.revision)?;
    input.target = service::normalize(&input.target)?;
    let source = files::join(&project.path, &input.target);
    let export = state
        .exports
        .get(&project.alias)
        .ok_or_else(Error::invalid)?;
    let sync = state.sync.as_ref().ok_or_else(Error::unsupported)?;
    if input.history_epoch != sync.epoch() {
        return Err(Error::conflict("HISTORY_EPOCH_CHANGED"));
    }
    Binding::new(
        input,
        &project,
        service::identity_of(&export.open_dir(&source)?)?,
        sync.capabilities(),
    )
}
fn same_binding(a: &Binding, b: &Binding) -> bool {
    a.owner == b.owner
        && a.project_id == b.project_id
        && a.revision == b.revision
        && a.sync_project_id == b.sync_project_id
        && a.dataset_id == b.dataset_id
        && a.history_epoch == b.history_epoch
        && a.target == b.target
        && a.target_identity == b.target_identity
        && a.direction == b.direction
}
pub(super) fn valid_direction(value: &str) -> Result<(), Error> {
    if ["both", "upload", "download"].contains(&value) {
        Ok(())
    } else {
        Err(Error::invalid())
    }
}
fn valid_id(id: &str) -> Result<(), Error> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Err(Error::invalid())
    } else {
        Ok(())
    }
}
