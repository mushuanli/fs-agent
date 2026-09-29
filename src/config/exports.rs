//! Export policy: alias derivation, overlap rejection and capability opening.
//!
//! Two configured roots may not contain one another, because nested exports
//! would let the same bytes be reached through two different identities
//! (different alias, different access mode, different lock). Equal paths are
//! rejected by the same test, since a path starts with itself.

use crate::config::model::{Access, ExportConfig};
use crate::core::ids;
use crate::fs::{Export, Exports};
use std::{collections::BTreeMap, path::Path, sync::Arc};

pub(crate) fn open_all(configs: &[ExportConfig]) -> Result<Exports, String> {
    if configs.is_empty() {
        return Err("no exports configured: add at least one [[exports]] section".to_owned());
    }
    let mut by_alias = BTreeMap::new();
    let mut roots: Vec<(String, std::path::PathBuf)> = Vec::new();
    for (index, config) in configs.iter().enumerate() {
        let alias = alias(config, index)?;
        if by_alias.contains_key(&alias) {
            return Err(format!(
                "exports[{index}]: alias {alias:?} is already defined"
            ));
        }
        // Resolve once and open the resolved path, so the overlap decision and
        // the capability refer to the same directory.
        let root = std::fs::canonicalize(&config.path)
            .map_err(|_| format!("exports[{index}]: path {:?} does not exist", config.path))?;
        reject_overlap(&roots, &root, &config.path, index)?;
        let capability = open(config, index, &root)?;
        roots.push((alias.clone(), root));
        by_alias.insert(alias, Arc::new(capability));
    }
    Ok(Exports::new(by_alias))
}

fn reject_overlap(
    roots: &[(String, std::path::PathBuf)],
    root: &Path,
    configured: &str,
    index: usize,
) -> Result<(), String> {
    let Some((other, _)) = roots
        .iter()
        .find(|(_, path)| root.starts_with(path) || path.starts_with(root))
    else {
        return Ok(());
    };
    Err(format!(
        "exports[{index}]: path {configured:?} overlaps export {other:?}"
    ))
}

fn alias(config: &ExportConfig, index: usize) -> Result<String, String> {
    let alias = match &config.alias {
        Some(alias) => alias.clone(),
        None => Path::new(&config.path)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .ok_or_else(|| {
                format!(
                    "exports[{index}]: cannot derive alias from path {:?}, set \"alias\"",
                    config.path
                )
            })?,
    };
    if !ids::is_identifier(&alias) {
        return Err(format!(
            "exports[{index}]: alias {alias:?} must be non-empty and use only ASCII letters, digits, '_' or '-'; set \"alias\" explicitly"
        ));
    }
    Ok(alias)
}

fn open(config: &ExportConfig, index: usize, root: &Path) -> Result<Export, String> {
    let opened = match config.access {
        Access::Ro => Export::open(root),
        Access::Rw => Export::exclusive(root),
    };
    opened.map_err(|_| {
        format!(
            "exports[{index}]: cannot open or lock path {:?}",
            config.path
        )
    })
}
