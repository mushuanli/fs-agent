//! Execution policy: which mounts, working directory and command a request may use.
//!
//! This module decides *what* is allowed and resolves it into a [`Plan`]. It
//! performs no mounting and builds no launcher: [`crate::process::sandbox`]
//! turns a plan into a bubblewrap invocation. Keeping the split here means the
//! authorization rules can be reviewed (and tested) without reading any
//! sandbox code.
//!
//! All paths in a request are virtual. A host path cannot be expressed: mount
//! sources are `(alias, export-relative path)` pairs that are resolved with
//! `openat2` under the export root, and mount targets are absolute virtual
//! paths that may not shadow the runtime directories the launcher provides.

use crate::{
    app::State,
    core::error::Error,
    fs::Export,
    process::model::{Mount, Request},
};
use std::sync::Arc;

/// Mount targets that would shadow the runtime the launcher builds itself.
const RUNTIME_ROOTS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/proc", "/dev", "/sys", "/tmp",
];
const MAX_MOUNTS: usize = 32;
const MAX_COMMAND_BYTES: usize = 32 * 1024;
const MAX_TIMEOUT_MS: u64 = 300_000;
const MAX_VIRTUAL_PATH: usize = 4096;

/// One authorized mount, already resolved to a directory capability.
pub struct MountPlan {
    pub alias: String,
    pub at: String,
    pub path: String,
    pub writable: bool,
    pub export: Arc<Export>,
}

/// An authorized, ready-to-launch command.
pub struct Plan {
    /// Sorted so a parent mount is always applied before a nested one.
    pub mounts: Vec<MountPlan>,
    pub cwd: String,
    pub command: String,
    pub args: Vec<String>,
    pub timeout_ms: u64,
}

impl Plan {
    /// The single writable export, if the command may write at all.
    pub fn writer(&self) -> Option<&Arc<Export>> {
        self.mounts
            .iter()
            .find(|mount| mount.writable)
            .map(|mount| &mount.export)
    }
}

/// Validate `request` and resolve it against the caller's authorizations.
pub fn plan(state: &State, identity: usize, request: &Request) -> Result<Plan, Error> {
    validate_shape(request)?;
    let mut ordered: Vec<&Mount> = request.mounts.iter().collect();
    ordered.sort_by_key(|mount| mount.at.len());
    let mut mounts = Vec::with_capacity(ordered.len());
    for (index, mount) in ordered.iter().copied().enumerate() {
        validate_mount(&ordered, index)?;
        mounts.push(resolve(state, identity, mount)?);
    }
    ensure_single_writer(&mounts)?;
    let cwd = resolve_cwd(&request.cwd, &mounts)?;
    Ok(Plan {
        mounts,
        cwd,
        command: request.command.clone(),
        args: request.args.clone(),
        timeout_ms: request.timeout_ms,
    })
}

fn validate_shape(request: &Request) -> Result<(), Error> {
    virtual_path(&request.cwd)?;
    if request.mounts.is_empty()
        || request.mounts.len() > MAX_MOUNTS
        || request.command.is_empty()
        || request.command.contains('\0')
        || request.args.iter().any(|argument| argument.contains('\0'))
        || request.command.len() + request.args.iter().map(String::len).sum::<usize>()
            > MAX_COMMAND_BYTES
        || request.timeout_ms == 0
        || request.timeout_ms > MAX_TIMEOUT_MS
    {
        return Err(Error::invalid());
    }
    Ok(())
}

fn validate_mount(mounts: &[&Mount], index: usize) -> Result<(), Error> {
    let mount = mounts[index];
    virtual_path(&mount.at)?;
    if mount.access != "ro" && mount.access != "rw" {
        return Err(Error::invalid());
    }
    if RUNTIME_ROOTS.iter().any(|root| inside(&mount.at, root)) {
        return Err(Error::invalid());
    }
    if mounts[..index].iter().any(|other| other.at == mount.at) {
        return Err(Error::invalid());
    }
    // Overlapping source trees cannot have both read-only and writable aliases:
    // either containment direction exposes protected content through a writable path.
    if mount.access == "ro"
        && mounts.iter().copied().any(|other| {
            other.access == "rw"
                && other.alias == mount.alias
                && (mount.path.is_empty()
                    || other.path.is_empty()
                    || inside(&mount.path, &other.path)
                    || inside(&other.path, &mount.path))
        })
    {
        return Err(Error::forbidden("EROFS"));
    }
    // The launcher must create the mount point for `at`; a read-only parent
    // makes that impossible, so reject it here instead of failing at spawn.
    if let Some(parent) = deepest_prefix(mounts, &mount.at) {
        if parent.access == "ro" {
            return Err(Error::unsupported());
        }
    }
    Ok(())
}

/// Authorize one mount and resolve its source directory.
fn resolve(state: &State, identity: usize, mount: &Mount) -> Result<MountPlan, Error> {
    let client = state.auth.client(identity);
    if !client.may_read(&mount.alias) {
        return Err(Error::forbidden("EACCES"));
    }
    let export = state
        .exports
        .get(&mount.alias)
        .ok_or_else(Error::invalid)?
        .clone();
    let writable = mount.access == "rw";
    if writable && (!client.may_write(&mount.alias) || !export.writable()) {
        return Err(Error::forbidden("EROFS"));
    }
    Ok(MountPlan {
        alias: mount.alias.clone(),
        at: mount.at.clone(),
        path: mount.path.clone(),
        writable,
        export,
    })
}

/// At most one export may be writable per command.
fn ensure_single_writer(mounts: &[MountPlan]) -> Result<(), Error> {
    let mut writer: Option<&str> = None;
    for mount in mounts.iter().filter(|mount| mount.writable) {
        match writer {
            None => writer = Some(&mount.alias),
            Some(alias) if alias == mount.alias => {}
            Some(_) => return Err(Error::unsupported()),
        }
    }
    Ok(())
}

/// The deepest mount point that contains `cwd`, and the existence check that
/// proves the directory is reachable inside the authorized export.
fn resolve_cwd(cwd: &str, mounts: &[MountPlan]) -> Result<String, Error> {
    let mount = mounts
        .iter()
        .rev()
        .find(|mount| inside(cwd, &mount.at))
        .ok_or_else(Error::invalid)?;
    let relative = cwd[mount.at.len()..].trim_start_matches('/');
    let path = [mount.path.as_str(), relative]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    mount.export.open_dir(&path)?;
    Ok(cwd.to_owned())
}

/// The longest proper mount-point prefix of `at`, if any.
fn deepest_prefix<'a>(mounts: &[&'a Mount], at: &str) -> Option<&'a Mount> {
    mounts
        .iter()
        .copied()
        .filter(|other| other.at.len() < at.len() && inside(at, &other.at))
        .max_by_key(|other| other.at.len())
}

/// An absolute virtual path: `/` separated, no empty, `.` or `..` components.
pub fn virtual_path(path: &str) -> Result<(), Error> {
    if !path.starts_with('/')
        || path.len() > MAX_VIRTUAL_PATH
        || path == "/"
        || path.contains(['\\', '\0'])
        || path
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(Error::invalid());
    }
    Ok(())
}

/// Whether `path` is `root` or lives below it, on a component boundary.
fn inside(path: &str, root: &str) -> bool {
    path == root || path.starts_with(&format!("{root}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(path: &str, at: &str, access: &str) -> Mount {
        Mount {
            alias: "project".into(),
            path: path.into(),
            at: at.into(),
            access: access.into(),
        }
    }

    #[test]
    fn rejects_readonly_source_overlap_in_both_directions() {
        for (readonly, writable) in [
            ("", "child"),
            ("child", ""),
            ("docs", "docs/sub"),
            ("docs/sub", "docs"),
            ("docs", "docs"),
        ] {
            let ro = mount(readonly, "/reference", "ro");
            let rw = mount(writable, "/workspace", "rw");
            assert!(validate_mount(&[&ro, &rw], 0).is_err());
            assert!(validate_mount(&[&rw, &ro], 1).is_err());
        }
    }

    #[test]
    fn permits_disjoint_source_trees_on_component_boundaries() {
        let ro = mount("docs", "/reference", "ro");
        let rw = mount("docs2", "/workspace", "rw");
        assert!(validate_mount(&[&ro, &rw], 0).is_ok());
        assert!(validate_mount(&[&ro, &rw], 1).is_ok());
    }
}
