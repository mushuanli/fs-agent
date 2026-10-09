//! Searches parsed, authorized native history instead of scanning an entire harness home.
use super::{bridge::Bridge, codex::Codex};
use crate::core::error::Error;
use serde_json::{json, Value};
use std::{collections::HashSet, time::Duration};
static SEARCHES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

pub async fn search(codex: &Codex, bridge: &Bridge, args: &Value) -> Result<Value, Error> {
    let _permit = SEARCHES
        .try_acquire()
        .map_err(|_| Error::too_many("SEARCH_BUSY"))?;
    let query = args["query"]
        .as_str()
        .filter(|q| !q.trim().is_empty() && q.len() <= 1024 && !q.contains('\0'))
        .ok_or_else(Error::invalid)?;
    let mode = args["mode"]
        .as_str()
        .filter(|m| matches!(*m, "title" | "content"))
        .ok_or_else(Error::invalid)?;
    tokio::time::timeout(
        Duration::from_secs(15),
        collect(codex, bridge, args, query, mode),
    )
    .await
    .map_err(|_| Error::timed_out())?
}
async fn collect(
    codex: &Codex,
    bridge: &Bridge,
    args: &Value,
    query: &str,
    mode: &str,
) -> Result<Value, Error> {
    let mut listing = args.clone();
    listing["limit"] = json!(100);
    let mut matches = Vec::new();
    let mut truncated = false;
    let mut bytes = 0usize;
    let mut seen = HashSet::new();
    for _ in 0..20 {
        let page = codex.sessions(bridge, &listing).await?;
        for session in page["sessions"].as_array().ok_or_else(Error::internal)? {
            if mode == "title" {
                if session["title"]
                    .as_str()
                    .is_some_and(|title| title.to_lowercase().contains(&query.to_lowercase()))
                {
                    matches.push(json!({"sessionId":session["id"],"title":session["title"],"summary":session["title"],"updatedAt":session["updatedAt"]}));
                }
            } else {
                search_history(
                    codex,
                    bridge,
                    args,
                    session,
                    query,
                    &mut matches,
                    &mut bytes,
                    &mut truncated,
                )
                .await?;
            }
            if matches.len() >= 100 || bytes >= 16 * 1024 * 1024 {
                truncated = true;
                break;
            }
        }
        if truncated || page["nextCursor"].is_null() {
            listing["cursor"] = Value::Null;
            break;
        }
        let cursor = page["nextCursor"].as_str().ok_or_else(Error::internal)?;
        if !seen.insert(cursor.to_string()) {
            return Err(Error::internal());
        }
        listing["cursor"] = json!(cursor);
    }
    truncated |= !listing["cursor"].is_null();
    matches.truncate(100);
    Ok(json!({"matches":matches,"truncated":truncated,"nextCursor":null}))
}
async fn search_history(
    codex: &Codex,
    bridge: &Bridge,
    args: &Value,
    session: &Value,
    query: &str,
    matches: &mut Vec<Value>,
    bytes: &mut usize,
    truncated: &mut bool,
) -> Result<(), Error> {
    let mut input = args.clone();
    input["sessionId"] = session["id"].clone();
    input["toolDetail"] = json!("summary");
    input["cursor"] = Value::Null;
    let mut seen = HashSet::new();
    for _ in 0..64 {
        let page = codex.history(bridge, &input).await?;
        *bytes += page.to_string().len();
        if *bytes > 16 * 1024 * 1024 {
            *truncated = true;
            break;
        }
        for turn in page["turns"].as_array().ok_or_else(Error::internal)? {
            for item in turn["items"].as_array().into_iter().flatten() {
                let text = displayed_text(item);
                if !text.to_lowercase().contains(&query.to_lowercase()) {
                    continue;
                }
                matches.push(json!({"sessionId":session["id"],"title":session["title"],"updatedAt":session["updatedAt"],
                    "turnId":if item["turnId"].is_string() {&item["turnId"]} else {&turn["id"]},"itemId":item["id"],"summary":excerpt(&text, query)}));
                if matches.len() >= 100 {
                    *truncated = true;
                    return Ok(());
                }
            }
        }
        if page["nextCursor"].is_null() {
            return Ok(());
        }
        let cursor = page["nextCursor"].as_str().ok_or_else(Error::internal)?;
        if !seen.insert(cursor.to_string()) {
            return Err(Error::internal());
        }
        input["cursor"] = json!(cursor);
    }
    *truncated = true;
    Ok(())
}
fn displayed_text(item: &Value) -> String {
    match item["type"].as_str() {
        Some("agentMessage" | "userMessage") => item["text"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| text_parts(&item["content"]).join("\n")),
        Some("commandExecution") => item["command"]
            .as_str()
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .into(),
        _ => String::new(),
    }
}
fn text_parts(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(parts) => parts.iter().flat_map(text_parts).collect(),
        Value::Object(_) => text_parts(if value["text"].is_string() {
            &value["text"]
        } else {
            &value["content"]
        }),
        _ => vec![],
    }
}
fn excerpt(text: &str, query: &str) -> String {
    let start = text.to_lowercase().find(&query.to_lowercase()).unwrap_or(0);
    // Lowercasing can change UTF-8 length; character iteration keeps excerpts valid.
    let start = text
        .char_indices()
        .take_while(|(at, _)| *at < start)
        .count()
        .saturating_sub(80);
    text.chars().skip(start).take(500).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_displayed_array_text_and_escaped_json_without_tool_payloads() {
        let item = json!({"type":"userMessage","content":[{"type":"text","text":"line\nquoted \"text\""},{"text":"中文"}]});
        assert_eq!(displayed_text(&item), "line\nquoted \"text\"\n中文");
        assert_eq!(
            displayed_text(&json!({"type":"toolOutput","text":"secret"})),
            ""
        );
        assert!(!excerpt("中文 quoted", "quoted").is_empty());
    }
}
