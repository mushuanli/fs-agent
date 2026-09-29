//! Startup recovery for writable exports.
//!
//! Staged uploads are created inside the target directory under a reserved
//! prefix. If the service died mid-upload they are abandoned, so they are
//! removed before the export starts serving. This runs while the writer lock
//! is already held, so no cooperating writer can be using them.
//!
//! Recovery is best effort: the directory is shared with editors, git and
//! other tools, so an entry this routine cannot inspect must never prevent the
//! export from opening. Only names with the reserved spelling that are not
//! directories are removed; a directory is left untouched because the service
//! never creates one under that name.

use crate::{core::error::Error, fs::export::Export, fs::path};
use rustix::fs::{AtFlags, FileType, OFlags};

/// Upper bound on directory entries inspected during recovery.
const MAX_VISITED: usize = 1_000_000;

pub(crate) fn clean_uploads(export: &Export) -> Result<(), Error> {
    let mut pending = vec![String::new()];
    let mut visited = 0usize;
    while let Some(directory_path) = pending.pop() {
        // An unreadable or already-vanished subtree is not a reason to refuse service.
        let Ok(directory) = export.open_dir(&directory_path) else {
            continue;
        };
        for entry in rustix::fs::Dir::read_from(&directory)? {
            let entry = entry?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if name == "." || name == ".." {
                continue;
            }
            visited += 1;
            if visited > MAX_VISITED {
                return Err(Error::invalid());
            }
            if path::is_reserved(name) {
                if remove_staged(&directory, name) {
                    continue;
                }
                // A directory under a reserved name was not created by this
                // service; leave the subtree alone.
                continue;
            }
            // Symlinks are not followed, so they are never descended into.
            let child = crate::fs::export::child_path(&directory_path, name);
            if let Ok(file) = export.open_path(&child, OFlags::PATH) {
                if file.metadata()?.is_dir() {
                    pending.push(child);
                }
            }
        }
    }
    Ok(())
}

/// Remove one staging entry if it is a file or symlink. Returns whether it was
/// handled, so a directory can be skipped instead of failing the whole scan.
fn remove_staged(directory: &std::fs::File, name: &str) -> bool {
    let Ok(stat) = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) else {
        return false;
    };
    if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
        return false;
    }
    rustix::fs::unlinkat(directory, name, AtFlags::empty()).is_ok()
}
