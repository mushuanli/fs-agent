//! Claude Code's SDK transport and JSONL history, independent of Codex's app-server.
mod history;
mod process;
mod protocol;
mod search;
use crate::{
    core::error::Error,
    harness::{
        config::{ProfileConfig, WorkspaceConfig},
        driver::HarnessDriver,
        events::Events,
        service::string,
    },
    projects::runtime::ProjectRuntime,
};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::Mutex as AsyncMutex;
static READS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
pub struct Claude {
    config: ProfileConfig,
    runtime: Option<Arc<ProjectRuntime>>,
    sessions: AsyncMutex<BTreeMap<String, Arc<process::Process>>>,
    events: Arc<Mutex<Events>>,
    closed: AtomicBool,
}
impl Claude {
    pub fn new(mut config: ProfileConfig, runtime: Option<Arc<ProjectRuntime>>) -> Self {
        if let Some(runtime) = &runtime {
            config.workspaces = vec![WorkspaceConfig {
                id: runtime.project.id.clone(),
                path: runtime.cwd().into(),
            }];
        }
        let mut events = Events::new();
        events.runtime = runtime.clone();
        Self {
            config,
            runtime,
            sessions: AsyncMutex::new(BTreeMap::new()),
            events: Arc::new(Mutex::new(events)),
            closed: AtomicBool::new(false),
        }
    }
    async fn catalog(&self) -> Result<history::Catalog, Error> {
        static WORK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
        let permit = WORK
            .try_acquire()
            .map_err(|_| Error::too_many("CLAUDE_HISTORY_BUSY"))?;
        let config = self.config.clone();
        let runtime = self.runtime.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            history::catalog(&config, runtime.as_deref())
        })
        .await
        .map_err(|_| Error::internal())?
    }
    async fn session(&self, id: &str) -> Result<Value, Error> {
        if !history::uuid(id) {
            return Err(Error::invalid());
        }
        if let Some(process) = self.sessions.lock().await.get(id) {
            if process.alive() {
                return Ok(process.session.lock().unwrap().clone());
            }
        }
        self.catalog()
            .await?
            .entries
            .into_iter()
            .find(|entry| entry.session["id"] == id)
            .map(|entry| entry.session)
            .ok_or_else(|| Error::not_found("ENOENT"))
    }
    async fn list(&self, args: &Value) -> Result<Value, Error> {
        if args["archived"].as_bool() == Some(true) {
            return Ok(json!({"sessions":[],"nextCursor":null}));
        }
        let mut sessions = self
            .catalog()
            .await?
            .entries
            .into_iter()
            .map(|entry| {
                (
                    entry.session["id"].as_str().unwrap().to_owned(),
                    entry.session,
                )
            })
            .collect::<BTreeMap<_, _>>();
        for (id, process) in self.sessions.lock().await.iter() {
            if process.alive() {
                sessions.insert(id.clone(), process.session.lock().unwrap().clone());
            }
        }
        list_page(sessions.into_values().collect(), args)
    }
    async fn read_history(&self, args: &Value) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        let session = self.session(id).await?;
        let entry = self
            .catalog()
            .await?
            .entries
            .into_iter()
            .find(|entry| entry.session["id"] == id);
        let mut result = if let Some(entry) = entry {
            self.history_page(entry, args.clone()).await?
        } else {
            let items = self.live_items(id).await;
            json!({"session":session,"turns":[{"id":null,"items":items}],"nextCursor":null})
        };
        result["session"] = session;
        Ok(result)
    }
    async fn history_page(&self, entry: history::Entry, args: Value) -> Result<Value, Error> {
        let permit = READS
            .try_acquire()
            .map_err(|_| Error::too_many("CLAUDE_HISTORY_BUSY"))?;
        let runtime = self.runtime.clone();
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            history::read(&config, runtime.as_deref(), &entry, &args)
        })
        .await
        .map_err(|_| Error::internal())?
    }
    async fn live_items(&self, id: &str) -> Vec<Value> {
        let sessions = self.sessions.lock().await;
        let Some(process) = sessions.get(id) else {
            return Vec::new();
        };
        let items = process.items.lock().unwrap();
        items
            .iter()
            .rev()
            .take(100)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
    async fn attach(&self, session: Value, resume: bool) -> Result<Value, Error> {
        let id = string(&session, "id")?.to_owned();
        let mut sessions = self.sessions.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::unavailable());
        }
        if let Some(process) = sessions.get(&id) {
            if process.alive() {
                return Ok(json!({"session":process.session.lock().unwrap().clone()}));
            }
        }
        sessions.retain(|_, process| process.alive());
        if sessions.len() >= 16 {
            return Err(Error::too_many("CLAUDE_SESSION_LIMIT"));
        }
        let resume = if resume {
            Some(self.resume_path(&id).await?)
        } else {
            None
        };
        let process = self.start(session, resume).await?;
        let session = process.session.lock().unwrap().clone();
        sessions.insert(id, process);
        Ok(json!({"session":session}))
    }
    async fn start(
        &self,
        session: Value,
        resume: Option<String>,
    ) -> Result<Arc<process::Process>, Error> {
        let mut session = session;
        session["owned"] = json!(true);
        session["status"] = json!("idle");
        session["resumable"] = json!(true);
        let process = process::Process::start(
            &self.config,
            self.runtime.clone(),
            session,
            resume,
            self.events.clone(),
        )
        .await?;
        if self.closed.load(Ordering::Acquire) {
            process.close();
            process.drain().await;
            return Err(Error::unavailable());
        }
        Ok(process)
    }
    async fn resume_path(&self, id: &str) -> Result<String, Error> {
        let entry = self
            .catalog()
            .await?
            .entries
            .into_iter()
            .find(|entry| entry.session["id"] == id)
            .ok_or_else(|| Error::not_found("ENOENT"))?;
        let config = self.config.clone();
        let runtime = self.runtime.clone();
        let permit = READS
            .try_acquire()
            .map_err(|_| Error::too_many("CLAUDE_HISTORY_BUSY"))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            history::resume_path(&config, runtime.as_deref(), &entry)
        })
        .await
        .map_err(|_| Error::internal())?
    }
    async fn create(&self, args: &Value) -> Result<Value, Error> {
        let workspace = self
            .config
            .workspaces
            .iter()
            .find(|w| Some(w.id.as_str()) == args["workspaceId"].as_str())
            .ok_or_else(Error::invalid)?;
        self.attach(json!({"id":uuid()?,"title":"","cwd":workspace.path,"status":"idle","createdAt":chrono::Utc::now().timestamp_millis(),"updatedAt":chrono::Utc::now().timestamp_millis(),"resumable":true,"owned":true,"archived":false}),false).await
    }
    async fn owned(&self, id: &str) -> Result<Arc<process::Process>, Error> {
        self.sessions
            .lock()
            .await
            .get(id)
            .filter(|process| process.alive())
            .cloned()
            .ok_or_else(|| Error::forbidden("HARNESS_NOT_OWNED"))
    }
    async fn turn(&self, args: &Value) -> Result<Value, Error> {
        let id = string(args, "sessionId")?;
        let process = self.owned(id).await?;
        if process.session.lock().unwrap()["status"] != "idle" {
            return Err(Error::busy());
        }
        let prompt = string(args, "prompt")?;
        if prompt.len() > 128 * 1024 {
            return Err(Error::invalid());
        }
        let input = crate::harness::attachments::input(prompt, &args["attachments"])?;
        let content = inputs(&input)?;
        let turn = uuid()?;
        if let Some(runtime) = &self.runtime {
            runtime.begin(id)?;
        }
        process.turn(&turn, content).await
    }
    async fn respond(&self, args: &Value) -> Result<Value, Error> {
        let request_id = args["nativeRequestId"]
            .as_str()
            .ok_or_else(Error::invalid)?;
        let request = self
            .events
            .lock()
            .unwrap()
            .requests
            .get(&json!(request_id).to_string())
            .cloned()
            .ok_or_else(Error::invalid)?;
        let process = self.owned(string(&request["params"], "threadId")?).await?;
        let response = args["response"].as_object().ok_or_else(Error::invalid)?;
        if request["method"] != "item/tool/requestUserInput"
            && !response
                .get("decision")
                .and_then(Value::as_str)
                .is_some_and(|v| ["accept", "decline", "cancel"].contains(&v))
        {
            return Err(Error::invalid());
        }
        process.respond(request_id, args["response"].clone()).await
    }
    async fn search(&self, args: &Value) -> Result<Value, Error> {
        let query = string(args, "query")?;
        if query.trim().is_empty() || query.len() > 1024 || query.contains('\0') {
            return Err(Error::invalid());
        }
        let mode = string(args, "mode")?;
        if !["title", "content"].contains(&mode) {
            return Err(Error::invalid());
        }
        if args["archived"] == true {
            return Ok(json!({"matches":[],"truncated":false,"nextCursor":null}));
        }
        let entries = self.catalog().await?.entries;
        let config = self.config.clone();
        let query = query.to_lowercase();
        let content = mode == "content";
        let runtime = self.runtime.clone();
        static SEARCHES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
        let permit = SEARCHES
            .try_acquire()
            .map_err(|_| Error::too_many("SEARCH_BUSY"))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            search::collect(&config, runtime.as_deref(), entries, &query, content)
        })
        .await
        .map_err(|_| Error::internal())?
    }
    fn check_read(&self) -> Result<(), Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::unavailable());
        }
        if let Some(runtime) = &self.runtime {
            runtime.state()?;
        }
        Ok(())
    }
    async fn read_command(&self, name: &str, args: &Value) -> Result<Value, Error> {
        match name {
            "harness_sessions" => self.list(args).await,
            "harness_session_read" => self.read_history(args).await,
            "harness_session_info" => {
                Ok(json!({"session":self.session(string(args,"sessionId")?).await?}))
            }
            "harness_events" => Ok(self.events.lock().unwrap().page(
                args["eventEpoch"].as_str(),
                args["after"].as_u64().unwrap_or(0),
            )),
            "harness_session_search" => self.search(args).await,
            _ => Err(Error::unsupported()),
        }
    }
    async fn execute_command(&self, name: &str, args: &Value) -> Result<Value, Error> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::unavailable());
        }
        if self
            .runtime
            .as_ref()
            .is_some_and(|r| r.project.access != "rw")
        {
            return Err(Error::forbidden("EROFS"));
        }
        match name {
            "harness_create" => self.create(args).await,
            "harness_resume" => {
                self.attach(self.session(string(args, "sessionId")?).await?, true)
                    .await
            }
            "harness_turn" => self.turn(args).await,
            "harness_respond" => self.respond(args).await,
            "harness_interrupt" => self.interrupt(args).await,
            _ => Err(Error::unsupported()),
        }
    }
    async fn interrupt(&self, args: &Value) -> Result<Value, Error> {
        let process = self.owned(string(args, "sessionId")?).await?;
        if process.session.lock().unwrap()["activeTurnId"] != args["turnId"] {
            return Err(Error::invalid());
        }
        process.control(json!({"subtype":"interrupt"})).await
    }
}
impl HarnessDriver for Claude {
    fn descriptor(&self) -> Value {
        json!({"id":self.config.id,"kind":"claude","workspaces":self.config.workspaces.iter().map(|w|json!({"id":w.id})).collect::<Vec<_>>(),"projectRuntime":self.config.projects,
        "capabilities":{"history":true,"create":true,"resume":true,"interrupt":true,"interactions":true,"search":true,"fork":false,"rename":false,"archive":false,"unarchive":false,"attachments":["text","image"]}})
    }
    fn read<'a>(&'a self, name: &'a str, args: Value) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(async move {
            self.check_read()?;
            let result = self.read_command(name, &args).await?;
            self.check_read()?;
            Ok(result)
        })
    }
    fn execute<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<Value, Error>> {
        Box::pin(self.execute_command(name, args))
    }
    fn stop(&self) {
        self.closed.store(true, Ordering::Release);
        if let Ok(sessions) = self.sessions.try_lock() {
            for process in sessions.values() {
                process.close();
            }
        }
    }
    fn drain(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut sessions = self.sessions.lock().await;
            for process in sessions.values() {
                process.close();
            }
            for process in sessions.values() {
                process.drain().await;
            }
            sessions.clear();
        })
    }
}
fn uuid() -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| Error::internal())?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}
fn inputs(input: &Value) -> Result<Value, Error> {
    input
        .as_array()
        .ok_or_else(Error::invalid)?
        .iter()
        .map(|item| match item["type"].as_str() {
            Some("text") => Ok(json!({"type":"text","text":item["text"]})),
            Some("image") => {
                let url = string(item, "url")?;
                let (prefix, data) = url.split_once(",").ok_or_else(Error::invalid)?;
                let mime = prefix
                    .strip_prefix("data:")
                    .and_then(|s| s.strip_suffix(";base64"))
                    .ok_or_else(Error::invalid)?;
                Ok(json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}))
            }
            _ => Err(Error::invalid()),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|items| json!(items))
}
fn list_page(mut sessions: Vec<Value>, args: &Value) -> Result<Value, Error> {
    sessions.sort_by(|a, b| {
        b["updatedAt"]
            .as_i64()
            .cmp(&a["updatedAt"].as_i64())
            .then(a["id"].as_str().cmp(&b["id"].as_str()))
    });
    let offset = args["cursor"]
        .as_str()
        .map(|v| v.parse::<usize>().map_err(|_| Error::invalid()))
        .transpose()?
        .unwrap_or(0);
    let limit = args["limit"].as_u64().unwrap_or(100).clamp(1, 100) as usize;
    if offset > sessions.len() {
        return Err(Error::invalid());
    }
    let end = (offset + limit).min(sessions.len());
    Ok(
        json!({"sessions":sessions[offset..end],"nextCursor":if end<sessions.len(){json!(end.to_string())}else{Value::Null}}),
    )
}
