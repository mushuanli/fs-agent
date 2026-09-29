//! Atomic replacement of file contents.
//!
//! An upload is written to a reserved staging name in the destination
//! directory and then published with a single `renameat2`. That makes the
//! namespace change atomic and lets a created-but-unpublished file disappear
//! with the process:
//!
//! * [`Export::stage`] creates the staging file; dropping it unlinks the file.
//! * [`commit`] verifies the expected revision and renames under the
//!   revision lock, so a conditional write cannot interleave with another.
//!
//! Permission policy: because `renameat2` moves the inode, the staging mode
//! would otherwise become the file's mode. A replacement therefore copies the
//! previous file's permission bits, and a creation uses [`DEFAULT_FILE_MODE`].

use crate::{
    core::error::Error,
    fs::{
        export::Export,
        model::{attributes, Stat},
        path,
        revision::Revisions,
    },
};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};
use std::{
    fs::File,
    os::unix::fs::{MetadataExt, PermissionsExt},
    sync::MutexGuard,
};

/// Mode for a newly created file. The service does not consult `umask`, so the
/// result is predictable for every deploying host.
const DEFAULT_FILE_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::RGRP)
    .union(Mode::ROTH);
/// Permission bits only; file type and setuid/setgid/sticky are not copied.
const MODE_MASK: u32 = 0o777;
/// Staging files start private and are widened only at commit.
const STAGING_MODE: Mode = Mode::RUSR.union(Mode::WUSR);
const RANDOM_BYTES: usize = 16;

/// A staging file that is removed unless it is committed.
pub struct StagedUpload {
    parent: File,
    name: String,
    file: File,
}

impl Drop for StagedUpload {
    fn drop(&mut self) {
        let _ = rustix::fs::unlinkat(&self.parent, self.name.as_str(), AtFlags::empty());
    }
}

impl StagedUpload {
    /// Duplicate the descriptor for the asynchronous body writer.
    pub fn writer(&self) -> Result<File, Error> {
        Ok(self.file.try_clone()?)
    }
}

impl Export {
    /// Create a staging file next to `path`.
    pub fn stage(&self, path: &str) -> Result<StagedUpload, Error> {
        let (parent, _) = self.parent(path)?;
        let mut random = [0u8; RANDOM_BYTES];
        getrandom::getrandom(&mut random).map_err(|_| Error::internal())?;
        let name = format!(
            "{}{}",
            path::RESERVED_PREFIX,
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let file = File::from(rustix::fs::openat(
            &parent,
            name.as_str(),
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            STAGING_MODE,
        )?);
        Ok(StagedUpload { parent, name, file })
    }
}

/// Publish a staged upload as `path`.
///
/// `expected` is `None` for create-only writes and `Some(revision)` for
/// replace writes; both cases re-check the condition under the lock.
pub fn commit(
    export: &Export,
    path: &str,
    upload: StagedUpload,
    expected: Option<&str>,
    checkpoint: impl Fn() -> Result<(), Error>,
) -> Result<Stat, Error> {
    let mut revisions = export
        .lock_revisions()
        .ok_or_else(|| Error::forbidden("EROFS"))?;
    checkpoint()?;
    // Re-resolve the parent under the lock: renaming it invalidates the upload.
    let (target, name) = export.parent(path)?;
    if identity(&target) != identity(&upload.parent) {
        return Err(Error::precondition_failed());
    }
    let mode = match expected {
        Some(expected) => replaced_mode(export, path, expected, &mut revisions)?,
        None => DEFAULT_FILE_MODE,
    };
    checkpoint()?;
    // Give the published inode the mode the caller would expect to find.
    rustix::fs::fchmod(&upload.file, mode)?;
    let mut stat = attributes(&upload.file)?;
    stat.revision = Some(revisions.get(&upload.file)?);
    rustix::fs::renameat_with(
        &upload.parent,
        upload.name.as_str(),
        &target,
        name.as_str(),
        if expected.is_none() {
            RenameFlags::NOREPLACE
        } else {
            RenameFlags::empty()
        },
    )
    .map_err(|error| {
        if expected.is_none() && error == rustix::io::Errno::EXIST {
            Error::precondition_failed()
        } else {
            error.into()
        }
    })?;
    Ok(stat)
}

/// Reject the write unless `path` still carries exactly `expected`, then
/// return the permission bits to carry over to the replacement.
fn replaced_mode(
    export: &Export,
    path: &str,
    expected: &str,
    revisions: &mut MutexGuard<'_, Revisions>,
) -> Result<Mode, Error> {
    let current = export
        .open_path(path, OFlags::PATH)
        .map_err(|_| Error::precondition_failed())?;
    let metadata = current.metadata()?;
    if !metadata.is_file() || revisions.get(&current)? != expected {
        return Err(Error::precondition_failed());
    }
    revisions.retire(&current)?;
    Ok(Mode::from_bits_truncate(
        metadata.permissions().mode() & MODE_MASK,
    ))
}

fn identity(file: &File) -> Result<(u64, u64), Error> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}
