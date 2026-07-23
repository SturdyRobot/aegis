//! # sturdy-mcp
//!
//! A native, async [Model Context Protocol](https://modelcontextprotocol.io)
//! client speaking JSON-RPC 2.0 over two transports:
//!
//! * **stdio** — [`RpcClient`], generic over any `AsyncRead`/`AsyncWrite` pair. A
//!   background reader task de-multiplexes responses to per-request oneshot
//!   channels keyed by id, so many calls can be in flight at once.
//! * **streamable HTTP** — [`HttpTransport`], for remote MCP servers. POSTs
//!   JSON-RPC and reads either a direct JSON response or an SSE stream, tracking
//!   the `Mcp-Session-Id` handshake header.
//!
//! Both implement the [`Transport`] trait; [`McpClient`] adds the MCP handshake
//! and `tools/*` methods over either. [`McpClientManager`] connects a fleet of
//! configured servers ([`McpServerConfig`]) and routes tool calls to the owner.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    #[error("connection to the MCP server is closed")]
    Closed,
    #[error("request `{method}` timed out after {secs}s")]
    Timeout { method: String, secs: u64 },
    #[error("unexpected response shape: {0}")]
    Protocol(String),
}

impl From<McpError> for HarnessError {
    fn from(e: McpError) -> Self {
        HarnessError::backend("mcp", e)
    }
}

pub type Result<T> = std::result::Result<T, McpError>;

// ── transport abstraction ──

/// The RPC primitive every transport provides: a bidirectional JSON-RPC 2.0 pipe.
///
/// [`RpcClient`] implements it over stdio; [`HttpTransport`] over streamable HTTP.
/// [`McpClient`]'s handshake and `tools/*` methods are written against this trait,
/// so they work unchanged regardless of how bytes reach the server.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Issue a request and await its response.
    async fn request(&self, method: &str, params: Value) -> Result<Value>;
    /// Fire a notification (no id, no response awaited).
    async fn notify(&self, method: &str, params: Value) -> Result<()>;
    /// Adjust the per-request deadline. No-op for transports without one.
    fn set_request_timeout(&self, _timeout: Duration) {}
}

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
    /// Set once the reader task ends (EOF), so new requests fail fast instead of
    /// registering a waiter nobody will ever complete.
    closed: Arc<AtomicBool>,
    /// Per-request deadline in ms; a silent server can never hang the caller
    /// forever. Atomic so it stays adjustable behind a shared `Arc<dyn Transport>`.
    request_timeout_ms: AtomicU64,
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
        let closed = Arc::new(AtomicBool::new(false));
        let reader_pending = pending.clone();
        let reader_closed = closed.clone();
        let handle = tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<RpcResponse>(&line) {
                            Ok(resp) => {
                                if let Some(id) = resp.id {
                                    if let Some(tx) = reader_pending
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .remove(&id)
                                    {
                                        let _ = tx.send(resp);
                                    }
                                    // Unmatched id → stale/duplicate response; drop.
                                }
                                // No id → a server notification; ignore.
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, line = %line, "unparseable rpc line")
                            }
                        }
                    }
                    // A single transport decode error (e.g. invalid UTF-8) must
                    // not tear down the reader — log and keep going.
                    Err(e) => tracing::warn!(error = %e, "rpc read error; continuing"),
                    // Real EOF: stop.
                    Ok(None) => break,
                }
            }
            // Stream closed: mark it and fail every outstanding request.
            reader_closed.store(true, Ordering::SeqCst);
            reader_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        });

        RpcClient {
            writer: tokio::sync::Mutex::new(Box::new(writer)),
            pending,
            next_id: AtomicU64::new(1),
            closed,
            request_timeout_ms: AtomicU64::new(60_000),
            _reader: handle,
        }
    }

    /// Set the per-request deadline (default 60s).
    pub fn set_request_timeout(&self, timeout: Duration) {
        self.request_timeout_ms
            .store(timeout.as_millis() as u64, Ordering::SeqCst);
    }

    async fn write_line(&self, bytes: Vec<u8>) -> Result<()> {
        let mut w = self.writer.lock().await;
        w.write_all(&bytes).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok(())
    }

    /// Issue a request and await its response, bounded by the request timeout.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        // Fail fast if the reader has already ended — otherwise we'd register a
        // waiter nobody will ever complete.
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::Closed);
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);

        let req = RpcRequest {
            jsonrpc: "2.0",
            id: Some(id),
            method,
            params,
        };
        if let Err(e) = self.write_line(serde_json::to_vec(&req)?).await {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id); // don't leak the waiter
            return Err(e);
        }

        let dur = Duration::from_millis(self.request_timeout_ms.load(Ordering::SeqCst));
        match tokio::time::timeout(dur, rx).await {
            Ok(Ok(resp)) => {
                if let Some(err) = resp.error {
                    return Err(McpError::Rpc {
                        code: err.code,
                        message: err.message,
                    });
                }
                Ok(resp.result.unwrap_or(Value::Null))
            }
            // Sender dropped ⇒ the reader exited (connection closed).
            Ok(Err(_)) => Err(McpError::ConnectionClosed(id)),
            // Deadline hit ⇒ reclaim the pending slot and report the timeout.
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(McpError::Timeout {
                    method: method.to_string(),
                    secs: dur.as_secs(),
                })
            }
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

#[async_trait::async_trait]
impl Transport for RpcClient {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        RpcClient::request(self, method, params).await
    }
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        RpcClient::notify(self, method, params).await
    }
    fn set_request_timeout(&self, timeout: Duration) {
        RpcClient::set_request_timeout(self, timeout);
    }
}

// ── streamable HTTP transport ──

/// A [`Transport`] for remote MCP servers over
/// [streamable HTTP](https://modelcontextprotocol.io/specification/2025-03-26/basic/transports).
///
/// Each call POSTs a JSON-RPC message; the server answers with either a direct
/// `application/json` body or a `text/event-stream` (SSE) carrying the response.
/// The `Mcp-Session-Id` returned by `initialize` is captured and echoed on every
/// later request.
pub struct HttpTransport {
    http: reqwest::Client,
    url: String,
    session_id: Mutex<Option<String>>,
    next_id: AtomicU64,
    request_timeout_ms: AtomicU64,
}

impl HttpTransport {
    /// Build a transport pointed at an MCP endpoint URL. `extra_headers` are sent
    /// on every request (e.g. `Authorization`).
    pub fn new(url: impl Into<String>, extra_headers: HashMap<String, String>) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        for (k, v) in extra_headers {
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(&v),
            ) {
                headers.insert(name, val);
            }
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| McpError::Protocol(format!("building http client: {e}")))?;
        Ok(HttpTransport {
            http,
            url: url.into(),
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(1),
            request_timeout_ms: AtomicU64::new(60_000),
        })
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.request_timeout_ms.load(Ordering::SeqCst))
    }

    /// POST one JSON-RPC message. `id` is `Some` for requests, `None` for
    /// notifications. Returns the raw response body text (empty for a 202).
    async fn post(&self, id: Option<u64>, method: &str, params: Value) -> Result<Option<Value>> {
        let body = RpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let mut req = self
            .http
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&body);
        if let Some(sid) = self
            .session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            req = req.header("mcp-session-id", sid);
        }

        let resp = tokio::time::timeout(self.timeout(), req.send())
            .await
            .map_err(|_| McpError::Timeout {
                method: method.to_string(),
                secs: self.timeout().as_secs(),
            })?
            .map_err(|e| McpError::Protocol(format!("http request failed: {e}")))?;

        // Capture the session id from the initialize response for reuse.
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(sid.to_string());
        }

        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return Err(McpError::Protocol(format!(
                "http {status}: {}",
                detail.chars().take(200).collect::<String>()
            )));
        }
        // A notification (no id) yields 202 Accepted with no useful body. Bind the
        // id here so the SSE path below can't be left holding an `unwrap`.
        let Some(want_id) = id else {
            return Ok(None);
        };

        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let value = if ctype.contains("text/event-stream") {
            read_sse_response(resp, want_id, self.timeout()).await?
        } else {
            resp.json::<Value>()
                .await
                .map_err(|e| McpError::Protocol(format!("decoding json response: {e}")))?
        };
        Ok(Some(value))
    }
}

/// Read an SSE body until the JSON-RPC response with `want_id` arrives.
///
/// SSE frames are blank-line-delimited; we accumulate `data:` payloads per frame,
/// parse each as JSON, and return the first that is the response we're waiting on.
async fn read_sse_response(
    resp: reqwest::Response,
    want_id: u64,
    deadline: Duration,
) -> Result<Value> {
    use futures_util::StreamExt;
    let read = async {
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| McpError::Protocol(format!("sse read: {e}")))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            // Process every complete frame (terminated by a blank line).
            while let Some(idx) = buf.find("\n\n").or_else(|| buf.find("\r\n\r\n")) {
                let sep = if buf[idx..].starts_with("\n\n") { 2 } else { 4 };
                let frame: String = buf.drain(..idx + sep).collect();
                let data: String = frame
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(|d| d.strip_prefix(' ').unwrap_or(d))
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&data) {
                    if v.get("id").and_then(|i| i.as_u64()) == Some(want_id) {
                        return Ok(v);
                    }
                }
            }
        }
        Err(McpError::Protocol(
            "sse stream ended before a response".into(),
        ))
    };
    tokio::time::timeout(deadline, read)
        .await
        .map_err(|_| McpError::Timeout {
            method: "sse".into(),
            secs: deadline.as_secs(),
        })?
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let raw = self
            .post(Some(id), method, params)
            .await?
            .ok_or_else(|| McpError::Protocol("empty http response for a request".into()))?;
        let resp: RpcResponse = serde_json::from_value(raw)?;
        if let Some(err) = resp.error {
            return Err(McpError::Rpc {
                code: err.code,
                message: err.message,
            });
        }
        Ok(resp.result.unwrap_or(Value::Null))
    }
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.post(None, method, params).await.map(|_| ())
    }
    fn set_request_timeout(&self, timeout: Duration) {
        self.request_timeout_ms
            .store(timeout.as_millis() as u64, Ordering::SeqCst);
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

/// A connected MCP client, transport-agnostic (stdio or HTTP).
pub struct McpClient {
    transport: Arc<dyn Transport>,
    /// Kept alive so the server subprocess is killed on drop. Behind a `Mutex`
    /// so the whole client is `Sync` and can be shared as a `dyn ToolExecutor`.
    /// `None` for HTTP transports (no owned process).
    _child: Mutex<Option<Child>>,
}

impl McpClient {
    /// Wrap an existing duplex stream (used in tests and custom transports).
    pub fn from_streams(
        writer: impl AsyncWrite + Unpin + Send + 'static,
        reader: impl AsyncRead + Unpin + Send + 'static,
    ) -> Self {
        McpClient {
            transport: Arc::new(RpcClient::new(writer, reader)),
            _child: Mutex::new(None),
        }
    }

    /// Wrap any [`Transport`] directly (e.g. an [`HttpTransport`]).
    pub fn from_transport(transport: Arc<dyn Transport>) -> Self {
        McpClient {
            transport,
            _child: Mutex::new(None),
        }
    }

    /// Connect a server described by an [`McpServerConfig`], dispatching on its
    /// transport (subprocess for stdio, HTTP client for http).
    pub async fn connect(config: &McpServerConfig) -> Result<Self> {
        match &config.transport {
            McpTransport::Stdio { command, args } => {
                let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                Self::connect_stdio_env(command, &argv, config.env.as_ref()).await
            }
            McpTransport::Http { url } => {
                let headers = config.env.clone().unwrap_or_default();
                Ok(Self::from_transport(Arc::new(HttpTransport::new(
                    url.clone(),
                    headers,
                )?)))
            }
        }
    }

    /// Set the per-request deadline (default 60s).
    pub fn set_request_timeout(&self, timeout: Duration) {
        self.transport.set_request_timeout(timeout);
    }

    /// Spawn an MCP server subprocess and speak to it over its stdio.
    pub async fn connect_stdio(
        program: impl AsRef<std::ffi::OsStr>,
        args: &[&str],
    ) -> Result<Self> {
        Self::connect_stdio_env(program, args, None).await
    }

    /// Like [`connect_stdio`](Self::connect_stdio) but with extra environment
    /// variables for the child (e.g. `GITHUB_TOKEN` for the GitHub MCP server).
    pub async fn connect_stdio_env(
        program: impl AsRef<std::ffi::OsStr>,
        args: &[&str],
        env: Option<&HashMap<String, String>>,
    ) -> Result<Self> {
        use std::process::Stdio;
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(env) = env {
            cmd.envs(env);
        }
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Protocol("server has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Protocol("server has no stdout".into()))?;
        Ok(McpClient {
            transport: Arc::new(RpcClient::new(stdin, stdout)),
            _child: Mutex::new(Some(child)),
        })
    }

    /// Perform the MCP `initialize` handshake and send `initialized`.
    #[tracing::instrument(name = "mcp.initialize", skip_all, fields(client = %client_name))]
    pub async fn initialize(&self, client_name: &str) -> Result<ServerInfo> {
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": client_name, "version": env!("CARGO_PKG_VERSION") },
        });
        let result = self.transport.request("initialize", params).await?;
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
        self.transport
            .notify("notifications/initialized", Value::Null)
            .await?;
        Ok(info)
    }

    /// List the tools the server exposes (auto-discovery).
    #[tracing::instrument(name = "mcp.list_tools", skip_all, fields(tool_count = tracing::field::Empty))]
    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let result = self
            .transport
            .request("tools/list", serde_json::json!({}))
            .await?;
        let tools = result
            .get("tools")
            .cloned()
            .ok_or_else(|| McpError::Protocol("tools/list missing `tools`".into()))?;
        let tools: Vec<McpTool> = serde_json::from_value(tools)?;
        tracing::Span::current().record("tool_count", tools.len());
        Ok(tools)
    }

    /// Invoke a tool by name.
    #[tracing::instrument(name = "mcp.call_tool", skip_all, fields(tool = %name, is_error = tracing::field::Empty))]
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolResult> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let result = self.transport.request("tools/call", params).await?;
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
        tracing::Span::current().record("is_error", is_error);
        Ok(ToolResult {
            text,
            is_error,
            raw: result,
        })
    }
}

/// Bridge an MCP server into the engine's tool interface: a `tools/call` becomes
/// a tool `Observation`. A tool-level error (`isError`) is surfaced as an *error
/// observation* the agent can react to, not a fatal harness error.
#[async_trait::async_trait]
impl sturdy_core::ToolExecutor for McpClient {
    async fn execute(
        &self,
        call: &sturdy_core::ToolCall,
    ) -> sturdy_core::Result<sturdy_core::Observation> {
        let result = self
            .call_tool(&call.name, call.arguments.clone())
            .await
            .map_err(sturdy_core::HarnessError::from)?;
        Ok(if result.is_error {
            sturdy_core::Observation::error(result.text)
        } else {
            sturdy_core::Observation::ok(result.text)
        })
    }
}

// ── multi-server configuration + manager ──

/// How to reach one MCP server.
#[derive(Debug, Clone)]
pub enum McpTransport {
    /// Launch a server subprocess and speak over its stdio.
    Stdio { command: String, args: Vec<String> },
    /// Connect to a remote server over streamable HTTP.
    Http { url: String },
}

/// A named MCP server to connect. `env` supplies child environment variables for
/// stdio servers, or extra HTTP headers for http servers.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    pub env: Option<HashMap<String, String>>,
}

/// One connected, initialized server.
struct ManagedServer {
    name: String,
    client: McpClient,
}

/// Connects a fleet of MCP servers, aggregates their tools, and routes each tool
/// call to the server that owns it. Implements [`sturdy_core::ToolExecutor`] so
/// the ReAct engine can drive every server's tools as a single toolset.
pub struct McpClientManager {
    servers: Vec<ManagedServer>,
    /// tool name → index into `servers`. On a name collision the first wins.
    routes: HashMap<String, usize>,
    tools: Vec<McpTool>,
    /// When set, each tool call is journaled to the ledger for the given run.
    ledger: Option<(Arc<sturdy_ledger::Ledger>, sturdy_core::TaskId)>,
}

impl McpClientManager {
    /// Connect and initialize every configured server, discovering its tools. A
    /// server that fails to connect is logged and skipped — one bad server can't
    /// sink the whole fleet.
    #[tracing::instrument(name = "mcp.connect_all", skip_all, fields(configured = configs.len(), connected = tracing::field::Empty))]
    pub async fn connect(configs: &[McpServerConfig], client_name: &str) -> Result<Self> {
        let mut servers = Vec::new();
        let mut routes = HashMap::new();
        let mut tools = Vec::new();
        for cfg in configs {
            match Self::connect_one(cfg, client_name).await {
                Ok((client, server_tools)) => {
                    let idx = servers.len();
                    for t in &server_tools {
                        routes.entry(t.name.clone()).or_insert(idx);
                    }
                    tools.extend(server_tools);
                    servers.push(ManagedServer {
                        name: cfg.name.clone(),
                        client,
                    });
                }
                Err(e) => tracing::warn!(server = %cfg.name, error = %e, "MCP server skipped"),
            }
        }
        tracing::Span::current().record("connected", servers.len());
        Ok(McpClientManager {
            servers,
            routes,
            tools,
            ledger: None,
        })
    }

    async fn connect_one(
        cfg: &McpServerConfig,
        client_name: &str,
    ) -> Result<(McpClient, Vec<McpTool>)> {
        let client = McpClient::connect(cfg).await?;
        client.initialize(client_name).await?;
        let tools = client.list_tools().await?;
        Ok((client, tools))
    }

    /// Journal every subsequent tool call as an `McpToolExecution` event on `run_id`.
    pub fn with_ledger(
        mut self,
        ledger: Arc<sturdy_ledger::Ledger>,
        run_id: sturdy_core::TaskId,
    ) -> Self {
        self.ledger = Some((ledger, run_id));
        self
    }

    /// Every tool discovered across all connected servers.
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// How many servers are connected.
    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    /// Route a tool call to the server that advertised it, journaling it if a
    /// ledger is attached.
    #[tracing::instrument(name = "mcp.route_call", skip_all, fields(tool = %name))]
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolResult> {
        let idx = *self.routes.get(name).ok_or_else(|| {
            McpError::Protocol(format!("no connected MCP server exposes tool `{name}`"))
        })?;
        let result = self.servers[idx]
            .client
            .call_tool(name, arguments.clone())
            .await?;
        if let Some((ledger, run_id)) = &self.ledger {
            let event = sturdy_ledger::Event::McpToolExecution {
                server: self.servers[idx].name.clone(),
                tool: name.to_string(),
                arguments,
                output: result.text.clone(),
                is_error: result.is_error,
            };
            if let Err(e) = ledger.record_event(*run_id, &event) {
                tracing::warn!(error = %e, "failed to journal MCP tool execution");
            }
        }
        Ok(result)
    }
}

#[async_trait::async_trait]
impl sturdy_core::ToolExecutor for McpClientManager {
    async fn execute(
        &self,
        call: &sturdy_core::ToolCall,
    ) -> sturdy_core::Result<sturdy_core::Observation> {
        let result = self
            .call_tool(&call.name, call.arguments.clone())
            .await
            .map_err(sturdy_core::HarnessError::from)?;
        Ok(if result.is_error {
            sturdy_core::Observation::error(result.text)
        } else {
            sturdy_core::Observation::ok(result.text)
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
                        let tool = req
                            .pointer("/params/name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("");
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
    async fn request_times_out_on_a_silent_server() {
        // A server that reads the request but never replies. The connection stays
        // open (no EOF), so only the request timeout can rescue the caller.
        let (c2s_client, mut c2s_server) = tokio::io::duplex(8192);
        let (_s2c_server, s2c_client) = tokio::io::duplex(8192);
        tokio::spawn(async move {
            let mut lines = BufReader::new(&mut c2s_server).lines();
            while let Ok(Some(_)) = lines.next_line().await {} // drain, never answer
        });

        let client = McpClient::from_streams(c2s_client, s2c_client);
        client.set_request_timeout(Duration::from_millis(150));
        let err = client.initialize("t").await.unwrap_err();
        assert!(matches!(err, McpError::Timeout { .. }), "got {err:?}");
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
                c.call_tool(&format!("tool{i}"), Value::Null)
                    .await
                    .unwrap()
                    .text
            }));
        }
        for (i, h) in handles.into_iter().enumerate() {
            assert_eq!(h.await.unwrap(), format!("called tool{i}"));
        }
    }
}
