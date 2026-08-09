mod automation;
mod channels;
mod cli;
mod host;
mod local;
mod logging;
mod session_status;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::run().await
}
