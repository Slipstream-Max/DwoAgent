//! Monty frontend for code mode.
//!
//! # Why the worker thread
//!
//! Two pieces force codemode onto a dedicated worker thread:
//!
//! 1. `monty`'s execution types (`PrintWriter<'_>`, `RunProgress<T>`) embed
//!    `Rc`/`Cell`, so any future driving the iterative loop is `!Send`.
//! 2. `rmcp::RunningService` is pinned to the Tokio runtime that created it.
//!    Using it across a different runtime would panic.
//!
//! We collapse both constraints by giving codemode its own OS thread that
//! hosts a single-threaded Tokio runtime + `LocalSet`. Both the MCP client
//! and every Monty run live on that thread. The public API (`MontyBackend`,
//! `CodeExecutor`) communicates with the worker over `mpsc` + `oneshot`
//! channels so it stays `Send + Sync` and the main runtime keeps running
//! Terminal / Subagent tool calls on other threads in parallel.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use monty::{
    ExtFunctionResult, MontyObject, MontyRun, NameLookupResult, NoLimitTracker, PrintWriter,
    RunProgress,
};
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use super::mcp_client::{McpClient, McpConfigModel, McpKind};

/// Names of the functions injected into the sandbox. Matches the four
/// async helpers Python's `_external_functions` registers.
const INJECTED_FUNCTIONS: &[&str] = &["search_mcp", "mcp_tool", "mcp_prompt", "mcp_resource"];

/// Messages exchanged with the worker thread.
enum WorkerMessage {
    Init {
        config: McpConfigModel,
        reply: oneshot::Sender<Result<Vec<String>>>,
    },
    Exec {
        code: String,
        reply: oneshot::Sender<Result<String>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// Public façade that owns the worker thread and its channel handle.
pub struct MontyBackend {
    tx: mpsc::UnboundedSender<WorkerMessage>,
    server_names: Vec<String>,
}

impl MontyBackend {
    /// Start the worker thread, load MCP servers from *path*, and return a
    /// backend ready to service `exec_chain` calls.
    pub async fn spawn(config_path: Option<&Path>) -> Result<Self> {
        let config = load_mcp_config(config_path)?;
        let (tx, rx) = mpsc::unbounded_channel::<WorkerMessage>();
        spawn_worker_thread(rx);

        let (init_tx, init_rx) = oneshot::channel();
        tx.send(WorkerMessage::Init {
            config,
            reply: init_tx,
        })
        .map_err(|_| anyhow!("Monty worker thread refused init message"))?;
        let server_names = init_rx
            .await
            .map_err(|_| anyhow!("Monty worker thread dropped init reply"))??;

        Ok(Self { tx, server_names })
    }

    pub fn server_names(&self) -> &[String] {
        &self.server_names
    }

    /// Shut down the worker, draining the MCP clients on its runtime.
    pub async fn shutdown(&self) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(WorkerMessage::Shutdown { reply: reply_tx })
            .is_ok()
        {
            let _ = reply_rx.await;
        }
    }

    /// Execute *code* under a duration budget, returning captured stdout
    /// wrapped in `{ "output": ... }` to match the Python payload shape.
    pub async fn execute(
        &self,
        code: &str,
        timeout_secs: f64,
        output_limit: usize,
    ) -> Result<Value> {
        if code.trim().is_empty() {
            bail!("code must be non-empty");
        }
        if !(timeout_secs > 0.0) {
            bail!("timeout must be > 0");
        }
        if output_limit == 0 {
            bail!("outputlimit must be >= 1");
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WorkerMessage::Exec {
                code: code.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("Monty worker has shut down"))?;

        let duration = Duration::from_secs_f64(timeout_secs);
        let output = match timeout(duration, reply_rx).await {
            Ok(Ok(Ok(out))) => out,
            Ok(Ok(Err(err))) => return Err(err),
            Ok(Err(_)) => bail!("Monty worker stopped before answering"),
            Err(_) => bail!("Monty execution timed out after {timeout_secs}s"),
        };

        let trimmed = truncate_output(&output, output_limit);
        Ok(json!({ "output": trimmed }))
    }
}

// ── Worker thread ──────────────────────────────────────────────────────────

fn spawn_worker_thread(mut rx: mpsc::UnboundedReceiver<WorkerMessage>) {
    thread::Builder::new()
        .name("dwo-agent-monty".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    drain_on_runtime_failure(rx, err.to_string());
                    return;
                }
            };
            rt.block_on(async move {
                let local = tokio::task::LocalSet::new();
                local
                    .run_until(async {
                        let mut mcp_client: Option<Arc<McpClient>> = None;
                        while let Some(msg) = rx.recv().await {
                            match msg {
                                WorkerMessage::Init { config, reply } => {
                                    let result = McpClient::new(config).await;
                                    match result {
                                        Ok(client) => {
                                            let names = client.server_names().to_vec();
                                            mcp_client = Some(client);
                                            let _ = reply.send(Ok(names));
                                        }
                                        Err(err) => {
                                            let _ = reply.send(Err(err));
                                        }
                                    }
                                }
                                WorkerMessage::Exec { code, reply } => {
                                    let Some(client) = mcp_client.clone() else {
                                        let _ = reply.send(Err(anyhow!(
                                            "Monty worker received exec before init",
                                        )));
                                        continue;
                                    };
                                    // Concurrent exec_chain calls share the
                                    // LocalSet: the interpreter is
                                    // single-threaded but await points
                                    // interleave, matching Python asyncio.
                                    tokio::task::spawn_local(async move {
                                        let outcome = run_monty_loop(client, code).await;
                                        let _ = reply.send(outcome);
                                    });
                                }
                                WorkerMessage::Shutdown { reply } => {
                                    if let Some(client) = mcp_client.take() {
                                        client.shutdown().await;
                                    }
                                    let _ = reply.send(());
                                    break;
                                }
                            }
                        }
                    })
                    .await;
            });
        })
        .expect("spawn monty worker thread");
}

fn drain_on_runtime_failure(mut rx: mpsc::UnboundedReceiver<WorkerMessage>, err_text: String) {
    while let Some(msg) = rx.blocking_recv() {
        let err_text = err_text.clone();
        match msg {
            WorkerMessage::Init { reply, .. } => {
                let _ = reply.send(Err(anyhow!("monty worker runtime init failed: {err_text}")));
            }
            WorkerMessage::Exec { reply, .. } => {
                let _ = reply.send(Err(anyhow!("monty worker runtime init failed: {err_text}")));
            }
            WorkerMessage::Shutdown { reply } => {
                let _ = reply.send(());
            }
        }
    }
}

fn load_mcp_config(path: Option<&Path>) -> Result<McpConfigModel> {
    let Some(path) = path else {
        return Ok(McpConfigModel::default());
    };
    let path: PathBuf = path.into();
    if !path.is_file() {
        bail!("MCP config file not found: {}", path.display());
    }
    let raw =
        std::fs::read_to_string(&path).map_err(|err| anyhow!("read {}: {err}", path.display()))?;
    let mut parsed: McpConfigModel = serde_json::from_str(&raw)
        .map_err(|err| anyhow!("parse JSON in {}: {err}", path.display()))?;
    parsed.base_dir = path.parent().map(Path::to_path_buf);
    Ok(parsed)
}

// ── Monty execution loop ───────────────────────────────────────────────────

async fn run_monty_loop(mcp_client: Arc<McpClient>, code: String) -> Result<String> {
    let runner = MontyRun::new(code, "exec_chain.py", Vec::new())
        .map_err(|err| anyhow!("monty parse error: {err}"))?;

    let mut captured = String::new();
    let mut progress = {
        let writer = PrintWriter::CollectString(&mut captured);
        runner
            .start(Vec::new(), NoLimitTracker, writer)
            .map_err(|err| anyhow!("monty start error: {err}"))?
    };

    loop {
        progress = match progress {
            RunProgress::Complete(_) => break,
            RunProgress::NameLookup(lookup) => {
                let name = lookup.name.clone();
                let result = if INJECTED_FUNCTIONS.iter().any(|f| *f == name.as_str()) {
                    NameLookupResult::Value(MontyObject::Function {
                        name: name.clone(),
                        docstring: None,
                    })
                } else {
                    NameLookupResult::Undefined
                };
                let writer = PrintWriter::CollectString(&mut captured);
                lookup
                    .resume(result, writer)
                    .map_err(|err| anyhow!("monty NameLookup resume failed: {err}"))?
            }
            RunProgress::FunctionCall(call) => {
                let name = call.function_name.clone();
                let args_json = call_args_to_json(&call.args)?;
                let dispatch = dispatch_injected(&mcp_client, &name, args_json).await;
                let result = match dispatch {
                    Ok(Some(value)) => ExtFunctionResult::Return(json_to_monty(&value)?),
                    Ok(None) => ExtFunctionResult::NotFound(name.clone()),
                    Err(err) => bail!("{err:#}"),
                };
                let writer = PrintWriter::CollectString(&mut captured);
                call.resume(result, writer)
                    .map_err(|err| anyhow!("monty FunctionCall resume failed: {err}"))?
            }
            other => bail!("unexpected Monty progress state: {other:?}"),
        };
    }

    Ok(captured)
}

fn truncate_output(raw: &str, limit: usize) -> String {
    if raw.chars().count() <= limit {
        return raw.to_string();
    }
    let truncated: String = raw.chars().take(limit).collect();
    format!("{truncated}\n[TRUNCATED]")
}

async fn dispatch_injected(
    mcp_client: &Arc<McpClient>,
    name: &str,
    args: Vec<Value>,
) -> Result<Option<Value>> {
    match name {
        "search_mcp" => {
            let query = args
                .first()
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let servername = args
                .get(1)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let kind = args
                .get(2)
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let limit = args.get(3).and_then(Value::as_u64).unwrap_or(5) as usize;
            let kind = McpKind::parse(&kind)?;
            let results = mcp_client.search(&query, &servername, kind, limit).await?;
            Ok(Some(Value::Array(results)))
        }
        "mcp_tool" => {
            let servername = args
                .first()
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let toolname = args
                .get(1)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let arguments: Map<String, Value> = args
                .get(2)
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            let result = mcp_client
                .call_tool(&servername, &toolname, arguments)
                .await?;
            Ok(Some(result))
        }
        "mcp_prompt" => {
            let servername = args
                .first()
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let promptname = args
                .get(1)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let arguments: Map<String, Value> = args
                .get(2)
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            let result = mcp_client
                .get_prompt(&servername, &promptname, arguments)
                .await?;
            Ok(Some(result))
        }
        "mcp_resource" => {
            let servername = args
                .first()
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let resourcename = args
                .get(1)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let result = mcp_client.read_resource(&servername, &resourcename).await?;
            Ok(Some(result))
        }
        _ => Ok(None),
    }
}

fn call_args_to_json(args: &[MontyObject]) -> Result<Vec<Value>> {
    args.iter().map(monty_to_json).collect()
}

fn monty_to_json(value: &MontyObject) -> Result<Value> {
    Ok(match value {
        MontyObject::None => Value::Null,
        MontyObject::Bool(b) => Value::Bool(*b),
        MontyObject::Int(i) => Value::Number((*i).into()),
        MontyObject::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        MontyObject::String(s) => Value::String(s.clone()),
        MontyObject::Bytes(bytes) => Value::String(String::from_utf8_lossy(bytes).to_string()),
        MontyObject::List(items) | MontyObject::Tuple(items) => Value::Array(
            items
                .iter()
                .map(monty_to_json)
                .collect::<Result<Vec<_>>>()?,
        ),
        MontyObject::Dict(pairs) => {
            let mut out = Map::new();
            for entry in pairs.clone().into_iter() {
                let (k, v) = entry;
                let key = match &k {
                    MontyObject::String(s) => s.clone(),
                    other => serde_json::to_string(&monty_to_json(other)?)?,
                };
                out.insert(key, monty_to_json(&v)?);
            }
            Value::Object(out)
        }
        other => Value::String(format!("{other:?}")),
    })
}

fn json_to_monty(value: &Value) -> Result<MontyObject> {
    Ok(match value {
        Value::Null => MontyObject::None,
        Value::Bool(b) => MontyObject::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MontyObject::Int(i)
            } else if let Some(f) = n.as_f64() {
                MontyObject::Float(f)
            } else {
                MontyObject::Float(0.0)
            }
        }
        Value::String(s) => MontyObject::String(s.clone()),
        Value::Array(items) => MontyObject::List(
            items
                .iter()
                .map(json_to_monty)
                .collect::<Result<Vec<_>>>()?,
        ),
        Value::Object(map) => {
            let pairs: monty::DictPairs = map
                .iter()
                .map(|(k, v)| {
                    let key = MontyObject::String(k.clone());
                    let val = json_to_monty(v)?;
                    Ok::<_, anyhow::Error>((key, val))
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .collect();
            MontyObject::Dict(pairs)
        }
    })
}
