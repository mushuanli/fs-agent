//! Binary entry point: load configuration, wire the runtime, serve, and shut
//! down in the order that keeps the exports fenced.
//!
//! Shutdown has two stages because a graceful stop has two waits:
//!
//! 1. stop admitting commands and let in-flight file work finish, bounded by
//!    [`DRAIN_TIMEOUT`];
//! 2. let open connections end on their own, bounded by [`CONNECTION_GRACE`].
//!
//! The second bound exists because a stalled download would otherwise keep the
//! process alive forever: the client never reads, so the connection never
//! closes. The order matters — the file gate is drained before the process is
//! allowed to exit.

use fs_agent::{app::State, config::launch, process, router};
use std::{sync::Arc, time::Duration};

/// Upper bound on how long shutdown waits for in-flight file work.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound on how long open connections may take to finish afterwards.
const CONNECTION_GRACE: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = launch::resolve(std::env::args().nth(1).as_deref())?;
    let config = launch::load(&path)?;
    let state = State::from_config(&config)?;
    // Advertise execution only after the sandbox has been proven to work.
    if config.execution {
        process::enable(&state).await?;
    }
    let app = router(state.clone(), &config.allowed_origins)?;
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    eprintln!(
        "fs-agent listening on {} (config {})",
        listener.local_addr()?,
        path.display()
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(state))
        .await?;
    Ok(())
}

/// Stop admission, drain file work, then arm a hard exit deadline.
async fn shutdown(state: Arc<State>) {
    let _ = tokio::signal::ctrl_c().await;
    eprintln!("fs-agent: shutting down");
    state.execution.stop_accepting();
    state.operations.cancel_all();
    state.execution.cancel_all();
    if tokio::time::timeout(DRAIN_TIMEOUT, state.files.drain())
        .await
        .is_err()
    {
        eprintln!("fs-agent: file work still in flight after {DRAIN_TIMEOUT:?}");
    }
    // Nothing left to protect; do not wait forever for a stalled client.
    tokio::spawn(async move {
        tokio::time::sleep(CONNECTION_GRACE).await;
        eprintln!("fs-agent: connection grace elapsed, exiting");
        std::process::exit(0);
    });
}
