use super::super::{
    model::{digest, random_id, Error, Result},
    policy,
};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub fn lock(root: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("sync.lock"))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| Error::new("SYNC_LOCKED", 409))?;
    Ok(file)
}
pub fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
pub fn private_dir(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    std::fs::create_dir(path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    sync_dir(path)?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}
pub fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(Error::storage)?;
    let temporary = parent.join(format!(".{}", random_id()?));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    sync_dir(parent)
}
pub fn object(root: &Path, namespace: &str, project: &str, hash: &str) -> Result<PathBuf> {
    policy::id(namespace)?;
    policy::id(project)?;
    policy::hash(hash)?;
    Ok(root
        .join("objects")
        .join(namespace)
        .join(project)
        .join(&hash[..2])
        .join(hash))
}
pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    let mut file = File::open(path).map_err(object_io)?;
    let mut sha = Sha256::new();
    let mut bytes = [0u8; 65536];
    let mut size = 0;
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        sha.update(&bytes[..n]);
        size += n as u64;
    }
    Ok((format!("{:x}", sha.finalize()), size))
}
pub fn verify(path: &Path, hash: &str) -> Result<u64> {
    let (actual, size) = hash_file(path)?;
    if actual != hash {
        return Err(Error::new("OBJECT_CORRUPT", 503));
    }
    Ok(size)
}
pub fn read(path: &Path, hash: &str, max: u64) -> Result<Vec<u8>> {
    if std::fs::metadata(path).map_err(object_io)?.len() > max {
        return Err(Error::new("LIMIT_EXCEEDED", 413));
    }
    let bytes = std::fs::read(path).map_err(object_io)?;
    if digest(&bytes) != hash {
        return Err(Error::new("OBJECT_CORRUPT", 503));
    }
    Ok(bytes)
}
fn object_io(error: std::io::Error) -> Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        Error::new("OBJECT_MISSING", 503)
    } else {
        error.into()
    }
}
pub fn space(root: &Path, reserve: u64, growth: u64) -> Result<()> {
    let info = rustix::fs::statvfs(root).map_err(std::io::Error::from)?;
    let free = info.f_bavail.saturating_mul(info.f_frsize);
    if free < reserve.saturating_add(growth) {
        return Err(Error::new("ENOSPC", 507));
    }
    Ok(())
}

pub fn visit(dir: &Path, action: &mut impl FnMut(&Path) -> Result<()>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            visit(&entry.path(), action)?;
        } else if entry.file_type()?.is_file() {
            action(&entry.path())?;
        } else {
            return Err(Error::new("UNSAFE_STORAGE_FILE", 503));
        }
    }
    Ok(())
}

pub fn temporary(root: &Path, id: &str) -> Result<File> {
    policy::id(id)?;
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("staging").join(id))?)
}
