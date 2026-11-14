//! MCP（Model Context Protocol）服务管理与连通性探测。
//!
//! 支持 stdio 与 HTTP/SSE 两种传输：保存配置后立即尝试 initialize + tools/list，
//! 前端据此显示「已连接 / 未连接」与工具列表。配置持久化在数据目录 mcp.json。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

const PROTOCOL_VERSION: &str = "2024-11-05";
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_transport() -> String {
    "stdio".into()
}

fn default_true() -> bool {
    true
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("名称不能为空".into());
        }
        match self.transport.as_str() {
            "stdio" => {
                if self.command.trim().is_empty() {
                    return Err("stdio 模式需要填写启动命令".into());
                }
            }
            "http" | "sse" => {
                if self.url.trim().is_empty() {
                    return Err("HTTP/SSE 模式需要填写 URL".into());
                }
            }
            other => return Err(format!("不支持的传输方式: {other}")),
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Default)]
pub struct McpStatus {
    pub connected: bool,
    pub error: Option<String>,
    pub tools: Vec<McpToolInfo>,
}

#[derive(Clone, Serialize)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

pub struct McpManager {
    path: PathBuf,
    servers: Mutex<Vec<McpServerConfig>>,
}

impl McpManager {
    pub fn new(path: PathBuf) -> Self {
        let servers = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<McpServerConfig>>(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            servers: Mutex::new(servers),
        }
    }

    async fn persist(&self, list: &[McpServerConfig]) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, text).map_err(|e| format!("写入 MCP 配置失败: {e}"))
    }

    pub async fn list(&self) -> Vec<McpServerConfig> {
        self.servers.lock().await.clone()
    }

    pub async fn upsert(&self, cfg: McpServerConfig) -> Result<(), String> {
        cfg.validate()?;
        let mut list = self.servers.lock().await;
        if let Some(slot) = list.iter_mut().find(|s| s.name == cfg.name) {
            *slot = cfg;
        } else {
            list.push(cfg);
        }
        self.persist(&list).await
    }

    pub async fn remove(&self, name: &str) -> bool {
        let mut list = self.servers.lock().await;
        let before = list.len();
        list.retain(|s| s.name != name);
        let removed = list.len() != before;
        let _ = self.persist(&list).await;
        removed
    }

    pub async fn probe(&self, cfg: &McpServerConfig) -> McpStatus {
        let result = timeout(PROBE_TIMEOUT, probe_inner(cfg)).await;
        match result {
            Err(_) => McpStatus {
                connected: false,
                error: Some("连接超时".into()),
                tools: Vec::new(),
            },
            Ok(Ok(tools)) => McpStatus {
                connected: true,
                error: None,
                tools,
            },
            Ok(Err(e)) => McpStatus {
                connected: false,
                error: Some(e),
                tools: Vec::new(),
            },
        }
    }
}

async fn probe_inner(cfg: &McpServerConfig) -> Result<Vec<McpToolInfo>, String> {
    match cfg.transport.as_str() {
        "stdio" => probe_stdio(cfg).await,
        "http" | "sse" => probe_http(cfg).await,
        other => Err(format!("不支持的传输方式: {other}")),
    }
}

fn rpc_line(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

fn rpc_notification(method: &str) -> String {
    json!({"jsonrpc": "2.0", "method": method}).to_string()
}

fn init_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "star-trek-computer", "version": "1.0"}
    })
}

fn parse_tools(result: &Value) -> Vec<McpToolInfo> {
    result
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let name = t.get("name")?.as_str()?.to_string();
                    let description = t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some(McpToolInfo { name, description })
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn probe_stdio(cfg: &McpServerConfig) -> Result<Vec<McpToolInfo>, String> {
    let mut cmd = Command::new(cfg.command.trim());
    cmd.args(&cfg.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 MCP 进程失败: {e}"))?;

    let mut stdin = child.stdin.take().ok_or("无法写入 MCP 进程")?;
    let stdout = child.stdout.take().ok_or("无法读取 MCP 进程")?;
    let mut lines = BufReader::new(stdout).lines();

    async fn send_line(
        stdin: &mut tokio::process::ChildStdin,
        line: String,
    ) -> Result<(), String> {
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        stdin.flush().await.map_err(|e| e.to_string())
    }

    send_line(&mut stdin, rpc_line(1, "initialize", init_params())).await?;

    let mut init_done = false;
    let mut tools: Option<Vec<McpToolInfo>> = None;
    while !(init_done && tools.is_some()) {
        let line = match timeout(Duration::from_secs(8), lines.next_line()).await {
            Err(_) => break,
            Ok(Ok(Some(l))) => l,
            Ok(Ok(None)) | Ok(Err(_)) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let id = msg.get("id").and_then(|v| v.as_u64());
        if id == Some(1) && !init_done {
            if let Some(err) = msg.get("error") {
                return Err(format!("initialize 失败: {err}"));
            }
            init_done = true;
            send_line(&mut stdin, rpc_notification("notifications/initialized")).await?;
            send_line(&mut stdin, rpc_line(2, "tools/list", json!({}))).await?;
        } else if id == Some(2) {
            if let Some(err) = msg.get("error") {
                return Err(format!("tools/list 失败: {err}"));
            }
            let result = msg.get("result").cloned().unwrap_or(Value::Null);
            tools = Some(parse_tools(&result));
        }
    }

    let _ = child.start_kill();
    match (init_done, tools) {
        (false, _) => Err("MCP 服务未返回 initialize 响应".into()),
        (true, None) => Err("MCP 服务未返回 tools/list 响应".into()),
        (_, Some(tools)) => Ok(tools),
    }
}

fn sse_payloads(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            current.push_str(data.trim_start());
            current.push('\n');
        } else if line.trim().is_empty() && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    let mut msgs = Vec::new();
    for chunk in out {
        if let Some(last) = chunk
            .lines()
            .filter(|l| !l.trim().is_empty())
            .next_back()
        {
            msgs.push(last.to_string());
        }
    }
    msgs
}

fn parse_http_body(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).ok();
    }
    for payload in sse_payloads(trimmed) {
        if let Ok(v) = serde_json::from_str::<Value>(&payload) {
            return Some(v);
        }
    }
    None
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

async fn post_rpc(
    client: &reqwest::Client,
    cfg: &McpServerConfig,
    session: Option<&str>,
    body: Value,
) -> Result<(Value, Option<String>), String> {
    let mut req = client
        .post(cfg.url.trim())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(sid) = session {
        req = req.header("Mcp-Session-Id", sid);
    }
    for (k, v) in &cfg.headers {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|e| format!("请求 MCP 服务失败: {e}"))?;
    let status = resp.status();
    let sid = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let text = resp.text().await.map_err(|e| e.to_string())?;
    let parsed = parse_http_body(&text)
        .ok_or_else(|| format!("MCP 返回无效响应（HTTP {status}）"))?;
    Ok((parsed, sid))
}

async fn probe_http(cfg: &McpServerConfig) -> Result<Vec<McpToolInfo>, String> {
    let client = http_client();
    let (init, sid) = post_rpc(
        &client,
        cfg,
        None,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await?;
    if let Some(err) = init.get("error") {
        return Err(format!("initialize 失败: {err}"));
    }
    let _ = post_rpc(
        &client,
        cfg,
        sid.as_deref(),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    let (res, _) = post_rpc(
        &client,
        cfg,
        sid.as_deref(),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )
    .await?;
    if let Some(err) = res.get("error") {
        return Err(format!("tools/list 失败: {err}"));
    }
    let result = res.get("result").cloned().unwrap_or(Value::Null);
    Ok(parse_tools(&result))
}
