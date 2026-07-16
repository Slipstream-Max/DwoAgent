mod automation;
mod channels;
mod cli;
mod host;
mod local;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::run().await
}
