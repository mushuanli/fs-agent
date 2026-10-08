use super::model::Content;
use crate::{core::error::Error, fs::Export, projects::service::identity_of};
use sha2::{Digest, Sha256};
use std::{io::Read, os::unix::fs::MetadataExt};
const MAX_FILE: u64 = 32 * 1024 * 1024;
pub fn join(root: &str, path: &str) -> String {
    [root, path]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}
pub fn capture(export: &Export, path: &str) -> Result<Option<Content>, Error> {
    Ok(snapshot(export, path)?.0)
}
pub fn snapshot(export: &Export, path: &str) -> Result<(Option<Content>, Option<String>), Error> {
    let Some(stat) = export.stat(path)? else {
        return Ok((None, None));
    };
    if stat.kind == "directory" {
        let content = Content {
            kind: "directory".into(),
            hash: None,
            executable: false,
            identity: Some(identity_of(&export.open_dir(path)?)?),
        };
        return Ok((Some(content), None));
    }
    if stat.kind != "file" || stat.size > MAX_FILE {
        return Err(Error::unsupported());
    }
    file_snapshot(export, path)
}
fn file_snapshot(export: &Export, path: &str) -> Result<(Option<Content>, Option<String>), Error> {
    let (mut file, revision) = export.read(path)?;
    let before = file.metadata()?;
    let hash = hash_file(&mut file)?;
    if stamp(&before) != stamp(&file.metadata()?) {
        return Err(Error::conflict("TARGET_CHANGED"));
    }
    let content = Content {
        kind: "file".into(),
        hash: Some(hash),
        executable: before.mode() & 0o111 != 0,
        identity: None,
    };
    Ok((Some(content), revision))
}
fn hash_file(file: &mut std::fs::File) -> Result<String, Error> {
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    let mut total = 0;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_FILE {
            return Err(Error::too_large("SYNC_FILE_LIMIT"));
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn stamp(meta: &std::fs::Metadata) -> (u64, u64, i64, i64, u64, u32) {
    (
        meta.dev(),
        meta.ino(),
        meta.ctime(),
        meta.ctime_nsec(),
        meta.len(),
        meta.mode(),
    )
}
pub fn same(actual: Option<&Content>, desired: &Content) -> bool {
    actual.is_some_and(|a| {
        a.kind == desired.kind && a.hash == desired.hash && a.executable == desired.executable
    })
}
pub fn sync_parent(export: &Export, path: &str) -> Result<(), Error> {
    export
        .open_dir(path.rsplit_once('/').map_or("", |(parent, _)| parent))?
        .sync_all()?;
    Ok(())
}
