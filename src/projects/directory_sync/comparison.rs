//! Content previews refer to a captured plan; they never read an arbitrary path.
use super::{
    files,
    model::{Binding, Content},
};
use crate::{app::State, core::error::Error, fs::Export};
use serde_json::{json, Value};
use std::io::Read;
const LIMIT: u64 = 256 * 1024;

pub(super) fn compare(
    state: &State,
    export: &Export,
    binding: &Binding,
    id: &str,
    path: &str,
) -> Result<Value, Error> {
    let plan = binding
        .plan
        .as_ref()
        .filter(|p| p.id == id && p.state == "ready")
        .ok_or_else(|| Error::conflict("PLAN_CHANGED"))?;
    let detail = plan
        .conflict_details
        .iter()
        .find(|c| c.path == path)
        .ok_or_else(Error::invalid)?;
    check_directory(state, export, binding, path, detail.directory.as_ref())?;
    let native = files::join(&binding.source, path);
    Ok(
        json!({"path":path,"baseline":object_preview(state,binding,detail.baseline.as_ref()),
        "dataset":object_preview(state,binding,detail.dataset.as_ref()),
        "directory":directory_preview(export,&native,detail.directory.as_ref())?}),
    )
}
fn check_directory(
    state: &State,
    export: &Export,
    binding: &Binding,
    path: &str,
    expected: Option<&Content>,
) -> Result<(), Error> {
    super::allowed(
        &super::authorize(state, binding)?.0,
        &files::join(&binding.target, path),
    )?;
    let native = files::join(&binding.source, path);
    if files::capture(export, &native)?.as_ref() != expected {
        return Err(Error::conflict("TARGET_CHANGED"));
    }
    Ok(())
}
fn object_preview(state: &State, binding: &Binding, content: Option<&Content>) -> Value {
    let mut result = metadata(content);
    let Some(hash) = content.and_then(|c| c.hash.as_ref()) else {
        return result;
    };
    let object = state
        .sync
        .as_ref()
        .unwrap()
        .object_file(&binding.sync_project_id, hash);
    match object {
        Ok((file, size)) => {
            result["size"] = json!(size);
            result["content"] =
                read(file, size).unwrap_or_else(|_| json!({"reason":"unavailable"}));
        }
        Err(_) => {
            result["content"] = json!({"reason":"unavailable"});
        }
    }
    result
}
fn directory_preview(
    export: &Export,
    path: &str,
    content: Option<&Content>,
) -> Result<Value, Error> {
    let mut result = metadata(content);
    if content.is_some_and(|c| c.kind == "file") {
        let (file, _) = export.read(path)?;
        let size = file.metadata()?.len();
        result["size"] = json!(size);
        result["content"] = read(file, size)?;
        if files::capture(export, path)?.as_ref() != content {
            return Err(Error::conflict("TARGET_CHANGED"));
        }
    }
    Ok(result)
}
fn metadata(content: Option<&Content>) -> Value {
    match content {
        Some(c) => json!({"kind":c.kind,"hash":c.hash,"executable":c.executable}),
        None => json!({"kind":"missing"}),
    }
}
fn read(mut file: std::fs::File, size: u64) -> Result<Value, Error> {
    if size > LIMIT {
        return Ok(json!({"reason":"too-large"}));
    }
    let mut bytes = vec![];
    Read::by_ref(&mut file)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != size {
        return Err(Error::conflict("TARGET_CHANGED"));
    }
    match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Ok(json!({"text":text})),
        _ => Ok(json!({"reason":"binary"})),
    }
}
