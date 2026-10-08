use super::{files, model::Binding};
use crate::{
    app::State,
    core::error::Error,
    fs::Export,
    projects::{model::Project, service},
};
pub(super) fn project(
    state: &State,
    identity: usize,
    id: &str,
    revision: u64,
) -> Result<Project, Error> {
    let project = state
        .projects
        .as_ref()
        .ok_or_else(Error::unsupported)?
        .get(identity, id)?;
    service::authorize(state, identity, &project)?;
    if project.revision != revision {
        return Err(Error::conflict("PROJECT_REVISION_CHANGED"));
    }
    if project.access != "rw" {
        return Err(Error::forbidden("EROFS"));
    }
    Ok(project)
}
pub(super) fn authorize<'a>(
    state: &'a State,
    binding: &Binding,
) -> Result<(Project, &'a Export), Error> {
    let project = project(state, binding.owner, &binding.project_id, binding.revision)?;
    if project.alias != binding.alias
        || files::join(&project.path, &binding.target) != binding.source
    {
        return Err(Error::conflict("BINDING_CHANGED"));
    }
    let export = state
        .exports
        .get(&project.alias)
        .ok_or_else(Error::invalid)?;
    if service::identity_of(&export.open_dir(&binding.source)?)? != binding.target_identity {
        return Err(Error::conflict("TARGET_REPLACED"));
    }
    allowed(&project, &binding.target)?;
    let sync = state.sync.as_ref().ok_or_else(Error::unsupported)?;
    let caps = sync.capabilities();
    if binding.history_epoch != sync.epoch()
        || caps["authorityId"] != binding.authority_id
        || caps["namespaceId"] != binding.namespace_id
    {
        return Err(Error::conflict("SYNC_IDENTITY_CHANGED"));
    }
    Ok((project, export))
}
pub(super) fn allowed(project: &Project, path: &str) -> Result<(), Error> {
    crate::fs::path::validate(path)?;
    if path.split('/').any(|p| p == ".mindos") {
        return Err(Error::forbidden("SYNC_CONTROL_PATH"));
    }
    for mount in &project.mounts {
        let at = mount
            .at
            .strip_prefix("/workspace/")
            .ok_or_else(Error::invalid)?;
        if path == at || path.starts_with(&(at.to_owned() + "/")) {
            return Err(Error::forbidden("SYNC_MOUNT_PATH"));
        }
    }
    Ok(())
}
pub(super) fn overlaps(a: &str, b: &str) -> bool {
    a == b
        || a.is_empty()
        || b.is_empty()
        || a.starts_with(&(b.to_owned() + "/"))
        || b.starts_with(&(a.to_owned() + "/"))
}
