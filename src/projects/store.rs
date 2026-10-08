//! Locked, bounded, atomic project metadata. No credentials or file contents.
use super::model::Project;
use crate::core::error::Error;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    version: u32,
    server_id: String,
    projects: BTreeMap<String, Project>,
    #[serde(default)]
    bindings: BTreeMap<String, super::directory_sync::model::Binding>,
}
pub struct Store {
    root: PathBuf,
    _lock: File,
    image: Mutex<Image>,
    poisoned: std::sync::atomic::AtomicBool,
}
impl Store {
    pub fn check_binding(
        &self,
        binding: super::directory_sync::model::Binding,
    ) -> Result<(), Error> {
        let image = self.image.lock().map_err(|_| Error::internal())?;
        if self.poisoned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::internal());
        }
        let mut next = image.clone();
        next.bindings.insert(binding.id.clone(), binding);
        if serde_json::to_vec(&next)
            .map_err(|_| Error::internal())?
            .len()
            > 8 * 1024 * 1024
        {
            return Err(Error::too_large("PROJECT_LIMIT"));
        }
        Ok(())
    }
    pub fn bindings(&self) -> Result<Vec<super::directory_sync::model::Binding>, Error> {
        if self.poisoned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::internal());
        }
        Ok(self
            .image
            .lock()
            .map_err(|_| Error::internal())?
            .bindings
            .values()
            .cloned()
            .collect())
    }
    pub fn save_binding(
        &self,
        binding: super::directory_sync::model::Binding,
    ) -> Result<(), Error> {
        let mut image = self.image.lock().map_err(|_| Error::internal())?;
        if self.poisoned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::internal());
        }
        if !image.bindings.contains_key(&binding.id) && image.bindings.len() >= 128 {
            return Err(Error::too_many("BINDING_LIMIT"));
        }
        let mut next = image.clone();
        next.bindings.insert(binding.id.clone(), binding);
        self.persist(&next)?;
        *image = next;
        Ok(())
    }
    pub fn remove_binding(&self, id: &str) -> Result<(), Error> {
        let mut image = self.image.lock().map_err(|_| Error::internal())?;
        if self.poisoned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::internal());
        }
        let mut next = image.clone();
        next.bindings.remove(id);
        self.persist(&next)?;
        *image = next;
        Ok(())
    }
    pub fn open(root: &Path, server_id: &str) -> Result<Self, Error> {
        std::fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc_flags())
            .open(root.join("projects.lock"))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
        let image = match std::fs::symlink_metadata(root.join("projects.json")) {
            Ok(meta)
                if meta.is_file()
                    && !meta.file_type().is_symlink()
                    && meta.len() <= 8 * 1024 * 1024 =>
            {
                let bytes = std::fs::read(root.join("projects.json"))?;
                let image: Image = serde_json::from_slice(&bytes).map_err(|_| Error::internal())?;
                if image.version != 1 || image.server_id != server_id || image.projects.len() > 1024
                {
                    return Err(Error::internal());
                }
                image
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Image {
                version: 1,
                server_id: server_id.into(),
                ..Default::default()
            },
            _ => return Err(Error::internal()),
        };
        Ok(Self {
            root,
            _lock: lock,
            image: Mutex::new(image),
            poisoned: Default::default(),
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn all(&self) -> Result<Vec<Project>, Error> {
        if self.poisoned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::internal());
        }
        Ok(self
            .image
            .lock()
            .map_err(|_| Error::internal())?
            .projects
            .values()
            .cloned()
            .collect())
    }
    pub fn get(&self, identity: usize, id: &str) -> Result<Project, Error> {
        if self.poisoned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::internal());
        }
        self.image
            .lock()
            .map_err(|_| Error::internal())?
            .projects
            .get(id)
            .filter(|p| p.owner == identity)
            .cloned()
            .ok_or_else(|| Error::not_found("PROJECT_UNKNOWN"))
    }
    pub fn save(&self, project: Project, expected: Option<u64>) -> Result<Project, Error> {
        let mut image = self.image.lock().map_err(|_| Error::internal())?;
        if self.poisoned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::internal());
        }
        let previous = image.projects.get(&project.id);
        if image.bindings.values().any(|b| {
            b.project_id == project.id && b.plan.as_ref().is_some_and(|p| p.state == "applying")
        }) {
            return Err(Error::busy());
        }
        if previous.map(|p| p.revision) != expected {
            return Err(Error::conflict("PROJECT_REVISION_CHANGED"));
        }
        if previous.is_none() && image.projects.len() >= 1024 {
            return Err(Error::too_many("PROJECT_LIMIT"));
        }
        let mut next = image.clone();
        next.projects.insert(project.id.clone(), project.clone());
        self.persist(&next)?;
        *image = next;
        Ok(project)
    }
    pub fn remove(&self, identity: usize, id: &str, revision: u64) -> Result<(), Error> {
        let mut image = self.image.lock().map_err(|_| Error::internal())?;
        if self.poisoned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::internal());
        }
        let previous = image
            .projects
            .get(id)
            .filter(|p| p.owner == identity)
            .ok_or_else(|| Error::not_found("PROJECT_UNKNOWN"))?;
        if image
            .bindings
            .values()
            .any(|b| b.project_id == id && b.plan.as_ref().is_some_and(|p| p.state == "applying"))
        {
            return Err(Error::busy());
        }
        if previous.revision != revision {
            return Err(Error::conflict("PROJECT_REVISION_CHANGED"));
        }
        let mut next = image.clone();
        next.projects.remove(id);
        next.bindings.retain(|_, b| b.project_id != id);
        self.persist(&next)?;
        *image = next;
        Ok(())
    }
    fn persist(&self, image: &Image) -> Result<(), Error> {
        let bytes = serde_json::to_vec(image).map_err(|_| Error::internal())?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(Error::too_large("PROJECT_LIMIT"));
        }
        let temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        temporary.as_file().write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.root.join("projects.json"))
            .map_err(|_| Error::internal())?;
        if File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .is_err()
        {
            self.poisoned
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(Error::internal());
        }
        Ok(())
    }
}
fn libc_flags() -> i32 {
    rustix::fs::OFlags::NOFOLLOW.bits() as i32
}
