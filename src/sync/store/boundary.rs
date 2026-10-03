//! Resolve host aliases before allowing exports or sandbox mounts.
use super::super::{
    model::{Config, Error, Result},
    policy,
};
use std::path::Path;
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
    let root = std::fs::canonicalize(&c.root)?;
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
