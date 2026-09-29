use fs_agent::{launch, router};

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
    let state = config.state()?;
    let app = router(state.clone(), &config.allowed_origins)?;
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    eprintln!(
        "fs-agent listening on {} (config {})",
        listener.local_addr()?,
        path.display()
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            state.operations.cancel_all();
        })
        .await?;
    Ok(())
}
