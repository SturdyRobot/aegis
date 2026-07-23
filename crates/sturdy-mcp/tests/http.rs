//! Integration tests for the streamable-HTTP transport and the multi-server
//! manager, driven against a self-contained raw-TCP MCP mock (no web framework).

use std::collections::HashMap;
use std::sync::Arc;

use sturdy_mcp::{HttpTransport, McpClient, McpClientManager, McpServerConfig, McpTransport};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A minimal MCP-over-HTTP server. Handles initialize / notifications/initialized
/// / tools/list / tools/call over keep-alive connections. When `sse` is set,
/// request responses come back as a one-event `text/event-stream`; otherwise as a
/// direct `application/json` body. Returns the endpoint URL.
async fn mock_http_server(sse: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                while let Some(body) = read_request(&mut sock).await {
                    let req: serde_json::Value =
                        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

                    // A notification (no id) is acknowledged with 202, no body.
                    let Some(id) = req.get("id").cloned() else {
                        let _ = sock
                            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                            .await;
                        continue;
                    };

                    let result = match method {
                        "initialize" => serde_json::json!({
                            "protocolVersion": "2024-11-05",
                            "serverInfo": { "name": "httpmock", "version": "9.9.9" },
                            "capabilities": {}
                        }),
                        "tools/list" => serde_json::json!({
                            "tools": [{ "name": "echo", "description": "echoes", "inputSchema": {} }]
                        }),
                        "tools/call" => {
                            let tool = req
                                .pointer("/params/name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("");
                            serde_json::json!({
                                "content": [{ "type": "text", "text": format!("http:{tool}") }],
                                "isError": false
                            })
                        }
                        _ => serde_json::json!(null),
                    };
                    let rpc = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
                    // Exercise the session-id handshake on initialize.
                    let session = if method == "initialize" {
                        "Mcp-Session-Id: sess-123\r\n"
                    } else {
                        ""
                    };

                    let response = if sse {
                        let data = format!("data: {}\n\n", serde_json::to_string(&rpc).unwrap());
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n{session}Content-Length: {}\r\n\r\n{data}",
                            data.len()
                        )
                    } else {
                        let data = serde_json::to_string(&rpc).unwrap();
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session}Content-Length: {}\r\n\r\n{data}",
                            data.len()
                        )
                    };
                    if sock.write_all(response.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    format!("http://{addr}/mcp")
}

/// Read one HTTP request off the socket (headers + Content-Length body). Returns
/// the body, or `None` at EOF.
async fn read_request(sock: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let headers_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let headers = String::from_utf8_lossy(&buf[..headers_end]).to_lowercase();
    let content_len: usize = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < headers_end + content_len {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Some(String::from_utf8_lossy(&buf[headers_end..headers_end + content_len]).to_string())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[tokio::test]
async fn http_json_transport_discovers_and_calls() {
    let url = mock_http_server(false).await;
    let client =
        McpClient::from_transport(Arc::new(HttpTransport::new(url, HashMap::new()).unwrap()));

    let info = client.initialize("aegis-test").await.unwrap();
    assert_eq!(info.name, "httpmock");
    assert_eq!(info.version, "9.9.9");

    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    let res = client
        .call_tool("echo", serde_json::json!({ "x": 1 }))
        .await
        .unwrap();
    assert!(!res.is_error);
    assert_eq!(res.text, "http:echo");
}

#[tokio::test]
async fn http_sse_transport_parses_event_stream() {
    let url = mock_http_server(true).await;
    let client =
        McpClient::from_transport(Arc::new(HttpTransport::new(url, HashMap::new()).unwrap()));

    client.initialize("aegis-test").await.unwrap();
    let res = client
        .call_tool("echo", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(res.text, "http:echo");
}

#[tokio::test]
async fn manager_routes_calls_and_journals_mcp_events() {
    let url = mock_http_server(false).await;
    let cfg = McpServerConfig {
        name: "mock".into(),
        transport: McpTransport::Http { url },
        env: None,
    };
    let ledger = Arc::new(sturdy_ledger::Ledger::in_memory().unwrap());
    let run_id = sturdy_core::TaskId::new();

    let mgr = McpClientManager::connect(std::slice::from_ref(&cfg), "aegis-test")
        .await
        .unwrap()
        .with_ledger(ledger.clone(), run_id);

    assert_eq!(mgr.server_count(), 1);
    assert_eq!(mgr.tools().len(), 1);
    assert_eq!(mgr.tools()[0].name, "echo");

    let res = mgr.call_tool("echo", serde_json::json!({})).await.unwrap();
    assert_eq!(res.text, "http:echo");

    // Unknown tool routes to a clear error, not a panic.
    assert!(mgr.call_tool("nope", serde_json::json!({})).await.is_err());

    // The call was journaled as an McpToolExecution event.
    let events = ledger.events(run_id).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        sturdy_ledger::Event::McpToolExecution {
            server,
            tool,
            output,
            is_error,
            ..
        } => {
            assert_eq!(server, "mock");
            assert_eq!(tool, "echo");
            assert_eq!(output, "http:echo");
            assert!(!is_error);
        }
    }
}
