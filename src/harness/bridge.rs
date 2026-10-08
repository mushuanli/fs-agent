//! Supervised stdio JSON-RPC. The child owns Codex state; we never edit its database.
use super::{config::ProfileConfig, events::Events};
use crate::core::error::Error;
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout},
    sync::{mpsc, oneshot, watch},
};
use tokio_util::sync::CancellationToken;

type Reply = oneshot::Sender<Result<Value, Error>>;
enum Command {
    Call(String, Value, Reply),
    Respond(Value, Value, Reply),
}
pub struct Bridge {
    sender: mpsc::Sender<Command>,
    pub events: Arc<Mutex<Events>>,
    stop: CancellationToken,
    exited: watch::Receiver<bool>,
}

impl Bridge {
    pub async fn start(config: &ProfileConfig) -> Result<Arc<Self>, Error> {
        Self::start_scoped(config, None).await
    }
    pub async fn start_scoped(
        config: &ProfileConfig,
        runtime: Option<Arc<crate::projects::runtime::ProjectRuntime>>,
    ) -> Result<Arc<Self>, Error> {
        let mut prepared = runtime
            .as_ref()
            .map(|r| r.prepare(config, vec!["app-server".into(), "--stdio".into()]))
            .transpose()?;
        let mut command = match prepared.as_mut() {
            Some(p) => {
                std::mem::replace(&mut p.command, tokio::process::Command::new("/bin/false"))
            }
            None => tokio::process::Command::new(&config.command),
        };
        command.env_clear();
        command.as_std_mut().process_group(0);
        for key in [
            "PATH",
            "HOME",
            "USER",
            "LANG",
            "TMPDIR",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        if runtime.is_none() {
            command.args(["app-server", "--stdio"]);
        }
        if let Some(runtime) = &runtime {
            command
                .env("HOME", "/tmp")
                .env("PI_AGENT_PROJECT", &runtime.project.id)
                .env("FS_AGENT_PROJECT", &runtime.project.id);
        }
        let mut child = command
            .env("CODEX_HOME", &config.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| Error::unavailable())?;
        let input = child.stdin.take().ok_or_else(Error::internal)?;
        let output = child.stdout.take().ok_or_else(Error::internal)?;
        let (sender, receiver) = mpsc::channel(128);
        let (exit, exited) = watch::channel(false);
        let bridge = Arc::new(Self {
            sender,
            events: Arc::new(Mutex::new(Events::new())),
            stop: CancellationToken::new(),
            exited,
        });
        bridge.events.lock().unwrap().runtime = runtime.clone();
        let events = bridge.events.clone();
        let stop = bridge.stop.clone();
        tokio::spawn(async move {
            supervise(child, input, output, receiver, events, stop).await;
            let _ = exit.send(true);
        });
        if let Err(error) = bridge.call("initialize", json!({"clientInfo":{"name":"itookit-agent","version":"0.1.0"},"capabilities":{"experimentalApi":true}})).await {
            bridge.close(); return Err(error);
        }
        // Notifications have no reply, but are written in the same ordered channel.
        bridge
            .respond(Value::Null, json!({"method":"initialized","params":{}}))
            .await?;
        Ok(bridge)
    }
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        tokio::time::timeout(
            Duration::from_secs(25),
            self.persistent_call(method, params),
        )
        .await
        .map_err(|_| Error::timed_out())?
    }
    pub async fn persistent_call(&self, method: &str, params: Value) -> Result<Value, Error> {
        if self.stop.is_cancelled() {
            return Err(Error::unavailable());
        }
        let (send, receive) = oneshot::channel();
        self.sender
            .try_send(Command::Call(method.into(), params, send))
            .map_err(|_| Error::unavailable())?;
        receive.await.map_err(|_| Error::unavailable())?
    }
    pub async fn respond(&self, id: Value, result: Value) -> Result<Value, Error> {
        let (send, receive) = oneshot::channel();
        self.sender
            .try_send(Command::Respond(id, result, send))
            .map_err(|_| Error::unavailable())?;
        await_reply(receive).await
    }
    pub fn close(&self) {
        self.stop.cancel();
    }
    pub async fn drain(&self) {
        self.close();
        let mut exited = self.exited.clone();
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while !*exited.borrow() {
                if exited.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
}

async fn await_reply(receive: oneshot::Receiver<Result<Value, Error>>) -> Result<Value, Error> {
    tokio::time::timeout(Duration::from_secs(25), receive)
        .await
        .map_err(|_| Error::timed_out())?
        .map_err(|_| Error::unavailable())?
}

async fn write(input: &mut ChildStdin, message: &Value) -> Result<(), Error> {
    let mut bytes = serde_json::to_vec(message).map_err(|_| Error::internal())?;
    bytes.push(b'\n');
    input
        .write_all(&bytes)
        .await
        .map_err(|_| Error::unavailable())
}

async fn supervise(
    mut child: Child,
    mut input: ChildStdin,
    mut output: ChildStdout,
    mut receiver: mpsc::Receiver<Command>,
    events: Arc<Mutex<Events>>,
    stop: CancellationToken,
) {
    let mut pending = HashMap::new();
    let mut next = 0u64;
    let mut line = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            command = receiver.recv() => {
                let Some(command) = command else { break; };
                let sent = tokio::select! {
                    _ = stop.cancelled() => break,
                    result = tokio::time::timeout(Duration::from_secs(25), send_command(command, &mut input, &mut pending, &mut next)) => result,
                };
                if !matches!(sent, Ok(Ok(()))) { break; }
            }
            read = output.read(&mut chunk) => {
                let Ok(n) = read else { break; }; if n == 0 { break; }
                if consume(&chunk[..n], &mut line, &mut pending, &events).is_err() { break; }
            }
        }
    }
    // Reclaim this supervisor's process group before reaping its leader.
    if let Some(pid) = child
        .id()
        .and_then(|id| rustix::process::Pid::from_raw(id as i32))
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
    let clean = child.wait().await.is_ok();
    if let Some(runtime) = events.lock().unwrap().runtime.as_ref() {
        runtime.stopped(clean);
    }
    stop.cancel();
    for (_, reply) in pending {
        let _ = reply.send(Err(Error::unavailable()));
    }
    let _ = events
        .lock()
        .unwrap()
        .push(json!({"method":"harness/disconnected","params":{}}));
}

fn consume(
    chunk: &[u8],
    line: &mut Vec<u8>,
    pending: &mut HashMap<u64, Reply>,
    events: &Mutex<Events>,
) -> Result<(), Error> {
    for byte in chunk {
        if *byte == b'\n' {
            let message = serde_json::from_slice::<Value>(line).map_err(|_| Error::internal())?;
            line.clear();
            dispatch(message, pending, events)?;
        } else {
            if line.len() >= 8 * 1024 * 1024 {
                return Err(Error::too_large("EFBIG"));
            }
            line.push(*byte);
        }
    }
    Ok(())
}

async fn send_command(
    command: Command,
    input: &mut ChildStdin,
    pending: &mut HashMap<u64, Reply>,
    next: &mut u64,
) -> Result<(), Error> {
    pending.retain(|_, reply| !reply.is_closed());
    match command {
        Command::Call(method, params, reply) => {
            if pending.len() >= 128 {
                let _ = reply.send(Err(Error::too_many("HARNESS_CALL_LIMIT")));
                return Ok(());
            }
            *next += 1;
            let result = write(input, &json!({"id":next,"method":method,"params":params})).await;
            if let Err(error) = result {
                let _ = reply.send(Err(error));
                return Err(error);
            }
            pending.insert(*next, reply);
        }
        Command::Respond(id, result, reply) => {
            let message = if id.is_null() {
                result
            } else {
                json!({"id":id,"result":result})
            };
            let result = write(input, &message).await;
            let _ = reply.send(result.map(|_| json!({})));
            result?;
        }
    }
    Ok(())
}

fn dispatch(
    message: Value,
    pending: &mut HashMap<u64, Reply>,
    events: &Mutex<Events>,
) -> Result<(), Error> {
    if message.get("method").is_some() {
        return events.lock().unwrap().push(message);
    }
    if let Some(reply) = message["id"].as_u64().and_then(|id| pending.remove(&id)) {
        let result = if message.get("error").is_some() {
            Err(native_error(&message["error"]))
        } else {
            Ok(message["result"].clone())
        };
        let _ = reply.send(result);
    }
    Ok(())
}

fn native_error(error: &Value) -> Error {
    if error["code"] == -32601 {
        return Error::unsupported();
    }
    if error["message"]
        .as_str()
        .is_some_and(|s| s.contains("not materialized yet"))
    {
        return Error::conflict("HARNESS_HISTORY_PENDING");
    }
    Error::conflict("HARNESS_RPC_ERROR")
}
