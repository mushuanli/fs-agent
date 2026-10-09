//! Stateless MCP 2026-07-28 control endpoint. File bytes keep their HTTP transport.
use crate::{app::State, core::error::Error, http::access};
use axum::{
    extract::State as AxumState,
    http::{HeaderMap, StatusCode},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::sync::Arc;
const VERSION: &str = "2026-07-28";

pub async fn mcp(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), Error> {
    let identity = access::identity(&state, &headers)?;
    if let Some(origin) = headers.get("origin") {
        let origin = origin.to_str().map_err(|_| Error::forbidden("EACCES"))?;
        if !state
            .allowed_origins
            .iter()
            .any(|allowed| allowed == origin || allowed == "*")
        {
            return Err(Error::forbidden("EACCES"));
        }
    }
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    if body["jsonrpc"] != "2.0" || !(id.is_string() || id.is_number()) {
        return Ok(rpc_error(
            id,
            -32600,
            "Invalid request",
            json!({}),
            StatusCode::BAD_REQUEST,
        ));
    }
    if let Some(error) = validate(&headers, &body, id.clone()) {
        return Ok(error);
    }
    let method = body["method"].as_str().unwrap_or("");
    let result = match method {
        "server/discover" => {
            json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","supportedVersions":[VERSION],"capabilities":{"tools":{}},
            "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"pi-agent","version":env!("CARGO_PKG_VERSION")},
                "itookit/pi-agent":capabilities(&state)}})
        }
        "ping" => json!({"resultType":"complete"}),
        "tools/list" => {
            json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":tools()})
        }
        "tools/call" => call(&state, identity, &body["params"]).await,
        _ => {
            return Ok(rpc_error(
                id,
                -32601,
                "Method not found",
                json!({}),
                StatusCode::NOT_FOUND,
            ))
        }
    };
    Ok((
        StatusCode::OK,
        Json(json!({"jsonrpc":"2.0","id":id,"result":result})),
    ))
}

fn validate(headers: &HeaderMap, body: &Value, id: Value) -> Option<(StatusCode, Json<Value>)> {
    let meta = &body["params"]["_meta"];
    let version = meta["io.modelcontextprotocol/protocolVersion"]
        .as_str()
        .unwrap_or("");
    let method = body["method"].as_str().unwrap_or("");
    let pairs = [("mcp-protocol-version", version), ("mcp-method", method)];
    let mismatch = pairs
        .iter()
        .any(|(name, value)| headers.get(*name).and_then(|v| v.to_str().ok()) != Some(*value))
        || (method == "tools/call"
            && decoded(headers, "mcp-name").as_deref() != body["params"]["name"].as_str());
    if mismatch {
        return Some(rpc_error(
            id,
            -32020,
            "Header mismatch",
            json!({}),
            StatusCode::BAD_REQUEST,
        ));
    }
    if version != VERSION {
        return Some(rpc_error(
            id,
            -32022,
            "Unsupported protocol version",
            json!({"supported":[VERSION],"requested":version}),
            StatusCode::BAD_REQUEST,
        ));
    }
    if !meta["io.modelcontextprotocol/clientCapabilities"].is_object()
        || !meta["io.modelcontextprotocol/clientInfo"]["name"].is_string()
        || !meta["io.modelcontextprotocol/clientInfo"]["version"].is_string()
    {
        return Some(rpc_error(
            id,
            -32602,
            "Missing client metadata",
            json!({}),
            StatusCode::BAD_REQUEST,
        ));
    }
    None
}

fn decoded(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?;
    if let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|s| s.strip_suffix("?="))
    {
        return String::from_utf8(STANDARD.decode(encoded).ok()?).ok();
    }
    Some(value.to_owned())
}

fn rpc_error(
    id: Value,
    code: i64,
    message: &str,
    data: Value,
    status: StatusCode,
) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message,"data":data}})),
    )
}

async fn call(state: &Arc<State>, identity: usize, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result = if name == "piagent_capabilities" || name == "fsagent_capabilities" {
        Ok(capabilities(state))
    } else if name.starts_with("project_") {
        crate::projects::call(state, identity, name, args).await
    } else if args.get("projectId").is_some() {
        state
            .harness
            .project_call(state, identity, name, args)
            .await
    } else {
        state.harness.call(name, args).await
    };
    let (value, error) = match result {
        Ok(value) => (value, false),
        Err(e) => (
            json!({"code":e.code,"outcome":if name.starts_with("project_sync_") && (name=="project_sync_execute" || e.status.is_server_error()) {"unknown"}else{"not-committed"}}),
            true,
        ),
    };
    json!({"resultType":"complete","isError":error,"structuredContent":value,
        "content":[{"type":"text","text":value.to_string()}]})
}

fn capabilities(state: &State) -> Value {
    json!({"version":1,"httpEndpoint":"./","serverId":state.auth.server_id(),
        "fileProtocol":"fs-agent-http-v1","harness":state.harness.enabled(),
        "projects":state.projects.is_some(),"sync":state.sync.is_some(),
        "directorySync":state.projects.is_some() && state.sync.is_some(),
        "fileWatch":state.projects.is_some(),"fileSearch":state.projects.is_some() && state.execution.ready() && std::path::Path::new("/usr/bin/rg").is_file(),"directorySyncVersion":2,"projectProtocol":"fs-agent-project-v1"})
}

fn tools() -> Vec<Value> {
    let definitions = [
        (
            "project_sync_bind",
            "Bind a files dataset to a writable project directory",
            vec![
                "bindingId",
                "projectId",
                "revision",
                "syncProjectId",
                "datasetId",
                "historyEpoch",
            ],
        ),
        (
            "project_sync_status",
            "Read the durable directory synchronization status",
            vec!["bindingId"],
        ),
        (
            "project_sync_preview",
            "Preview directory changes without writing project files",
            vec!["bindingId"],
        ),
        (
            "project_sync_execute",
            "Apply or resume an explicitly reviewed directory plan",
            vec!["bindingId", "planId"],
        ),
        (
            "project_sync_unbind",
            "Remove a completed directory binding without deleting files",
            vec!["bindingId"],
        ),
        (
            "project_sync_configure",
            "Change direction and invalidate the previous preview",
            vec!["bindingId", "policyRevision", "direction"],
        ),
        (
            "project_sync_compare",
            "Compare bounded conflict content from a captured plan",
            vec!["bindingId", "planId", "path"],
        ),
        (
            "project_sync_directories",
            "Browse authorized project-relative synchronization directories",
            vec!["projectId", "revision", "path"],
        ),
        (
            "project_sync_resolve",
            "Select conflict sources and create a new plan for review",
            vec!["bindingId", "planId", "decisions"],
        ),
        (
            "project_roots",
            "List authorized roots in which projects may be registered",
            vec![],
        ),
        (
            "project_list",
            "List executable directory projects, separate from sync datasets",
            vec![],
        ),
        (
            "project_read",
            "Read the project directory grant and mount policy",
            vec!["projectId"],
        ),
        ("project_watch", "Observe bounded project directory changes without returning paths or content", vec!["projectId", "revision"]),
        ("project_unwatch", "Release a project directory observer", vec!["projectId", "revision", "watchId"]),
        (
            "project_search",
            "Search literal paths or text in the pinned read-only project view",
            vec!["projectId", "revision", "query", "mode"],
        ),
        (
            "project_register",
            "Register an existing directory or create one child under an authorized parent",
            vec!["name", "alias", "path", "access"],
        ),
        (
            "project_configure",
            "Replace project mounts using a revision fence; refuses active execution",
            vec!["projectId", "revision", "name", "mounts"],
        ),
        (
            "project_forget",
            "Forget a project grant without deleting directory contents",
            vec!["projectId", "revision"],
        ),
        (
            "project_exec",
            "Run a command using only the server-owned project grants",
            vec![
                "projectId",
                "revision",
                "epoch",
                "requestId",
                "command",
                "args",
            ],
        ),
        (
            "piagent_capabilities",
            "Discover this gateway's same-origin HTTP file endpoint and installation identity",
            vec![],
        ),
        (
            "fsagent_capabilities",
            "Compatibility alias for piagent_capabilities",
            vec![],
        ),
        (
            "harness_profiles",
            "List configured harness profiles and authorized workspaces",
            vec![],
        ),
        (
            "harness_sessions",
            "List stored sessions, including explicit source and archive filtering",
            vec!["profileId"],
        ),
        (
            "harness_session_read",
            "Read session history without resuming execution",
            vec!["profileId", "sessionId"],
        ),
        (
            "harness_session_info",
            "Read session metadata without loading history or resuming execution",
            vec!["profileId", "sessionId"],
        ),
        ("harness_session_search", "Search all authorized native session titles or parsed displayed history with explicit capacity limits", vec!["profileId", "query", "mode"]),
        ("harness_rename", "Rename an authorized native session without adopting execution", vec!["profileId", "epoch", "requestId", "sessionId", "name"]),
        ("harness_archive", "Archive an idle session owned by this gateway; preserves native history", vec!["profileId", "epoch", "requestId", "sessionId"]),
        ("harness_delete", "Permanently delete a native session and its authorized spawned descendants", vec!["profileId", "epoch", "requestId", "sessionId"]),
        ("harness_unarchive", "Restore an archived native session without starting a turn", vec!["profileId", "epoch", "requestId", "sessionId"]),
        (
            "harness_fork",
            "Create a native branch preserving the source history",
            vec!["profileId", "epoch", "requestId", "sessionId"],
        ),
        (
            "harness_events",
            "Poll ordered events and outstanding interactions; gap means history must be refreshed",
            vec!["profileId"],
        ),
        (
            "harness_operation",
            "Query a retained mutation receipt without replaying its effect",
            vec!["profileId", "epoch", "requestId"],
        ),
        (
            "harness_create",
            "Create a session in a configured workspace",
            vec!["profileId", "epoch", "requestId", "workspaceId"],
        ),
        (
            "harness_resume",
            "Resume a stored session whose working directory is authorized",
            vec!["profileId", "epoch", "requestId", "sessionId"],
        ),
        (
            "harness_turn",
            "Submit a message to a session owned by this gateway",
            vec!["profileId", "epoch", "requestId", "sessionId", "prompt"],
        ),
        (
            "harness_interrupt",
            "Interrupt a turn owned by this gateway",
            vec!["profileId", "epoch", "requestId", "sessionId", "turnId"],
        ),
        (
            "harness_respond",
            "Answer an outstanding native approval or user-input request",
            vec![
                "profileId",
                "epoch",
                "requestId",
                "nativeRequestId",
                "response",
            ],
        ),
    ];
    definitions
        .into_iter()
        .map(|(name, description, required)| tool(name, description, required))
        .collect()
}

fn tool(name: &str, description: &str, required: Vec<&str>) -> Value {
    let mut properties = serde_json::Map::new();
    for key in [
        "bindingId",
        "syncProjectId",
        "datasetId",
        "historyEpoch",
        "target",
        "direction",
        "planId",
        "profileId",
        "epoch",
        "requestId",
        "workspaceId",
        "sessionId",
        "turnId",
        "prompt",
        "eventEpoch",
        "cursor",
        "projectId",
        "alias",
        "path",
        "access",
        "command",
        "cwd",
    ] {
        properties.insert(key.into(), json!({"type":"string"}));
    }
    if matches!(name, "harness_session_read" | "harness_events") {
        properties.insert(
            "toolDetail".into(),
            json!({"type":"string","enum":["summary"]}),
        );
    }
    for key in ["revision", "timeoutMs"] {
        properties.insert(key.into(), json!({"type":"integer","minimum":1}));
    }
    properties.insert(
        "policyRevision".into(),
        json!({"type":"integer","minimum":0}),
    );
    properties.insert(
        "direction".into(),
        json!({"type":"string","enum":["both","upload","download"]}),
    );
    properties.insert("decisions".into(), json!({"type":"object","additionalProperties":{"type":"string","enum":["dataset","directory"]}}));
    for key in ["createDirectory", "readOnly"] {
        properties.insert(key.into(), json!({"type":"boolean"}));
    }
    properties.insert("name".into(), json!({"type":"string"}));
    properties.insert(
        "query".into(),
        json!({"type":"string","minLength":1,"maxLength":1024}),
    );
    properties.insert(
        "mode".into(),
        json!({"type":"string","enum":["path","content","title"]}),
    );
    properties.insert(
        "args".into(),
        json!({"type":"array","items":{"type":"string"}}),
    );
    properties.insert("mounts".into(),json!({"type":"array","maxItems":31,"items":{"type":"object","properties":{"alias":{"type":"string"},"path":{"type":"string"},"at":{"type":"string"},"access":{"type":"string","enum":["ro","rw"]}},"required":["alias","path","at","access"],"additionalProperties":false}}));
    properties.insert("after".into(), json!({"type":"integer","minimum":0}));
    properties.insert(
        "limit".into(),
        json!({"type":"integer","minimum":1,"maximum":100}),
    );
    properties.insert("archived".into(), json!({"type":"boolean"}));
    properties.insert(
        "nativeRequestId".into(),
        json!({"type":["string","integer"]}),
    );
    properties.insert("response".into(), json!({"type":"object"}));
    properties.insert("watchId".into(), json!({"type":"string","maxLength":128}));
    properties.insert("attachments".into(), json!({"type":"array","maxItems":5,"items":{"type":"object","required":["kind","name","content"],"properties":{"kind":{"enum":["text","image"]},"name":{"type":"string","maxLength":256},"content":{"type":"string","maxLength":524288},"mimeType":{"type":"string"}},"additionalProperties":false}}));
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required},
        "annotations":{"readOnlyHint":matches!(name,"harness_session_search"|"project_watch"|"project_unwatch"|"project_search"|"project_sync_directories"|"project_sync_compare"|"project_sync_status"|"piagent_capabilities"|"fsagent_capabilities"|"project_roots"|"project_list"|"project_read"|"harness_profiles"|"harness_sessions"|"harness_session_read"|"harness_session_info"|"harness_events"|"harness_operation"),
        "openWorldHint":true}})
}

#[cfg(test)]
mod schema_tests {
    #[test]
    fn every_required_tool_argument_has_a_schema() {
        for tool in super::tools() {
            for key in tool["inputSchema"]["required"].as_array().unwrap() {
                assert!(
                    !tool["inputSchema"]["properties"][key.as_str().unwrap()].is_null(),
                    "{}: {key}",
                    tool["name"]
                );
            }
            if tool["name"] == "harness_respond" {
                assert_eq!(tool["inputSchema"]["required"].as_array().unwrap().len(), 5);
            }
        }
    }
}
