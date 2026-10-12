use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dwo_cli::run(run_daemon).await
}

async fn run_daemon(config_path: PathBuf) -> anyhow::Result<()> {
    // rustls 的进程级 CryptoProvider 先在入口钉死：openlark 的长连接（飞书事件
    // 监听）在自己 spawn 的任务里建 TLS 客户端，靠「自动探测」时因为 ring
    // （reqwest / teloxide 那条线）和 aws-lc-rs（openlark 默认档）同时在树上，
    // 会直接 panic（`Could not automatically determine the process-level
    // CryptoProvider`）。装 ring 的默认档后，探测路径永不再走。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _logging = dwo_host::logging::init(&config_path)?;
    tracing::info!(
        event = "daemon.starting",
        config_path = %config_path.display(),
        "daemon starting"
    );
    // Take the daemon lock before anything touches profile state.
    let _instance = match dwo_ipc::InstanceLock::acquire() {
        Ok(instance) => instance,
        Err(error) => {
            tracing::error!(
                event = "daemon.instance_locked",
                error = %format!("{error:#}"),
                "another daemon already owns this profile"
            );
            return Err(error);
        }
    };
    let result = async {
        let host = dwo_host::Host::build(&config_path, dwo_channels::default_factory()).await?;
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
