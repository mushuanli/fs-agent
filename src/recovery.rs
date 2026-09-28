use crate::{error::Error, filesystem::Export};
use rustix::fs::{AtFlags, OFlags};

/// Reserved staging names are never user-addressable. Cleanup runs with the writer lock held.
pub fn clean_uploads(export: &Export) -> Result<(), Error> {
    let mut pending = vec![String::new()];
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        let directory = export.open_path(&path, OFlags::RDONLY | OFlags::DIRECTORY)?;
        for entry in rustix::fs::Dir::read_from(&directory)? {
            let entry = entry?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if name == "." || name == ".." {
                continue;
            }
            visited += 1;
            if visited > 1_000_000 {
                return Err(crate::error::invalid());
            }
            if name.starts_with(".itookit-upload-") {
                // Only files created by this service have this reserved spelling.
                rustix::fs::unlinkat(&directory, name, AtFlags::empty())?;
            } else {
                let child = if path.is_empty() {
                    name.to_owned()
                } else {
                    format!("{path}/{name}")
                };
                if let Ok(file) = export.open_path(&child, OFlags::PATH) {
                    if file.metadata()?.is_dir() {
                        pending.push(child);
                    }
                }
            }
        }
    }
    Ok(())
}
