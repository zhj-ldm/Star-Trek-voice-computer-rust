//! 多引擎联网搜索（主 Agent 唯一的信息获取来源）。
//! 顺序 fallback：Bing RSS → 百度 → 360，限速 + GBK 容错。
//! 以同名工具 WebSearch 覆盖 SDK 内置占位实现。

use async_trait::async_trait;
use open_agent_sdk::types::{Tool, ToolError, ToolInputSchema, ToolResult, ToolUseContext};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// searchpin-ai 独立可执行 MCP server 的绝对路径（随项目内置在 resources/）。
const SEARCHPIN_BIN: &str = "/Users/zhj/Projects/star-trek-assistant/resources/searchpin-ai";

#[derive(Clone)]
pub struct SearchItem {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

struct HostLimiter {
    last: Instant,
}

pub struct MultiEngineSearcher {
    client: reqwest::Client,
    limiters: Mutex<HashMap<String, HostLimiter>>,
    min_interval: Duration,
}

impl Default for MultiEngineSearcher {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent(
                    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/126.0 Safari/537.36",
                )
                .build()
                .expect("reqwest client build"),
            limiters: Mutex::new(HashMap::new()),
            min_interval: Duration::from_millis(600),
        }
    }
}

impl MultiEngineSearcher {
    async fn pace(&self, host: &str) {
        let mut map = self.limiters.lock().await;
        let entry = map.entry(host.to_string()).or_insert(HostLimiter {
            last: Instant::now() - self.min_interval,
        });
        let elapsed = entry.last.elapsed();
        if elapsed < self.min_interval {
            tokio::time::sleep(self.min_interval - elapsed).await;
        }
        entry.last = Instant::now();
    }

    /// 执行搜索，多引擎顺序 fallback
    pub async fn search(&self, query: &str, max: usize) -> Result<Vec<SearchItem>, String> {
        // Bing RSS
        match self.search_bing(query, max).await {
            Ok(items) if !items.is_empty() => return Ok(items),
            Ok(_) => {}
            Err(e) => tracing::warn!("Bing 搜索失败: {e}"),
        }
        // 百度
        match self.search_baidu(query, max).await {
            Ok(items) if !items.is_empty() => return Ok(items),
            Ok(_) => {}
            Err(e) => tracing::warn!("百度搜索失败: {e}"),
        }
        // 360
        match self.search_360(query, max).await {
            Ok(items) if !items.is_empty() => return Ok(items),
            Ok(_) => {}
            Err(e) => tracing::warn!("360 搜索失败: {e}"),
        }
        Err("所有搜索引擎均未返回结果".into())
    }

    async fn search_bing(&self, query: &str, max: usize) -> Result<Vec<SearchItem>, String> {
        self.pace("bing.com").await;
        let url = format!(
            "https://www.bing.com/search?q={}&format=rss&count={}",
            urlencode(query),
            max.min(15)
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .text()
            .await
            .map_err(|e| e.to_string())?;
        parse_rss(&resp, max)
    }

    async fn search_baidu(&self, query: &str, max: usize) -> Result<Vec<SearchItem>, String> {
        self.pace("baidu.com").await;
        let url = format!("https://www.baidu.com/s?wd={}", urlencode(query));
        let bytes = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .bytes()
            .await
            .map_err(|e| e.to_string())?;
        let html = decode_gbk_or_utf8(&bytes);
        parse_html_links(&html, max, "baidu")
    }

    async fn search_360(&self, query: &str, max: usize) -> Result<Vec<SearchItem>, String> {
        self.pace("so.com").await;
        let url = format!("https://www.so.com/s?q={}", urlencode(query));
        let bytes = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .bytes()
            .await
            .map_err(|e| e.to_string())?;
        let html = decode_gbk_or_utf8(&bytes);
        parse_html_links(&html, max, "360")
    }
}

// ============================================================
// SearchpinSearcher — 子进程 MCP stdio 客户端（searchpin-ai）
// 四引擎并行 + 本地 embedding 语义重排，零 API Key。
// 通过标准 MCP Content-Length 帧协议与 searchpin-ai 可执行文件通信。
// ============================================================

struct SearchpinProc {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl SearchpinProc {
    async fn spawn() -> Result<Self, String> {
        let mut cmd = Command::new(SEARCHPIN_BIN);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().map_err(|e| {
            format!("无法启动 searchpin-ai（{}）: {e}", SEARCHPIN_BIN)
        })?;
        let stdin = child.stdin.take().ok_or("searchpin-ai stdin 不可用")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("searchpin-ai stdout 不可用")?);

        let mut proc = Self {
            child,
            stdin,
            stdout,
        };
        // MCP 握手
        proc.rpc(
            1,
            "initialize",
            &json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "star-trek-assistant", "version": "1"}}),
        )
        .await?;
        proc.write_frame(&json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}))
            .await?;
        Ok(proc)
    }

    async fn write_frame(&mut self, obj: &Value) -> Result<(), String> {
        let data = serde_json::to_vec(obj).map_err(|e| e.to_string())?;
        let header = format!("Content-Length: {}\r\n\r\n", data.len());
        self.stdin
            .write_all(header.as_bytes())
            .await
            .map_err(|e| format!("searchpin-ai 写入失败: {e}"))?;
        self.stdin
            .write_all(&data)
            .await
            .map_err(|e| format!("searchpin-ai 写入失败: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("searchpin-ai 写入失败: {e}"))?;
        Ok(())
    }

    async fn read_response(&mut self) -> Result<Value, String> {
        // 读 header 行直到空行
        let mut content_length: usize = 0;
        loop {
            let mut line = String::new();
            let n = self
                .stdout
                .read_line(&mut line)
                .await
                .map_err(|e| format!("searchpin-ai 读取失败: {e}"))?;
            if n == 0 {
                return Err("searchpin-ai 进程已退出".to_string());
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some(v) = trimmed
                .split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .map(|(_, v)| v.trim())
            {
                content_length = v.parse().unwrap_or(0);
            }
        }
        if content_length == 0 {
            return Err("searchpin-ai 响应缺少 Content-Length".to_string());
        }
        let mut buf = vec![0u8; content_length];
        self.stdout
            .read_exact(&mut buf)
            .await
            .map_err(|e| format!("searchpin-ai 响应体读取失败: {e}"))?;
        serde_json::from_slice(&buf).map_err(|e| format!("searchpin-ai 响应解析失败: {e}"))
    }

    async fn rpc(&mut self, id: u64, method: &str, params: &Value) -> Result<Value, String> {
        self.write_frame(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let resp = self.read_response().await?;
        if let Some(err) = resp.get("error") {
            return Err(format!(
                "searchpin-ai 错误 {}: {}",
                err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1),
                err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown")
            ));
        }
        resp.get("result").cloned().ok_or_else(|| "searchpin-ai 响应缺 result".to_string())
    }

    async fn web_search(&mut self, query: &str, max: usize) -> Result<Vec<SearchItem>, String> {
        let result = self
            .rpc(
                2,
                "tools/call",
                &json!({
                    "name": "web_search",
                    "arguments": {"query": query, "max_results": max}
                }),
            )
            .await?;
        let text = result
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| "searchpin-ai 返回内容为空".to_string())?;
        let data: Value =
            serde_json::from_str(text).map_err(|e| format!("searchpin-ai 结果解析失败: {e}"))?;
        let mut items = Vec::new();
        if let Some(results) = data.get("results").and_then(|r| r.as_array()) {
            for r in results {
                let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let mut snippet = r.get("snippet").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if snippet.is_empty() {
                    snippet = r.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                }
                let engine = r.get("_source_engine").and_then(|v| v.as_str()).unwrap_or("");
                if !title.is_empty() && !url.is_empty() {
                    items.push(SearchItem {
                        title: format!("[{}] {}", engine, title),
                        url,
                        snippet: snippet.split_whitespace().collect::<Vec<_>>().join(" "),
                    });
                }
            }
        }
        Ok(items)
    }

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

pub struct SearchpinSearcher {
    inner: Mutex<Option<SearchpinProc>>,
}

impl Default for SearchpinSearcher {
    fn default() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }
}

impl SearchpinSearcher {
    /// 执行搜索；失败（含子进程异常）时自动清理并返回 Err，供上层回退。
    pub async fn search(&self, query: &str, max: usize) -> Result<Vec<SearchItem>, String> {
        let mut guard = self.inner.lock().await;
        let result = tokio::time::timeout(Duration::from_secs(45), async {
            if guard.is_none() {
                match SearchpinProc::spawn().await {
                    Ok(p) => *guard = Some(p),
                    Err(e) => return Err(e),
                }
            }
            let proc = guard.as_mut().unwrap();
            proc.web_search(query, max).await
        })
        .await;

        match result {
            Ok(Ok(items)) => Ok(items),
            Ok(Err(e)) => {
                // 通信异常：重建子进程，返回错误交由上层回退
                if let Some(mut p) = guard.take() {
                    p.kill().await;
                }
                Err(e)
            }
            Err(_) => {
                if let Some(mut p) = guard.take() {
                    p.kill().await;
                }
                Err("searchpin-ai 搜索超时".to_string())
            }
        }
    }
}

// ============================================================
// WebSearch 自定义工具（覆盖 SDK 占位）
// ============================================================

pub struct WebSearchTool {
    searcher: MultiEngineSearcher,
    searchpin: SearchpinSearcher,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self {
            searcher: MultiEngineSearcher::default(),
            searchpin: SearchpinSearcher::default(),
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "WebSearch"
    }
    fn description(&self) -> &str {
        "联网搜索获取最新信息。输入搜索关键词（query），返回相关网页标题、链接与摘要。"
    }
    fn input_schema(&self) -> ToolInputSchema {
        ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::from([
                (
                    "query".to_string(),
                    json!({"type": "string", "description": "搜索关键词"}),
                ),
                (
                    "max_results".to_string(),
                    json!({"type": "number", "description": "最多返回条数（默认5）"}),
                ),
            ]),
            required: vec!["query".to_string()],
            additional_properties: Some(false),
        }
    }
    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }
    async fn call(&self, input: Value, _ctx: &ToolUseContext) -> Result<ToolResult, ToolError> {
        let query = input
            .get("query")
            .and_then(|q| q.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if query.is_empty() {
            return Ok(ToolResult::error("缺少搜索关键词"));
        }
        let max = input.get("max_results").and_then(|m| m.as_u64()).unwrap_or(5) as usize;
        let max = max.clamp(1, 20);
        // 优先走 searchpin-ai（四引擎 + 语义重排），失败/不可用时回退自研多引擎
        match self.searchpin.search(&query, max).await {
            Ok(items) if !items.is_empty() => {
                let mut out = String::new();
                for (i, it) in items.iter().enumerate() {
                    out.push_str(&format!(
                        "{}. {}\n   {}\n   {}\n",
                        i + 1,
                        it.title,
                        it.url,
                        it.snippet
                    ));
                }
                Ok(ToolResult::text(out))
            }
            _ => match self.searcher.search(&query, max).await {
                Ok(items) => {
                    let mut out = String::new();
                    for (i, it) in items.iter().enumerate() {
                        out.push_str(&format!(
                            "{}. {}\n   {}\n   {}\n",
                            i + 1,
                            it.title,
                            it.url,
                            it.snippet
                        ));
                    }
                    Ok(ToolResult::text(out))
                }
                Err(e) => Ok(ToolResult::error(format!("搜索失败: {e}"))),
            },
        }
    }
}

// ============================================================
// 解析辅助
// ============================================================

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_gbk_or_utf8(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(bytes).to_string(),
    }
}

/// 解析 Bing RSS XML
fn parse_rss(xml: &str, max: usize) -> Result<Vec<SearchItem>, String> {
    let mut items = Vec::new();
    for entry in xml.split("<item>").skip(1) {
        let title = tag_text(entry, "title");
        let link = tag_text(entry, "link");
        let desc = tag_text(entry, "description");
        if !link.is_empty() {
            items.push(SearchItem {
                title: strip_tags(&title),
                url: link,
                snippet: strip_tags(&desc).chars().take(200).collect(),
            });
        }
        if items.len() >= max {
            break;
        }
    }
    Ok(items)
}

fn tag_text(xml: &str, tag: &str) -> String {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    if let Some(i) = xml.find(&start) {
        let rest = &xml[i + start.len()..];
        if let Some(j) = rest.find(&end) {
            return rest[..j].to_string();
        }
    }
    String::new()
}

/// 解析百度/360 的 HTML 搜索结果（结果容器 <h3><a href=...>标题</a></h3> + 摘要）
fn parse_html_links(html: &str, max: usize, _engine: &str) -> Result<Vec<SearchItem>, String> {
    let mut items = Vec::new();
    let lower = html.to_lowercase();
    let mut idx = 0usize;
    while items.len() < max {
        let next = lower[idx..].find("<h3");
        let Some(rel) = next else { break };
        idx += rel;
        let start = idx;
        // 找 <a ... href="...">
        let Some(a_start) = lower[idx..].find("<a") else { break };
        let a_start = idx + a_start;
        let Some(href_start) = lower[a_start..].find("href=") else {
            idx = start + 3;
            continue;
        };
        let href_start = a_start + href_start + 5;
        let quote = lower.as_bytes()[href_start];
        let href_end = if quote == b'"' || quote == b'\'' {
            lower[href_start + 1..].find(quote as char).map(|p| href_start + 1 + p)
        } else {
            lower[href_start..].find([' ', '>']).map(|p| href_start + p)
        };
        let Some(href_end) = href_end else { break };
        // href_start 指向引号本身（引号情形内容从引号后开始），
        // 原实现 html[href_start..href_end] 会把前导引号带入 URL，导致 360 链接变成
        // "https://\"https://..." 的坏链接。
        let raw_url = if quote == b'"' || quote == b'\'' {
            &html[href_start + 1..href_end]
        } else {
            &html[href_start..href_end]
        };
        let url = normalize_url(raw_url);
        // 到 </h3> 结束，提取标题文本
        let Some(h3_end) = lower[idx..].find("</h3>") else { break };
        let title_html = &html[idx..idx + h3_end];
        let title = strip_tags(title_html).trim().to_string();
        // 摘要：找后续 <div ...> 或 <p ...> 文本（简化取 h3 后 600 字符内文本）
        let snippet = extract_snippet(html, idx + h3_end);
        if !url.is_empty() && !title.is_empty() && !is_junk_title(&title) {
            items.push(SearchItem {
                title,
                url,
                snippet,
            });
        }
        idx = idx + h3_end + 5;
    }
    Ok(items)
}

/// 过滤搜索结果页里的导航/推荐/广告等垃圾条目（如 360 的“其他人还搜了”）
fn is_junk_title(t: &str) -> bool {
    const JUNK: &[&str] = &[
        "其他人还搜了",
        "相关搜索",
        "猜你想搜",
        "大家都在搜",
        "百度热榜",
        "热搜",
        "广告",
        "搜索工具",
    ];
    JUNK.iter().any(|k| t.contains(k))
}

fn normalize_url(raw: &str) -> String {
    // 防御：去掉可能残留的前导引号/空白（部分页面 href 提取会带引号）
    let raw = raw.trim().trim_matches('"').trim_matches('\'');
    if raw.is_empty() {
        return String::new();
    }
    // 百度跳转链接 /link?url=...：补全完整跳转地址。
    // 原实现只返回 url= 后的裸 token（丢失 https://www.baidu.com/link?url= 前缀），
    // 导致结果链接完全不可用，是“搜索出来都是什么鬼”的主因之一。
    if raw.contains("/link?url=") {
        if let Some(q) = raw.split("url=").nth(1) {
            let u = q.split('&').next().unwrap_or("");
            if !u.is_empty() {
                return format!("https://www.baidu.com/link?url={}", percent_decode(u));
            }
        }
    }
    // 360 等已带协议的完整跳转链接（/link?m=...）原样保留
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn extract_snippet(html: &str, from: usize) -> String {
    let rest = &html[from.min(html.len())..];
    // 优先从常见摘要容器中提取：
    // 百度 c-abstract / content-right，360 res-desc / res-comm-con。
    // 原实现只取 h3 后固定 600 字符 strip 文本，百度摘要不紧跟 h3 导致摘要为空。
    for key in [
        "class=\"c-abstract\"",
        "class=\"content-right",
        "class=\"res-desc\"",
        "class=\"res-comm-con\"",
    ] {
        if let Some(p) = rest.find(key) {
            let start = p + key.len();
            let block = &rest[start..];
            let end = block.find("</div>").unwrap_or(block.len().min(400));
            let txt = strip_tags(&block[..end]);
            let txt = txt.trim();
            if txt.chars().count() > 8 {
                return txt.chars().take(200).collect();
            }
        }
    }
    // 回退：较大窗口内取文本
    let win = &rest[..rest.len().min(1500)];
    let text = strip_tags(win);
    text.chars().take(200).collect()
}
