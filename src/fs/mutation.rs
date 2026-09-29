//! Structural mutations: create a directory, rename an entry, remove an entry.
//!
//! The wire protocol carries these as `action` strings; this module is the
//! only place that turns them into syscalls, and it invalidates revisions in
//! the same critical section that performs the change.

use crate::{core::error::Error, fs::export::Export, fs::path};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};

/// Mode for a newly created directory. The service does not consult `umask`,
/// so the result is predictable for every deploying host.
const DIRECTORY_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::XUSR)
    .union(Mode::RGRP)
    .union(Mode::XGRP)
    .union(Mode::ROTH)
    .union(Mode::XOTH);

/// One validated structural change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Mkdir { path: String },
    Rename { path: String, to: String },
    Remove { path: String },
}

impl Change {
    /// Map a protocol action onto a validated change.
    pub fn parse(action: &str, path: String, to: Option<String>) -> Result<Self, Error> {
        path::validate(&path)?;
        Ok(match action {
            "mkdir" => Self::Mkdir { path },
            "rename" => {
                let to = to.ok_or_else(Error::invalid)?;
                path::validate(&to)?;
                Self::Rename { path, to }
            }
            "remove" => Self::Remove { path },
            _ => return Err(Error::invalid()),
        })
    }

    /// The entry the change applies to.
    pub fn path(&self) -> &str {
        match self {
            Self::Mkdir { path } | Self::Rename { path, .. } | Self::Remove { path } => path,
        }
    }
}

impl Export {
    /// Apply `change` while holding the revision lock.
    ///
    /// `checkpoint` is polled once before the syscall so a cancelled or
    /// expired operation never mutates the tree.
    pub fn apply(
        &self,
        change: &Change,
        checkpoint: impl Fn() -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut revisions = self
            .lock_revisions()
            .ok_or_else(|| Error::forbidden("EROFS"))?;
        checkpoint()?;
        match change {
            Change::Mkdir { path } => {
                let (directory, name) = self.parent(path)?;
                rustix::fs::mkdirat(&directory, name.as_str(), DIRECTORY_MODE)?;
            }
            Change::Rename { path, to } => {
                let (from, name) = self.parent(path)?;
                let (destination, destination_name) = self.parent(to)?;
                rustix::fs::renameat_with(
                    &from,
                    name.as_str(),
                    &destination,
                    destination_name.as_str(),
                    RenameFlags::NOREPLACE,
                )?;
            }
            Change::Remove { path } => {
                let (directory, name) = self.parent(path)?;
                let target = self.open_path(path, OFlags::PATH)?;
                let flags = if target.metadata()?.is_dir() {
                    AtFlags::REMOVEDIR
                } else {
                    AtFlags::empty()
                };
                rustix::fs::unlinkat(&directory, name.as_str(), flags)?;
            }
        }
        revisions.invalidate();
        Ok(())
    }
}
