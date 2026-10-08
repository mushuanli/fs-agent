//! Bounded native metadata and compact tool presentation, independent of the UI.
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub fn time_ms(value: &Value) -> Value {
    if let Some(time) = value.as_i64() {
        if time <= 0 {
            return Value::Null;
        }
        return json!(if time.unsigned_abs() < 100_000_000_000 {
            time.saturating_mul(1000)
        } else {
            time
        });
    }
    value
        .as_str()
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|time| json!(time.timestamp_millis()))
        .unwrap_or(Value::Null)
}
pub fn session_title(thread: &Value) -> &str {
    ["name", "preview"]
        .iter()
        .filter_map(|key| thread[*key].as_str())
        .map(str::trim)
        .find(|title| !title.is_empty())
        .unwrap_or("")
}
pub fn item(value: &Value) -> Option<Value> {
    let kind = value["type"].as_str().unwrap_or("");
    if matches!(kind, "toolOutput" | "customToolCallOutput") {
        return None;
    }
    if !is_tool(kind) {
        return Some(value.clone());
    }
    let name = value["tool"]
        .as_str()
        .or(value["name"].as_str())
        .unwrap_or(kind);
    let input = arguments(value);
    let parsed = input
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or_else(|| input.clone());
    let command = command(value, &parsed, input, name);
    let paths = targets(value, &parsed, input, command);
    let preview = command_preview(value["commandPreview"].as_str().unwrap_or(command));
    let action = operation(value, &format!("{kind} {name} {preview}").to_lowercase());
    Some(
        json!({"id":value["id"],"type":kind,"name":name.chars().take(256).collect::<String>(),
        "paths":paths,"operation":action,"commandPreview":preview,
        "timestamp":value["timestamp"],"turnId":value["turnId"],"status":value["status"]}),
    )
}
fn command<'a>(value: &'a Value, parsed: &'a Value, input: &'a Value, name: &str) -> &'a str {
    value["command"]
        .as_str()
        .or(parsed["cmd"].as_str())
        .or(parsed["command"].as_str())
        .or(parsed["code"].as_str())
        .or_else(|| {
            (["exec", "exec_command", "shell", "bash"].contains(&name) && parsed.is_string())
                .then(|| input.as_str())
                .flatten()
        })
        .unwrap_or("")
}
fn command_preview(command: &str) -> String {
    let line = command
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let mut preview = line.chars().take(240).collect::<String>();
    if line.chars().count() > 240 {
        preview.push('…');
    }
    preview
}
fn arguments(value: &Value) -> &Value {
    value
        .get("arguments")
        .filter(|input| !input.is_null())
        .or(value.get("input"))
        .unwrap_or(&Value::Null)
}
fn is_tool(kind: &str) -> bool {
    kind.contains("Execution")
        || kind.contains("Change")
        || kind.contains("ToolCall")
        || matches!(kind, "toolCall" | "webSearch" | "imageGeneration")
}
fn targets(value: &Value, parsed: &Value, input: &Value, command: &str) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    collect_paths(value, &mut paths, 0);
    collect_paths(parsed, &mut paths, 0);
    if let Some(text) = input.as_str() {
        patch_paths(text, &mut paths);
    }
    for path in command_paths(command) {
        insert_path(&mut paths, path);
    }
    paths
}
fn command_paths(command: &str) -> Vec<&str> {
    if command.contains(['|', ';', '&', '<', '>', '`', '\n']) {
        return vec![];
    }
    let mut tokens = command.split_whitespace();
    let name = tokens.next().unwrap_or("").rsplit('/').next().unwrap_or("");
    if !["cat", "head", "tail", "ls", "find", "rg", "grep", "sed"].contains(&name) {
        return vec![];
    }
    let tokens = tokens.collect::<Vec<_>>();
    if tokens.iter().any(|token| {
        token.starts_with('-')
            && !["-n", "-i", "-l", "-r", "-R", "-S", "-s", "--hidden"].contains(token)
    }) {
        return vec![];
    }
    let mut operands = tokens.into_iter().filter(|token| !token.starts_with('-'));
    if ["rg", "grep", "sed"].contains(&name) {
        operands.next();
    }
    operands
        .filter(|path| {
            !path.contains(['\"', '\''])
                && !path.contains("://")
                && (path.starts_with(['.', '/', '~'])
                    || path.rsplit_once('.').is_some_and(|(_, extension)| {
                        extension.len() <= 8 && extension.chars().all(|c| c.is_ascii_alphanumeric())
                    }))
        })
        .collect()
}
fn patch_paths(text: &str, paths: &mut BTreeSet<String>) {
    for line in text.lines() {
        for prefix in ["*** Add File: ", "*** Update File: ", "*** Delete File: "] {
            if let Some(path) = line.strip_prefix(prefix) {
                insert_path(paths, path);
            }
        }
    }
}
fn operation<'a>(value: &'a Value, hint: &str) -> &'a str {
    if let Some(action) = value["operation"]
        .as_str()
        .filter(|action| ["read", "write", "search", "list", "execute"].contains(action))
    {
        return action;
    }
    for (action, words) in [
        ("write", &["change", "patch", "write", "edit"][..]),
        ("search", &["search", "grep", "rg "][..]),
        ("read", &["read", "cat ", "sed "][..]),
        ("list", &["list", "ls "][..]),
    ] {
        if words.iter().any(|word| hint.contains(word)) {
            return action;
        }
    }
    "execute"
}
fn insert_path(paths: &mut BTreeSet<String>, path: &str) {
    if paths.len() < 32 {
        paths.insert(path.chars().take(1024).collect());
    }
}
fn collect_paths(value: &Value, paths: &mut BTreeSet<String>, depth: usize) {
    if depth > 4 || paths.len() >= 32 {
        return;
    }
    match value {
        Value::Object(fields) => {
            for (key, child) in fields {
                path_field(key, child, paths, depth);
            }
        }
        Value::Array(items) => {
            for child in items.iter().take(32) {
                collect_paths(child, paths, depth + 1);
            }
        }
        _ => (),
    }
}
fn path_field(key: &str, child: &Value, paths: &mut BTreeSet<String>, depth: usize) {
    if [
        "path",
        "file_path",
        "filePath",
        "filename",
        "target",
        "cwd",
        "workdir",
    ]
    .contains(&key)
    {
        if let Some(path) = child.as_str() {
            insert_path(paths, path);
        }
    } else if ["paths", "files"].contains(&key) {
        if let Some(items) = child.as_array() {
            for path in items.iter().take(32).filter_map(Value::as_str) {
                insert_path(paths, path);
            }
        }
    } else {
        collect_paths(child, paths, depth + 1);
    }
}
pub fn events(mut page: Value) -> Value {
    if let Some(events) = page["events"].as_array_mut() {
        events.retain_mut(|event| {
            let message = &mut event["message"];
            if message["method"] == "item/commandExecution/outputDelta" {
                return false;
            }
            if matches!(
                message["method"].as_str(),
                Some("item/started" | "item/completed")
            ) {
                let Some(summary) = item(&message["params"]["item"]) else {
                    return false;
                };
                message["params"]["item"] = summary;
            }
            true
        });
    }
    page
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn time_accepts_seconds_milliseconds_and_rfc3339_without_changing_the_instant() {
        let instant = 1791458565000i64;
        assert_eq!(time_ms(&json!(instant / 1000)), instant);
        assert_eq!(time_ms(&json!(instant)), instant);
        assert_eq!(time_ms(&json!("2026-10-08T19:22:45+08:00")), instant);
        assert!(time_ms(&json!("unknown")).is_null());
        assert!(time_ms(&json!(0)).is_null());
        assert!(time_ms(&json!(-1)).is_null());
    }
    #[test]
    fn titles_use_the_first_nonempty_native_name_or_preview() {
        assert_eq!(
            session_title(&json!({"name":" Named thread ","preview":"Prompt"})),
            "Named thread"
        );
        assert_eq!(
            session_title(&json!({"name":"  ","preview":" First request "})),
            "First request"
        );
        assert_eq!(session_title(&json!({"name":null,"preview":""})), "");
    }
    #[test]
    fn event_summary_preserves_cursor_requests_and_all_assistant_items() {
        let page = json!({"cursor":4,"requests":[{"id":"approval"}],"events":[
            {"seq":1,"message":{"method":"item/started","params":{"item":{"type":"commandExecution","id":"tool","command":"cat src/main.ts","aggregatedOutput":"private"}}}},
            {"seq":2,"message":{"method":"item/commandExecution/outputDelta","params":{"delta":"private"}}},
            {"seq":3,"message":{"method":"item/agentMessage/delta","params":{"delta":"Answer one"}}},
            {"seq":4,"message":{"method":"item/completed","params":{"item":{"type":"agentMessage","text":"Answer two"}}}}
        ]});
        let result = events(page);
        assert_eq!(result["cursor"], 4);
        assert_eq!(result["requests"][0]["id"], "approval");
        assert_eq!(result["events"].as_array().unwrap().len(), 3);
        let tool = &result["events"][0]["message"]["params"]["item"];
        assert_eq!(tool["paths"], json!(["src/main.ts"]));
        assert_eq!(tool["commandPreview"], "cat src/main.ts");
        assert!(tool.get("command").is_none());
        assert!(tool.get("aggregatedOutput").is_none());
        assert_eq!(
            result["events"][2]["message"]["params"]["item"]["text"],
            "Answer two"
        );
    }
    #[test]
    fn tool_targets_do_not_include_code_command_arguments_or_search_patterns() {
        for command in [
            "curl -H Authorization:Bearer.header.value https://example.com",
            "node -e private.code.value",
            "echo private.code.value",
        ] {
            assert!(command_paths(command).is_empty());
        }
        assert_eq!(
            command_paths("rg pattern.ts src/main.ts"),
            vec!["src/main.ts"]
        );
        assert!(command_paths("cat \"src/my file.ts\"").is_empty());
    }
    #[test]
    fn tool_summary_keeps_only_a_bounded_first_command_line_across_repeated_projection() {
        let native = json!({"type":"toolCall","name":"exec","arguments":r#"{"cmd":"\n pnpm typecheck \ncat private-output","workdir":"/project"}"#});
        let summary = item(&native).unwrap();
        assert_eq!(summary["commandPreview"], "pnpm typecheck");
        assert_eq!(summary["paths"], json!(["/project"]));
        assert_eq!(
            item(&summary).unwrap()["commandPreview"],
            summary["commandPreview"]
        );
        assert!(!summary.to_string().contains("private-output"));
        let large = item(&json!({"type":"commandExecution","command":"x".repeat(300)})).unwrap();
        assert_eq!(
            large["commandPreview"].as_str().unwrap().chars().count(),
            241
        );
    }
}
