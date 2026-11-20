//! MCP（Model Context Protocol）服务管理与真实调用。
//!
//! 支持 stdio 与 HTTP/SSE 两种传输。连接是**持久会话**：initialize 一次后
//! 复用（stdio 子进程常驻 / HTTP 复用 Mcp-Session-Id），探测与 tools/call
//! 走同一条会话；传输层出错时丢弃会话并在下次调用时自动重建。
//! 工具清单（含 inputSchema）缓存在内存，供 Agent 每轮注册为可调用工具。
//! 配置持久化在数据目录 mcp.json。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

const PROTOCOL_VERSION: &str = "2024-11-05";
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

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
    /// 上游 inputSchema：注册进 Agent 时作为参数结构
    pub input_schema: Value,
}

/// 一条持久 MCP 会话
enum McpSession {
    Stdio {
        child: Child,
        stdin: ChildStdin,
        lines: Lines<BufReader<ChildStdout>>,
        next_id: u64,
    },
    Http {
        sid: Option<String>,
    },
}

impl McpSession {
    fn shutdown(&mut self) {
        if let McpSession::Stdio { child, .. } = self {
            let _ = child.start_kill();
        }
    }
}

pub struct McpManager {
    path: PathBuf,
    servers: Mutex<Vec<McpServerConfig>>,
    /// 最近一次探测结果（含工具 schema）：Agent 每轮从这里注册 MCP 工具
    status_cache: Mutex<HashMap<String, McpStatus>>,
    /// 持久会话（按服务名）。调用期间持锁串行化，出错即丢弃重建。
    sessions: Mutex<HashMap<String, McpSession>>,
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
            status_cache: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
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
            *slot = cfg.clone();
        } else {
            list.push(cfg.clone());
        }
        self.persist(&list).await?;
        drop(list);
        // 配置可能已变（命令/URL/头），丢弃旧会话强制下次重建
        self.drop_session(&cfg.name).await;
        Ok(())
    }

    pub async fn remove(&self, name: &str) -> bool {
        let mut list = self.servers.lock().await;
        let before = list.len();
        list.retain(|s| s.name != name);
        let removed = list.len() != before;
        let _ = self.persist(&list).await;
        drop(list);
        if removed {
            self.drop_session(name).await;
            self.status_cache.lock().await.remove(name);
        }
        removed
    }

    async fn drop_session(&self, name: &str) {
        if let Some(mut s) = self.sessions.lock().await.remove(name) {
            s.shutdown();
        }
    }

    /// 最近一次探测状态（不发起网络请求）
    pub async fn cached_status(&self, name: &str) -> Option<McpStatus> {
        self.status_cache.lock().await.get(name).cloned()
    }

    /// 已启用且已连接的服务及其工具清单：供 Agent 每轮注册为可调用工具
    pub async fn agent_tools(&self) -> Vec<(McpServerConfig, Vec<McpToolInfo>)> {
        let servers = self.servers.lock().await;
        let cache = self.status_cache.lock().await;
        servers
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| {
                cache
                    .get(&s.name)
                    .filter(|st| st.connected && !st.tools.is_empty())
                    .map(|st| (s.clone(), st.tools.clone()))
            })
            .collect()
    }

    /// 探测连通性并刷新工具缓存（复用/建立持久会话）
    pub async fn probe(&self, cfg: &McpServerConfig) -> McpStatus {
        let result = timeout(PROBE_TIMEOUT, self.probe_inner(cfg)).await;
        let status = match result {
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
        };
        self.status_cache
            .lock()
            .await
            .insert(cfg.name.clone(), status.clone());
        status
    }

    async fn probe_inner(&self, cfg: &McpServerConfig) -> Result<Vec<McpToolInfo>, String> {
        let mut sessions = self.sessions.lock().await;
        for attempt in 0..2 {
            if !sessions.contains_key(&cfg.name) {
                match establish(cfg).await {
                    Ok(s) => {
                        sessions.insert(cfg.name.clone(), s);
                    }
                    Err(e) => {
                        if attempt == 0 {
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
            let result = match sessions.get_mut(&cfg.name).unwrap() {
                McpSession::Stdio {
                    stdin,
                    lines,
                    next_id,
                    ..
                } => stdio_rpc(stdin, lines, next_id, "tools/list", json!({})).await,
                McpSession::Http { sid } => {
                    let client = http_client();
                    post_rpc(
                        &client,
                        cfg,
                        sid.as_deref(),
                        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
                    )
                    .await
                    .map(|(v, new_sid)| {
                        if new_sid.is_some() {
                            *sid = new_sid;
                        }
                        v
                    })
                }
            };
            match result {
                Ok(msg) => {
                    if let Some(err) = msg.get("error") {
                        return Err(format!("tools/list 失败: {err}"));
                    }
                    let result = msg.get("result").cloned().unwrap_or(Value::Null);
                    return Ok(parse_tools(&result));
                }
                Err(e) => {
                    // 传输层错误：丢弃会话重试一次（进程可能已退出 / HTTP 会话过期）
                    if let Some(mut s) = sessions.remove(&cfg.name) {
                        s.shutdown();
                    }
                    if attempt == 0 {
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        Err("MCP 连接失败".into())
    }

    /// 真实调用 MCP 工具（tools/call），返回文本结果。
    /// 会话不存在时自动建立；传输出错自动重建并重试一次。
    pub async fn call(&self, server: &str, tool: &str, args: Value) -> Result<String, String> {
        let cfg = self
            .servers
            .lock()
            .await
            .iter()
            .find(|s| s.name == server)
            .cloned()
            .ok_or_else(|| format!("MCP 服务「{server}」不存在"))?;
        if !cfg.enabled {
            return Err(format!("MCP 服务「{server}」已停用"));
        }
        let params = json!({"name": tool, "arguments": args});
        let mut sessions = self.sessions.lock().await;
        let mut last_err = String::from("MCP 调用失败");
        for attempt in 0..2 {
            if !sessions.contains_key(server) {
                match timeout(PROBE_TIMEOUT, establish(&cfg)).await {
                    Ok(Ok(s)) => {
                        sessions.insert(server.to_string(), s);
                    }
                    Ok(Err(e)) => {
                        last_err = e;
                        continue;
                    }
                    Err(_) => {
                        last_err = "连接 MCP 服务超时".into();
                        continue;
                    }
                }
            }
            let result = match sessions.get_mut(server).unwrap() {
                McpSession::Stdio {
                    stdin,
                    lines,
                    next_id,
                    ..
                } => {
                    timeout(
                        CALL_TIMEOUT,
                        stdio_rpc(stdin, lines, next_id, "tools/call", params.clone()),
                    )
                    .await
                    .unwrap_or_else(|_| Err("MCP 工具调用超时（60s）".into()))
                }
                McpSession::Http { sid } => {
                    let client = http_client();
                    post_rpc(
                        &client,
                        &cfg,
                        sid.as_deref(),
                        json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":params.clone()}),
                    )
                    .await
                    .map(|(v, new_sid)| {
                        if new_sid.is_some() {
                            *sid = new_sid;
                        }
                        v
                    })
                }
            };
            match result {
                Ok(msg) => {
                    if let Some(err) = msg.get("error") {
                        return Err(format!("tools/call 失败: {err}"));
                    }
                    let result = msg.get("result").cloned().unwrap_or(Value::Null);
                    return parse_call_result(&result);
                }
                Err(e) => {
                    if let Some(mut s) = sessions.remove(server) {
                        s.shutdown();
                    }
                    last_err = e;
                    if attempt == 0 {
                        continue;
                    }
                }
            }
        }
        Err(last_err)
    }
}

// ============================================================
// 会话建立与 RPC
// ============================================================

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
                    let input_schema = t
                        .get("inputSchema")
                        .filter(|s| s.is_object())
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"}));
                    Some(McpToolInfo {
                        name,
                        description,
                        input_schema,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// tools/call 结果 → 文本。isError=true 时作为错误返回给模型。
fn parse_call_result(result: &Value) -> Result<String, String> {
    let mut text = String::new();
    if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
        for item in content {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                }
                Some("image") => text.push_str("[图片内容已省略]"),
                Some("resource") => {
                    if let Some(r) = item.get("resource") {
                        if let Some(t) = r.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if text.is_empty() {
        if let Some(sc) = result.get("structuredContent") {
            text = serde_json::to_string(sc).unwrap_or_default();
        }
    }
    if result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(if text.is_empty() {
            "MCP 工具返回错误".into()
        } else {
            text
        });
    }
    Ok(if text.is_empty() {
        "（工具执行成功，无文本输出）".into()
    } else {
        text
    })
}

async fn send_line(stdin: &mut ChildStdin, line: String) -> Result<(), String> {
    stdin
        .write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|e| format!("写入 MCP 进程失败: {e}"))?;
    stdin
        .flush()
        .await
        .map_err(|e| format!("写入 MCP 进程失败: {e}"))
}

/// 建立持久会话：stdio 启动子进程并完成 initialize 握手；HTTP 拿到会话 id。
async fn establish(cfg: &McpServerConfig) -> Result<McpSession, String> {
    match cfg.transport.as_str() {
        "stdio" => {
            let mut cmd = Command::new(cfg.command.trim());
            cmd.args(&cfg.args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            for (k, v) in &cfg.env {
                cmd.env(k, v);
            }
            let mut child = cmd
                .spawn()
                .map_err(|e| format!("启动 MCP 进程失败: {e}"))?;
            let mut stdin = child.stdin.take().ok_or("无法写入 MCP 进程")?;
            let stdout = child.stdout.take().ok_or("无法读取 MCP 进程")?;
            let mut lines = BufReader::new(stdout).lines();
            let mut next_id = 1u64;

            send_line(&mut stdin, rpc_line(next_id, "initialize", init_params())).await?;
            let resp = read_response(&mut lines, next_id).await?;
            if let Some(err) = resp.get("error") {
                let _ = child.start_kill();
                return Err(format!("initialize 失败: {err}"));
            }
            next_id += 1;
            send_line(&mut stdin, rpc_notification("notifications/initialized")).await?;
            Ok(McpSession::Stdio {
                child,
                stdin,
                lines,
                next_id,
            })
        }
        "http" | "sse" => {
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
            Ok(McpSession::Http { sid })
        }
        other => Err(format!("不支持的传输方式: {other}")),
    }
}

/// 读 stdio 输出直到拿到指定 id 的响应（跳过通知/无关行）
async fn read_response(
    lines: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
) -> Result<Value, String> {
    loop {
        let line = match timeout(Duration::from_secs(8), lines.next_line()).await {
            Err(_) => return Err("等待 MCP 响应超时".into()),
            Ok(Ok(Some(l))) => l,
            Ok(Ok(None)) => return Err("MCP 进程已退出".into()),
            Ok(Err(e)) => return Err(format!("读取 MCP 输出失败: {e}")),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
            return Ok(msg);
        }
    }
}

async fn stdio_rpc(
    stdin: &mut ChildStdin,
    lines: &mut Lines<BufReader<ChildStdout>>,
    next_id: &mut u64,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let id = *next_id;
    *next_id += 1;
    send_line(stdin, rpc_line(id, method, params)).await?;
    read_response(lines, id).await
}

// ============================================================
// HTTP / SSE 传输
// ============================================================

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
        .timeout(Duration::from_secs(60))
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
    // 通知类请求可能返回 202 空体：视为成功
    if text.trim().is_empty() && status.is_success() {
        return Ok((Value::Null, sid));
    }
    let parsed = parse_http_body(&text)
        .ok_or_else(|| format!("MCP 返回无效响应（HTTP {status}）"))?;
    Ok((parsed, sid))
}