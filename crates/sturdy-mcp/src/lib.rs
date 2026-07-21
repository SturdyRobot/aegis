//! # sturdy-mcp
//!
//! A native, async [Model Context Protocol](https://modelcontextprotocol.io)
//! client speaking JSON-RPC 2.0 over newline-delimited stdio.
//!
//! The wire logic ([`RpcClient`]) is generic over any `AsyncRead`/`AsyncWrite`
//! pair: a background reader task de-multiplexes responses to per-request
//! oneshot channels keyed by id, so many calls can be in flight at once. The
//! [`McpClient`] layer adds the MCP handshake and `tools/*` methods, and
//! [`McpClient::connect_stdio`] wires it to a server subprocess.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::oneshot;

use sturdy_core::HarnessError;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("transport i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("connection closed before response for request {0}")]
    ConnectionClosed(u64),
    #[error("unexpected response shape: {0}")]
    Protocol(String),
}

impl From<McpError> for HarnessError {
    fn from(e: McpError) -> Self {
        HarnessError::backend("mcp", e)
    }
}

pub type Result<T> = std::result::Result<T, McpError>;

// ── JSON-RPC 2.0 wire types ──

#[derive(Debug, Serialize)]
struct RpcRequest<'a> {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    method: &'a str,
    params: Value,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    #[allow(dead_code)]
    #[serde(default)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcErrorBody>,
}

#[derive(Debug, Deserialize)]
struct RpcErrorBody {
    code: i64,
    message: String,
    #[allow(dead_code)]
    #[serde(default)]
    data: Option<Value>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>>;

/// A JSON-RPC 2.0 client over an arbitrary duplex byte stream.
pub struct RpcClient {
    writer: tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    pending: Pending,
    next_id: AtomicU64,
    _reader: tokio::task::JoinHandle<()>,
}

impl RpcClient {
    /// Build a client from a write half and a read half. Spawns the background
    /// reader that dispatches responses; requires a Tokio runtime.
    pub fn new(
        writer: impl AsyncWrite + Unpin + Send + 'static,
        reader: impl AsyncRead + Unpin + Send + 'static,
    ) -> Self {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        let handle = tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<RpcResponse>(&line) {
                    Ok(resp) => {
                        if let Some(id) = resp.id {
                            if let Some(tx) = reader_pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(resp);
                            }
                            // Unmatched id → a stale/duplicate response; drop it.
                        }
                        // No id → a server notification; nothing to await, ignore.
                    }
                    Err(e) => tracing::warn!(error = %e, line = %line, "unparseable rpc line"),
                }
            }
            // Stream closed: fail every outstanding request.
            reader_pending.lock().unwrap().clear();
        });

        RpcClient {
            writer: tokio::sync::Mutex::new(Box::new(writer)),
            pending,
            next_id: AtomicU64::new(1),
            _reader: handle,
        }
    }

    async fn write_line(&self, bytes: Vec<u8>) -> Result<()> {
        let mut w = self.writer.lock().await;
        w.write_all(&bytes).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok(())
    }

    /// Issue a request and await its response.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let req = RpcRequest {
            jsonrpc: "2.0",
            id: Some(id),
            method,
            params,
        };
        self.write_line(serde_json::to_vec(&req)?).await?;

        match rx.await {
            Ok(resp) => {
                if let Some(err) = resp.error {
                    return Err(McpError::Rpc {
                        code: err.code,
                        message: err.message,
                    });
                }
                Ok(resp.result.unwrap_or(Value::Null))
            }
            Err(_) => Err(McpError::ConnectionClosed(id)),
        }
    }

    /// Fire a notification (no id, no response awaited).
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let req = RpcRequest {
            jsonrpc: "2.0",
            id: None,
            method,
            params,
        };
        self.write_line(serde_json::to_vec(&req)?).await
    }
}

// ── MCP layer ──

/// A tool advertised by an MCP server.
#[derive(Debug, Clone, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

/// The distilled result of a `tools/call`.
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// All `text` content items concatenated.
    pub text: String,
    pub is_error: bool,
    pub raw: Value,
}

/// Server identity from the `initialize` handshake.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub protocol_version: String,
}

/// The protocol version this client negotiates.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// A connected MCP client.
pub struct McpClient {
    rpc: RpcClient,
    /// Kept alive so the server subprocess is killed on drop.
    _child: Option<Child>,
}

impl McpClient {
    /// Wrap an existing duplex stream (used in tests and custom transports).
    pub fn from_streams(
        writer: impl AsyncWrite + Unpin + Send + 'static,
        reader: impl AsyncRead + Unpin + Send + 'static,
    ) -> Self {
        McpClient {
            rpc: RpcClient::new(writer, reader),
            _child: None,
        }
    }

    /// Spawn an MCP server subprocess and speak to it over its stdio.
    pub async fn connect_stdio(
        program: impl AsRef<std::ffi::OsStr>,
        args: &[&str],
    ) -> Result<Self> {
        use std::process::Stdio;
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Protocol("server has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Protocol("server has no stdout".into()))?;
        Ok(McpClient {
            rpc: RpcClient::new(stdin, stdout),
            _child: Some(child),
        })
    }

    /// Perform the MCP `initialize` handshake and send `initialized`.
    pub async fn initialize(&self, client_name: &str) -> Result<ServerInfo> {
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": client_name, "version": env!("CARGO_PKG_VERSION") },
        });
        let result = self.rpc.request("initialize", params).await?;
        let info = ServerInfo {
            protocol_version: result
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or(PROTOCOL_VERSION)
                .to_string(),
            name: result
                .pointer("/serverInfo/name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            version: result
                .pointer("/serverInfo/version")
                .and_then(|v| v.as_str())
                .unwrap_or("0.0.0")
                .to_string(),
        };
        self.rpc
            .notify("notifications/initialized", Value::Null)
            .await?;
        Ok(info)
    }

    /// List the tools the server exposes.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let result = self.rpc.request("tools/list", serde_json::json!({})).await?;
        let tools = result
            .get("tools")
            .cloned()
            .ok_or_else(|| McpError::Protocol("tools/list missing `tools`".into()))?;
        Ok(serde_json::from_value(tools)?)
    }

    /// Invoke a tool by name.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolResult> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let result = self.rpc.request("tools/call", params).await?;
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let text = result
            .get("content")
            .and_then(|c| c.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Ok(ToolResult {
            text,
            is_error,
            raw: result,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spin up an in-memory MCP-ish server on a duplex pair and return a client
    /// wired to it. The server handles initialize / tools/list / tools/call.
    fn mock_server() -> McpClient {
        // client -> server
        let (c2s_client, mut c2s_server) = tokio::io::duplex(8192);
        // server -> client
        let (mut s2c_server, s2c_client) = tokio::io::duplex(8192);

        tokio::spawn(async move {
            let mut lines = BufReader::new(&mut c2s_server).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = req.get("id").cloned();
                let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                // Notifications (no id) get no reply.
                let Some(id) = id else { continue };

                let result = match method {
                    "initialize" => serde_json::json!({
                        "protocolVersion": PROTOCOL_VERSION,
                        "serverInfo": { "name": "mock", "version": "1.2.3" },
                        "capabilities": {}
                    }),
                    "tools/list" => serde_json::json!({
                        "tools": [
                            { "name": "read_file", "description": "reads a file", "inputSchema": {} }
                        ]
                    }),
                    "tools/call" => {
                        let tool = req.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("");
                        serde_json::json!({
                            "content": [ { "type": "text", "text": format!("called {tool}") } ],
                            "isError": false
                        })
                    }
                    _ => serde_json::json!(null),
                };
                let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
                let mut bytes = serde_json::to_vec(&resp).unwrap();
                bytes.push(b'\n');
                use tokio::io::AsyncWriteExt;
                if s2c_server.write_all(&bytes).await.is_err() {
                    break;
                }
                let _ = s2c_server.flush().await;
            }
        });

        McpClient::from_streams(c2s_client, s2c_client)
    }

    #[tokio::test]
    async fn initialize_handshake() {
        let client = mock_server();
        let info = client.initialize("sturdy-test").await.unwrap();
        assert_eq!(info.name, "mock");
        assert_eq!(info.version, "1.2.3");
    }

    #[tokio::test]
    async fn list_and_call_tools() {
        let client = mock_server();
        client.initialize("sturdy-test").await.unwrap();

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");

        let res = client
            .call_tool("read_file", serde_json::json!({ "path": "x" }))
            .await
            .unwrap();
        assert!(!res.is_error);
        assert_eq!(res.text, "called read_file");
    }

    #[tokio::test]
    async fn concurrent_requests_are_demultiplexed() {
        let client = Arc::new(mock_server());
        client.initialize("t").await.unwrap();
        // Fire several calls concurrently; each must get its own answer.
        let mut handles = Vec::new();
        for i in 0..8 {
            let c = client.clone();
            handles.push(tokio::spawn(async move {
                c.call_tool(&format!("tool{i}"), Value::Null).await.unwrap().text
            }));
        }
        for (i, h) in handles.into_iter().enumerate() {
            assert_eq!(h.await.unwrap(), format!("called tool{i}"));
        }
    }
}
