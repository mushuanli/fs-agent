use super::lease_model::{random_id, Journal, Lease, LeaseError, Result};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

struct Image {
    journal: Journal,
    poisoned: bool,
}
/// Atomic storage mechanism. Lease transitions and expiry policy live in the registry.
pub(super) struct LeaseJournal {
    path: PathBuf,
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
            .mode(0o600)
            .open(directory.join("leases.lock"))
            .map_err(storage)?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| LeaseError::Storage)?;
        let path = directory.join("leases.json");
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
    pub(super) fn read<T>(
        &self,
        action: impl FnOnce(&BTreeMap<String, Lease>) -> Result<T>,
    ) -> Result<T> {
        let image = self.image.lock().map_err(|_| LeaseError::Storage)?;
        if image.poisoned {
            return Err(LeaseError::Storage);
        }
        action(&image.journal.leases)
    }
    pub(super) fn update<T>(
        &self,
        action: impl FnOnce(&mut BTreeMap<String, Lease>) -> Result<T>,
    ) -> Result<T> {
        let mut image = self.image.lock().map_err(|_| LeaseError::Storage)?;
        if image.poisoned {
            return Err(LeaseError::Storage);
        }
        let mut next = image.journal.clone();
        let result = action(&mut next.leases)?;
        if next == image.journal {
            return Ok(result);
        }
        if let Err(error) = persist(&self.path, &next) {
            // Rename can commit before fsync fails; reject every subsequent operation.
            image.poisoned = true;
            return Err(error);
        }
        image.journal = next;
        Ok(result)
    }
}
fn load(path: &Path) -> Result<Journal> {
    let journal: Journal = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| LeaseError::Storage)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Journal {
            version: 1,
            leases: BTreeMap::new(),
        },
        Err(_) => return Err(LeaseError::Storage),
    };
    if journal.version != 1 {
        return Err(LeaseError::Storage);
    }
    Ok(journal)
}
fn storage(_: std::io::Error) -> LeaseError {
    LeaseError::Storage
}
fn persist(path: &Path, journal: &Journal) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", random_id()?));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(storage)?;
        let bytes = serde_json::to_vec(journal).map_err(|_| LeaseError::Storage)?;
        file.write_all(&bytes).map_err(storage)?;
        file.sync_all().map_err(storage)?;
        std::fs::rename(&temporary, path).map_err(storage)?;
        File::open(path.parent().ok_or(LeaseError::Storage)?)
            .map_err(storage)?
            .sync_all()
            .map_err(storage)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
