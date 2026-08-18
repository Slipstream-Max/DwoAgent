use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dwo_cli::run(run_daemon).await
}

async fn run_daemon(config_path: PathBuf) -> anyhow::Result<()> {
    let _logging = dwo_host::logging::init(&config_path)?;
    tracing::info!(
        event = "daemon.starting",
        config_path = %config_path.display(),
        "daemon starting"
    );
    let result = async {
        let host = dwo_host::Host::load(&config_path).await?;
        tracing::info!(
            event = "daemon.ready",
            endpoint = %dwo_ipc::endpoint(&config_path),
            "daemon ready"
        );
        let (ipc_result, websocket_result) = tokio::join!(
            dwo_ipc::serve(host.clone(), &config_path),
            dwo_websocket::serve(host),
        );
        ipc_result?;
        websocket_result
    }
    .await;
    match &result {
        Ok(()) => tracing::info!(event = "daemon.stopped", "daemon stopped"),
        Err(error) => tracing::error!(
            event = "daemon.failed",
            error = %format!("{error:#}"),
            "daemon failed"
        ),
    }
    result
}
