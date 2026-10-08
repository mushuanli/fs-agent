//! Read-only fallback for native history APIs that are not implemented by a CLI version.
use crate::{core::error::Error, fs::Export};
use serde_json::{json, Value};
use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
};

pub async fn read(
    home: &Path,
    path: &str,
    before: Option<u64>,
    summary: bool,
) -> Result<Value, Error> {
    let home = home.to_owned();
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || read_log(&home, &path, before, summary))
        .await
        .map_err(|_| Error::internal())?
}

fn relative_log<'a>(home: &Path, path: &'a str) -> Result<&'a str, Error> {
    let relative = Path::new(path)
        .strip_prefix(home)
        .map_err(|_| Error::forbidden("EACCES"))?;
    let relative = relative.to_str().ok_or_else(Error::invalid)?;
    if !(relative.starts_with("sessions/") || relative.starts_with("archived_sessions/"))
        || !relative.ends_with(".jsonl")
    {
        return Err(Error::forbidden("EACCES"));
    }
    Ok(relative)
}
fn open_log(home: &Path, path: &str) -> Result<std::fs::File, Error> {
    let relative = relative_log(home, path)?;
    // Directory capabilities reject symlinks and traversal even if native metadata is stale.
    let export = Export::open(home)?;
    let file = export.open_dir("")?;
    let file = rustix::fs::openat2(
        &file,
        relative,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
        rustix::fs::ResolveFlags::BENEATH
            | rustix::fs::ResolveFlags::NO_SYMLINKS
            | rustix::fs::ResolveFlags::NO_MAGICLINKS,
    )?;
    Ok(std::fs::File::from(file))
}
fn read_log(home: &Path, path: &str, before: Option<u64>, summary: bool) -> Result<Value, Error> {
    let mut file = open_log(home, path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::invalid());
    }
    let end = before.unwrap_or(metadata.len());
    if end > metadata.len() {
        return Err(Error::invalid());
    }
    let start = end.saturating_sub(8 * 1024 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    (&mut file).take(end - start).read_to_end(&mut bytes)?;
    let first = complete_lines(&bytes, start)?
        .first()
        .map(|(at, _)| *at)
        .unwrap_or(start);
    let prior = prior_turn(&mut file, first)?;
    page(&bytes, start, prior, summary)
}
fn context(record: &Value) -> Option<String> {
    if record["type"] != "turn_context"
        && !(record["type"] == "event_msg" && record["payload"]["type"] == "task_started")
    {
        return None;
    }
    record["payload"]["turn_id"].as_str().map(str::to_owned)
}
fn prior_turn(file: &mut std::fs::File, mut end: u64) -> Result<Option<String>, Error> {
    // Search backwards using bounded buffers; large tool output does not allocate a large record.
    while end > 0 {
        let start = end.saturating_sub(256 * 1024);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        (&mut *file).take(end - start).read_to_end(&mut bytes)?;
        for line in bytes.split(|b| *b == b'\n').rev() {
            if !(line.windows(12).any(|word| word == b"turn_context")
                || line.windows(12).any(|word| word == b"task_started"))
            {
                continue;
            }
            if let Ok(record) = serde_json::from_slice::<Value>(line) {
                if let Some(id) = context(&record) {
                    return Ok(Some(id));
                }
            }
        }
        if start == 0 {
            break;
        }
        end = start.saturating_add(4096).min(end - 1);
    }
    Ok(None)
}
fn complete_lines(bytes: &[u8], start: u64) -> Result<Vec<(u64, &[u8])>, Error> {
    let skip = if start == 0 {
        0
    } else {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + 1)
            .ok_or_else(|| Error::too_large("HARNESS_HISTORY_RECORD_TOO_LARGE"))?
    };
    let mut offset = start + skip as u64;
    Ok(bytes[skip..]
        .split_inclusive(|b| *b == b'\n')
        .map(|line| {
            let at = offset;
            offset += line.len() as u64;
            (at, line)
        })
        .filter(|(_, line)| line.ends_with(b"\n"))
        .collect())
}
struct Page {
    items: Vec<Value>,
    budget: usize,
    next: u64,
}
impl Page {
    fn push(&mut self, at: u64, entry: Option<Value>) -> Result<bool, Error> {
        if let Some(entry) = entry {
            let size = entry.to_string().len();
            if size > 2 * 1024 * 1024 {
                return Err(Error::too_large("HARNESS_HISTORY_ITEM_TOO_LARGE"));
            }
            if size > self.budget || self.items.len() >= 100 {
                return Ok(false);
            }
            self.budget -= size;
            self.items.push(entry);
        }
        self.next = at;
        Ok(true)
    }
}
fn entries(
    lines: Vec<(u64, &[u8])>,
    mut turn: Option<String>,
    summary: bool,
) -> Result<Vec<(u64, Option<Value>)>, Error> {
    let mut entries = Vec::with_capacity(lines.len());
    for (at, line) in lines {
        let record: Value = serde_json::from_slice(line).map_err(|_| Error::internal())?;
        if let Some(id) = context(&record) {
            turn = Some(id);
        }
        let entry = record_item(&record, at, turn.as_deref()).and_then(|entry| {
            if summary {
                super::presentation::item(&entry)
            } else {
                Some(entry)
            }
        });
        entries.push((at, entry));
    }
    Ok(entries)
}
fn record_item(record: &Value, at: u64, turn: Option<&str>) -> Option<Value> {
    let mut entry = item(record)?;
    entry["id"] = record["payload"]["id"]
        .as_str()
        .map(|id| json!(id))
        .unwrap_or_else(|| json!(format!("rollout:{at}")));
    entry["timestamp"] = super::presentation::time_ms(&record["timestamp"]);
    entry["turnId"] = json!(record["payload"]["turn_id"].as_str().or(turn));
    Some(entry)
}
fn page(bytes: &[u8], start: u64, turn: Option<String>, summary: bool) -> Result<Value, Error> {
    let entries = entries(complete_lines(bytes, start)?, turn, summary)?;
    let mut page = Page {
        items: Vec::new(),
        budget: 2 * 1024 * 1024,
        next: entries.first().map(|(at, _)| *at).unwrap_or(start),
    };
    for (at, entry) in entries.into_iter().rev() {
        if !page.push(at, entry)? {
            break;
        }
    }
    page.items.reverse();
    Ok(
        json!({"turns":[{"items":page.items}],"nextCursor":if page.next > 0 {Some(page.next.to_string())} else {None}}),
    )
}

fn item(record: &Value) -> Option<Value> {
    if record["type"] != "response_item" {
        return None;
    }
    let payload = &record["payload"];
    match payload["type"].as_str()? {
        "message" => {
            let role = payload["role"].as_str()?;
            if !matches!(role, "user" | "assistant") {
                return None;
            }
            Some(
                json!({"type":if role=="user" {"userMessage"} else {"agentMessage"},"content":payload["content"]}),
            )
        }
        "function_call" | "custom_tool_call" => Some(
            json!({"type":"toolCall","name":payload["name"],"arguments":payload["arguments"],"input":payload["input"]}),
        ),
        "function_call_output" | "custom_tool_call_output" => {
            Some(json!({"type":"toolOutput","text":payload["output"]}))
        }
        "reasoning" => Some(json!({"type":"reasoning","summary":payload["summary"]})),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message(index: usize, text: &str) -> String {
        format!(
            "{}\n",
            json!({"type":"response_item","payload":{"type":"message","role":if index % 2 == 0 {"user"} else {"assistant"},"content":[{"type":"text","text":text}]}})
        )
    }
    #[test]
    fn large_history_pages_preserve_all_roles_and_stable_ids_without_duplicates() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("sessions");
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("session.jsonl");
        let text = "x".repeat(90_000);
        let records = (0..220).map(|i| message(i, &text)).collect::<String>();
        std::fs::write(&path, records + "{\"unfinished\":").unwrap();
        let mut cursor = None;
        let mut ids = std::collections::HashSet::new();
        let mut count = 0;
        loop {
            let page = read_log(home.path(), path.to_str().unwrap(), cursor, false).unwrap();
            for item in page["turns"][0]["items"].as_array().unwrap() {
                assert!(ids.insert(item["id"].as_str().unwrap().to_owned()));
                count += 1;
                assert!(matches!(
                    item["type"].as_str(),
                    Some("userMessage" | "agentMessage")
                ));
            }
            match page["nextCursor"].as_str() {
                Some(next) => cursor = Some(next.parse().unwrap()),
                None => break,
            }
        }
        assert_eq!(count, 220);
    }
    #[test]
    fn history_rejects_outside_paths_symlinks_and_invalid_cursors() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("sessions");
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("session.jsonl");
        std::fs::write(&path, message(0, "visible")).unwrap();
        assert!(read_log(home.path(), "/tmp/other.jsonl", None, false).is_err());
        assert!(read_log(home.path(), path.to_str().unwrap(), Some(u64::MAX), false).is_err());
        let link = folder.join("link.jsonl");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_log(home.path(), link.to_str().unwrap(), None, false).is_err());
    }
    #[test]
    fn summary_preserves_turns_times_request_arrays_and_custom_tools_without_large_outputs() {
        let records = [
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"first"}}),
            json!({"type":"response_item","timestamp":"2026-10-08T11:22:45Z","payload":{"type":"message","role":"user","content":[{"text":"Request one"},{"text":"Request two"}]}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call","id":"patch","name":"apply_patch","input":"*** Begin Patch\n*** Update File: src/main.ts\n@@\n-private\n+content\n*** End Patch"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call_output","output":"x".repeat(3 * 1024 * 1024)}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"text":"Answer one"},{"text":"Answer two"}]}}),
            json!({"type":"turn_context","payload":{"turn_id":"second"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"text":"Next request"}]}}),
        ];
        let bytes = records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>();
        let result = page(bytes.as_bytes(), 0, None, true).unwrap();
        let items = result["turns"][0]["items"].as_array().unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0]["turnId"], "first");
        assert_eq!(items[0]["timestamp"], 1791458565000i64);
        assert_eq!(items[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(items[1]["id"], "patch");
        assert_eq!(items[1]["paths"], json!(["src/main.ts"]));
        assert_eq!(items[1]["operation"], "write");
        assert!(items[1].get("input").is_none());
        assert_eq!(items[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(items[3]["turnId"], "second");
        assert!(result["turns"][0].get("id").is_none());
        assert!(result["nextCursor"].is_null());
    }
    #[test]
    fn pages_keep_the_turn_context_before_the_bounded_read_window() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("sessions");
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("session.jsonl");
        let context = format!(
            "{}\n",
            json!({"type":"turn_context","payload":{"turn_id":"native-turn"}})
        );
        let text = "x".repeat(90_000);
        let records = context + &(0..110).map(|i| message(i, &text)).collect::<String>();
        std::fs::write(&path, records).unwrap();
        let result = read_log(home.path(), path.to_str().unwrap(), None, true).unwrap();
        assert!(result["nextCursor"].is_string());
        assert!(result["turns"][0]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["turnId"] == "native-turn"));
    }
    #[test]
    fn compact_pages_reach_the_first_request_after_many_exec_tools() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("sessions")).unwrap();
        let path = home.path().join("sessions/session.jsonl");
        let mut records = message(0, "First request");
        for index in 0..230 {
            records.push_str(&format!("{}\n", json!({"type":"response_item","payload":{
                "id":format!("exec-{index}"),"type":"function_call","name":"exec",
                "arguments":json!({"cmd":"pnpm typecheck\ncat details","workdir":"/project"}).to_string()}})));
        }
        std::fs::write(&path, records + &message(1, "Last answer")).unwrap();
        let mut cursor = None;
        let mut messages = Vec::new();
        loop {
            let page = read_log(home.path(), path.to_str().unwrap(), cursor, true).unwrap();
            messages.splice(
                0..0,
                page["turns"][0]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned(),
            );
            match page["nextCursor"].as_str() {
                Some(next) => cursor = Some(next.parse().unwrap()),
                None => break,
            }
        }
        assert_eq!(messages.len(), 232);
        assert_eq!(messages[0]["content"][0]["text"], "First request");
        assert_eq!(
            messages.last().unwrap()["content"][0]["text"],
            "Last answer"
        );
        assert!(messages[1..231]
            .iter()
            .all(|item| item["commandPreview"] == "pnpm typecheck"));
    }
}
