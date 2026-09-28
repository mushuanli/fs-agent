use crate::error::{invalid, Error};
use crate::revision::Revisions;
use rustix::fs::{openat2, Mode, OFlags, ResolveFlags};
use serde::Serialize;
use std::{fs::File, sync::Mutex, time::UNIX_EPOCH};

pub struct Export {
    pub(crate) root: File,
    pub(crate) revisions: Option<Mutex<Revisions>>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stat {
    pub kind: &'static str,
    pub size: u64,
    pub modified_at: u64,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}
#[derive(Serialize)]
pub struct Entry {
    pub name: String,
    pub stat: Stat,
}

impl Export {
    pub fn open(root: &str) -> Result<Self, Error> {
        let root = File::open(root)?;
        if !root.metadata()?.is_dir() {
            return Err(invalid());
        }
        let export = Self {
            root,
            revisions: None,
        };
        // Fail closed on systems without openat2 rather than falling back to path checks.
        export.open_path("", OFlags::PATH)?;
        Ok(export)
    }
    pub fn exclusive(root: &str) -> Result<Self, Error> {
        let mut export = Self::open(root)?;
        rustix::fs::flock(
            &export.root,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )?;
        export.revisions = Some(Mutex::new(Revisions::new()?));
        crate::recovery::clean_uploads(&export)?;
        Ok(export)
    }
    pub fn writable(&self) -> bool {
        self.revisions.is_some()
    }
    pub(crate) fn open_path(&self, path: &str, flags: OFlags) -> Result<File, Error> {
        validate_path(path)?;
        let flags = flags | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let flags = if flags.contains(OFlags::PATH) {
            flags
        } else {
            flags | OFlags::NONBLOCK
        };
        let fd = openat2(
            &self.root,
            if path.is_empty() { "." } else { path },
            flags,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        )?;
        Ok(File::from(fd))
    }
    pub fn stat(&self, path: &str) -> Result<Option<Stat>, Error> {
        let mut revisions = self.revisions.as_ref().map(|v| v.lock().unwrap());
        match self.open_path(path, OFlags::PATH) {
            Ok(file) => {
                let mut stat = attributes(&file)?;
                stat.revision = revisions.as_mut().map(|r| r.get(&file)).transpose()?;
                Ok(Some(stat))
            }
            Err(Error(axum::http::StatusCode::NOT_FOUND, _)) => Ok(None),
            Err(error) => Err(error),
        }
    }
    pub fn read(&self, path: &str) -> Result<(File, Option<String>), Error> {
        let mut revisions = self.revisions.as_ref().map(|v| v.lock().unwrap());
        let file = self.open_path(path, OFlags::RDONLY)?;
        if !file.metadata()?.is_file() {
            return Err(Error(
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "ECAPABILITY",
            ));
        }
        let revision = revisions.as_mut().map(|r| r.get(&file)).transpose()?;
        Ok((file, revision))
    }
    pub fn attributes(&self, file: &File) -> Result<Stat, Error> {
        let mut stat = attributes(file)?;
        stat.revision = self
            .revisions
            .as_ref()
            .map(|r| r.lock().unwrap().get(file))
            .transpose()?;
        Ok(stat)
    }
    pub fn list(
        &self,
        path: &str,
        cancelled: &tokio_util::sync::CancellationToken,
    ) -> Result<(Vec<Entry>, Vec<String>), Error> {
        let file = self.open_path(path, OFlags::RDONLY | OFlags::DIRECTORY)?;
        let mut entries = Vec::new();
        let mut warnings = Vec::new();
        for item in rustix::fs::Dir::read_from(&file)? {
            if cancelled.is_cancelled() {
                return Err(Error(axum::http::StatusCode::REQUEST_TIMEOUT, "ECANCELLED"));
            }
            let item = item?;
            let Ok(name) = item.file_name().to_str() else {
                warnings.push("NON_UTF8_NAME".into());
                continue;
            };
            if name == "." || name == ".." || name.starts_with(".itookit-upload-") {
                continue;
            }
            if entries.len() + warnings.len() >= 100_000 {
                return Err(Error(
                    axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                    "DIRECTORY_LIMIT",
                ));
            }
            let child = if path.is_empty() {
                name.to_owned()
            } else {
                format!("{path}/{name}")
            };
            match self.stat(&child) {
                Ok(Some(stat)) => entries.push(Entry {
                    name: name.into(),
                    stat,
                }),
                Ok(None) => {}
                Err(Error(axum::http::StatusCode::UNPROCESSABLE_ENTITY, _)) => {
                    warnings.push("UNSUPPORTED_NODE".into())
                }
                Err(error) => return Err(error),
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((entries, warnings))
    }
}

pub(crate) fn attributes(file: &File) -> Result<Stat, Error> {
    let metadata = file.metadata()?;
    let kind = if metadata.is_symlink() {
        "symlink"
    } else if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else {
        return Err(Error(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "ECAPABILITY",
        ));
    };
    let millis = |time: std::io::Result<std::time::SystemTime>| {
        time.ok()
            .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
            .map(|v| v.as_millis().min(9_007_199_254_740_991) as u64)
            .unwrap_or(0)
    };
    Ok(Stat {
        kind,
        size: metadata.len(),
        modified_at: millis(metadata.modified()),
        created_at: millis(metadata.created()),
        revision: None,
    })
}
pub fn validate_path(path: &str) -> Result<(), Error> {
    if path.len() > 4096
        || path.starts_with('/')
        || path.contains(['\\', '\0', ':'])
        || path.split('/').any(|part| {
            part == ".."
                || part == "."
                || part.starts_with(".itookit-upload-")
                || (part.is_empty() && !path.is_empty())
        })
    {
        return Err(invalid());
    }
    Ok(())
}
