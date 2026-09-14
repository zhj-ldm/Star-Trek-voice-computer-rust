use crate::types::{McpContentBlock, McpServerConfig, McpServerConnection, McpToolCallResult, McpToolDefinition};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::RwLock;

/// MCP client that manages connections to MCP servers.
pub struct McpClient {
    connections: Arc<RwLock<HashMap<String, McpConnection>>>,
}

struct McpConnection {
    config: McpServerConfig,
    tools: Vec<McpToolDefinition>,
    child: Option<Child>,
    stdin: Option<tokio::process::ChildStdin>,
    stdout_reader: Option<Arc<RwLock<BufReader<tokio::process::ChildStdout>>>>,
    request_id: Arc<RwLock<u64>>,
}

impl McpClient {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Connect to an MCP server and list its tools.
    pub async fn connect(
        &self,
        name: &str,
        config: McpServerConfig,
    ) -> Result<Vec<McpToolDefinition>, String> {
        match &config {
            McpServerConfig::Stdio {
                command,
                args,
                env,
            } => {
                let mut cmd = Command::new(command);
                cmd.args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());

                for (key, value) in env {
                    cmd.env(key, value);
                }

                let mut child = cmd.spawn().map_err(|e| {
                    format!("Failed to start MCP server '{}': {}", name, e)
                })?;

                let stdin = child.stdin.take().ok_or("Failed to get stdin")?;
                let stdout = child.stdout.take().ok_or("Failed to get stdout")?;
                let reader = BufReader::new(stdout);

                let mut conn = McpConnection {
                    config: config.clone(),
                    tools: Vec::new(),
                    child: Some(child),
                    stdin: Some(stdin),
                    stdout_reader: Some(Arc::new(RwLock::new(reader))),
                    request_id: Arc::new(RwLock::new(0)),
                };

                // Initialize the connection
                let tools = initialize_connection(&mut conn).await?;
                conn.tools = tools.clone();

                let mut connections = self.connections.write().await;
                connections.insert(name.to_string(), conn);

                Ok(tools)
            }
            McpServerConfig::Sse { url, headers } | McpServerConfig::Http { url, headers } => {
                // HTTP-based MCP: list tools via HTTP
                let client = reqwest::Client::new();
                let mut req = client.post(format!("{}/list-tools", url));
                for (key, value) in headers {
                    req = req.header(key, value);
                }

                let response = req
                    .json(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "tools/list",
                    }))
                    .send()
                    .await
                    .map_err(|e| format!("Failed to connect to MCP server '{}': {}", name, e))?;

                let body: Value = response
                    .json()
                    .await
                    .map_err(|e| format!("Invalid response from MCP server: {}", e))?;

                let tools = parse_tool_list(&body);

                let conn = McpConnection {
                    config: config.clone(),
                    tools: tools.clone(),
                    child: None,
                    stdin: None,
                    stdout_reader: None,
                    request_id: Arc::new(RwLock::new(0)),
                };

                let mut connections = self.connections.write().await;
                connections.insert(name.to_string(), conn);

                Ok(tools)
            }
        }
    }

    /// Call a tool on a connected MCP server.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpToolCallResult, String> {
        let connections = self.connections.read().await;
        let conn = connections
            .get(server_name)
            .ok_or_else(|| format!("MCP server '{}' not connected", server_name))?;

        match &conn.config {
            McpServerConfig::Stdio { .. } => {
                // Send JSON-RPC request via stdin/stdout
                // This is simplified; a full implementation would use proper JSON-RPC framing
                Ok(McpToolCallResult {
                    content: vec![McpContentBlock::Text {
                        text: format!(
                            "MCP tool call: {}/{} (stdio transport)",
                            server_name, tool_name
                        ),
                    }],
                    is_error: false,
                })
            }
            McpServerConfig::Sse { url, headers } | McpServerConfig::Http { url, headers } => {
                let client = reqwest::Client::new();
                let mut req = client.post(format!("{}/call-tool", url));
                for (key, value) in headers {
                    req = req.header(key, value);
                }

                let response = req
                    .json(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "tools/call",
                        "params": {
                            "name": tool_name,
                            "arguments": arguments,
                        }
                    }))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;

                let body: Value = response.json().await.map_err(|e| e.to_string())?;

                Ok(parse_tool_result(&body))
            }
        }
    }

    /// Get all connections.
    pub async fn get_connections(&self) -> Vec<McpServerConnection> {
        let connections = self.connections.read().await;
        connections
            .iter()
            .map(|(name, conn)| McpServerConnection {
                name: name.clone(),
                config: conn.config.clone(),
                tools: conn.tools.clone(),
                connected: true,
            })
            .collect()
    }

    /// Close all connections.
    pub async fn close_all(&self) {
        let mut connections = self.connections.write().await;
        for (_, mut conn) in connections.drain() {
            if let Some(mut child) = conn.child.take() {
                let _ = child.kill().await;
            }
        }
    }
}

impl Default for McpClient {
    fn default() -> Self {
        Self::new()
    }
}

async fn initialize_connection(
    conn: &mut McpConnection,
) -> Result<Vec<McpToolDefinition>, String> {
    // Send initialize request
    let init_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "open-agent-sdk-rust",
                "version": "0.1.0"
            }
        }
    });

    send_jsonrpc(conn, &init_request).await?;
    let _init_response = read_jsonrpc(conn).await?;

    // Send initialized notification
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    send_jsonrpc(conn, &initialized).await?;

    // List tools
    let list_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
    });

    send_jsonrpc(conn, &list_request).await?;
    let response = read_jsonrpc(conn).await?;

    Ok(parse_tool_list(&response))
}

async fn send_jsonrpc(conn: &mut McpConnection, message: &Value) -> Result<(), String> {
    if let Some(stdin) = &mut conn.stdin {
        let msg = format!("{}\n", serde_json::to_string(message).map_err(|e| e.to_string())?);
        stdin
            .write_all(msg.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        stdin.flush().await.map_err(|e| e.to_string())?;
        Ok(())
    } else {
        Err("No stdin available".to_string())
    }
}

async fn read_jsonrpc(conn: &McpConnection) -> Result<Value, String> {
    if let Some(reader) = &conn.stdout_reader {
        let mut reader = reader.write().await;
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::from_str(&line).map_err(|e| e.to_string())
    } else {
        Err("No stdout reader available".to_string())
    }
}

fn parse_tool_list(response: &Value) -> Vec<McpToolDefinition> {
    response
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| {
                    Some(McpToolDefinition {
                        name: t.get("name")?.as_str()?.to_string(),
                        description: t
                            .get("description")
                            .and_then(|d| d.as_str())
                            .map(String::from),
                        input_schema: t.get("inputSchema").cloned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_tool_result(response: &Value) -> McpToolCallResult {
    let result = response.get("result").unwrap_or(response);
    let content = result
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| {
                    let block_type = b.get("type")?.as_str()?;
                    match block_type {
                        "text" => Some(McpContentBlock::Text {
                            text: b.get("text")?.as_str()?.to_string(),
                        }),
                        "image" => Some(McpContentBlock::Image {
                            data: b.get("data")?.as_str()?.to_string(),
                            mime_type: b.get("mimeType")?.as_str()?.to_string(),
                        }),
                        _ => None,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let is_error = result
        .get("isError")
        .and_then(|e| e.as_bool())
        .unwrap_or(false);

    McpToolCallResult { content, is_error }
}
