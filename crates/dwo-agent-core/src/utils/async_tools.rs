//! Helpers for bridging sync and async code.

use std::future::Future;
use std::thread;

use anyhow::Result;

/// Run *future* to completion from a synchronous context.
///
/// If no Tokio runtime is active, blocks directly on a fresh current-thread
/// runtime. Otherwise spawns a dedicated OS thread to avoid nesting runtimes.
pub fn run_awaitable_blocking<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_err() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        return rt.block_on(future);
    }

    let handle = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(future)
    });
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("run_awaitable_blocking worker thread panicked"))?
}
