//! Canonical schemas use ASCII keys and integer version 1, a JCS subset.
use super::{
    model::{Config, Error, Result},
    policy,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Files {
    format: String,
    version: u32,
    entries: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum Entry {
    Directory {
        path: String,
    },
    File {
        path: String,
        hash: String,
        size: String,
        executable: Option<bool>,
    },
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Bundle {
    format: String,
    version: u32,
    media_type: String,
    root: String,
    objects: Vec<Reference>,
}
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub hash: String,
    pub size: String,
}
pub struct Validated {
    pub format: String,
    pub refs: Vec<Reference>,
}

pub fn validate(bytes: &[u8], config: &Config) -> Result<Validated> {
    if bytes.len() as u64 > config.max_manifest_bytes {
        return Err(invalid());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if serde_json::to_vec(&value).map_err(|_| invalid())? != bytes {
        return Err(invalid());
    }
    match value["format"].as_str() {
        Some("fs-agent.files") => files(bytes, config),
        Some("fs-agent.bundle") => bundle(bytes, config),
        _ => Err(invalid()),
    }
}
fn files(bytes: &[u8], config: &Config) -> Result<Validated> {
    let files: Files = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if files.version != 1 || files.entries.len() > config.max_manifest_entries {
        return Err(invalid());
    }
    let mut paths = BTreeMap::new();
    let mut refs = Vec::new();
    let mut previous = String::new();
    for entry in files.entries {
        let (path, file) = split_entry(entry);
        valid_path(&path)?;
        validate_parent(&path, &previous, &paths)?;
        previous = path.clone();
        paths.insert(path, file.is_some());
        if let Some(reference) = file {
            valid_ref(&reference)?;
            refs.push(reference);
        }
    }
    Ok(Validated {
        format: files.format,
        refs,
    })
}
fn split_entry(entry: Entry) -> (String, Option<Reference>) {
    match entry {
        Entry::Directory { path } => (path, None),
        Entry::File {
            path,
            hash,
            size,
            executable,
        } => {
            let _ = executable;
            (path, Some(Reference { hash, size }))
        }
    }
}
fn validate_parent(path: &str, previous: &str, paths: &BTreeMap<String, bool>) -> Result<()> {
    if path <= previous {
        return Err(invalid());
    }
    if let Some((parent, _)) = path.rsplit_once('/') {
        if paths.get(parent) != Some(&false) {
            return Err(invalid());
        }
    }
    Ok(())
}
fn bundle(bytes: &[u8], config: &Config) -> Result<Validated> {
    let bundle: Bundle = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if bundle.version != 1
        || bundle.media_type.is_empty()
        || bundle.media_type.len() > 256
        || bundle.objects.len() > config.max_manifest_entries
    {
        return Err(invalid());
    }
    policy::hash(&bundle.root).map_err(|_| invalid())?;
    let mut hashes = BTreeSet::new();
    let mut previous = String::new();
    for reference in &bundle.objects {
        valid_ref(reference)?;
        if reference.hash <= previous {
            return Err(invalid());
        }
        previous = reference.hash.clone();
        hashes.insert(reference.hash.clone());
    }
    if !hashes.contains(&bundle.root) {
        return Err(invalid());
    }
    Ok(Validated {
        format: bundle.format,
        refs: bundle.objects,
    })
}
fn valid_ref(reference: &Reference) -> Result<()> {
    policy::hash(&reference.hash).map_err(|_| invalid())?;
    policy::number(&reference.size).map_err(|_| invalid())?;
    Ok(())
}
fn valid_path(path: &str) -> Result<()> {
    if path.len() > 4096
        || path.contains(['\\', '\0'])
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(invalid());
    }
    Ok(())
}
fn invalid() -> Error {
    Error::new("INVALID_MANIFEST", 422)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_noncanonical_duplicate_and_traversal() {
        for source in [
            r#"{"format":"fs-agent.files","version":1,"entries":[]}"#,
            r#"{"entries":[],"entries":[],"format":"fs-agent.files","version":1}"#,
            r#"{"entries":[{"kind":"directory","path":"../x"}],"format":"fs-agent.files","version":1}"#,
        ] {
            assert!(validate(source.as_bytes(), &Config::default()).is_err());
        }
        assert!(validate(
            br#"{"entries":[],"format":"fs-agent.files","version":1}"#,
            &Config::default()
        )
        .is_ok());
    }
}
