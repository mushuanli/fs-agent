//! Explicit host profiles; no executable or host path comes from a remote request.
use crate::{
    config::{Access, Config},
    core::ids,
};
use serde::Deserialize;
use std::{collections::HashSet, path::PathBuf};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    pub id: String,
    pub kind: String,
    #[serde(default = "codex")]
    pub command: String,
    pub home: PathBuf,
    #[serde(default)]
    pub projects: bool,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub id: String,
    pub path: PathBuf,
}

fn codex() -> String {
    "codex".into()
}

pub fn validate(config: &Config) -> Result<Vec<ProfileConfig>, String> {
    validate_with_plugins(config, &super::HarnessPlugins::default())
}

pub fn validate_with_plugins(
    config: &Config,
    plugins: &super::HarnessPlugins,
) -> Result<Vec<ProfileConfig>, String> {
    let mut profiles = config.harnesses.clone();
    let mut ids = HashSet::new();
    for profile in &mut profiles {
        if !ids.insert(profile.id.clone())
            || !ids::is_identifier(&profile.id)
            || profile.command.is_empty()
        {
            return Err("Invalid or duplicate harness profile".into());
        }
        profile.home = directory(&profile.home)?;
        let mut workspaces = HashSet::new();
        for workspace in &mut profile.workspaces {
            if !workspaces.insert(workspace.id.clone()) || !ids::is_identifier(&workspace.id) {
                return Err("Invalid or duplicate harness workspace".into());
            }
            workspace.path = directory(&workspace.path)?;
            reject_exclusive(config, &workspace.path)?;
        }
        plugins.validate(profile)?;
    }
    Ok(profiles)
}

fn directory(path: &std::path::Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("Harness paths must be absolute".into());
    }
    let path = path
        .canonicalize()
        .map_err(|_| "Harness directory unavailable")?;
    if !path.is_dir() {
        return Err("Harness path is not a directory".into());
    }
    Ok(path)
}

fn reject_exclusive(config: &Config, path: &std::path::Path) -> Result<(), String> {
    for export in config.exports.iter().filter(|e| e.access == Access::Rw) {
        let root = std::path::Path::new(&export.path)
            .canonicalize()
            .map_err(|_| "Export unavailable")?;
        if path.starts_with(&root) || root.starts_with(path) {
            return Err("Harness workspaces must not overlap exclusive writable exports".into());
        }
    }
    Ok(())
}
