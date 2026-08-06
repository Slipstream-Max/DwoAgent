use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub(crate) struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    pub(crate) fn invalid_params(error: impl std::fmt::Display) -> Self {
        Self::new(-32602, "Invalid params", Some(json!(error.to_string())))
    }

    pub(crate) fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(-32603, "Internal error", Some(json!(error.to_string())))
    }

    pub(crate) fn method_not_found(method: &str) -> Self {
        Self::new(
            -32601,
            "Method not found",
            Some(json!(format!("unsupported ACP method: {method}"))),
        )
    }

    fn new(code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }

    fn from_value(value: &Value) -> Self {
        Self {
            code: value.get("code").and_then(Value::as_i64).unwrap_or(-32603),
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("JSON-RPC request failed")
                .to_string(),
            data: value.get("data").cloned(),
        }
    }

    fn to_value(&self) -> Value {
        let mut value = Map::from_iter([
            ("code".to_string(), json!(self.code)),
            ("message".to_string(), json!(self.message)),
        ]);
        if let Some(data) = &self.data {
            value.insert("data".to_string(), data.clone());
        }
        Value::Object(value)
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

#[derive(Debug)]
pub(crate) struct Incoming {
    pub(crate) id: Option<Value>,
    pub(crate) method: String,
    pub(crate) params: Value,
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>;

#[derive(Clone)]
pub(crate) struct Connection {
    outgoing: mpsc::UnboundedSender<Value>,
    pending: Arc<Mutex<Pending>>,
    next_id: Arc<AtomicU64>,
    closed: CancellationToken,
}

impl Connection {
    pub(crate) fn notify<T: Serialize>(&self, method: &str, params: T) -> Result<()> {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": serde_json::to_value(params)?,
        }))
    }

    pub(crate) async fn request<T: Serialize>(&self, method: &str, params: T) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        if let Err(error) = self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": serde_json::to_value(params)?,
        })) {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        Ok(tokio::select! {
            response = receiver => response.context("ACP client dropped request response")??,
            _ = self.closed.cancelled() => bail!("ACP connection closed"),
        })
    }

    pub(crate) async fn closed(&self) {
        self.closed.cancelled().await;
    }

    pub(crate) fn closed_token(&self) -> CancellationToken {
        self.closed.clone()
    }

    fn respond(&self, id: Value, result: Result<Value, RpcError>) -> Result<()> {
        let value = match result {
            Ok(result) => json!({"jsonrpc":"2.0", "id":id, "result":result}),
            Err(error) => {
                json!({"jsonrpc":"2.0", "id":id, "error":error.to_value()})
            }
        };
        self.send(value)
    }

    fn send(&self, value: Value) -> Result<()> {
        self.outgoing
            .send(value)
            .map_err(|_| anyhow::anyhow!("ACP stdout closed"))
    }

    async fn resolve(&self, id: u64, result: Result<Value, RpcError>) {
        if let Some(sender) = self.pending.lock().await.remove(&id) {
            let _ = sender.send(result);
        }
    }
}

pub(crate) async fn serve<R, W, H, Fut>(stdin: R, stdout: W, handler: H) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
    H: Fn(Connection, Incoming) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<Option<Value>, RpcError>> + Send + 'static,
{
    let (outgoing, mut output) = mpsc::unbounded_channel::<Value>();
    let closed = CancellationToken::new();
    let connection = Connection {
        outgoing,
        pending: Arc::new(Mutex::new(HashMap::new())),
        next_id: Arc::new(AtomicU64::new(1)),
        closed: closed.clone(),
    };
    let writer = tokio::spawn(async move {
        let mut stdout = stdout;
        while let Some(value) = output.recv().await {
            let mut bytes = serde_json::to_vec(&value)?;
            bytes.push(b'\n');
            stdout.write_all(&bytes).await?;
            stdout.flush().await?;
        }
        Ok::<(), anyhow::Error>(())
    });

    let mut lines = BufReader::new(stdin).lines();
    while let Some(line) = lines.next_line().await? {
        let value: Value = serde_json::from_str(&line).context("parse ACP JSON-RPC message")?;
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            let incoming = Incoming {
                id: value.get("id").cloned(),
                method: method.to_string(),
                params: value.get("params").cloned().unwrap_or_else(|| json!({})),
            };
            let id = incoming.id.clone();
            let connection = connection.clone();
            let handler = handler.clone();
            // Construct the handler future in wire order. Adapters may use this
            // synchronous boundary to coalesce adjacent protocol messages.
            let result = handler(connection.clone(), incoming);
            tokio::spawn(async move {
                let result = result.await;
                if let Some(id) = id {
                    let response = result.and_then(|value| {
                        value
                            .ok_or_else(|| RpcError::internal("request handler returned no result"))
                    });
                    let _ = connection.respond(id, response);
                }
            });
            continue;
        }
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let result = match value.get("error") {
            Some(error) => Err(RpcError::from_value(error)),
            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
        };
        connection.resolve(id, result).await;
    }

    closed.cancel();
    connection.pending.lock().await.clear();
    drop(connection);
    writer.abort();
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn eof_stops_stdio_server() {
        let (client_stdin, agent_stdin) = tokio::io::duplex(1024);
        let (agent_stdout, _client_stdout) = tokio::io::duplex(1024);
        let task = tokio::spawn(serve(
            agent_stdin,
            agent_stdout,
            |_connection, _incoming| async { Ok(Some(Value::Null)) },
        ));

        drop(client_stdin);

        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("stdio server should stop after stdin EOF")
            .expect("stdio task should not panic")
            .expect("stdin EOF should be a clean shutdown");
    }

    #[tokio::test]
    async fn handler_futures_are_constructed_in_wire_order() {
        let (mut client_input, agent_input) = tokio::io::duplex(1024);
        let (agent_output, _client_output) = tokio::io::duplex(1024);
        let methods = Arc::new(StdMutex::new(Vec::new()));
        let observed = methods.clone();
        let task = tokio::spawn(serve(
            agent_input,
            agent_output,
            move |_connection, incoming| {
                observed.lock().unwrap().push(incoming.method);
                async { Ok(None) }
            },
        ));

        client_input
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"method\":\"session/cancel\"}\n\
                  {\"jsonrpc\":\"2.0\",\"method\":\"session/prompt\"}\n",
            )
            .await
            .unwrap();
        drop(client_input);
        task.await.unwrap().unwrap();

        assert_eq!(
            *methods.lock().unwrap(),
            ["session/cancel", "session/prompt"]
        );
    }

    #[tokio::test]
    async fn stdio_correlates_bidirectional_requests() {
        let (mut client_input, agent_input) = tokio::io::duplex(16 * 1024);
        let (agent_output, client_output) = tokio::io::duplex(16 * 1024);
        let task = tokio::spawn(serve(
            agent_input,
            agent_output,
            |connection, incoming| async move {
                assert_eq!(incoming.method, "trigger");
                let result = connection
                    .request("client/confirm", json!({"question":"continue?"}))
                    .await
                    .map_err(RpcError::internal)?;
                Ok(Some(result))
            },
        ));
        client_input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"trigger\"}\n")
            .await
            .unwrap();
        client_input.flush().await.unwrap();

        let mut output = BufReader::new(client_output);
        let mut line = String::new();
        output.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "client/confirm");
        assert_eq!(request["params"]["question"], "continue?");
        let response = json!({
            "jsonrpc":"2.0",
            "id":request["id"],
            "result":{"accepted":true},
        });
        client_input
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
        client_input.flush().await.unwrap();

        line.clear();
        output.read_line(&mut line).await.unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], 10);
        assert_eq!(response["result"]["accepted"], true);

        drop(client_input);
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
