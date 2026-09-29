//! Atomic storage for lease records.
//!
//! One JSON document is the whole state. Updates are copy-on-write in memory
//! and persisted with `write temp -> fsync -> rename -> fsync directory`, so a
//! reader either sees the previous document or the next one, never a blend.
//!
//! Failure handling distinguishes the two halves of that sequence:
//!
//! * **before the rename** the durable document is provably unchanged, so the
//!   in-memory image stays authoritative and the caller may retry;
//! * **after the rename** the new document is live but its durability is
//!   unknown, so the journal is poisoned and every later call fails.
//!
//! Serving from a maybe-stale image would silently break fencing, which is
//! worse than refusing to serve.

use super::model::{random_id, Journal, Lease, LeaseError, Result};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

/// Journal format version understood by this build.
const VERSION: u32 = 1;
const LOCK_FILE: &str = "leases.lock";
const JOURNAL_FILE: &str = "leases.json";
/// Private permissions: the journal names workspaces and token digests.
const MODE: u32 = 0o600;

struct Image {
    journal: Journal,
    poisoned: bool,
}

/// How far a failed persist may have got.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Durability {
    /// The rename did not happen; disk and memory still agree.
    Intact,
    /// The rename may have happened; the durable state is unknown.
    Unknown,
}

/// Atomic storage mechanism. Transitions and expiry policy live in the registry.
pub(super) struct LeaseJournal {
    path: PathBuf,
    /// Held for the lifetime of the journal: the single-writer fence.
    _lock: File,
    image: Mutex<Image>,
}

impl LeaseJournal {
    pub(super) fn open(directory: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory).map_err(storage)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(MODE)
            .open(directory.join(LOCK_FILE))
            .map_err(storage)?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| LeaseError::Storage)?;
        // Hold the lock before touching anything: no other writer is active.
        sweep_temporaries(directory);
        let path = directory.join(JOURNAL_FILE);
        let journal = load(&path)?;
        Ok(Self {
            path,
            _lock: lock,
            image: Mutex::new(Image {
                journal,
                poisoned: false,
            }),
        })
    }

    /// Read the records without allowing a mutation.
    pub(super) fn read<T>(
        &self,
        action: impl FnOnce(&BTreeMap<String, Lease>) -> Result<T>,
    ) -> Result<T> {
        let image = self.lock()?;
        action(&image.journal.leases)
    }

    /// Apply a transition and persist it if it changed anything.
    pub(super) fn update<T>(
        &self,
        action: impl FnOnce(&mut BTreeMap<String, Lease>) -> Result<T>,
    ) -> Result<T> {
        let mut image = self.lock()?;
        let mut next = image.journal.clone();
        let result = action(&mut next.leases)?;
        if next == image.journal {
            return Ok(result);
        }
        match persist(&self.path, &next) {
            Ok(()) => {
                image.journal = next;
                Ok(result)
            }
            // The durable document is unchanged; the caller may retry.
            Err(Durability::Intact) => Err(LeaseError::Storage),
            // The rename may have committed; never serve the old image again.
            Err(Durability::Unknown) => {
                image.poisoned = true;
                Err(LeaseError::Storage)
            }
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Image>> {
        self.image.lock().map_err(|_| LeaseError::Storage)
    }
}

fn load(path: &Path) -> Result<Journal> {
    let journal: Journal = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| LeaseError::Storage)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Journal {
            version: VERSION,
            leases: BTreeMap::new(),
        },
        Err(_) => return Err(LeaseError::Storage),
    };
    if journal.version != VERSION {
        return Err(LeaseError::Storage);
    }
    Ok(journal)
}

/// Replace the journal with `next` atomically, or leave the previous one intact.
fn persist(path: &Path, next: &Journal) -> std::result::Result<(), Durability> {
    let temporary = random_id()
        .map(|id| path.with_extension(format!("{id}.tmp")))
        .map_err(|_| Durability::Intact)?;
    let outcome = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(MODE)
            .open(&temporary)
            .map_err(|_| Durability::Intact)?;
        let bytes = serde_json::to_vec(next).map_err(|_| Durability::Intact)?;
        file.write_all(&bytes).map_err(|_| Durability::Intact)?;
        file.sync_all().map_err(|_| Durability::Intact)?;
        std::fs::rename(&temporary, path).map_err(|_| Durability::Intact)?;
        // The new document is live; only directory durability is still unknown.
        File::open(path.parent().ok_or(Durability::Unknown)?)
            .map_err(|_| Durability::Unknown)?
            .sync_all()
            .map_err(|_| Durability::Unknown)
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    outcome
}

/// Remove temporary documents left behind by a crash mid-write.
fn sweep_temporaries(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with("leases.") && name.ends_with(".tmp") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn storage(_: std::io::Error) -> LeaseError {
    LeaseError::Storage
}
