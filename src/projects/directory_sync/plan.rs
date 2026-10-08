use super::model::Content;
use super::{
    allowed, catalog, files,
    model::{Action, Binding, Conflict, Entry, Manifest, Plan},
    sync_error,
};
use crate::{app::State, core::error::Error, fs::Export, projects::model::Project};
use std::collections::{BTreeMap, BTreeSet};

pub fn preview(
    state: &State,
    project: &Project,
    export: &Export,
    binding: &mut Binding,
) -> Result<(), Error> {
    if binding.plan.as_ref().is_some_and(|p| p.state == "applying") {
        return Err(Error::conflict("SYNC_APPLY_PENDING"));
    }
    let (manifest, mut plan) = snapshot(state, binding)?;
    plan.direction = binding.direction.clone();
    for entry in manifest.entries {
        dataset_entry(project, binding, entry, &mut plan);
    }
    plan.directory = scan(project, export, binding, &mut plan.conflict_details)?;
    compare_entries(binding, &mut plan);
    finalize(&mut plan);
    binding.plan = Some(plan);
    catalog(state)?.save_binding(binding.clone())
}
fn compare_entries(binding: &Binding, plan: &mut Plan) {
    let paths: BTreeSet<String> = plan
        .dataset
        .keys()
        .chain(plan.directory.keys())
        .cloned()
        .collect();
    for path in paths {
        if !plan.conflict_details.iter().any(|c| c.path == path) {
            record(binding, &path, plan);
        }
    }
}
fn snapshot(state: &State, binding: &Binding) -> Result<(Manifest, Plan), Error> {
    let sync = state.sync.as_ref().ok_or_else(Error::unsupported)?;
    let head = sync
        .directory_head(&binding.sync_project_id, &binding.dataset_id)
        .map_err(sync_error)?;
    let hash = head["manifestHash"].as_str().ok_or_else(Error::internal)?;
    let bytes = sync
        .manifest_bytes(&binding.sync_project_id, hash)
        .map_err(sync_error)?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|_| Error::invalid())?;
    if manifest.format != "fs-agent.files"
        || manifest.version != 1
        || manifest.entries.len() > 10000
    {
        return Err(Error::unsupported());
    }
    let generation = head["generation"].as_str().ok_or_else(Error::internal)?;
    Ok((manifest, Plan::new(hash.into(), generation.into())?))
}
fn dataset_entry(project: &Project, binding: &Binding, entry: Entry, plan: &mut Plan) {
    let supported = entry.supported();
    let (path, content) = entry.content();
    let code = if !supported {
        Some("SYNC_FILE_LIMIT")
    } else {
        allowed(project, &files::join(&binding.target, &path))
            .err()
            .map(|e| e.code)
    };
    if let Some(code) = code {
        plan.conflict_details
            .push(conflict(binding, &path, code, Some(content), None, false));
    } else {
        plan.dataset.insert(path, content);
    }
}
pub(super) fn scan(
    project: &Project,
    export: &Export,
    binding: &Binding,
    conflicts: &mut Vec<Conflict>,
) -> Result<BTreeMap<String, Content>, Error> {
    let mut tree = TreeScan {
        found: BTreeMap::new(),
        queue: vec![String::new()],
        conflicts: vec![],
    };
    while let Some(parent) = tree.queue.pop() {
        scan_directory(project, export, binding, &parent, &mut tree)?;
        if tree.found.len() + tree.conflicts.len() + conflicts.len() > 10000 {
            return Err(Error::too_large("DIRECTORY_LIMIT"));
        }
    }
    conflicts.extend(tree.conflicts);
    Ok(tree.found)
}
struct TreeScan {
    found: BTreeMap<String, Content>,
    queue: Vec<String>,
    conflicts: Vec<Conflict>,
}
fn scan_directory(
    project: &Project,
    export: &Export,
    binding: &Binding,
    parent: &str,
    tree: &mut TreeScan,
) -> Result<(), Error> {
    let listing = export.list(
        &files::join(&binding.source, parent),
        &tokio_util::sync::CancellationToken::new(),
    )?;
    for code in listing.warnings {
        let path = if parent.is_empty() { "." } else { parent };
        tree.conflicts
            .push(conflict(binding, path, code, None, None, false));
    }
    for entry in listing.entries {
        let path = files::join(parent, &entry.name);
        scan_child(project, export, binding, &path, tree)?;
    }
    Ok(())
}
fn scan_child(
    project: &Project,
    export: &Export,
    binding: &Binding,
    path: &str,
    tree: &mut TreeScan,
) -> Result<(), Error> {
    if allowed(project, &files::join(&binding.target, path)).is_err() {
        return Ok(());
    }
    match files::capture(export, &files::join(&binding.source, path)) {
        Ok(Some(content)) => {
            if content.kind == "directory" {
                tree.queue.push(path.to_owned());
            }
            tree.found.insert(path.to_owned(), content);
        }
        Ok(None) => return Err(Error::conflict("TARGET_CHANGED")),
        Err(error) => tree
            .conflicts
            .push(conflict(binding, path, error.code, None, None, false)),
    }
    Ok(())
}
fn record(binding: &Binding, path: &str, plan: &mut Plan) {
    let dataset = plan.dataset.get(path).cloned();
    let directory = plan.directory.get(path).cloned();
    if equivalent(dataset.as_ref(), directory.as_ref()) {
        if let Some(content) = directory {
            plan.accepted.insert(path.into(), content);
        }
        return;
    }
    record_difference(binding, path, plan, dataset, directory);
}
fn record_difference(
    binding: &Binding,
    path: &str,
    plan: &mut Plan,
    dataset: Option<Content>,
    directory: Option<Content>,
) {
    if different_kind(dataset.as_ref(), directory.as_ref()) {
        add_conflict(binding, path, plan, "TYPE_CONFLICT", dataset, directory);
        return;
    }
    let baseline = binding.baseline.get(path);
    let local_changed = !equivalent(directory.as_ref(), baseline);
    let remote_changed = !equivalent(dataset.as_ref(), baseline);
    let side = choose_side(
        &binding.direction,
        dataset.is_some(),
        directory.is_some(),
        local_changed,
        remote_changed,
    );
    match side {
        Some("ignore") => {}
        Some(side) => add_action(plan, path, side, dataset, directory),
        None => add_conflict(binding, path, plan, "CONTENT_CONFLICT", dataset, directory),
    }
}
fn different_kind(a: Option<&Content>, b: Option<&Content>) -> bool {
    a.zip(b).is_some_and(|(a, b)| a.kind != b.kind)
}
fn add_conflict(
    binding: &Binding,
    path: &str,
    plan: &mut Plan,
    code: &str,
    dataset: Option<Content>,
    directory: Option<Content>,
) {
    plan.conflict_details.push(conflict(
        binding,
        path,
        code,
        dataset,
        directory,
        code == "CONTENT_CONFLICT",
    ));
}
fn choose_side(
    direction: &str,
    remote: bool,
    local: bool,
    local_changed: bool,
    remote_changed: bool,
) -> Option<&'static str> {
    match direction {
        "download" if !remote => Some("ignore"),
        "upload" if !local => Some("ignore"),
        "download" if !local_changed => Some("download"),
        "upload" if !remote_changed => Some("upload"),
        "both" if remote && !local_changed => Some("download"),
        "both" if local && !remote_changed => Some("upload"),
        _ => None,
    }
}
pub(super) fn equivalent(a: Option<&Content>, b: Option<&Content>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => files::same(Some(a), b),
        _ => false,
    }
}
fn add_action(
    plan: &mut Plan,
    path: &str,
    side: &str,
    dataset: Option<Content>,
    directory: Option<Content>,
) {
    let (before, after) = if side == "upload" {
        (dataset, directory)
    } else {
        (directory, dataset)
    };
    if let Some(after) = after {
        plan.actions.push(Action {
            path: path.into(),
            before,
            after,
            side: side.into(),
        });
    }
}
fn conflict(
    binding: &Binding,
    path: &str,
    code: &str,
    dataset: Option<Content>,
    directory: Option<Content>,
    resolvable: bool,
) -> Conflict {
    Conflict {
        path: path.into(),
        code: code.into(),
        baseline: binding.baseline.get(path).cloned(),
        dataset,
        directory,
        resolvable,
    }
}
pub fn resolve(
    state: &State,
    project: &Project,
    export: &Export,
    binding: &mut Binding,
    id: &str,
    decisions: &BTreeMap<String, String>,
) -> Result<(), Error> {
    let mut plan = binding
        .plan
        .clone()
        .filter(|p| p.id == id && p.state == "ready")
        .ok_or_else(|| Error::conflict("PLAN_CHANGED"))?;
    super::apply::check_head(state.sync.as_ref().ok_or_else(Error::unsupported)?, binding)?;
    let mut problems = vec![];
    if scan(project, export, binding, &mut problems)? != plan.directory || !problems.is_empty() {
        return Err(Error::conflict("TARGET_CHANGED"));
    }
    for (path, choice) in decisions {
        select_source(&mut plan, path, choice)?;
    }
    plan.id = super::random_id()?;
    finalize(&mut plan);
    plan.actions.sort_by(|a, b| a.path.cmp(&b.path));
    binding.plan = Some(plan);
    catalog(state)?.save_binding(binding.clone())
}
fn select_source(plan: &mut Plan, path: &str, choice: &str) -> Result<(), Error> {
    let detail = plan
        .conflict_details
        .iter()
        .find(|c| c.path == path && c.resolvable)
        .cloned()
        .ok_or_else(Error::invalid)?;
    let side = match choice {
        "dataset" if plan.direction != "upload" && detail.dataset.is_some() => "download",
        "directory" if plan.direction != "download" && detail.directory.is_some() => "upload",
        _ => return Err(Error::invalid()),
    };
    add_action(plan, path, side, detail.dataset, detail.directory);
    plan.conflict_details.retain(|c| c.path != path);
    Ok(())
}
fn finalize(plan: &mut Plan) {
    plan.conflicts = plan
        .conflict_details
        .iter()
        .map(|c| c.path.clone())
        .collect();
}
