//! Resolve host aliases before allowing exports or sandbox mounts.
use super::super::{
    model::{Config, Error, Result},
    policy,
};
use std::path::{Path, PathBuf};
pub fn overlap(a: &Path, b: &Path) -> bool {
    policy::overlap(a, b) || ancestor_alias(a, b) || ancestor_alias(b, a)
}
fn ancestor_alias(root: &Path, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(root) = std::fs::metadata(root) else {
        return false;
    };
    path.ancestors()
        .filter_map(|p| std::fs::metadata(p).ok())
        .any(|m| m.dev() == root.dev() && m.ino() == root.ino())
}
pub fn validate(c: &Config, config: &crate::config::Config) -> Result<()> {
    let root = resolve(&c.root)?;
    for export in &config.exports {
        if overlap(&root, &std::fs::canonicalize(&export.path)?) {
            return Err(Error::new("SYNC_EXPORT_OVERLAP", 400));
        }
    }
    if config.execution {
        for source in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
            if let Ok(path) = std::fs::canonicalize(source) {
                if overlap(&root, &path) {
                    return Err(Error::new("SYNC_SANDBOX_OVERLAP", 400));
                }
            }
        }
    }
    Ok(())
}
/// Canonicalize the sync root, which may not exist before its first start.
///
/// Resolving the deepest existing ancestor already resolves every symlink in
/// that prefix, so a root that is about to be created is still compared against
/// the real export paths instead of an unresolved one.
fn resolve(root: &Path) -> Result<PathBuf> {
    let mut suffix = Vec::new();
    let mut current = root.to_path_buf();
    loop {
        match std::fs::canonicalize(&current) {
            Ok(mut base) => {
                for name in suffix.iter().rev() {
                    base.push(name);
                }
                return Ok(base);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = current
                    .file_name()
                    .ok_or_else(|| Error::new("INVALID_SYNC_CONFIG", 400))?;
                suffix.push(name.to_os_string());
                current = match current.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                    _ => PathBuf::from("."),
                };
            }
            Err(error) => return Err(error.into()),
        }
    }
}
