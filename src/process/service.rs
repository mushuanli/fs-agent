//! Process API operations: admission policy around the registry and launcher.
//!
//! The HTTP handlers only extract the identity and the body; everything about
//! epochs, duplicate request ids, capacity and the single-command guarantee is
//! decided here.

use crate::{
    app::State,
    core::{error::Error, ids},
    process::{
        model::{Process, Request, Status},
        policy, runner, sandbox,
    },
};
use std::sync::Arc;

/// Start a command, or return the record of an identical earlier request.
pub fn start(state: &Arc<State>, identity: usize, request: Request) -> Result<Status, Error> {
    let id: String = request.request_id.chars().take(128).collect();
    start_authorized(state, identity, request).map_err(|error| {
        crate::core::events::emit(
            crate::core::events::Level::Warn,
            "process.rejected",
            serde_json::json!({"requestId": id, "identity": identity, "code": error.code}),
        );
        error
    })
}

fn start_authorized(
    state: &Arc<State>,
    identity: usize,
    request: Request,
) -> Result<Status, Error> {
    authorize(state, &request.epoch, &request.request_id)?;
    if state.auth.server_id() != Some(request.server_id.as_str()) {
        return Err(Error::conflict("SERVER_ID_CHANGED"));
    }
    let process =
        state
            .execution
            .registry()
            .get_or_create(identity, &request.request_id, || {
                // The command owns every export until it has been reaped.
                let gate = state.files.exclusive()?;
                let plan = policy::plan(state, identity, &request)?;
                let lock = match plan.writer() {
                    Some(export) => export.try_clone_root()?,
                    None => state.execution.lock_handle()?,
                };
                let prepared = sandbox::assemble(&plan, lock)?;
                let process = Arc::new(Process::new());
                crate::core::events::emit(crate::core::events::Level::Debug, "process.accepted", serde_json::json!({"requestId": request.request_id,
                    "identity": identity, "cwd": plan.cwd, "mountCount": plan.mounts.len(), "timeoutMs": plan.timeout_ms}));
                // The registered task, not the HTTP waiter, owns spawn and cleanup.
                tokio::spawn(runner::run(
                    state.clone(),
                    prepared,
                    process.clone(),
                    gate,
                    plan.timeout_ms,
                    identity,
                    request.request_id.clone(),
                ));
                Ok(process)
            })?;
    Ok(process.status())
}

/// Query one command by request id.
pub fn status(state: &State, identity: usize, epoch: &str, id: &str) -> Result<Status, Error> {
    authorize(state, epoch, id)?;
    state
        .execution
        .registry()
        .get(identity, id)
        .map(|process| process.status())
        .ok_or_else(|| Error::not_found("PROCESS_UNKNOWN"))
}

/// Cancel a command. An id that was never started yields a refusal record, so
/// the caller learns the request will not run.
pub fn cancel(state: &State, identity: usize, epoch: &str, id: &str) -> Result<Status, Error> {
    authorize(state, epoch, id)?;
    let status = state.execution.registry().cancel(identity, id)?.status();
    crate::core::events::emit(
        crate::core::events::Level::Info,
        "process.cancel_requested",
        serde_json::json!({"requestId": id, "identity": identity}),
    );
    Ok(status)
}

/// Shared admission checks: capability, epoch and identifier shape.
fn authorize(state: &State, epoch: &str, id: &str) -> Result<(), Error> {
    if !state.execution.ready() {
        return Err(Error::unsupported());
    }
    if epoch != state.execution.epoch() {
        return Err(Error::conflict("STALE_SERVER_EPOCH"));
    }
    if !ids::is_identifier_within(id, ids::IDENTIFIER_MAX) {
        return Err(Error::invalid());
    }
    Ok(())
}
